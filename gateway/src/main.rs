//! Feraro gateway — the WebSocket edge between the browser and the engine.
//!
//! Four kinds of task:
//!
//! - **feeds** — Binance, Kraken, Twelve Data and Finnhub — push observations
//!   at the **aggregator**, which picks a source per instrument and pushes
//!   two-sided quotes at the engine
//! - **engine** owns every piece of trading state and applies commands in order
//! - **sessions**, one per browser, forward client commands and relay
//!   broadcasts back
//!
//! Nothing but the engine task touches the portfolio, so the single-writer
//! guarantee holds all the way from the socket to the journal.

mod auth;
mod candles;
mod engine;
mod feeds;
mod instruments;
mod markups;
mod payments;
mod protocol;
mod snapshot;
mod totp;

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use domain::{AccountId, UserId};
use futures_util::{SinkExt, StreamExt};
use money::{Money, Price, Qty};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use crate::auth::Role;
use crate::engine::{
    amend_reason_from, AdminAction, AdminOutcome, Amendment, Broadcast, Command, Engine,
    EngineConfig, FLUSH_INTERVAL,
};
use crate::markups::Markups;
use crate::payments::{Payments, Rail};
use crate::protocol::{
    instrument_id, kind_from, price_str, side_from, AccountSummaryDto, AdminUserDto, BarDto,
    ClientMessage, DepositRequestDto, MarkupDto, PaymentMethodDto, PayoutMethodDto, PowerDto,
    ServerMessage,
};

/// The most audit lines the gateway will send in one answer.
const AUDIT_PAGE: usize = 200;

/// The most statement lines the gateway will send in one answer.
const STATEMENT_PAGE: usize = 200;

/// Where the browser connects.
/// Where the browser socket listens, unless `FERARO_BIND` says otherwise.
///
/// Loopback by default on purpose: a gateway that binds every interface the
/// moment it starts is one plain `ws://` away from the internet. A
/// deployment sets this to `0.0.0.0:8081` and puts TLS in front of it.
const BIND: &str = "127.0.0.1:8081";

/// What an authenticator app calls this platform.
///
/// It appears in the app's list beside the login, so it has to say which
/// platform a code belongs to. A deployment running several brands from one
/// server would want this per server; until then it is the project's name.
const ISSUER: &str = "Feraro";

/// Where the health endpoint listens. Same reasoning, and never public.
const HEALTH_BIND: &str = "127.0.0.1:8082";
/// Depth of the command channel. Quotes are dropped when it fills, which is
/// correct: the next tick supersedes the one that did not fit.
const COMMAND_QUEUE: usize = 4096;
/// Depth of the broadcast channel, per session.
const BROADCAST_QUEUE: usize = 256;

#[tokio::main]
async fn main() -> Result<()> {
    // rustls will not pick a crypto backend for us, and the upstream TLS
    // handshake panics in a worker thread if none is installed. Do it first,
    // before anything can try to connect.
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("a rustls crypto provider was already installed"))?;

    // `gateway --healthcheck` asks the running process how it is and exits
    // on the answer. It lives in this binary because the runtime image has
    // no curl and should not grow one: a container that can make arbitrary
    // HTTP requests is a container with a tool an attacker would enjoy.
    if std::env::args().any(|a| a == "--healthcheck") {
        let at = std::env::var("FERARO_HEALTH_BIND").unwrap_or_else(|_| HEALTH_BIND.to_owned());
        return match ask_health(&at).await {
            Ok(true) => Ok(()),
            Ok(false) => std::process::exit(1),
            Err(err) => {
                eprintln!("healthcheck: {err:#}");
                std::process::exit(1)
            }
        };
    }

    // API keys and settings from gateway/.env, if it exists. Real environment
    // variables win over the file, so a deployment can override it.
    match dotenvy::dotenv() {
        Ok(path) => println!("gateway: loaded settings from {}", path.display()),
        Err(e) if e.not_found() => {}
        Err(e) => eprintln!("gateway: could not read .env: {e}"),
    }

    // A directory of numbered segments, not one file. The first argument
    // chooses it; everything inside is named `<base>.NNNNNN.jnl`.
    let journal_dir = std::env::args()
        .nth(1)
        .filter(|a| !a.starts_with("--"))
        .map(PathBuf::from)
        .or_else(|| std::env::var("FERARO_JOURNAL").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("journal"));
    let journal_base = "feraro";

    // Segment size is an operational knob, not a constant: eight megabytes is
    // right for a running desk and useless for a test that needs to see a roll
    // happen in ten seconds.
    let segment_bytes = std::env::var("FERARO_SEGMENT_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(journal::DEFAULT_SEGMENT_BYTES);

    // The longest a quote may stay unsynced. Decisions never wait for this —
    // they are synced before they are announced, whatever it is set to.
    let sync_ms = std::env::var("FERARO_SYNC_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(200);

    // UTC hour of the daily rollover. UTC because a schedule that follows a
    // server's timezone runs twice on the night the clocks change.
    let rollover_hour = std::env::var("FERARO_ROLLOVER_HOUR")
        .ok()
        .and_then(|v| v.parse::<u8>().ok())
        .filter(|h| *h < 24)
        .unwrap_or(22);

    // How many events between snapshots. Snapshots bound how much of the
    // journal's tail a restart has to re-fold; zero turns them off, and a
    // journal with none replays from genesis as it always did.
    let snapshot_every = std::env::var("FERARO_SNAPSHOT_EVERY")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(snapshot::SNAPSHOT_EVERY);

    let config = EngineConfig {
        segment_bytes,
        snapshot_every,
        durability: journal::Durability {
            interval: Duration::from_millis(sync_ms),
        },
        rollover_hour,
    };

    // Who may sign in. Credentials live here rather than in the journal —
    // see the note in `auth`. The file is created with a starter login the
    // first time the gateway runs, and the password is printed once.
    let users_path = std::env::var("FERARO_USERS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| journal_dir.join("users.json"));
    let (users, seeded) = auth::Users::load_or_seed(&users_path)
        .with_context(|| format!("opening the user store at {}", users_path.display()))?;
    if !seeded.is_empty() {
        println!("\n  A user store was created at {}.", users_path.display());
        println!("  These passwords are shown once and are not recoverable:\n");
        for (login, password) in &seeded {
            println!("      {login:<8} {password}");
        }
        println!("\n  Sign in at the terminal and change them.\n");
    } else {
        println!(
            "auth: {} user(s) from {}",
            users.all().len(),
            users_path.display()
        );
    }
    let payments_path = std::env::var("FERARO_PAYMENTS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| journal_dir.join("payments.json"));
    let payments = Payments::load(&payments_path)
        .with_context(|| format!("reading {}", payments_path.display()))?;
    println!(
        "payments: {} method(s) from {}",
        payments.methods().len(),
        payments_path.display()
    );

    let markups_path = std::env::var("FERARO_MARKUPS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| journal_dir.join("markups.json"));
    let markups = Arc::new(Mutex::new(
        Markups::load(&markups_path)
            .with_context(|| format!("reading {}", markups_path.display()))?,
    ));
    {
        let held = markups.lock().expect("the markup lock");
        println!(
            "markups: {} instrument(s) marked up, from {}",
            held.all().count(),
            markups_path.display()
        );
    }

    let gate = Gate {
        users: Arc::new(Mutex::new(users)),
        markups: markups.clone(),
        sessions: Arc::new(Mutex::new(auth::Sessions::new())),
        payments: Arc::new(Mutex::new(payments)),
    };

    let (events_tx, _) = broadcast::channel::<Broadcast>(BROADCAST_QUEUE);
    let (commands_tx, commands_rx) = mpsc::channel::<Command>(COMMAND_QUEUE);

    let engine = Engine::open(&journal_dir, journal_base, config, events_tx.clone())
        .with_context(|| format!("opening the engine in {}", journal_dir.display()))?;
    println!(
        "engine: journal {} · segments roll at {segment_bytes} bytes · quotes sync within {sync_ms} ms, decisions before announcement",
        journal_dir.display()
    );

    tokio::spawn(engine.run(commands_rx));
    let pulse = Arc::new(Pulse::default());
    spawn_feeds(commands_tx.clone(), pulse.clone(), gate.markups.clone());

    // The flush tick is what turns hundreds of upstream ticks a second into
    // ten broadcasts a second.
    {
        let tx = commands_tx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if tx.send(Command::Flush).await.is_err() {
                    break;
                }
            }
        });
    }

    let bind = std::env::var("FERARO_BIND").unwrap_or_else(|_| BIND.to_owned());
    let addr: SocketAddr = bind.parse().context("parsing the bind address")?;

    // A supervisor needs to know this process is alive and still being
    // priced, not merely that it is running: a gateway whose feeds stalled
    // is up and useless, and that is the outage worth catching.
    let health = std::env::var("FERARO_HEALTH_BIND").unwrap_or_else(|_| HEALTH_BIND.to_owned());
    if health != "off" {
        let watch = pulse.clone();
        let started = Instant::now();
        match health.parse::<SocketAddr>() {
            Ok(at) => {
                tokio::spawn(async move {
                    if let Err(err) = serve_health(at, watch, started).await {
                        eprintln!("health: {err:#}");
                    }
                });
            }
            Err(err) => eprintln!("health: cannot parse {health}: {err}"),
        }
    }
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    println!("gateway: listening on ws://{addr}");

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = accepted.context("accepting a connection")?;
                let commands = commands_tx.clone();
                let events = events_tx.subscribe();
                let gate = gate.clone();
                tokio::spawn(async move {
                    if let Err(err) = session(stream, peer, gate, commands, events).await {
                        eprintln!("session {peer}: {err:#}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                println!("gateway: shutting down");
                // Give the engine a moment to flush the journal.
                tokio::time::sleep(Duration::from_millis(200)).await;
                return Ok(());
            }
        }
    }
}

/// Starts every provider that can run, and the aggregator between them and
/// the engine.
/// How many prices have reached the engine, and when the last one did.
///
/// Shared between the feed task and the health endpoint. Relaxed ordering
/// throughout: this is a liveness signal read once a few seconds by a
/// supervisor, not a sequence anything depends on.
#[derive(Debug, Default)]
struct Pulse {
    quotes: AtomicU64,
    /// Epoch milliseconds of the last quote, or zero before the first.
    last: AtomicI64,
}

impl Pulse {
    fn beat(&self) {
        self.quotes.fetch_add(1, Ordering::Relaxed);
        self.last.store(now_ms(), Ordering::Relaxed);
    }
}

fn spawn_feeds(commands: mpsc::Sender<Command>, pulse: Arc<Pulse>, markups: Arc<Mutex<Markups>>) {
    use crate::feeds::aggregator::Aggregator;
    use crate::feeds::{self, binance, finnhub, kraken, twelvedata, FeedUpdate, Source};
    use crate::instruments::{catalogue, subscriptions};

    let listings = catalogue();
    let routes = listings
        .iter()
        .map(|l| (l.terms.instrument.id, l.route()))
        .collect();

    let (updates_tx, mut updates_rx) = mpsc::channel::<FeedUpdate>(8_192);

    let binance_subs = subscriptions(&listings, Source::BINANCE);
    if !binance_subs.is_empty() {
        println!("feed binance: {} symbols", binance_subs.len());
        tokio::spawn(feeds::run(
            binance::Binance {
                subscriptions: binance_subs,
            },
            updates_tx.clone(),
        ));
    }

    let kraken_subs = subscriptions(&listings, Source::KRAKEN);
    if !kraken_subs.is_empty() {
        println!("feed kraken: {} symbols", kraken_subs.len());
        tokio::spawn(feeds::run(
            kraken::Kraken {
                subscriptions: kraken_subs,
            },
            updates_tx.clone(),
        ));
    }

    match key("TWELVEDATA_API_KEY") {
        Some(api_key) => {
            let cap = limit("TWELVEDATA_MAX_SYMBOLS", 8);
            let mut subs = subscriptions(&listings, Source::TWELVE_DATA);
            report_cap("twelvedata", &mut subs, cap);
            tokio::spawn(feeds::run(
                twelvedata::TwelveData {
                    api_key,
                    subscriptions: subs,
                },
                updates_tx.clone(),
            ));
        }
        None => println!(
            "feed twelvedata: off — set TWELVEDATA_API_KEY in gateway/.env to enable FX and metals"
        ),
    }

    match key("FINNHUB_API_KEY") {
        Some(api_key) => {
            let cap = limit("FINNHUB_MAX_SYMBOLS", 50);
            let mut subs = subscriptions(&listings, Source::FINNHUB);
            report_cap("finnhub", &mut subs, cap);
            tokio::spawn(feeds::run(
                finnhub::Finnhub {
                    api_key,
                    subscriptions: subs,
                },
                updates_tx.clone(),
            ));
        }
        None => println!(
            "feed finnhub: off — set FINNHUB_API_KEY in gateway/.env to enable it as a backup"
        ),
    }

    tokio::spawn(async move {
        let mut aggregator = Aggregator::new(routes);
        let mut rejected = 0u64;
        while let Some(update) = updates_rx.recv().await {
            match aggregator.observe(
                update.instrument,
                update.source,
                update.observation,
                std::time::Instant::now(),
            ) {
                Ok(Some(quote)) => {
                    if quote.switched {
                        println!(
                            "aggregator: instrument {} now priced by {}",
                            quote.instrument.0,
                            quote.source.name()
                        );
                    }
                    pulse.beat();
                    // Read per tick rather than cached, so a markup the
                    // desk changes takes effect on the next price rather
                    // than on the next restart.
                    let (markup, commission) = {
                        let held = markups.lock().expect("the markup lock");
                        (
                            held.of(quote.instrument),
                            held.commission_of(quote.instrument),
                        )
                    };
                    let _ = commands.try_send(Command::Quote {
                        instrument: quote.instrument,
                        bid: quote.bid,
                        ask: quote.ask,
                        source: quote.source,
                        markup,
                        commission,
                    });
                }
                Ok(None) => {}
                Err(reason) => {
                    rejected += 1;
                    // Report the first and then every thousandth, so a feed
                    // sending garbage is visible without flooding the log.
                    if rejected == 1 || rejected % 1_000 == 0 {
                        eprintln!(
                            "aggregator: refused a quote from {} ({reason:?}); {rejected} refused so far",
                            update.source.name()
                        );
                    }
                }
            }
        }
    });
}

/// Asks the health endpoint whether the platform is well.
///
/// A deliberately tiny HTTP client: one request, read the status line,
/// decide. Anything more would be a dependency the runtime image carries
/// for a check it performs every twenty seconds.
async fn ask_health(at: &str) -> Result<bool> {
    // A container asks itself, so a bind of 0.0.0.0 is reached on loopback.
    let target = at.replace("0.0.0.0", "127.0.0.1");
    let addr: SocketAddr = target.parse().context("parsing the health address")?;
    let mut stream = tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(addr))
        .await
        .context("the health endpoint did not answer in time")?
        .context("connecting to the health endpoint")?;

    tokio::io::AsyncWriteExt::write_all(
        &mut stream,
        b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await
    .context("asking the health endpoint")?;

    let mut head = [0u8; 64];
    let read = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::io::AsyncReadExt::read(&mut stream, &mut head),
    )
    .await
    .context("the health endpoint stopped answering")?
    .context("reading the health endpoint")?;

    Ok(String::from_utf8_lossy(&head[..read]).starts_with("HTTP/1.1 200"))
}

/// Epoch milliseconds.
///
/// Only the health endpoint uses this. Trade time comes from the engine,
/// which journals the value it read so a replay never asks the clock again;
/// this is a liveness reading and belongs to nothing.
/// Seconds since the epoch, for the one thing that needs wall-clock time.
///
/// A one-time password is a function of the time on two clocks that have
/// never met, so this is the real one rather than a monotonic reading.
fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// How long without a price before the platform calls itself unhealthy.
///
/// Long enough that a quiet minute on a thin instrument does not page
/// anybody, short enough that a feed which died in the night is found
/// before the desk opens.
const STALE_AFTER_MS: i64 = 120_000;

/// The health endpoint: one route, no dependencies, no framing library.
///
/// It answers `200` while prices are arriving and `503` once they have
/// stopped, because "the process is running" is not the question a
/// supervisor is asking — a gateway whose feeds stalled is up and useless.
///
/// Deliberately a second listener rather than a route on the browser
/// socket: the socket speaks WebSocket from its first byte, and teaching it
/// to also speak HTTP would put a parser in front of the thing that holds
/// the session.
async fn serve_health(at: SocketAddr, pulse: Arc<Pulse>, started: Instant) -> Result<()> {
    let listener = TcpListener::bind(at)
        .await
        .with_context(|| format!("binding the health endpoint to {at}"))?;
    println!("health: listening on http://{at}/health");

    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            continue;
        };
        let pulse = pulse.clone();
        tokio::spawn(async move {
            // Read and discard the request. Nothing here varies by route or
            // method: a health endpoint that can be asked for the wrong
            // thing is a health endpoint with a bug in it.
            let mut scratch = [0u8; 1024];
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                tokio::io::AsyncReadExt::read(&mut stream, &mut scratch),
            )
            .await;

            let quotes = pulse.quotes.load(Ordering::Relaxed);
            let last = pulse.last.load(Ordering::Relaxed);
            let now = now_ms();
            let since = if last == 0 { -1 } else { now - last };
            let fresh = last != 0 && since < STALE_AFTER_MS;

            let body = format!(
                "{{\"ok\":{},\"uptimeSeconds\":{},\"quotes\":{},\"msSinceQuote\":{}}}",
                fresh,
                started.elapsed().as_secs(),
                quotes,
                since
            );
            let status = if fresh {
                "200 OK"
            } else {
                "503 Service Unavailable"
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await;
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut stream).await;
        });
    }
}

