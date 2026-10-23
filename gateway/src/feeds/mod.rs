//! Upstream market data: four providers, one aggregator.
//!
//! Each provider is a small [`Provider`]: where to connect, what to send on
//! open, how often to heartbeat, and a pure `parse` from a text frame to
//! observations. One shared loop in [`run`] handles connecting, reconnecting
//! with backoff, and heartbeats, so a provider is its protocol and nothing else.
//!
//! Everything a provider observes goes to the [`aggregator`], which decides
//! which source prices each instrument and hands the engine a two-sided quote.
//! Providers never talk to the engine directly: the engine should not need to
//! know there is more than one feed, let alone which one is misbehaving today.
//!
//! # API keys
//!
//! Twelve Data and Finnhub need keys; Binance and Kraken do not. Keys are read
//! from the gateway's environment or its `.env` file, go into a WebSocket URL
//! the gateway opens, and are never sent to a browser or written to a log. A
//! provider with no key is simply not started, and says so once.

pub mod aggregator;
pub mod binance;
pub mod finnhub;
pub mod kraken;
pub mod number;
pub mod twelvedata;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use domain::InstrumentId;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

pub use aggregator::Observation;

/// Which feed a price came from. The numbering is journaled, so it is stable:
/// never renumber, only add.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Source(pub u8);

impl Source {
    pub const BINANCE: Source = Source(1);
    pub const KRAKEN: Source = Source(2);
    pub const TWELVE_DATA: Source = Source(3);
    pub const FINNHUB: Source = Source(4);

    /// Lowercase name, as the terminal shows it.
    pub const fn name(self) -> &'static str {
        match self.0 {
            1 => "binance",
            2 => "kraken",
            3 => "twelvedata",
            4 => "finnhub",
            _ => "unknown",
        }
    }
}

/// One observation from one provider about one instrument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedUpdate {
    pub instrument: InstrumentId,
    pub source: Source,
    pub observation: Observation,
}

/// A provider's upstream symbol for each instrument it prices.
pub type Subscriptions = Vec<(String, InstrumentId)>;

/// What makes a feed a feed.
pub trait Provider: Send + 'static {
    /// Which source this is.
    fn source(&self) -> Source;
    /// Human name for logs.
    fn label(&self) -> &'static str;
    /// Where to connect. May contain a key; never log it.
    fn url(&self) -> String;
    /// Frames to send once connected — subscriptions, typically.
    fn on_open(&self) -> Vec<String>;
    /// A frame to send on a fixed interval to keep the socket alive, if the
    /// provider requires one.
    fn heartbeat(&self) -> Option<(Duration, String)> {
        None
    }
    /// A frame to send in answer to a server frame, such as a pong.
    fn reply_to(&self, _text: &str) -> Option<String> {
        None
    }
    /// Reads one text frame. Pure, so every provider is testable offline.
    fn parse(&self, text: &str, now: Instant) -> Vec<FeedUpdate>;
}

/// Reconnect delay: exponential, capped at thirty seconds, with jitter so that
/// a fleet of gateways does not reconnect in lockstep after an outage.
pub fn backoff(attempt: u32) -> Duration {
    let base = 500u64.saturating_mul(1 << attempt.min(6)).min(30_000);
    let jitter = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_millis()))
        .unwrap_or(0)
        % 400;
    Duration::from_millis(base + jitter)
}

/// With no frame at all for this long — not even a heartbeat — the connection
/// is presumed dead. Every provider here sends something at least every ten
/// seconds, but an FX symbol can legitimately be quiet, so this is generous.
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Runs one provider until the process ends.
pub async fn run<P: Provider>(provider: P, tx: mpsc::Sender<FeedUpdate>) {
    let label = provider.label();
    let source = provider.source();
    let mut attempt = 0u32;

    loop {
        match connect_async(provider.url()).await {
            Ok((stream, _)) => {
                attempt = 0;
                let (mut write, mut read) = stream.split();

                let mut opened = true;
                for frame in provider.on_open() {
                    if write.send(Message::Text(frame.into())).await.is_err() {
                        opened = false;
                        break;
                    }
                }
                if opened {
                    eprintln!("feed {label}: connected");
                }

                let heartbeat = provider.heartbeat();
                let mut ticker = tokio::time::interval(
                    heartbeat
                        .as_ref()
                        .map(|(every, _)| *every)
                        .unwrap_or(Duration::from_secs(3_600)),
                );
                ticker.tick().await; // the first tick fires immediately

                // Measured from the last frame *received*: our own heartbeats
                // going out say nothing about whether the far end is alive.
                let mut last_heard = tokio::time::Instant::now();

                while opened {
                    tokio::select! {
                        () = tokio::time::sleep_until(last_heard + IDLE_TIMEOUT) => {
                            // Half-open sockets never close on their own.
                            eprintln!("feed {label}: silent for {}s, reconnecting", IDLE_TIMEOUT.as_secs());
                            opened = false;
                        }
                        frame = read.next() => {
                            let Some(Ok(frame)) = frame else { break };
                            last_heard = tokio::time::Instant::now();
                            match frame {
                                Message::Text(text) => {
                                    let text = text.as_str();
                                    if let Some(reply) = provider.reply_to(text) {
                                        let _ = write.send(Message::Text(reply.into())).await;
                                    }
                                    for mut update in provider.parse(text, Instant::now()) {
                                        // A provider only ever speaks for itself.
                                        update.source = source;
                                        // A full queue means the aggregator is
                                        // behind; the next tick supersedes this.
                                        let _ = tx.try_send(update);
                                    }
                                }
                                Message::Ping(payload) => {
                                    let _ = write.send(Message::Pong(payload)).await;
                                }
                                Message::Close(_) => break,
                                _ => {}
                            }
                        }
                        _ = ticker.tick(), if heartbeat.is_some() => {
                            if let Some((_, frame)) = &heartbeat {
                                if write.send(Message::Text(frame.clone().into())).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                }
                eprintln!("feed {label}: disconnected");
            }
            Err(err) => {
                attempt = attempt.saturating_add(1);
                // The error text from a failed handshake can echo the URL, and
                // the URL can carry a key. Report the kind, not the text.
                eprintln!(
                    "feed {label}: connect failed ({}), attempt {attempt}",
                    error_kind(&err)
                );
            }
        }
        tokio::time::sleep(backoff(attempt)).await;
    }
}

/// A key-free description of a connection error.
fn error_kind(err: &tokio_tungstenite::tungstenite::Error) -> &'static str {
    use tokio_tungstenite::tungstenite::Error as E;
    match err {
        E::Http(resp) if resp.status().as_u16() == 401 || resp.status().as_u16() == 403 => {
            "rejected — check the API key"
        }
        E::Http(_) => "HTTP error from the provider",
        E::Io(_) => "network error",
        E::Tls(_) => "TLS error",
        E::Url(_) => "bad URL",
        _ => "protocol error",
    }
}