/// A non-empty environment value, trimmed.
fn key(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty() && !v.starts_with("your_"))
}

fn limit(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Trims a subscription list to a plan's cap and says what was dropped.
fn report_cap(label: &str, subs: &mut Vec<(String, domain::InstrumentId)>, cap: usize) {
    if subs.len() > cap {
        let dropped: Vec<String> = subs.drain(cap..).map(|(s, _)| s).collect();
        println!(
            "feed {label}: {} symbols (plan cap {cap}); not subscribed: {}",
            subs.len(),
            dropped.join(", ")
        );
    } else {
        println!("feed {label}: {} symbols", subs.len());
    }
}

/// The writing half of one browser's socket.
type Sink = futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<TcpStream>, Message>;

/// Sends one message to one terminal.
async fn send(sink: &mut Sink, message: &ServerMessage) -> Result<()> {
    let text = serde_json::to_string(message).context("serialising a server message")?;
    sink.send(Message::Text(text.into()))
        .await
        .context("writing to the socket")
}

/// The shared sign-in state, cloned into every connection.
#[derive(Clone)]
struct Gate {
    users: Arc<Mutex<auth::Users>>,
    sessions: Arc<Mutex<auth::Sessions>>,
    /// How money may be paid in, and the claims waiting to be checked.
    payments: Arc<Mutex<Payments>>,
    /// What the house adds to each feed price.
    markups: Arc<Mutex<Markups>>,
}

/// What one connection is allowed to do, once it has signed in.
struct Signed {
    token: String,
    user: UserId,
    login: String,
    role: Role,
    /// Accounts this session may act on.
    allowed: Vec<AccountId>,
    /// What this login has been given, read when it signed in.
    ///
    /// Held on the session rather than looked up per message, so one
    /// action cannot be judged against two different sets of rules because
    /// an administrator clicked something halfway through. The cost is
    /// that a change lands on the manager's next sign-in, which is also
    /// the behaviour an operator can reason about.
    powers: BTreeSet<auth::Power>,
    /// The one it is acting on.
    selected: AccountId,
}

impl Signed {
    /// Whether this session may act on an account.
    ///
    /// A client may act on what they hold, and a manager on what the clients
    /// assigned to them hold — the same check over a wider list, because a
    /// manager's limit is *whose accounts*, not which buttons. An
    /// administrator may act on any account that exists, which is the point
    /// of the role, and why the audit log matters: every such action is
    /// journaled with the name of whoever took it.
    fn may_act_on(&self, account: AccountId, exists: bool) -> bool {
        match self.role {
            Role::Client | Role::Manager => self.allowed.contains(&account),
            Role::Admin => exists,
        }
    }

    /// Whether this session may do a thing only an administrator may do.
    ///
    /// Money in and out, identity, roles, and creating a login. A manager
    /// works a book; these are the powers that make one.
    const fn is_admin(&self) -> bool {
        matches!(self.role, Role::Admin)
    }
}

/// One browser connection: signs in first, trades second.
async fn session(
    stream: TcpStream,
    peer: SocketAddr,
    gate: Gate,
    commands: mpsc::Sender<Command>,
    mut events: broadcast::Receiver<Broadcast>,
) -> Result<()> {
    let address = engine::address_bytes(Some(peer));
    let ws = tokio_tungstenite::accept_async(stream)
        .await
        .context("websocket handshake")?;
    let (mut sink, mut source) = ws.split();

    // Say so immediately, rather than leaving the terminal waiting for a
    // snapshot that will not come until it signs in.
    send(&mut sink, &ServerMessage::AuthRequired).await?;

    let mut signed: Option<Signed> = None;

    loop {
        tokio::select! {
            incoming = source.next() => {
                let Some(message) = incoming else { break };
                let message = message.context("reading from the socket")?;
                let text = match message {
                    Message::Text(text) => text,
                    Message::Ping(payload) => { sink.send(Message::Pong(payload)).await?; continue }
                    Message::Close(_) => break,
                    _ => continue,
                };
                let Ok(client) = serde_json::from_str::<ClientMessage>(text.as_str()) else {
                    // A terminal that sends nonsense is a bug in the terminal,
                    // and the engine is not the place to negotiate about it.
                    continue;
                };

                match client {
                    ClientMessage::Login {
                        login,
                        password,
                        code,
                    } => {
                        match sign_in(&gate, &login, &password, &code, &commands).await? {
                            Ok(session) => {
                                note_session(
                                    &commands,
                                    session.user,
                                    domain::SessionEvent::Opened,
                                    address,
                                )
                                .await;
                                let welcome = welcome_for(&session, &gate, &commands).await?;
                                send(&mut sink, &welcome).await?;
                                send(&mut sink, &verification_of(&gate, session.user)).await?;
                                let snapshot = snapshot_of(session.selected, &commands).await;
                                if let Some(snapshot) = snapshot {
                                    send(&mut sink, &snapshot).await?;
                                }
                                println!("auth: {} signed in as {}", session.login, session.role);
                                signed = Some(session);
                            }
                            Err(failure) => {
                                // Refusals are recorded too, and they are the
                                // interesting ones: a run of them from one
                                // address is how a password attack looks.
                                note_session(
                                    &commands,
                                    UserId::SYSTEM,
                                    domain::SessionEvent::Refused,
                                    address,
                                )
                                .await;
                                send(&mut sink, &failure).await?;
                            }
                        }
                    }
                    ClientMessage::Resume { token } => {
                        match resume(&gate, &token, &commands).await? {
                            Ok(session) => {
                                note_session(
                                    &commands,
                                    session.user,
                                    domain::SessionEvent::Resumed,
                                    address,
                                )
                                .await;
                                let welcome = welcome_for(&session, &gate, &commands).await?;
                                send(&mut sink, &welcome).await?;
                                if let Some(snapshot) = snapshot_of(session.selected, &commands).await {
                                    send(&mut sink, &snapshot).await?;
                                }
                                signed = Some(session);
                            }
                            Err(failure) => {
                                note_session(
                                    &commands,
                                    UserId::SYSTEM,
                                    domain::SessionEvent::Refused,
                                    address,
                                )
                                .await;
                                send(&mut sink, &failure).await?;
                            }
                        }
                    }
                    // Everything else needs a session. An unsigned connection
                    // is told once and otherwise ignored: it is either a
                    // terminal with a bug or somebody probing.
                    other => {
                        let Some(session) = signed.as_mut() else {
                            send(&mut sink, &ServerMessage::AuthRequired).await?;
                            continue;
                        };
                        match other {
                            ClientMessage::PaymentMethods => {
                                let reply = payment_methods(&gate, session);
                                send(&mut sink, &reply).await?;
                            }
                            ClientMessage::DepositRequested { method, amount, reference } => {
                                let raised = {
                                    let mut pay = gate.payments.lock().expect("the payment lock");
                                    pay.raise(payments::Asked {
                                        kind: payments::Kind::Deposit,
                                        account: session.selected,
                                        user: session.user,
                                        method,
                                        amount: &amount,
                                        reference: &reference,
                                        at: now_ms(),
                                    })
                                };
                                let reply = match raised {
                                    Ok(id) => ServerMessage::AdminResult {
                                        done: true,
                                        detail: format!(
                                            "Deposit {id} recorded. The desk will credit account {} once the money is seen.",
                                            session.selected.0
                                        ),
                                    },
                                    Err(why) => ServerMessage::AdminResult {
                                        done: false,
                                        detail: format!("Refused: {why}"),
                                    },
                                };
                                send(&mut sink, &reply).await?;
                                send(&mut sink, &payment_methods(&gate, session)).await?;
                            }
                            ClientMessage::PayoutMethods => {
                                let payouts = {
                                    let pay = gate.payments.lock().expect("the payment lock");
                                    pay.payouts_of(session.user)
                                        .into_iter()
                                        .map(describe_payout)
                                        .collect()
                                };
                                send(&mut sink, &ServerMessage::PayoutMethods { payouts }).await?;
                            }
                            ClientMessage::SavePayoutMethod {
                                id, rail, label, address, note, preferred,
                            } => {
                                let reply = match payments::Rail::parse(&rail) {
                                    None => ServerMessage::AdminResult {
                                        done: false,
                                        detail: "Not a way of being paid that I know.".to_owned(),
                                    },
                                    Some(rail) => {
                                        let saved = gate
                                            .payments
                                            .lock()
                                            .expect("the payment lock")
                                            .put_payout(payments::Saving {
                                                id,
                                                user: session.user,
                                                rail,
                                                label: &label,
                                                address: &address,
                                                note: &note,
                                                preferred,
                                            });
                                        match saved {
                                            Ok(id) => ServerMessage::AdminResult {
                                                done: true,
                                                detail: format!("Saved {label} as payout {id}."),
                                            },
                                            Err(why) => ServerMessage::AdminResult {
                                                done: false,
                                                detail: format!("Refused: {why}"),
                                            },
                                        }
                                    }
                                };
                                send(&mut sink, &reply).await?;
                            }
                            ClientMessage::ForgetPayoutMethod { id } => {
                                let gone = gate
                                    .payments
                                    .lock()
                                    .expect("the payment lock")
                                    .forget_payout(id, session.user);
                                let reply = match gone {
                                    Ok(()) => ServerMessage::AdminResult {
                                        done: true,
                                        detail: format!("Forgot payout {id}."),
                                    },
                                    Err(why) => ServerMessage::AdminResult {
                                        done: false,
                                        detail: format!("Refused: {why}"),
                                    },
                                };
                                send(&mut sink, &reply).await?;
                            }
                            ClientMessage::WithdrawalRequested { payout, amount, note } => {
                                let reply = ask_to_withdraw(
                                    &gate, session, payout, &amount, &note, &commands,
                                )
                                .await?;
                                send(&mut sink, &reply).await?;
                                send(&mut sink, &payment_methods(&gate, session)).await?;
                            }
                            ClientMessage::OpenAccount { live } => {
                                // A live account needs identity checks the
                                // session holds; the engine knows nothing
                                // about them, so the refusal happens here.
                                let kyc = gate
                                    .users
                                    .lock()
                                    .expect("the user store lock")
                                    .by_id(session.user)
                                    .map(|u| u.kyc)
                                    .unwrap_or(auth::Kyc::None);
                                if live && kyc != auth::Kyc::Approved {
                                    send(&mut sink, &ServerMessage::Unauthorized {
                                        detail: "a live account needs your identity verified first",
                                    }).await?;
                                    continue;
                                }

                                let (reply_tx, reply_rx) = oneshot::channel();
                                commands
                                    .send(Command::OpenAccount {
                                        user: session.user,
                                        live,
                                        reply: reply_tx,
                                    })
                                    .await
                                    .ok();
                                match reply_rx.await {
                                    Ok(Ok(opened)) => {
                                        // Switch to it, and send its state:
                                        // opening an account nobody is shown
                                        // is a confusing way to succeed.
                                        let visible = accounts_for(session, &gate, &commands).await?;
                                        session.allowed =
                                            visible.iter().map(|a| AccountId(a.id)).collect();
                                        session.selected = opened;
                                        let welcome = welcome_for(session, &gate, &commands).await?;
                                        send(&mut sink, &welcome).await?;
                                        if let Some(snapshot) =
                                            snapshot_of(opened, &commands).await
                                        {
                                            send(&mut sink, &snapshot).await?;
                                        }
                                    }
                                    Ok(Err(why)) => {
                                        send(&mut sink, &ServerMessage::AdminResult {
                                            done: false,
                                            detail: format!("Refused: {why}."),
                                        })
                                        .await?;
                                    }
                                    Err(_) => {}
                                }
                            }
                            ClientMessage::RequestVerification => {
                                let users = gate.users.clone();
                                let user = session.user;
                                let now = tokio::task::spawn_blocking(move || {
                                    let mut users = users.lock().expect("the user store lock");
                                    let current = users
                                        .by_id(user)
                                        .map(|u| u.kyc)
                                        .unwrap_or(auth::Kyc::None);
                                    // Asking again when already approved or
                                    // already waiting changes nothing: a
                                    // client cannot un-approve themselves.
                                    if current == auth::Kyc::None || current == auth::Kyc::Rejected
                                    {
                                        let _ = users.set_kyc(user, auth::Kyc::Submitted);
                                    }
                                    users.by_id(user).map(|u| u.kyc).unwrap_or(current)
                                })
                                .await
                                .context("recording a verification request")?;
                                send(&mut sink, &ServerMessage::Verification {
                                    kyc: now.as_str(),
                                    may_go_live: now == auth::Kyc::Approved,
                                })
                                .await?;
                            }
                            ClientMessage::Statement { limit } => {
                                let (reply_tx, reply_rx) = oneshot::channel();
                                commands
                                    .send(Command::Statement {
                                        // The session's own account, never one
                                        // named in the message: a statement is
                                        // the most personal thing here.
                                        account: session.selected,
                                        limit: limit.clamp(1, STATEMENT_PAGE),
                                        reply: reply_tx,
                                    })
                                    .await
                                    .ok();
                                if let Ok(entries) = reply_rx.await {
                                    send(&mut sink, &ServerMessage::Statement { entries }).await?;
                                }
                            }
                            ClientMessage::Candles {
                                instrument,
                                interval,
                                limit,
                            } => {
                                let reply = chart(
                                    &commands,
                                    AccountId(0),
                                    instrument,
                                    &interval,
                                    limit,
                                )
                                .await;
                                if let Some(reply) = reply {
                                    send(&mut sink, &reply).await?;
                                }
                            }
                            ClientMessage::Hello => {
                                if let Some(snapshot) = snapshot_of(session.selected, &commands).await {
                                    send(&mut sink, &snapshot).await?;
                                }
                            }
                            ClientMessage::SelectAccount { account } => {
                                let wanted = AccountId(account);
                                let visible = accounts_for(session, &gate, &commands).await?;
                                let exists = visible.iter().any(|a| a.id == account);
                                if !session.may_act_on(wanted, exists) {
                                    // The same answer whether or not it exists:
                                    // a client must not be able to map the book
                                    // by asking for account numbers.
                                    send(&mut sink, &ServerMessage::Unauthorized {
                                        detail: "that account is not yours to trade",
                                    }).await?;
                                    continue;
                                }
                                session.selected = wanted;
                                session.allowed = visible.iter().map(|a| AccountId(a.id)).collect();
                                if let Some(snapshot) = snapshot_of(wanted, &commands).await {
                                    send(&mut sink, &snapshot).await?;
                                }
                            }
                            // Desk work. The role is checked here, once,
                            // before anything reaches the engine: the engine
                            // records who acted, it does not decide who may.
                            //
                            // A manager gets the screen and the book they
                            // were given; an administrator gets the powers
                            // that made it. `desk_refusal` draws that line in
                            // one place, so a new message cannot quietly
                            // arrive on the wrong side of it.
                            admin_message @ (ClientMessage::AdminOverview
                            | ClientMessage::AdminBooks { .. }
                            | ClientMessage::AdminActivity { .. }
                            | ClientMessage::AdminAudit { .. }
                            | ClientMessage::AdminAction { .. }
                            | ClientMessage::AdminAmend { .. }
                            | ClientMessage::AdminCreateUser { .. }
                            | ClientMessage::AdminSetKyc { .. }
                            | ClientMessage::AdminAssignManager { .. }
                            | ClientMessage::AdminSetRole { .. }
                            | ClientMessage::AdminSetPower { .. }
                            | ClientMessage::AdminOpenAccount { .. }
                            | ClientMessage::AdminPutPaymentMethod { .. }
                            | ClientMessage::AdminRemovePaymentMethod { .. }
                            | ClientMessage::AdminDepositRequests
                            | ClientMessage::AdminSettleDeposit { .. }
                            | ClientMessage::AdminSettle { .. }
                            | ClientMessage::AdminRaise { .. }
                            | ClientMessage::Markups
                            | ClientMessage::AdminSetMarkup { .. }
                            | ClientMessage::AdminSetCommission { .. }
                            | ClientMessage::AdminSetPassword { .. }) => {
                                if let Some(detail) = desk_refusal(session, &admin_message) {
                                    send(&mut sink, &ServerMessage::Unauthorized { detail }).await?;
                                    continue;
                                }
                                let reply = administer(&gate, session, admin_message, &commands).await?;
                                send(&mut sink, &reply).await?;
                            }
                            ClientMessage::BeginSecondFactor => {
                                let made = gate
                                    .users
                                    .lock()
                                    .expect("the user store lock")
                                    .begin_second_factor(session.user);
                                let reply = match made {
                                    Ok(totp) => ServerMessage::SecondFactorSecret {
                                        secret: totp.to_base32(),
                                        grouped: totp.to_grouped(),
                                        uri: totp.uri(ISSUER, &session.login),
                                    },
                                    Err(why) => ServerMessage::AdminResult {
                                        done: false,
                                        detail: format!("Refused: {why}"),
                                    },
                                };
                                send(&mut sink, &reply).await?;
                            }
                            ClientMessage::ConfirmSecondFactor { code } => {
                                let done = gate
                                    .users
                                    .lock()
                                    .expect("the user store lock")
                                    .confirm_second_factor(
                                        session.user,
                                        &code,
                                        unix_seconds(),
                                    );
                                let reply = match done {
                                    Ok(()) => ServerMessage::AdminResult {
                                        done: true,
                                        detail: "Two-factor is on. You will be asked for a code every time you sign in — including from this browser.".to_owned(),
                                    },
                                    Err(why) => ServerMessage::AdminResult {
                                        done: false,
                                        detail: format!("Refused: {why}"),
                                    },
                                };
                                send(&mut sink, &reply).await?;
                                // The nag in the welcome frame is now wrong
                                // one way or the other, so it is resent.
                                let welcome = welcome_for(session, &gate, &commands).await?;
                                send(&mut sink, &welcome).await?;
                            }
                            ClientMessage::StopSecondFactor { code } => {
                                let done = gate
                                    .users
                                    .lock()
                                    .expect("the user store lock")
                                    .stop_second_factor(session.user, &code, unix_seconds());
                                let reply = match done {
                                    Ok(()) => ServerMessage::AdminResult {
                                        done: true,
                                        detail: "Two-factor is off. This login is back to a password alone."
                                            .to_owned(),
                                    },
                                    Err(why) => ServerMessage::AdminResult {
                                        done: false,
                                        detail: format!("Refused: {why}"),
                                    },
                                };
                                send(&mut sink, &reply).await?;
                                let welcome = welcome_for(session, &gate, &commands).await?;
                                send(&mut sink, &welcome).await?;
                            }
                            ClientMessage::Transfer { to, amount } => {
                                let reply =
                                    transfer(session, &commands, to, &amount).await?;
                                send(&mut sink, &reply).await?;
                            }
                            trade => {
                                if let Some(command) = decode(session.user, session.selected, trade) {
                                    // A full queue drops the command rather than
                                    // stalling the socket; the client sees no
                                    // fill and can retry.
                                    let _ = commands.try_send(command);
                                }
                            }
                        }
                    }
                }
            }
            broadcast = events.recv() => {
                match broadcast {
                    Ok(event) => {
                        // Prices are everyone's; anything addressed to an
                        // account goes only to sessions watching that account.
                        let mine = match (event.to, signed.as_ref()) {
                            (None, Some(_)) => true,
                            (Some(to), Some(session)) => to == session.selected,
                            // Nothing reaches a connection that has not signed
                            // in — not even a price.
                            (_, None) => false,
                        };
                        if mine {
                            send(&mut sink, &event.message).await?;
                        }
                    }
                    // Lagged means this session could not keep up. The next
                    // portfolio broadcast is a full replacement, so skipping
                    // what was missed loses nothing.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    // However the socket ended — signed out, closed, or dropped — the log
    // says when somebody stopped being signed in.
    if let Some(session) = signed.as_ref() {
        note_session(
            &commands,
            session.user,
            domain::SessionEvent::Closed,
            address,
        )
        .await;
    }
    Ok(())
}

/// Records a sign-in, a refusal or a sign-out.
///
/// Best effort: a full command queue drops the record rather than stalling
/// somebody's sign-in. The journal is the audit, and an audit that can block
/// a login is an audit that gets switched off.
async fn note_session(
    commands: &mpsc::Sender<Command>,
    user: UserId,
    what: domain::SessionEvent,
    address: [u8; 16],
) {
    let _ = commands.try_send(Command::NoteSession {
        user,
        what,
        address,
    });
}

/// Checks a password and opens a session.
///
/// The hashing runs on a blocking thread: Argon2 is deliberately slow — about
/// a tenth of a second — and a tenth of a second of an async worker is every
/// other connection's prices stopping.
async fn sign_in(
    gate: &Gate,
    login: &str,
    password: &str,
    code: &str,
    commands: &mpsc::Sender<Command>,
) -> Result<std::result::Result<Signed, ServerMessage>> {
    let users = gate.users.clone();
    let login_owned = login.to_owned();
    let password_owned = password.to_owned();
    let code_owned = code.to_owned();
    let seconds = unix_seconds();
    let outcome = tokio::task::spawn_blocking(move || {
        let mut users = users.lock().expect("the user store lock");
        users.sign_in(
            &login_owned,
            &password_owned,
            &code_owned,
            seconds,
            Instant::now(),
        )
    })
    .await
    .context("verifying a password")?;

    let user = match outcome {
        auth::SignIn::Ok(user) => user,
        auth::SignIn::Refused => {
            return Ok(Err(ServerMessage::AuthFailed {
                reason: "refused",
                retry_in_seconds: None,
            }))
        }
        // The password was right; the terminal asks for the code and sends
        // the whole lot again. Told apart from a refusal on purpose: the
        // alternative is a sign-in screen that asks everybody for a code,
        // including the clients who have none.
        auth::SignIn::CodeRequired => {
            return Ok(Err(ServerMessage::AuthFailed {
                reason: "codeRequired",
                retry_in_seconds: None,
            }))
        }
        auth::SignIn::CodeRefused(_) => {
            return Ok(Err(ServerMessage::AuthFailed {
                reason: "codeRefused",
                retry_in_seconds: None,
            }))
        }
        auth::SignIn::TooManyAttempts { retry_in } => {
            return Ok(Err(ServerMessage::AuthFailed {
                reason: "tooManyAttempts",
                retry_in_seconds: Some(retry_in.as_secs().max(1)),
            }))
        }
    };

    let token =
        gate.sessions
            .lock()
            .expect("the session lock")
            .open(user.id, user.role, Instant::now());
    build_session(token, user.id, user.login, user.role, gate, commands).await
}

/// Takes up an existing session from its token.
async fn resume(
    gate: &Gate,
    token: &str,
    commands: &mpsc::Sender<Command>,
) -> Result<std::result::Result<Signed, ServerMessage>> {
    let found = gate
        .sessions
        .lock()
        .expect("the session lock")
        .resume(token, Instant::now());
    let Some(live) = found else {
        return Ok(Err(ServerMessage::AuthFailed {
            reason: "expired",
            retry_in_seconds: None,
        }));
    };
    let login = gate
        .users
        .lock()
        .expect("the user store lock")
        .by_id(live.user)
        .map(|u| u.login.clone());
    // A token for a user who has since been removed is not a session.
    let Some(login) = login else {
        gate.sessions
            .lock()
            .expect("the session lock")
            .close_user(live.user);
        return Ok(Err(ServerMessage::AuthFailed {
            reason: "expired",
            retry_in_seconds: None,
        }));
    };
    build_session(
        token.to_owned(),
        live.user,
        login,
        live.role,
        gate,
        commands,
    )
    .await
}

/// Moves money from the session's account to another.
///
/// # Who may, and why it is not simply "the desk"
///
/// Two accounts with the **same owner** is one person moving their own
/// money about: no money enters or leaves the platform, nobody else is
/// better or worse off, and the only question is whether this session may
/// act on both accounts. A client may, for their own; a manager may, for a
/// client in their book.
///
/// Two accounts with **different owners** is money moving between people.
/// That is a withdrawal and a deposit wearing a disguise, and it is an
/// administrator's — otherwise a manager could move a client's balance
/// onto an account they themselves hold, and the audit trail would read as
/// an ordinary transfer.
///
/// The engine checks the rest: positive, same currency, both demo or both
/// live, free margin, and cash rather than bonus.
async fn transfer(
    session: &Signed,
    commands: &mpsc::Sender<Command>,
    to: u64,
    amount: &str,
) -> Result<ServerMessage> {
    let refuse = |why: String| ServerMessage::AdminResult {
        done: false,
        detail: format!("Refused: {why}"),
    };

    let Ok(moving) = money::Money::parse(amount.trim()) else {
        return Ok(refuse(format!("{amount:?} is not an amount.")));
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    commands
        .send(Command::Accounts {
            user: None,
            reply: reply_tx,
        })
        .await
        .ok();
    let all = reply_rx.await.unwrap_or_default();

    let target = AccountId(to);
    let here = all.iter().find(|a| a.id == session.selected.0);
    let there = all.iter().find(|a| a.id == to);

    if there.is_none() {
        return Ok(refuse(format!("there is no account {to}.")));
    }
    if !session.may_act_on(session.selected, here.is_some()) {
        return Ok(refuse("that is not your account to send from.".to_owned()));
    }
    if !session.may_act_on(target, true) {
        return Ok(refuse(format!("account {to} is not yours to send to.")));
    }
    // Between two people rather than between one person's accounts. See the
    // doc comment: this is a withdrawal and a deposit by another name.
    let same_owner = here.and_then(|a| a.owner) == there.and_then(|a| a.owner)
        && here.and_then(|a| a.owner).is_some();
    if !same_owner && !session.is_admin() {
        return Ok(refuse(
            "those accounts belong to different people, and moving money between people is an administrator's.".to_owned(),
        ));
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    commands
        .send(Command::Administer {
            by: session.user,
            account: session.selected,
            action: AdminAction::Transfer {
                to: target,
                amount: moving,
            },
            reply: reply_tx,
        })
        .await
        .ok();

    Ok(match reply_rx.await {
        Ok(AdminOutcome::Done) => ServerMessage::AdminResult {
            done: true,
            detail: format!(
                "Moved {amount} from account {} to {to}.",
                session.selected.0
            ),
        },
        Ok(AdminOutcome::Refused(why)) => refuse(why.to_owned()),
        Err(_) => refuse("the engine did not answer. Nothing moved.".to_owned()),
    })
}

/// Approves or refuses one request, and applies it.
///
/// # The order matters
///
/// The request is settled **first**, and only then is the money moved. A
/// request can leave `pending` exactly once, so a double click cannot pay
/// twice — which is the failure that costs real money. The opposite order
/// would move the money and then try to record it, and a crash in between
/// would leave a paid-out withdrawal still sitting in the queue.
///
/// The cost of this order is the rarer failure: if the engine refuses the
/// movement after the request is closed, the request says approved and the
/// account was not touched. That case is reported in those words, telling
/// the operator exactly what to do by hand, rather than being hidden
/// behind a success.
async fn settle(
    gate: &Gate,
    session: &Signed,
    commands: &mpsc::Sender<Command>,
    id: u64,
    approve: bool,
    detail: &str,
) -> Result<ServerMessage> {
    // Which power this needs depends on what is being approved, and the
    // message carried only a request id — so the kind is read first, and
    // the permission checked before anything is settled. A refusal then
    // leaves the request in the queue for somebody who may decide it.
    let asked_kind = gate
        .payments
        .lock()
        .expect("the payment lock")
        .requests()
        .iter()
        .find(|r| r.id == id)
        .map(|r| r.kind);

    if approve {
        let Some(asked_kind) = asked_kind else {
            return Ok(ServerMessage::AdminResult {
                done: false,
                detail: format!("Refused: there is no request {id}."),
            });
        };
        let needed = match asked_kind {
            payments::Kind::Withdrawal => auth::Power::ProcessWithdrawals,
            payments::Kind::Deposit => auth::Power::ProcessDeposits,
            payments::Kind::BonusIn | payments::Kind::BonusOut => auth::Power::CreditBonus,
            payments::Kind::InsuranceIn | payments::Kind::InsuranceOut => {
                auth::Power::TransferFunds
            }
        };
        if !session.is_admin() && !session.powers.contains(&needed) {
            return Ok(ServerMessage::AdminResult {
                done: false,
                detail: format!(
                    "Refused: your desk has not been given {}.",
                    needed.said().to_lowercase()
                ),
            });
        }
    }

    let settled =
        gate.payments
            .lock()
            .expect("the payment lock")
            .settle(id, session.user, approve, detail);

    let request = match settled {
        Ok(r) => r,
        Err(why) => {
            return Ok(ServerMessage::AdminResult {
                done: false,
                detail: format!("Refused: {why}"),
            })
        }
    };

    if !approve {
        return Ok(ServerMessage::AdminResult {
            done: true,
            detail: format!(
                "{} {id} refused. The client is told.",
                request.kind.as_str()
            ),
        });
    }

    let Ok(amount) = money::Money::parse(request.amount.trim()) else {
        return Ok(ServerMessage::AdminResult {
            done: false,
            detail: format!(
                "The request says {} and that is not an amount I can apply. Do it by hand.",
                request.amount
            ),
        });
    };

    // What approving each kind actually does to the account. Bonus moves
    // the credit pot as a delta — the operator asked for *more* or *less*,
    // not for a new absolute figure — so the current one is read first.
    let action = match request.kind {
        payments::Kind::Deposit => AdminAction::Deposit(amount),
        payments::Kind::Withdrawal => AdminAction::Withdraw(amount),
        payments::Kind::BonusIn | payments::Kind::BonusOut => {
            let now = snapshot_of(AccountId(request.account), commands)
                .await
                .and_then(|m| match m {
                    ServerMessage::Snapshot { account, .. } => {
                        money::Money::parse(&account.credit).ok()
                    }
                    _ => None,
                })
                .unwrap_or(money::Money::ZERO);
            let next = if request.kind == payments::Kind::BonusIn {
                now.add(amount)
            } else {
                now.sub(amount)
            };
            match next {
                Ok(v) if v >= money::Money::ZERO => AdminAction::SetCredit(v),
                Ok(_) => {
                    return Ok(ServerMessage::AdminResult {
                        done: false,
                        detail: format!(
                            "Request {id} is marked approved but {} is more bonus than account {} has. Nothing was changed.",
                            request.amount, request.account
                        ),
                    })
                }
                Err(_) => {
                    return Ok(ServerMessage::AdminResult {
                        done: false,
                        detail: format!("Request {id} is marked approved but the figures would not add up. Nothing was changed."),
                    })
                }
            }
        }
        payments::Kind::InsuranceIn | payments::Kind::InsuranceOut => {
            let now = snapshot_of(AccountId(request.account), commands)
                .await
                .and_then(|m| match m {
                    ServerMessage::Snapshot { account, .. } => {
                        money::Money::parse(&account.insurance).ok()
                    }
                    _ => None,
                })
                .unwrap_or(money::Money::ZERO);
            let next = if request.kind == payments::Kind::InsuranceIn {
                now.add(amount)
            } else {
                now.sub(amount)
            };
            match next {
                Ok(v) if v >= money::Money::ZERO => AdminAction::SetInsurance(v),
                Ok(_) => {
                    return Ok(ServerMessage::AdminResult {
                        done: false,
                        detail: format!(
                            "Request {id} is marked approved but {} is more insurance than account {} has set aside. Nothing was changed.",
                            request.amount, request.account
                        ),
                    })
                }
                Err(_) => {
                    return Ok(ServerMessage::AdminResult {
                        done: false,
                        detail: format!("Request {id} is marked approved but the figures would not add up. Nothing was changed."),
                    })
                }
            }
        }
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    commands
        .send(Command::Administer {
            by: session.user,
            account: AccountId(request.account),
            action,
            reply: reply_tx,
        })
        .await
        .ok();

    Ok(match reply_rx.await {
        Ok(AdminOutcome::Done) => ServerMessage::AdminResult {
            done: true,
            detail: format!(
                "{} {id} approved: {} on account {}.",
                request.kind.as_str(),
                request.amount,
                request.account
            ),
        },
        Ok(AdminOutcome::Refused(why)) => ServerMessage::AdminResult {
            done: false,
            detail: format!(
                "Request {id} is marked approved but the engine refused it: {why}. Account {} was NOT changed — do it by hand.",
                request.account
            ),
        },
        Err(_) => ServerMessage::AdminResult {
            done: false,
            detail: format!(
                "Request {id} is marked approved but the engine did not answer. Check account {} before trying again.",
                request.account
            ),
        },
    })
}

/// The ways to pay, as this session may see them.
///
/// A client is shown only what is switched on, and only their own claims.
/// The desk sees everything, including the methods it has turned off.
fn payment_methods(gate: &Gate, session: &Signed) -> ServerMessage {
    let users = gate.users.lock().expect("the user store lock");
    let pay = gate.payments.lock().expect("the payment lock");
    let desk = !matches!(session.role, Role::Client);

    let methods = if desk {
        pay.methods().iter().map(describe_method).collect()
    } else {
        pay.offered().map(describe_method).collect()
    };
    let mine = pay
        .mine(session.user)
        .into_iter()
        .map(|r| describe_request(r, &pay, &users))
        .collect();

    ServerMessage::PaymentMethods { methods, mine }
}

fn describe_payout(p: &payments::Payout) -> PayoutMethodDto {
    PayoutMethodDto {
        id: p.id,
        rail: p.rail.as_str(),
        label: p.label.clone(),
        address: p.address.clone(),
        note: p.note.clone(),
        preferred: p.preferred,
    }
}

/// A client asking to be paid out.
///
/// Three things are checked before it reaches the queue, and all three are
/// refusals a client would rather hear now than after waiting a day:
///
/// * **A demo account has no money to send.** The figures in it are
///   practice, and paying real money against them is the single worst
///   thing this platform could do.
/// * **Outstanding bonus is cleared first.** Bonus counts toward margin
///   but is never withdrawable; letting it leave is how a broker pays out
///   its own promotion.
/// * **Only free margin may go.** Money holding an open position up is not
///   the client's to take — withdrawing it would stop the position out.
///
/// What is *not* checked here is whether the desk wants to pay: that is
/// the approval, and it is a person's decision.
async fn ask_to_withdraw(
    gate: &Gate,
    session: &Signed,
    payout: u64,
    amount: &str,
    note: &str,
    commands: &mpsc::Sender<Command>,
) -> Result<ServerMessage> {
    let refuse = |why: String| ServerMessage::AdminResult {
        done: false,
        detail: format!("Refused: {why}"),
    };

    let Ok(wanted) = money::Money::parse(amount.trim()) else {
        return Ok(refuse(format!("{amount:?} is not an amount.")));
    };
    if wanted <= money::Money::ZERO {
        return Ok(refuse("say how much, as a positive amount.".to_owned()));
    }

    let Some(state) = snapshot_of(session.selected, commands).await else {
        return Ok(refuse("that account is not answering.".to_owned()));
    };
    let ServerMessage::Snapshot { account, .. } = &state else {
        return Ok(refuse("that account is not answering.".to_owned()));
    };

    if !account.live {
        return Ok(refuse(
            "a demo account holds no real money. There is nothing to send.".to_owned(),
        ));
    }
    if money::Money::parse(&account.credit).is_ok_and(|c| c > money::Money::ZERO) {
        return Ok(refuse(format!(
            "clear the {} bonus on this account before withdrawing. A bonus counts toward margin but is not yours to take.",
            account.credit
        )));
    }
    let free = money::Money::parse(&account.free_margin).unwrap_or(money::Money::ZERO);
    if wanted > free {
        return Ok(refuse(format!(
            "only {free} is free. The rest is holding your open positions up, and taking it would close them."
        )));
    }

    let raised = gate
        .payments
        .lock()
        .expect("the payment lock")
        .raise(payments::Asked {
            kind: payments::Kind::Withdrawal,
            account: session.selected,
            user: session.user,
            method: payout,
            amount,
            reference: note,
            at: now_ms(),
        });
    Ok(match raised {
        Ok(id) => ServerMessage::AdminResult {
            done: true,
            detail: format!(
                "Withdrawal {id} requested. The desk will send it and credit nothing until they have."
            ),
        },
        Err(why) => refuse(why.to_string()),
    })
}

fn describe_method(m: &payments::Method) -> PaymentMethodDto {
    PaymentMethodDto {
        id: m.id,
        rail: m.rail.as_str(),
        label: m.label.clone(),
        address: m.address.clone(),
        note: m.note.clone(),
        enabled: m.enabled,
    }
}

fn describe_request(
    r: &payments::Request,
    pay: &Payments,
    users: &auth::Users,
) -> DepositRequestDto {
    let method = pay.method(r.method);
    DepositRequestDto {
        id: r.id,
        account: r.account,
        user: r.user,
        login: users.by_id(UserId(r.user)).map(|u| u.login.clone()),
        method: r.method,
        kind: r.kind.as_str(),
        pot: r.kind.pot(),
        method_label: match r.kind {
            payments::Kind::Withdrawal => pay
                .payout(r.method)
                .map(|p| p.label.clone())
                .unwrap_or_default(),
            _ => method.map(|m| m.label.clone()).unwrap_or_default(),
        },
        rail: match r.kind {
            payments::Kind::Withdrawal => pay
                .payout(r.method)
                .map(|p| p.rail.as_str())
                .unwrap_or("bank"),
            _ => method.map(|m| m.rail.as_str()).unwrap_or("bank"),
        },
        amount: r.amount.clone(),
        reference: r.reference.clone(),
        at: r.at,
        stage: r.stage.as_str(),
        settled_by: r.settled_by,
        detail: r.detail.clone(),
    }
}

/// Why a desk message is refused, when it is.
///
/// Three groups:
///
/// * **Nobody but the desk.** A client has no business here at all.
/// * **The whole book.** The trial balance and the audit trail are the
///   platform's, not one manager's, and a manager reading either would be
///   reading other managers' clients.
/// * **The powers that make a book.** Money in and out, identity, passwords,
///   roles and assignment. A manager works the book an administrator gave
///   them; handing out credit or approving identity is how a book is made,
///   and the person accountable for the platform keeps it.
///
/// Everything else — the overview, an account's activity, suspending,
/// renewing a demo, switching hedging — a manager may do, and the scoping in
/// [`accounts_for`] decides *whose*.
/// The power a message needs, when it needs one.
///
/// A message with no power here is either a client's own, or one the gate
/// below decides on some other ground — being on the right book, usually.
/// Returning `None` is not "anybody may": it is "this is not the question
/// permissions answer".
fn power_for(message: &ClientMessage) -> Option<auth::Power> {
    use auth::Power;
    Some(match message {
        ClientMessage::AdminCreateUser { .. } => Power::CreateClients,
        ClientMessage::AdminAudit { .. } => Power::ViewAudit,
        ClientMessage::AdminSetMarkup { .. } | ClientMessage::AdminSetCommission { .. } => {
            Power::EditSpread
        }
        ClientMessage::AdminAmend { .. } => Power::EditTrades,
        ClientMessage::AdminRaise { kind, .. } => match payments::Kind::parse(kind) {
            Some(payments::Kind::BonusIn | payments::Kind::BonusOut) => Power::CreditBonus,
            // Insurance is the client's own money moving between their own
            // pots, so it is judged as a transfer rather than as a grant.
            _ => Power::TransferFunds,
        },
        // `AdminSettle` is deliberately absent: it carries a request id,
        // not a kind, so the gate cannot tell approving a withdrawal from
        // approving a deposit. That check happens in `settle`, which has
        // read the request and knows which it is.
        ClientMessage::AdminSettleDeposit { credit: true, .. } => Power::ProcessDeposits,
        ClientMessage::Transfer { .. } => Power::TransferFunds,
        ClientMessage::PlaceOrder { .. } => Power::ManualTrade,
        ClientMessage::ClosePosition { .. } | ClientMessage::CancelOrder { .. } => {
            Power::CloseTrades
        }
        ClientMessage::AdminAction { kind, .. } => match kind.as_str() {
            "deposit" => Power::ProcessDeposits,
            "withdraw" => Power::ProcessWithdrawals,
            "credit" => Power::CreditBonus,
            "suspend" | "reinstate" => Power::SuspendAccounts,
            _ => return None,
        },
        _ => return None,
    })
}

fn desk_refusal(session: &Signed, message: &ClientMessage) -> Option<&'static str> {
    if matches!(session.role, Role::Client) {
        return Some("this needs the desk");
    }
    if session.is_admin() {
        return None;
    }
    // A manager's own permissions come first: an administrator may have
    // taken a power away that the rules below would otherwise allow, and
    // the whole point of the switch is that it wins.
    if let Some(power) = power_for(message) {
        if !session.powers.contains(&power) {
            return Some("your desk has not been given that");
        }
    }
    match message {
        ClientMessage::AdminBooks { .. } | ClientMessage::AdminAudit { .. } => {
            Some("the books and the audit trail are the platform's, not one manager's")
        }
        // Configuring where money is sent is the platform's own banking,
        // not a book to be worked.
        ClientMessage::AdminPutPaymentMethod { .. }
        | ClientMessage::AdminRemovePaymentMethod { .. } => {
            Some("only an administrator may change where money is paid")
        }
        // What every client is quoted is the platform's pricing. A manager
        // reads it — they are asked why a spread is what it is — but does
        // not set it.
        ClientMessage::AdminSetMarkup { .. } | ClientMessage::AdminSetCommission { .. } => {
            Some("only an administrator may change what the house charges")
        }
        // Confirming money arrived credits an account, which is the same
        // decision as a deposit and goes the same way.
        ClientMessage::AdminSetPower { .. }
        | ClientMessage::AdminSetPassword { .. }
        | ClientMessage::AdminSetKyc { .. }
        | ClientMessage::AdminAssignManager { .. }
        | ClientMessage::AdminSetRole { .. }
        | ClientMessage::AdminAmend { .. } => Some("only an administrator may do that"),
        // Opening an account that holds somebody's money is the same
        // decision as making one live, and goes the same way.
        ClientMessage::AdminOpenAccount { live, .. } if *live => {
            Some("money and going live are an administrator's to decide")
        }
        ClientMessage::AdminAction { account, kind, .. } => {
            match kind.as_str() {
                // Going live is not a permission anybody is given: it
                // turns practice money into money a client can withdraw,
                // and it is the one account change with no way back.
                "live" | "demo" => Some("going live is an administrator's to decide"),
                // Allowed in kind, but only on an account in their book: the
                // kind of a message says what is being done, never to whom.
                _ if !session.allowed.contains(&AccountId(*account)) => {
                    Some("that account is not in your book")
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Asks the engine which accounts this user may act on, and picks one.
///
/// A session always starts on the user's **own** account, whatever their role.
/// An administrator who signs in and lands on the first client in the book is
/// one mis-click away from trading somebody else's money; acting on a client's
/// account is a deliberate `selectAccount`, and the account picker is how they
/// get there.
async fn build_session(
    token: String,
    user: UserId,
    login: String,
    role: Role,
    gate: &Gate,
    commands: &mpsc::Sender<Command>,
) -> Result<std::result::Result<Signed, ServerMessage>> {
    let own = ask_accounts(Some(user), commands).await?;
    let visible = match role {
        Role::Client => own.clone(),
        // A manager sees their own account and those of the clients assigned
        // to them, and nothing else on the book.
        Role::Manager => {
            let book = gate
                .users
                .lock()
                .expect("the user store lock")
                .book_of(user);
            let all = ask_accounts(None, commands).await?;
            all.into_iter()
                .filter(|a| a.owner.is_some_and(|o| book.contains(&UserId(o))))
                .collect()
        }
        // An administrator sees the book — their own account included.
        Role::Admin => ask_accounts(None, commands).await?,
    };
    let Some(selected) = own.first().map(|a| AccountId(a.id)) else {
        // The engine opens an account for a user who has none, so this is a
        // failure to do that, not an empty book.
        return Ok(Err(ServerMessage::AuthFailed {
            reason: "noAccounts",
            retry_in_seconds: None,
        }));
    };
    let powers = gate
        .users
        .lock()
        .expect("the user store lock")
        .by_id(user)
        .map(|u| {
            u.powers()
                .into_iter()
                .filter(|(_, held)| *held)
                .map(|(p, _)| p)
                .collect()
        })
        .unwrap_or_default();

    Ok(Ok(Signed {
        token,
        user,
        login,
        role,
        allowed: visible.iter().map(|a| AccountId(a.id)).collect(),
        powers,
        selected,
    }))
}

async fn ask_accounts(
    user: Option<UserId>,
    commands: &mpsc::Sender<Command>,
) -> Result<Vec<AccountSummaryDto>> {
    let (reply_tx, reply_rx) = oneshot::channel();
    commands
        .send(Command::Accounts {
            user,
            reply: reply_tx,
        })
        .await
        .ok();
    Ok(reply_rx.await.unwrap_or_default())
}

/// The account list as this session sees it: their own, or the whole book.
async fn accounts_for(
    session: &Signed,
    gate: &Gate,
    commands: &mpsc::Sender<Command>,
) -> Result<Vec<AccountSummaryDto>> {
    match session.role {
        Role::Client => ask_accounts(Some(session.user), commands).await,
        Role::Manager => {
            // Their own account first, which also opens one if the engine has
            // not yet; then the book, narrowed to the clients they hold.
            let own = ask_accounts(Some(session.user), commands).await?;
            let book = gate
                .users
                .lock()
                .expect("the user store lock")
                .book_of(session.user);
            let mut all: Vec<AccountSummaryDto> = ask_accounts(None, commands)
                .await?
                .into_iter()
                .filter(|a| a.owner.is_some_and(|o| book.contains(&UserId(o))))
                .collect();
            for account in own {
                if !all.iter().any(|a| a.id == account.id) {
                    all.push(account);
                }
            }
            all.sort_by_key(|a| a.id);
            Ok(all)
        }
        Role::Admin => ask_accounts(None, commands).await,
    }
}

async fn welcome_for(
    session: &Signed,
    gate: &Gate,
    commands: &mpsc::Sender<Command>,
) -> Result<ServerMessage> {
    let accounts = accounts_for(session, gate, commands).await?;
    let (has_second, wants_second) = {
        let users = gate.users.lock().expect("the user store lock");
        match users.by_id(session.user) {
            Some(u) => (
                u.has_second_factor(),
                u.should_have_second_factor() && !u.has_second_factor(),
            ),
            None => (false, false),
        }
    };
    Ok(ServerMessage::Welcome {
        token: session.token.clone(),
        user: session.user.0,
        login: session.login.clone(),
        role: session.role.as_str(),
        accounts,
        account: session.selected.0,
        second_factor: has_second,
        second_factor_wanted: wants_second,
    })
}

/// How far a user's identity checks have got, for their own terminal.
fn verification_of(gate: &Gate, user: UserId) -> ServerMessage {
    let kyc = gate
        .users
        .lock()
        .expect("the user store lock")
        .by_id(user)
        .map(|u| u.kyc)
        .unwrap_or(auth::Kyc::None);
    ServerMessage::Verification {
        kyc: kyc.as_str(),
        may_go_live: kyc == auth::Kyc::Approved,
    }
}

/// Bars for a chart, or nothing when the interval is not one we serve.
///
/// `_account` is unused on purpose: a price is not account-shaped, and every
/// signed-in session may ask for any listed instrument's chart. What it may
/// not do is ask before signing in, which the caller has already decided.
async fn chart(
    commands: &mpsc::Sender<Command>,
    _account: AccountId,
    instrument: u32,
    interval: &str,
    limit: usize,
) -> Option<ServerMessage> {
    let seconds = candles::seconds_of(interval)?;
    let (reply_tx, reply_rx) = oneshot::channel();
    commands
        .send(Command::Chart {
            instrument: instrument_id(instrument),
            seconds,
            limit: limit.clamp(1, candles::MAX_BARS),
            reply: reply_tx,
        })
        .await
        .ok();
    let bars = reply_rx.await.ok()?;
    Some(ServerMessage::Candles {
        instrument,
        interval: interval.to_owned(),
        bars: bars
            .into_iter()
            .map(|b| BarDto {
                start: b.start,
                open: price_str(b.open),
                high: price_str(b.high),
                low: price_str(b.low),
                close: price_str(b.close),
            })
            .collect(),
    })
}

/// The full state of one account.
async fn snapshot_of(
    account: AccountId,
    commands: &mpsc::Sender<Command>,
) -> Option<ServerMessage> {
    let (reply_tx, reply_rx) = oneshot::channel();
    commands
        .send(Command::Snapshot {
            account,
            reply: reply_tx,
        })
        .await
        .ok();
    reply_rx.await.ok().map(|boxed| *boxed)
}

/// Carries out one administrator's request.
///
/// Money and suspensions go to the engine, which journals them with the
/// administrator's id on the event. Logins and passwords are the user store's,
/// which the engine knows nothing about — and does not need to.
async fn administer(
    gate: &Gate,
    session: &Signed,
    message: ClientMessage,
    commands: &mpsc::Sender<Command>,
) -> Result<ServerMessage> {
    match message {
        ClientMessage::AdminOverview => {
            let (reply_tx, reply_rx) = oneshot::channel();
            commands
                .send(Command::Overview { reply: reply_tx })
                .await
                .ok();
            let mut accounts = reply_rx.await.unwrap_or_default();
            let users = gate.users.lock().expect("the user store lock");

            // A manager's overview is their book and no more. Narrowed here,
            // on the way out, because the engine does not know who manages
            // whom — and should not: it knows accounts and events.
            let book = (!session.is_admin()).then(|| users.book_of(session.user));
            if let Some(book) = &book {
                accounts.retain(|a| a.owner.is_some_and(|o| book.contains(&UserId(o))));
            }

            // The engine holds ids; the names live here. Joining them in the
            // session keeps logins out of the journal and out of the engine.
            for account in &mut accounts {
                account.owner_login = account
                    .owner
                    .and_then(|id| users.by_id(UserId(id)))
                    .map(|u| u.login.clone());
            }

            let listed: Vec<&auth::User> = users
                .all()
                .iter()
                .filter(|u| book.as_ref().is_none_or(|b| b.contains(&u.id)))
                .collect();

            Ok(ServerMessage::AdminBook {
                accounts,
                users: listed
                    .iter()
                    .map(|u| AdminUserDto {
                        id: u.id.0,
                        login: u.login.clone(),
                        role: u.role.as_str(),
                        kyc: u.kyc.as_str(),
                        manager: u.manager.map(|m| m.0),
                        manager_login: u
                            .manager
                            .and_then(|m| users.by_id(m))
                            .map(|m| m.login.clone()),
                        // Only a manager's. An administrator holds
                        // everything by definition and a client holds
                        // none of it, so a list against either is a row
                        // of switches that do nothing.
                        powers: if u.role == auth::Role::Manager {
                            u.powers()
                                .into_iter()
                                .map(|(p, held)| PowerDto {
                                    key: p.as_str(),
                                    said: p.said(),
                                    held,
                                    possible: p.manager_may(),
                                    why_not: p.why_not(),
                                })
                                .collect()
                        } else {
                            Vec::new()
                        },
                    })
                    .collect(),
            })
        }

        ClientMessage::AdminSetPower {
            user,
            power,
            allowed,
        } => {
            let Some(power) = auth::Power::parse(&power) else {
                return Ok(ServerMessage::AdminResult {
                    done: false,
                    detail: "Not a permission I know.".to_owned(),
                });
            };
            let done = gate.users.lock().expect("the user store lock").set_power(
                UserId(user),
                power,
                allowed,
            );
            Ok(match done {
                Ok(()) => ServerMessage::AdminResult {
                    done: true,
                    detail: format!(
                        "{} {} for user {user}. It takes effect when they next sign in.",
                        power.said(),
                        if allowed { "allowed" } else { "withheld" }
                    ),
                },
                Err(why) => ServerMessage::AdminResult {
                    done: false,
                    detail: format!("Refused: {why}"),
                },
            })
        }

        ClientMessage::AdminAssignManager { user, manager } => {
            let users = gate.users.clone();
            let client = UserId(user);
            let to = manager.map(UserId);
            let changed = tokio::task::spawn_blocking(move || {
                users
                    .lock()
                    .expect("the user store lock")
                    .set_manager(client, to)
            })
            .await
            .context("assigning a client to a manager")?;
            Ok(match changed {
                Ok(()) => ServerMessage::AdminResult {
                    done: true,
                    detail: match manager {
                        Some(m) => format!("Client {user} is now in manager {m}'s book."),
                        None => format!("Client {user} is no longer in anybody's book."),
                    },
                },
                Err(why) => ServerMessage::AdminResult {
                    done: false,
                    detail: format!("Refused: {why}"),
                },
            })
        }

        ClientMessage::AdminOpenAccount { user, live } => {
            let target = UserId(user);
            // Whose client this is, and whether they are verified, are both
            // questions for the session layer — the engine knows neither.
            {
                let users = gate.users.lock().expect("the user store lock");
                let Some(client) = users.by_id(target) else {
                    return Ok(ServerMessage::AdminResult {
                        done: false,
                        detail: format!("Refused: no user {user}."),
                    });
                };
                if !session.is_admin() && !users.book_of(session.user).contains(&target) {
                    return Ok(ServerMessage::AdminResult {
                        done: false,
                        detail: "Refused: that client is not in your book.".to_owned(),
                    });
                }
                if live && client.kyc != auth::Kyc::Approved {
                    return Ok(ServerMessage::AdminResult {
                        done: false,
                        detail: format!(
                            "Refused: {} is not verified, so a live account cannot be opened for them.",
                            client.login
                        ),
                    });
                }
            }

            let (reply_tx, reply_rx) = oneshot::channel();
            commands
                .send(Command::OpenAccount {
                    user: target,
                    live,
                    reply: reply_tx,
                })
                .await
                .ok();
            Ok(match reply_rx.await {
                Ok(Ok(opened)) => ServerMessage::AdminResult {
                    done: true,
                    detail: format!(
                        "Opened {} account {} for user {user}.",
                        if live { "a live" } else { "a demo" },
                        opened.0
                    ),
                },
                Ok(Err(why)) => ServerMessage::AdminResult {
                    done: false,
                    detail: format!("Refused: {why}"),
                },
                Err(_) => ServerMessage::AdminResult {
                    done: false,
                    detail: "The engine did not answer.".to_owned(),
                },
            })
        }

        ClientMessage::AdminPutPaymentMethod {
            id,
            rail,
            label,
            address,
            note,
            enabled,
        } => {
            let Some(rail) = Rail::parse(&rail) else {
                return Ok(ServerMessage::AdminResult {
                    done: false,
                    detail: "Not a payment rail I know.".to_owned(),
                });
            };
            let put = gate
                .payments
                .lock()
                .expect("the payment lock")
                .put(id, rail, &label, &address, &note, enabled);
            Ok(match put {
                Ok(id) => ServerMessage::AdminResult {
                    done: true,
                    detail: format!(
                        "Saved {label} as method {id}{}.",
                        if enabled { "" } else { ", switched off" }
                    ),
                },
                Err(why) => ServerMessage::AdminResult {
                    done: false,
                    detail: format!("Refused: {why}"),
                },
            })
        }

        ClientMessage::AdminRemovePaymentMethod { id } => {
            let gone = gate.payments.lock().expect("the payment lock").remove(id);
            Ok(match gone {
                Ok(()) => ServerMessage::AdminResult {
                    done: true,
                    detail: format!("Removed payment method {id}."),
                },
                Err(why) => ServerMessage::AdminResult {
                    done: false,
                    detail: format!("Refused: {why}"),
                },
            })
        }

        ClientMessage::Markups => {
            // Copied out and the lock dropped before anything else: a
            // guard held across an await is a future that cannot be sent
            // between threads, and tokio says so at compile time.
            let (set, charged) = {
                let held = gate.markups.lock().expect("the markup lock");
                let marks: std::collections::BTreeMap<u32, money::Price> =
                    held.all().map(|(id, p)| (id.0, p)).collect();
                let charges: std::collections::BTreeMap<u32, money::Money> =
                    instruments::catalogue()
                        .iter()
                        .map(|l| {
                            let id = l.terms.instrument.id;
                            (id.0, held.commission_of(id))
                        })
                        .collect();
                (marks, charges)
            };

            // Every listed instrument, not only the marked-up ones: the
            // desk is deciding what to charge, and a list that hides the
            // ones charging nothing hides exactly what it needs to see.
            let markups = instruments::catalogue()
                .iter()
                .map(|listing| {
                    let id = listing.terms.instrument.id;
                    let markup = set.get(&id.0).copied().unwrap_or(money::Price::ZERO);
                    MarkupDto {
                        instrument: id.0,
                        symbol: listing.terms.instrument.symbol.to_string(),
                        markup: markup.to_string(),
                        commission: charged
                            .get(&id.0)
                            .copied()
                            .unwrap_or(money::Money::ZERO)
                            .to_string(),
                        spread: None,
                    }
                })
                .collect();
            Ok(ServerMessage::Markups { markups })
        }

        ClientMessage::AdminSetMarkup { instrument, markup } => {
            let Ok(price) = money::Price::parse(markup.trim()) else {
                return Ok(ServerMessage::AdminResult {
                    done: false,
                    detail: format!("{markup:?} is not a price I can charge."),
                });
            };
            let set = gate
                .markups
                .lock()
                .expect("the markup lock")
                .set(domain::InstrumentId(instrument), price);
            Ok(match set {
                Ok(()) => ServerMessage::AdminResult {
                    done: true,
                    detail: if price == money::Price::ZERO {
                        format!("Instrument {instrument} is quoted at the feed's price.")
                    } else {
                        format!(
                            "Instrument {instrument} is marked up {price} a side - the spread a client sees widens by twice that. It takes effect on the next tick."
                        )
                    },
                },
                Err(why) => ServerMessage::AdminResult {
                    done: false,
                    detail: format!("Refused: {why}"),
                },
            })
        }

        ClientMessage::AdminSetCommission {
            instrument,
            commission,
        } => {
            let Ok(amount) = money::Money::parse(commission.trim()) else {
                return Ok(ServerMessage::AdminResult {
                    done: false,
                    detail: format!("{commission:?} is not an amount I can charge."),
                });
            };
            let set = gate
                .markups
                .lock()
                .expect("the markup lock")
                .set_commission(domain::InstrumentId(instrument), amount);
            Ok(match set {
                Ok(()) => ServerMessage::AdminResult {
                    done: true,
                    detail: if amount == money::Money::ZERO {
                        format!("Instrument {instrument} is traded without commission.")
                    } else {
                        format!(
                            "Instrument {instrument} charges {amount} a unit, on opening and on closing. It takes effect on the next tick."
                        )
                    },
                },
                Err(why) => ServerMessage::AdminResult {
                    done: false,
                    detail: format!("Refused: {why}"),
                },
            })
        }

        ClientMessage::AdminRaise {
            kind,
            account,
            amount,
            note,
        } => {
            let Some(kind) = payments::Kind::parse(&kind) else {
                return Ok(ServerMessage::AdminResult {
                    done: false,
                    detail: "Not a movement I know.".to_owned(),
                });
            };
            if kind.client_may_ask() {
                return Ok(ServerMessage::AdminResult {
                    done: false,
                    detail: "A deposit or a withdrawal is the client's to ask for. Use the account's own menu to move money directly.".to_owned(),
                });
            }
            let owner = {
                let (reply_tx, reply_rx) = oneshot::channel();
                commands
                    .send(Command::Accounts {
                        user: None,
                        reply: reply_tx,
                    })
                    .await
                    .ok();
                reply_rx
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .find(|a| a.id == account)
                    .and_then(|a| a.owner)
            };
            let Some(owner) = owner else {
                return Ok(ServerMessage::AdminResult {
                    done: false,
                    detail: format!("Account {account} has no owner to credit."),
                });
            };
            let raised = gate
                .payments
                .lock()
                .expect("the payment lock")
                .raise(payments::Asked {
                    kind,
                    account: AccountId(account),
                    user: UserId(owner),
                    method: 0,
                    amount: &amount,
                    reference: &note,
                    at: now_ms(),
                });
            Ok(match raised {
                Ok(id) => ServerMessage::AdminResult {
                    done: true,
                    detail: format!(
                        "{} of {amount} queued as request {id} on account {account}. Approve it to apply it.",
                        kind.as_str()
                    ),
                },
                Err(why) => ServerMessage::AdminResult {
                    done: false,
                    detail: format!("Refused: {why}"),
                },
            })
        }

        ClientMessage::AdminSettle {
            id,
            approve,
            detail,
        } => settle(gate, session, commands, id, approve, &detail).await,

        ClientMessage::AdminDepositRequests => {
            let users = gate.users.lock().expect("the user store lock");
            let pay = gate.payments.lock().expect("the payment lock");
            // A manager sees the claims of the clients in their book, and
            // no others: a deposit names an account and an amount.
            let book = (!session.is_admin()).then(|| users.book_of(session.user));
            let mut requests: Vec<DepositRequestDto> = pay
                .requests()
                .iter()
                .filter(|r| book.as_ref().is_none_or(|b| b.contains(&UserId(r.user))))
                .map(|r| describe_request(r, &pay, &users))
                .collect();
            requests.sort_by_key(|r| std::cmp::Reverse(r.id));
            Ok(ServerMessage::DepositRequests { requests })
        }

        ClientMessage::AdminSettleDeposit { id, credit, detail } => {
            // Settled first, and only then credited: the claim can go from
            // pending exactly once, so a double click cannot pay twice.
            let settled = gate.payments.lock().expect("the payment lock").settle(
                id,
                session.user,
                credit,
                &detail,
            );

            let request = match settled {
                Ok(r) => r,
                Err(why) => {
                    return Ok(ServerMessage::AdminResult {
                        done: false,
                        detail: format!("Refused: {why}"),
                    })
                }
            };

            if !credit {
                return Ok(ServerMessage::AdminResult {
                    done: true,
                    detail: format!("Deposit {id} refused. The client is told."),
                });
            }

            // The money itself is an ordinary journaled deposit with the
            // operator's name on it — the same event as any other, because
            // it is the same fact.
            let Ok(amount) = money::Money::parse(request.amount.trim()) else {
                return Ok(ServerMessage::AdminResult {
                    done: false,
                    detail: format!(
                        "The claim says {} and that is not an amount I can credit. Deposit it by hand.",
                        request.amount
                    ),
                });
            };
            let (reply_tx, reply_rx) = oneshot::channel();
            let _ = commands
                .send(Command::Administer {
                    by: session.user,
                    account: AccountId(request.account),
                    action: AdminAction::Deposit(amount),
                    reply: reply_tx,
                })
                .await;
            Ok(match reply_rx.await {
                Ok(AdminOutcome::Done) => ServerMessage::AdminResult {
                    done: true,
                    detail: format!(
                        "Deposit {id} confirmed: {} credited to account {}.",
                        request.amount, request.account
                    ),
                },
                // The claim is already closed — it can leave `pending` only
                // once, which is what stops a double click paying twice —
                // so a refusal here leaves money owed and uncredited. Say
                // so plainly rather than reporting a success nobody got.
                Ok(AdminOutcome::Refused(why)) => ServerMessage::AdminResult {
                    done: false,
                    detail: format!(
                        "Deposit {id} is marked confirmed but the engine refused the credit: {why}.                          Account {} has NOT been credited — deposit {} to it by hand.",
                        request.account, request.amount
                    ),
                },
                Err(_) => ServerMessage::AdminResult {
                    done: false,
                    detail: format!(
                        "Deposit {id} is marked confirmed but the engine did not answer.                          Check account {} before crediting it again.",
                        request.account
                    ),
                },
            })
        }

        ClientMessage::AdminSetRole { user, role } => {
            let Some(wanted) = Role::parse(&role) else {
                return Ok(ServerMessage::AdminResult {
                    done: false,
                    detail: "Not a role I know.".to_owned(),
                });
            };
            // An administrator who demotes themselves locks the desk out of
            // its own platform, and nobody is left who can undo it.
            if UserId(user) == session.user && wanted != session.role {
                return Ok(ServerMessage::AdminResult {
                    done: false,
                    detail: "Refused: you cannot change your own role.".to_owned(),
                });
            }
            let users = gate.users.clone();
            let target = UserId(user);
            let changed = tokio::task::spawn_blocking(move || {
                users
                    .lock()
                    .expect("the user store lock")
                    .set_role(target, wanted)
            })
            .await
            .context("setting a role")?;
            Ok(match changed {
                Ok(()) => ServerMessage::AdminResult {
                    done: true,
                    detail: format!("User {user} is now a {}.", wanted.as_str()),
                },
                Err(why) => ServerMessage::AdminResult {
                    done: false,
                    detail: format!("Refused: {why}"),
                },
            })
        }

        ClientMessage::AdminActivity { limit } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            commands
                .send(Command::Activity {
                    limit: limit.clamp(1, AUDIT_PAGE),
                    reply: reply_tx,
                })
                .await
                .ok();
            let mut entries = reply_rx.await.unwrap_or_default();
            let users = gate.users.lock().expect("the user store lock");
            // When somebody else's client signed in is not a manager's
            // business, so the feed is narrowed to their own book.
            if !session.is_admin() {
                let book = users.book_of(session.user);
                entries.retain(|e| book.contains(&UserId(e.user)));
            }
            for entry in &mut entries {
                entry.login = users.by_id(UserId(entry.user)).map(|u| u.login.clone());
            }
            Ok(ServerMessage::AdminActivity { entries })
        }

        ClientMessage::AdminBooks { account } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            commands
                .send(Command::Books {
                    // Zero is not an account: it is the house, and asking for
                    // the house means asking for everything.
                    account: (account != 0).then_some(AccountId(account)),
                    reply: reply_tx,
                })
                .await
                .ok();
            Ok(match reply_rx.await {
                Ok(books) => ServerMessage::AdminBooks { books },
                Err(_) => ServerMessage::AdminResult {
                    done: false,
                    detail: "The engine did not answer.".to_owned(),
                },
            })
        }

        ClientMessage::AdminAudit { limit } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            commands
                .send(Command::Audit {
                    limit: limit.clamp(1, AUDIT_PAGE),
                    reply: reply_tx,
                })
                .await
                .ok();
            let mut entries = reply_rx.await.unwrap_or_default();
            let users = gate.users.lock().expect("the user store lock");
            for entry in &mut entries {
                entry.by_login = users.by_id(UserId(entry.by)).map(|u| u.login.clone());
            }
            Ok(ServerMessage::AdminAudit { entries })
        }

        ClientMessage::AdminAction {
            account,
            kind,
            amount,
        } => {
            let parsed = match kind.as_str() {
                "suspend" => Some(AdminAction::Suspend),
                "hedging" => Some(AdminAction::SetHedging(true)),
                "live" => Some(AdminAction::SetLive(true)),
                "demo" => Some(AdminAction::SetLive(false)),
                "renew" => Some(AdminAction::RenewDemo),
                "netting" => Some(AdminAction::SetHedging(false)),
                "reinstate" => Some(AdminAction::Reinstate),
                other => match (other, Money::parse(amount.trim())) {
                    ("deposit", Ok(m)) => Some(AdminAction::Deposit(m)),
                    ("withdraw", Ok(m)) => Some(AdminAction::Withdraw(m)),
                    ("credit", Ok(m)) => Some(AdminAction::SetCredit(m)),
                    // A malformed amount is the operator's typing, not a
                    // protocol error: say so rather than dropping it.
                    (_, Err(_)) => None,
                    _ => None,
                },
            };
            let Some(action) = parsed else {
                return Ok(ServerMessage::AdminResult {
                    done: false,
                    detail: "Not an action I know, or not an amount I can read.".to_owned(),
                });
            };

            let (reply_tx, reply_rx) = oneshot::channel();
            commands
                .send(Command::Administer {
                    by: session.user,
                    account: AccountId(account),
                    action,
                    reply: reply_tx,
                })
                .await
                .ok();
            Ok(match reply_rx.await {
                Ok(AdminOutcome::Done) => ServerMessage::AdminResult {
                    done: true,
                    detail: describe(action, account),
                },
                Ok(AdminOutcome::Refused(why)) => ServerMessage::AdminResult {
                    done: false,
                    detail: format!("Refused: {why}."),
                },
                Err(_) => ServerMessage::AdminResult {
                    done: false,
                    detail: "The engine did not answer.".to_owned(),
                },
            })
        }

        ClientMessage::AdminAmend {
            account,
            ticket,
            kind,
            reason,
            value,
        } => {
            let Some(reason) = amend_reason_from(&reason) else {
                return Ok(ServerMessage::AdminResult {
                    done: false,
                    detail: "Not a reason I know. Every amendment needs one.".to_owned(),
                });
            };
            let amendment = match kind.as_str() {
                "adjust" => Money::parse(value.trim()).ok().map(Amendment::Adjust),
                "correctPrice" => Price::parse(value.trim())
                    .ok()
                    .map(Amendment::CorrectOpenPrice),
                _ => None,
            };
            let Some(amendment) = amendment else {
                return Ok(ServerMessage::AdminResult {
                    done: false,
                    detail: "Not an amendment I know, or not a number I can read.".to_owned(),
                });
            };

            let (reply_tx, reply_rx) = oneshot::channel();
            commands
                .send(Command::Amend {
                    by: session.user,
                    account: AccountId(account),
                    ticket: domain::OrderId(ticket),
                    amendment,
                    reason,
                    reply: reply_tx,
                })
                .await
                .ok();
            Ok(match reply_rx.await {
                Ok(AdminOutcome::Done) => ServerMessage::AdminResult {
                    done: true,
                    detail: match amendment {
                        Amendment::Adjust(m) => format!(
                            "Adjusted account {account} by {} on ticket {ticket}. The client has been told.",
                            m.to_decimal_string()
                        ),
                        Amendment::CorrectOpenPrice(p) => format!(
                            "Ticket {ticket} now treated as opened at {}. The client has been told.",
                            p.to_decimal_string()
                        ),
                    },
                },
                Ok(AdminOutcome::Refused(why)) => ServerMessage::AdminResult {
                    done: false,
                    detail: format!("Refused: {why}."),
                },
                Err(_) => ServerMessage::AdminResult {
                    done: false,
                    detail: "The engine did not answer.".to_owned(),
                },
            })
        }

        ClientMessage::AdminCreateUser { login, password } => {
            let users = gate.users.clone();
            // Hashing is slow on purpose; keep it off the async threads.
            let made = tokio::task::spawn_blocking(move || {
                users
                    .lock()
                    .expect("the user store lock")
                    .add(&login, &password, Role::Client)
            })
            .await
            .context("creating a user")?;
            Ok(match made {
                Ok(id) => ServerMessage::AdminResult {
                    done: true,
                    detail: format!(
                        "Login created as user {}. Their account opens the first time they sign in.",
                        id.0
                    ),
                },
                Err(e) => ServerMessage::AdminResult {
                    done: false,
                    detail: format!("Refused: {e}."),
                },
            })
        }

        ClientMessage::AdminSetKyc { user, kyc } => {
            let Some(decision) = kyc_from(&kyc) else {
                return Ok(ServerMessage::AdminResult {
                    done: false,
                    detail: "Not a verification state I know.".to_owned(),
                });
            };
            let users = gate.users.clone();
            let target = UserId(user);
            let changed = tokio::task::spawn_blocking(move || {
                users
                    .lock()
                    .expect("the user store lock")
                    .set_kyc(target, decision)
            })
            .await
            .context("recording a verification decision")?;
            Ok(match changed {
                Ok(()) => ServerMessage::AdminResult {
                    done: true,
                    detail: match decision {
                        auth::Kyc::Approved => {
                            format!("User {user} is verified and may open a live account.")
                        }
                        auth::Kyc::Rejected => {
                            format!("User {user} is refused. They can still trade a demo account.")
                        }
                        auth::Kyc::Submitted => format!("User {user} is waiting on the desk."),
                        auth::Kyc::None => format!("User {user}'s checks are cleared."),
                    },
                },
                Err(e) => ServerMessage::AdminResult {
                    done: false,
                    detail: format!("Refused: {e}."),
                },
            })
        }

        ClientMessage::AdminSetPassword { user, password } => {
            let users = gate.users.clone();
            let sessions = gate.sessions.clone();
            let target = UserId(user);
            let changed = tokio::task::spawn_blocking(move || {
                users
                    .lock()
                    .expect("the user store lock")
                    .set_password(target, &password)
            })
            .await
            .context("setting a password")?;
            Ok(match changed {
                Ok(()) => {
                    // A password change that leaves old sessions alive has not
                    // revoked anything.
                    sessions
                        .lock()
                        .expect("the session lock")
                        .close_user(target);
                    ServerMessage::AdminResult {
                        done: true,
                        detail: format!("Password set for user {user}; their sessions are closed."),
                    }
                }
                Err(e) => ServerMessage::AdminResult {
                    done: false,
                    detail: format!("Refused: {e}."),
                },
            })
        }

        _ => Ok(ServerMessage::AdminResult {
            done: false,
            detail: "Not an administrator's request.".to_owned(),
        }),
    }
}

/// Parses the wire's verification state.
fn kyc_from(text: &str) -> Option<auth::Kyc> {
    match text {
        "none" => Some(auth::Kyc::None),
        "submitted" => Some(auth::Kyc::Submitted),
        "approved" => Some(auth::Kyc::Approved),
        "rejected" => Some(auth::Kyc::Rejected),
        _ => None,
    }
}

/// What to tell the administrator they just did.
fn describe(action: AdminAction, account: u64) -> String {
    match action {
        AdminAction::Deposit(m) => format!("Deposited {} to {account}.", m.to_decimal_string()),
        AdminAction::Withdraw(m) => format!("Withdrew {} from {account}.", m.to_decimal_string()),
        AdminAction::SetCredit(m) => {
            format!("Bonus on {account} set to {}.", m.to_decimal_string())
        }
        AdminAction::SetInsurance(m) => format!(
            "Insurance on {account} set to {}. It is out of the balance and the market cannot reach it.",
            m.to_decimal_string()
        ),
        AdminAction::Transfer { to, amount } => format!(
            "Moved {} from {account} to {}. Both legs are one journal record, so there is no moment when the money is in neither.",
            amount.to_decimal_string(),
            to.0
        ),
        AdminAction::SetLive(true) => {
            format!("Account {account} is live. The money in it is the client's from now on.")
        }
        AdminAction::SetLive(false) => format!("Account {account} is back to demo."),
        AdminAction::RenewDemo => {
            format!("Account {account} has another {} days.", risk::DEMO_DAYS)
        }
        AdminAction::SetHedging(true) => {
            format!("Account {account} may now hold both sides of an instrument.")
        }
        AdminAction::SetHedging(false) => {
            format!("Account {account} nets opposing orders off again.")
        }
        AdminAction::Suspend => format!("Account {account} suspended; it cannot open positions."),
        AdminAction::Reinstate => format!("Account {account} can trade again."),
    }
}

/// Turns one trading frame into a command on `account`, or nothing.
///
/// The account comes from the session, never from the frame: a terminal cannot
/// name an account it has not been given, because there is nowhere in the
/// message to say one. `by` is the signed-in user, so an order an
/// administrator places for a client is journaled as theirs.
/// A price the terminal may leave out: empty means zero, which every reader
/// of these fields takes as "none". A value that is not a price at all is
/// `None`, which drops the whole command rather than silently clearing a stop.
fn optional_price(text: &str) -> Option<Price> {
    let text = text.trim();
    if text.is_empty() {
        return Some(Price::ZERO);
    }
    Price::parse(text).ok()
}

fn decode(by: UserId, account: AccountId, message: ClientMessage) -> Option<Command> {
    match message {
        ClientMessage::PlaceOrder {
            instrument,
            side,
            kind,
            qty,
            price,
            limit_price,
        } => Some(Command::Place {
            by,
            account,
            instrument: instrument_id(instrument),
            side: side_from(&side)?,
            kind: kind_from(&kind)?,
            qty: Qty::parse(&qty).ok()?,
            price: if price.is_empty() {
                Price::ZERO
            } else {
                Price::parse(&price).ok()?
            },
            limit_price: optional_price(&limit_price)?,
        }),
        ClientMessage::SetProtection {
            ticket,
            stop_loss,
            take_profit,
        } => Some(Command::SetProtection {
            by,
            account,
            ticket: domain::OrderId(ticket),
            stop_loss: optional_price(&stop_loss)?,
            take_profit: optional_price(&take_profit)?,
        }),
        ClientMessage::ClosePosition { ticket, qty } => Some(Command::Close {
            by,
            account,
            ticket: domain::OrderId(ticket),
            // An unreadable size is zero, which means all of it. The other
            // reading — drop the command — would leave a trader pressing
            // Close and nothing happening, which is worse than closing the
            // whole position they asked about.
            qty: domain::Qty::parse(qty.trim()).unwrap_or(domain::Qty::ZERO),
        }),
        ClientMessage::AmendOrder {
            ticket,
            qty,
            price,
            limit_price,
        } => Some(Command::AmendOrder {
            by,
            account,
            ticket: domain::OrderId(ticket),
            // Unreadable means "leave it", which is what empty means too:
            // the alternative is dropping the whole command, and a trader
            // pressing Amend and watching nothing happen is worse than
            // one field not changing.
            qty: domain::Qty::parse(qty.trim()).unwrap_or(domain::Qty::ZERO),
            trigger_price: domain::Price::parse(price.trim()).unwrap_or(domain::Price::ZERO),
            limit_price: domain::Price::parse(limit_price.trim()).unwrap_or(domain::Price::ZERO),
        }),
        ClientMessage::CancelOrder { ticket } => Some(Command::CancelOrder {
            by,
            account,
            ticket: domain::OrderId(ticket),
        }),
        // Everything else is a sign-in or an administrator's request, and
        // neither is a trading command.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The account a signed-in session would stamp on the command.
    const TRADER: AccountId = AccountId(1001);

    /// Decodes a frame the way a session does: parse, then stamp the account
    /// the session is acting on. A frame cannot name an account itself.
    fn decode_text(text: &str) -> Option<Command> {
        decode(
            UserId(2),
            TRADER,
            serde_json::from_str::<ClientMessage>(text).ok()?,
        )
    }

    #[test]
    fn a_power_taken_away_is_refused_and_the_rest_still_work() {
        // The whole point of the switch. An administrator takes one power
        // off a manager and everything else they do carries on.
        let full = session_as(Role::Manager, &[1001, 1002]);
        let no_deposits =
            session_without(Role::Manager, &[1001, 1002], auth::Power::ProcessDeposits);

        assert_eq!(desk_refusal(&full, &act(1001, "deposit")), None);
        assert!(desk_refusal(&no_deposits, &act(1001, "deposit")).is_some());

        // Withdrawals are a different switch, and this one is still on.
        assert_eq!(desk_refusal(&no_deposits, &act(1001, "withdraw")), None);
        // And the ordinary work of a book is untouched.
        assert_eq!(desk_refusal(&no_deposits, &act(1001, "suspend")), None);
        assert_eq!(
            desk_refusal(&no_deposits, &ClientMessage::AdminOverview),
            None
        );
    }

    #[test]
    fn each_kind_of_money_has_its_own_switch() {
        for (power, kind) in [
            (auth::Power::ProcessDeposits, "deposit"),
            (auth::Power::ProcessWithdrawals, "withdraw"),
            (auth::Power::CreditBonus, "credit"),
            (auth::Power::SuspendAccounts, "suspend"),
        ] {
            let without = session_without(Role::Manager, &[1001], power);
            assert!(
                desk_refusal(&without, &act(1001, kind)).is_some(),
                "{kind} went through without {}",
                power.as_str()
            );
            // Every other kind still goes through, so one switch does not
            // quietly turn several things off.
            for (other, other_kind) in [
                (auth::Power::ProcessDeposits, "deposit"),
                (auth::Power::ProcessWithdrawals, "withdraw"),
                (auth::Power::CreditBonus, "credit"),
                (auth::Power::SuspendAccounts, "suspend"),
            ] {
                if other == power {
                    continue;
                }
                assert_eq!(
                    desk_refusal(&without, &act(1001, other_kind)),
                    None,
                    "taking {} away also stopped {other_kind}",
                    power.as_str()
                );
            }
        }
    }

    #[test]
    fn an_administrator_keeps_everything_whatever_the_switches_say() {
        // An administrator with no powers on the session is still an
        // administrator: the role is checked first. A role that could be
        // whittled to nothing is a way to lock everybody out, including
        // whoever is doing the whittling.
        let mut boss = session_as(Role::Admin, &[1001]);
        boss.powers.clear();
        for kind in ["deposit", "withdraw", "credit", "live", "suspend"] {
            assert_eq!(
                desk_refusal(&boss, &act(1001, kind)),
                None,
                "an administrator was refused {kind}"
            );
        }
        assert_eq!(
            desk_refusal(&boss, &ClientMessage::AdminAudit { limit: 10 }),
            None
        );
    }

    #[test]
    fn a_manager_may_never_hold_the_platforms_own_powers() {
        // Three are not a book's business whatever anybody switches: the
        // audit trail covers every manager's clients, pricing is what
        // every client on the platform is quoted, and correcting a trade
        // rewrites what the journal says happened.
        let manager = session_as(Role::Manager, &[1001]);
        for power in [
            auth::Power::ViewAudit,
            auth::Power::EditSpread,
            auth::Power::EditTrades,
        ] {
            assert!(
                !power.manager_may(),
                "{} should not be a manager's",
                power.as_str()
            );
            assert!(
                !manager.powers.contains(&power),
                "a full manager session held {}",
                power.as_str()
            );
            assert!(
                power.why_not().is_some(),
                "{} has no reason",
                power.as_str()
            );
        }

        assert!(desk_refusal(&manager, &ClientMessage::AdminAudit { limit: 10 }).is_some());
        assert!(desk_refusal(
            &manager,
            &ClientMessage::AdminSetMarkup {
                instrument: 1,
                markup: "0.5".to_owned()
            }
        )
        .is_some());
    }

    #[test]
    fn every_power_has_a_stable_name_and_reads_back() {
        // The names are keys in a file somebody's permissions are stored
        // against. Renaming one silently restores a power that was taken
        // away, so they are pinned here rather than trusted.
        let names: Vec<&str> = auth::POWERS.into_iter().map(|p| p.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "createClients",
                "processDeposits",
                "processWithdrawals",
                "creditBonus",
                "transferFunds",
                "manualTrade",
                "closeTrades",
                "editTrades",
                "suspendAccounts",
                "viewAudit",
                "editSpread",
            ]
        );
        for power in auth::POWERS {
            assert_eq!(auth::Power::parse(power.as_str()), Some(power));
            assert!(!power.said().is_empty());
        }
        assert_eq!(auth::Power::parse("whatever"), None);
    }

    /// A session as the gate would have built it, holding everything a
    /// manager may. The tests that care about one power missing build
    /// their own with [`session_without`].
    fn session_as(role: Role, allowed: &[u64]) -> Signed {
        Signed {
            token: "t".to_owned(),
            user: UserId(7),
            login: "boss@feraro.local".to_owned(),
            role,
            allowed: allowed.iter().copied().map(AccountId).collect(),
            powers: auth::POWERS
                .into_iter()
                .filter(|p| p.manager_may())
                .collect(),
            selected: AccountId(allowed.first().copied().unwrap_or(0)),
        }
    }

    /// The same, with one power taken away.
    fn session_without(role: Role, allowed: &[u64], gone: auth::Power) -> Signed {
        let mut s = session_as(role, allowed);
        s.powers.remove(&gone);
        s
    }

    fn act(account: u64, kind: &str) -> ClientMessage {
        ClientMessage::AdminAction {
            account,
            kind: kind.to_owned(),
            amount: "100.00".to_owned(),
        }
    }

    #[test]
    fn a_client_is_refused_every_desk_message() {
        let client = session_as(Role::Client, &[1001]);
        for message in [
            ClientMessage::AdminOverview,
            ClientMessage::AdminAudit { limit: 10 },
            act(1001, "suspend"),
        ] {
            assert!(
                desk_refusal(&client, &message).is_some(),
                "a client reached the desk: {message:?}"
            );
        }
    }

    #[test]
    fn a_manager_works_their_book_but_does_not_make_one() {
        let manager = session_as(Role::Manager, &[1001, 1002]);

        // Theirs to do, on an account in their book.
        for message in [
            ClientMessage::AdminOverview,
            ClientMessage::AdminActivity { limit: 20 },
            act(1001, "suspend"),
            act(1002, "reinstate"),
            act(1001, "renew"),
            act(1002, "hedging"),
        ] {
            assert_eq!(
                desk_refusal(&manager, &message),
                None,
                "a manager was refused their own work: {message:?}"
            );
        }

        // Money in and out is a manager's when their desk has been given
        // it. This manager holds everything a manager may.
        for kind in ["deposit", "withdraw", "credit"] {
            assert_eq!(
                desk_refusal(&manager, &act(1001, kind)),
                None,
                "a manager with the power was refused {kind}"
            );
        }

        // Going live is nobody's to be given: it turns practice money into
        // money a client can withdraw, and there is no way back.
        for kind in ["live", "demo"] {
            assert!(
                desk_refusal(&manager, &act(1001, kind)).is_some(),
                "a manager was allowed to {kind}"
            );
        }

        // The powers that make a book, rather than work one.
        for message in [
            ClientMessage::AdminSetPassword {
                user: 3,
                password: "a-password-1".to_owned(),
            },
            ClientMessage::AdminSetKyc {
                user: 3,
                kyc: "approved".to_owned(),
            },
            ClientMessage::AdminAssignManager {
                user: 3,
                manager: Some(7),
            },
            ClientMessage::AdminSetRole {
                user: 3,
                role: "manager".to_owned(),
            },
            ClientMessage::AdminSetPower {
                user: 3,
                power: "processDeposits".to_owned(),
                allowed: true,
            },
        ] {
            assert!(
                desk_refusal(&manager, &message).is_some(),
                "a manager was allowed to make a book: {message:?}"
            );
        }

        // The platform's own records are not one manager's.
        assert!(desk_refusal(&manager, &ClientMessage::AdminBooks { account: 0 }).is_some());
        assert!(desk_refusal(&manager, &ClientMessage::AdminAudit { limit: 10 }).is_some());
    }

    #[test]
    fn a_manager_cannot_reach_an_account_outside_their_book() {
        let manager = session_as(Role::Manager, &[1001, 1002]);

        // The kind is one a manager may do; the account is not theirs. The
        // kind of a message says what is being done, never to whom.
        let refusal = desk_refusal(&manager, &act(1009, "suspend"));
        assert_eq!(refusal, Some("that account is not in your book"));

        assert_eq!(desk_refusal(&manager, &act(1002, "suspend")), None);
    }

    #[test]
    fn an_administrator_is_refused_nothing_at_the_desk() {
        let admin = session_as(Role::Admin, &[]);
        for message in [
            ClientMessage::AdminOverview,
            ClientMessage::AdminBooks { account: 0 },
            ClientMessage::AdminAudit { limit: 10 },
            // Not in `allowed`, which an administrator does not go by.
            act(1009, "deposit"),
            ClientMessage::AdminSetRole {
                user: 3,
                role: "manager".to_owned(),
            },
        ] {
            assert_eq!(
                desk_refusal(&admin, &message),
                None,
                "an administrator was refused: {message:?}"
            );
        }
    }

    #[test]
    fn a_manager_may_act_only_on_the_accounts_they_hold() {
        let manager = session_as(Role::Manager, &[1001]);
        assert!(manager.may_act_on(AccountId(1001), true));
        // Exists, but is somebody else's client.
        assert!(!manager.may_act_on(AccountId(1002), true));

        let admin = session_as(Role::Admin, &[]);
        assert!(admin.may_act_on(AccountId(1002), true));
        assert!(!admin.may_act_on(AccountId(4242), false), "no such account");
    }

    #[test]
    fn decodes_a_market_order() {
        let json = r#"{"type":"placeOrder","instrument":1,"side":"buy","kind":"market","qty":"0.50","price":""}"#;
        let Some(Command::Place {
            instrument,
            side,
            qty,
            ..
        }) = decode_text(json)
        else {
            panic!("expected a place command");
        };
        assert_eq!(instrument.0, 1);
        assert_eq!(side, money::Side::Buy);
        assert_eq!(qty.to_display_string(2).unwrap(), "0.50");
    }

    #[test]
    fn decodes_a_close() {
        let Some(Command::Close { ticket, .. }) =
            decode_text(r#"{"type":"closePosition","ticket":100482}"#)
        else {
            panic!("expected a close command");
        };
        assert_eq!(ticket.0, 100_482);
    }

    #[test]
    fn decodes_a_limit_order_with_its_price() {
        let json = r#"{"type":"placeOrder","instrument":1,"side":"buy","kind":"limit","qty":"1","price":"70000.00"}"#;
        let Some(Command::Place { kind, price, .. }) = decode_text(json) else {
            panic!("expected a place command");
        };
        assert_eq!(kind, domain::OrderType::Limit);
        assert_eq!(price.to_display_string(2).unwrap(), "70000.00");
    }

    #[test]
    fn decodes_a_cancel() {
        let Some(Command::CancelOrder { ticket, .. }) =
            decode_text(r#"{"type":"cancelOrder","ticket":100003}"#)
        else {
            panic!("expected a cancel command");
        };
        assert_eq!(ticket.0, 100_003);
    }

    #[test]
    fn drops_anything_malformed_rather_than_guessing() {
        assert!(decode_text("nonsense").is_none());
        assert!(decode_text(
            r#"{"type":"placeOrder","instrument":1,"side":"sideways","kind":"market","qty":"1"}"#
        )
        .is_none());
        assert!(decode_text(
            r#"{"type":"placeOrder","instrument":1,"side":"buy","kind":"market","qty":"NaN"}"#
        )
        .is_none());
        assert!(decode_text(r#"{"type":"hello"}"#).is_none());
        // Sign-in frames are not trading commands, whoever sends them.
        assert!(decode_text(r#"{"type":"login","login":"demo","password":"x"}"#).is_none());
        assert!(decode_text(r#"{"type":"selectAccount","account":1002}"#).is_none());
    }
}
