//! The engine task: one thread, one owner of all trading state.
//!
//! # This is where invariant P0-5 is actually enforced
//!
//! Everything that can change state arrives here as a [`Command`] on one
//! channel and is applied in the order it arrives. Sessions do not share the
//! portfolio, do not lock it, and cannot observe it half-updated. Two orders
//! cannot both pass the margin check against the same free balance, because
//! there is no moment between check and fill for the second one to run.
//!
//! # Nobody is told about a decision before it is on disk
//!
//! Handlers never broadcast directly. They [`announce`](Engine::announce),
//! which only queues, and [`commit_at`](Engine::commit_at) runs after each
//! command: it syncs the journal if anything durable was written, and only
//! then releases the queue. A trader told "filled" can therefore never find,
//! after a power cut, that the engine has no record of the trade — the
//! confirmation is structurally unable to leave before the fsync.
//!
//! This is also group commit for free: a market order journals
//! `OrderAccepted` and `OrderFilled`, and both ride one sync.
//!
//! # Journaling and applying are two different things
//!
//! [`Engine::emit`] journals an event *and* folds it into the portfolio, which
//! is the normal path. [`Engine::append`] journals without folding, and exists
//! for exactly one case: a stop-out, where `Portfolio::evaluate` has already
//! closed the position. Folding the journaled event again would close a second
//! one. Replay takes the other route and folds it, which is why
//! `a_stop_out_replays_to_the_same_state` in the risk crate has to pass.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use domain::{AccountId, Event, InstrumentId, OrderId, OrderType, RejectReason, Timestamp, UserId};
use journal::{Durability, Replay, SegmentedJournal};
use ledger::Ledger;
use money::{Money, Price, Qty, Side};
use pending::{Activation, PendingBook};
use risk::{InstrumentTerms, MarginPolicy, Portfolio, ProposedOrder};
use rollover::{RolloverError, SwapTerms};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::candles::{self, Candles};
use crate::feeds::Source;
use crate::instruments::{catalogue, conversions};
use crate::protocol::{
    kind_str, money_str, price_str, qty_str, reason_str, side_str, AccountDto, AccountSummaryDto,
    ActivityDto, AdminAccountDto, AuditDto, BookLineDto, BooksDto, InstrumentDto, PendingDto,
    PositionDto, QuoteDto, ServerMessage, StatementDto, TrialBalanceDto, FIRST_ACCOUNT,
    FIRST_ORDER_ID,
};
use crate::snapshot;

/// How often accumulated quotes and portfolio changes are broadcast.
///
/// The browser conflates again on its own side; this keeps the broadcast
/// channel from carrying hundreds of messages a second in the first place.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(100);

/// How often a quote is written to the journal, per instrument.
///
/// Not every tick. A tick that changed no decision does not need to be
/// replayable: every decision the engine takes — a fill, a call, a stop-out —
/// is journaled with the price it used, so replaying reproduces balances
/// exactly. What a throttled tick stream loses is intermediate
/// mark-to-market, which no journaled event depends on. Production archives
/// full tick data separately, in ClickHouse, not here.
pub const QUOTE_JOURNAL_INTERVAL: Duration = Duration::from_millis(250);

/// How the engine stores what it decides.
#[derive(Debug, Clone, Copy)]
pub struct EngineConfig {
    /// Size at which a journal segment is sealed.
    pub segment_bytes: u64,
    /// When the journal is forced to disk.
    pub durability: Durability,
    /// UTC hour at which each trading day's rollover runs.
    pub rollover_hour: u8,
    /// How many journaled events may pass between snapshots.
    ///
    /// An operational knob like the segment size, and for the same reason: it
    /// trades how often the gateway writes against how much of the journal's
    /// tail a restart has to re-fold. Zero turns snapshots off.
    pub snapshot_every: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            segment_bytes: journal::DEFAULT_SEGMENT_BYTES,
            durability: Durability::default(),
            // 22:00 UTC is 17:00 New York in winter, the FX market's day end.
            rollover_hour: 22,
            snapshot_every: snapshot::SNAPSHOT_EVERY,
        }
    }
}

/// Opening balance for the demo account, the first time the journal is empty.
const OPENING_BALANCE: &str = "25000.00";
/// Opening bonus. Supports margin, never withdrawable.
const OPENING_CREDIT: &str = "500.00";
const LEVERAGE: u32 = 100;

/// Anything that can change engine state.
#[derive(Debug)]
pub enum Command {
    /// A price arrived from upstream.
    Quote {
        instrument: InstrumentId,
        bid: Price,
        ask: Price,
        /// The feed the aggregator chose.
        source: Source,
        /// What the house adds to each side of this price.
        ///
        /// Supplied by the caller rather than read by the engine, because
        /// it is configuration — a broker changes it — and the engine
        /// holds journaled facts, not settings. Zero passes the feed's
        /// price through untouched.
        markup: Price,
        /// What opening one unit of this instrument costs in commission.
        ///
        /// Arrives with the price for the same reason the markup does: a
        /// resting order fills hours after it was placed and should be
        /// charged what the instrument charges *then*, read from the same
        /// place a market order reads.
        commission: Money,
    },
    /// A terminal wants to trade.
    ///
    /// The account is decided by the session, which knows who signed in; the
    /// engine takes it as given and checks only what it owns — that the
    /// account exists, and that a ticket named by a command belongs to it.
    Place {
        /// Who placed it: the holder, or an administrator acting for them.
        by: UserId,
        account: AccountId,
        instrument: InstrumentId,
        side: Side,
        kind: OrderType,
        qty: Qty,
        price: Price,
        /// Where a stop-limit rests once triggered. Zero means "at the stop".
        limit_price: Price,
    },
    /// Attach, move or remove a position's protection.
    SetProtection {
        by: UserId,
        account: AccountId,
        ticket: OrderId,
        stop_loss: Price,
        take_profit: Price,
    },
    /// A terminal wants to close a position, or part of one.
    Close {
        by: UserId,
        account: AccountId,
        ticket: OrderId,
        /// How much to close. Zero means all of it.
        qty: Qty,
    },
    /// A terminal wants to withdraw a resting order.
    CancelOrder {
        by: UserId,
        account: AccountId,
        ticket: OrderId,
    },
    /// An administrator moves money, sets a bonus, or stops an account.
    ///
    /// One command rather than four because they are one decision with four
    /// shapes, and the engine applies them identically: check, journal, tell
    /// the account. Authorisation happens in the session — only an
    /// administrator can send this — and the engine records who sent it.
    Administer {
        by: UserId,
        account: AccountId,
        action: AdminAction,
        /// Reported back to the administrator's own session.
        reply: oneshot::Sender<AdminOutcome>,
    },
    /// Put a trade right: an adjustment, or a corrected entry price.
    Amend {
        by: UserId,
        account: AccountId,
        ticket: OrderId,
        amendment: Amendment,
        reason: domain::AmendReason,
        reply: oneshot::Sender<AdminOutcome>,
    },
    /// Every account with its figures, for the administrator's book.
    Overview {
        reply: oneshot::Sender<Vec<AdminAccountDto>>,
    },
    /// The most recent attributed actions, newest first.
    Audit {
        limit: usize,
        reply: oneshot::Sender<Vec<AuditDto>>,
    },
    /// Record that somebody signed in, or failed to.
    ///
    /// A command rather than a direct write because the engine is the only
    /// writer: a session thread appending to the journal would be a second
    /// one, and the sequence numbers would stop meaning "in this order".
    NoteSession {
        user: UserId,
        what: domain::SessionEvent,
        address: [u8; 16],
    },
    /// Every movement on one account, newest first.
    Statement {
        account: AccountId,
        limit: usize,
        reply: oneshot::Sender<Vec<StatementDto>>,
    },
    /// Bars for a chart.
    Chart {
        instrument: InstrumentId,
        /// Seconds per bar, already validated.
        seconds: i64,
        limit: usize,
        reply: oneshot::Sender<Vec<candles::Bar>>,
    },
    /// Who signed in, newest first.
    Activity {
        limit: usize,
        reply: oneshot::Sender<Vec<ActivityDto>>,
    },
    /// The double-entry books, for one account or for everything.
    Books {
        account: Option<AccountId>,
        reply: oneshot::Sender<BooksDto>,
    },
    /// Broadcast whatever has accumulated.
    Flush,
    /// Send the full state of one account to one caller.
    Snapshot {
        account: AccountId,
        reply: oneshot::Sender<Box<ServerMessage>>,
    },
    /// Open another account for a user, demo or live.
    ///
    /// The session decides whether they are *allowed* a live one — that needs
    /// identity checks, which live with the logins and not in the journal —
    /// and the engine decides whether the request makes sense at all.
    OpenAccount {
        user: UserId,
        live: bool,
        reply: oneshot::Sender<std::result::Result<AccountId, &'static str>>,
    },
    /// Which accounts a session may act on.
    ///
    /// `user` is `Some` for one person's own accounts, opening a starter
    /// account if they hold none, and `None` for every account on the book —
    /// which is an administrator's view. Opening an account is a write, which
    /// is why this is a command to the single writer rather than a read of
    /// state.
    Accounts {
        user: Option<UserId>,
        reply: oneshot::Sender<Vec<AccountSummaryDto>>,
    },
}

/// Putting a trade right: money, or an entry price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Amendment {
    /// Credit or debit the account. Signed; positive pays the client.
    Adjust(money::Money),
    /// Treat an open position as having opened at this price.
    CorrectOpenPrice(Price),
}

/// What an administrator asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminAction {
    /// Pay cleared funds in.
    Deposit(money::Money),
    /// Pay funds out. Refused if it would eat into margin.
    Withdraw(money::Money),
    /// Set the non-withdrawable bonus to an amount.
    SetCredit(money::Money),
    /// Set aside, or release, money the market cannot reach.
    SetInsurance(money::Money),
    /// Move money to another account, both legs in one event.
    Transfer {
        /// Where it goes. The account being administered is where it leaves.
        to: AccountId,
        amount: money::Money,
    },
    /// Hold both sides of an instrument at once, or net them off.
    SetHedging(bool),
    /// Make the account live, or put it back to demo.
    SetLive(bool),
    /// Start a demo account's thirty days again.
    RenewDemo,
    /// Stop the account opening positions.
    Suspend,
    /// Let it trade again.
    Reinstate,
}

/// Whether an administrator's action went through, and why not if it did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminOutcome {
    Done,
    /// The reason, in words an operator can act on.
    Refused(&'static str),
}

/// One broadcast, and who it is for.
///
/// Quotes are everybody's. A fill, a portfolio, a margin call and a stop-out
/// belong to one account, and a session is sent only what its selected
/// account is entitled to: with several traders signed in at once, the
/// alternative is one client watching another's positions.
#[derive(Debug, Clone)]
pub struct Broadcast {
    /// `None` for anything that is true for everyone.
    pub to: Option<AccountId>,
    pub message: ServerMessage,
}

/// The single writer.
pub struct Engine {
    portfolio: Portfolio,
    resting: PendingBook,
    journal: SegmentedJournal,
    policy: MarginPolicy,
    terms: BTreeMap<InstrumentId, InstrumentTerms>,
    next_order: u64,
    tx: broadcast::Sender<Broadcast>,
    dirty_quotes: BTreeMap<InstrumentId, (Price, Price)>,
    last_journaled: BTreeMap<InstrumentId, Instant>,
    /// Accounts whose figures have changed since the last flush.
    ///
    /// A set rather than a flag: with many accounts open, re-sending every
    /// portfolio because one of them traded is work nobody asked for.
    dirty_accounts: BTreeSet<AccountId>,
    /// Messages waiting for the commit that makes them true.
    outbox: Vec<Broadcast>,
    /// Set when a decision has been written and not yet synced.
    decision_pending: bool,
    last_sync: Instant,
    durability: Durability,
    swap_terms: BTreeMap<InstrumentId, SwapTerms>,
    /// When each instrument last had a price, and from where.
    quote_at: BTreeMap<InstrumentId, Instant>,
    quote_source: BTreeMap<InstrumentId, Source>,
    /// Older than this, an instrument's last quote is not a price to fill at.
    max_quote_age: BTreeMap<InstrumentId, Duration>,
    /// Why an instrument cannot be traded, when it cannot.
    notes: BTreeMap<InstrumentId, &'static str>,
    rollover_hour: u8,
    /// Set once the engine has refused an unattended catch-up, so the refusal
    /// is reported once rather than every hundred milliseconds.
    rollover_blocked: bool,
    /// Where snapshots live, and the sequence the last one covered.
    journal_dir: std::path::PathBuf,
    journal_base: String,
    snapshot_through: u64,
    snapshot_every: u64,
    /// Bars, built from every quote the engine has seen.
    candles: Candles,
    /// The double-entry books, folded from the same journal.
    ///
    /// Not a second source of truth: every posting here is derived from an
    /// event the portfolio also applied, and the two are checked against each
    /// other — client cash must equal the account's balance, and every
    /// currency must net to zero. When they disagree the bug is ours, and we
    /// know within one event rather than in an audit.
    ledger: Ledger,
    /// Every money movement, oldest first, with the account it was on.
    ///
    /// A window like the audit, and for the same reason: the journal is the
    /// record, and a statement screen should not have to read all of it. The
    /// balance on each line is the balance *after* that movement, taken as it
    /// was applied — which is what makes it a statement rather than a list.
    statement: Vec<(AccountId, StatementDto)>,
    /// The most recent sign-ins, oldest first. A window on the journal, like
    /// the audit.
    activity: Vec<ActivityDto>,
    /// The most recent attributed actions, oldest first.
    ///
    /// A window, not the record: the record is the journal, which holds every
    /// one of these for ever. This is what an administrator's screen can be
    /// handed without reading a gigabyte of history, and it is rebuilt by
    /// replay like everything else.
    audit: Vec<AuditDto>,
}

/// How many accounts one login may hold.
///
/// Enough for a demo, a live account and a few strategies kept apart; few
/// enough that a script cannot open ten thousand of them.
pub const ACCOUNTS_PER_USER: usize = 8;

/// How many recent actions the engine keeps for the audit view.
const AUDIT_WINDOW: usize = 500;

/// How many money movements the engine keeps for statements, across all
/// accounts. Two thousand lines is a long statement and a small amount of
/// memory; the journal has the rest.
const STATEMENT_WINDOW: usize = 2_000;

/// Turns an applied event into a statement line, if money moved.
///
/// Reads the balance *after* the event from the portfolio it was just applied
/// to — which is why this is called with both, and why the line cannot be
/// rebuilt later from the event alone.
/// Every statement line one event produces.
///
/// All but one movement touch a single account and make a single line. A
/// transfer touches two, and both legs carry the same sequence number —
/// they are one event, and a statement showing them at different moments
/// would invite somebody to go looking for the money in between.
fn statement_lines(
    record: &domain::Record,
    applied: &risk::Applied,
    portfolio: &Portfolio,
) -> Vec<(AccountId, StatementDto)> {
    if let Event::FundsTransferred {
        from, to, amount, ..
    } = record.event
    {
        let out = amount.neg().unwrap_or(money::Money::ZERO);
        return vec![
            statement_line(record, portfolio, from, "transferOut", None, out),
            statement_line(record, portfolio, to, "transferIn", None, amount),
        ];
    }
    statement_entry(record, applied, portfolio)
        .into_iter()
        .collect()
}

/// One line, with the balance the account was left holding.
fn statement_line(
    record: &domain::Record,
    portfolio: &Portfolio,
    account: AccountId,
    kind: &'static str,
    ticket: Option<u64>,
    amount: money::Money,
) -> (AccountId, StatementDto) {
    let balance = portfolio
        .account(account)
        .map_or(money::Money::ZERO, |a| a.balance);
    (
        account,
        StatementDto {
            seq: record.seq.0,
            at: record.at.0 / 1_000_000,
            kind,
            ticket,
            amount: money_str(amount),
            balance: money_str(balance),
        },
    )
}

fn statement_entry(
    record: &domain::Record,
    applied: &risk::Applied,
    portfolio: &Portfolio,
) -> Option<(AccountId, StatementDto)> {
    let (account, kind, ticket, amount) = match record.event {
        Event::FundsDeposited {
            account, amount, ..
        } => (account, "deposit", None, amount),
        Event::FundsWithdrawn {
            account, amount, ..
        } => (
            account,
            "withdrawal",
            None,
            amount.neg().unwrap_or(Money::ZERO),
        ),
        Event::CreditSet { account, .. } => {
            let moved = applied.credit_moved.map(|(_, m)| m)?;
            if moved.is_zero() {
                return None;
            }
            (account, "bonus", None, moved)
        }
        Event::TradeAdjusted {
            account,
            order,
            amount,
            ..
        } => (
            account,
            "adjustment",
            (order.0 != 0).then_some(order.0),
            amount,
        ),
        Event::SwapCharged {
            account,
            order,
            amount,
            ..
        } => {
            // Accrued on the position, not yet in the balance — so it is on
            // the statement as financing with no balance change, because a
            // trader asking "what did last night cost" is owed an answer
            // before they close.
            (account, "financing", Some(order.0), amount)
        }
        Event::OrderFilled { order, .. } => {
            let (account, realised) = applied.realised?;
            if realised.is_zero() {
                // Opening a position moves no money.
                return None;
            }
            (
                account,
                "trade",
                Some(order.0),
                realised.total().unwrap_or(Money::ZERO),
            )
        }
        Event::PositionClosed { account, order, .. } => {
            let realised = applied.realised.map(|(_, r)| r)?;
            (
                account,
                "trade",
                Some(order.0),
                realised.total().unwrap_or(Money::ZERO),
            )
        }
        Event::PositionStoppedOut { account, order, .. } => {
            let realised = applied.realised.map(|(_, r)| r)?;
            (
                account,
                "stopOut",
                Some(order.0),
                realised.total().unwrap_or(Money::ZERO),
            )
        }
        Event::ProtectionTriggered {
            account,
            order,
            stop_loss,
            ..
        } => {
            let realised = applied.realised.map(|(_, r)| r)?;
            (
                account,
                if stop_loss { "stopLoss" } else { "takeProfit" },
                Some(order.0),
                realised.total().unwrap_or(Money::ZERO),
            )
        }
        _ => return None,
    };

    Some(statement_line(
        record, portfolio, account, kind, ticket, amount,
    ))
}

/// True when `value` is a whole multiple of `step`.
///
/// A step of zero means no grid, so everything is on it. The same rule the
/// risk crate applies to an order's size, said again here because a close
/// is judged against the same grid and the risk crate's copy is private to
/// its own admission.
fn multiple_of(value: Qty, step: Qty) -> bool {
    step.is_zero() || value.raw() % step.raw() == 0
}

/// True when `price` sits on the instrument's tick grid.
fn price_on_tick(price: Price, tick: Price) -> bool {
    tick.is_zero() || price.raw() % tick.raw() == 0
}

/// Posts one applied event to the double-entry books.
///
/// Called from exactly two places — the live `emit`/`append_realised` and the
/// replay loop — with the same event and the same realisation, so the books a
/// restart rebuilds are the books that were live. The journal sequence is the
/// transaction id: unique, never reused, and already the order things
/// happened in.
/// What a fill of this size costs to open.
///
/// The rate is per unit of quantity, so this is a multiplication — done as
/// a ratio in integers, like every other figure that reaches a balance.
fn commission_on(market: &risk::Market, instrument: InstrumentId, qty: Qty) -> Result<Money> {
    let per_unit = market.commission_of(instrument);
    if per_unit.is_zero() {
        return Ok(Money::ZERO);
    }
    Ok(per_unit.mul_ratio(i128::from(qty.raw()), i128::from(money::QTY_ONE))?)
}

fn post_to_books(
    ledger: &mut Ledger,
    portfolio: &Portfolio,
    record: &domain::Record,
    applied: &risk::Applied,
) -> Result<()> {
    use ledger::TxId;

    let currency_of = |account: AccountId| {
        portfolio
            .account(account)
            .map_or_else(|| portfolio.market.settlement(), |a| a.currency)
    };
    let id = TxId(record.seq.0);

    let tx = match record.event {
        // Opening a position moves no cash — margin is posted, not paid —
        // so a fill reaches the books only when it charged something.
        Event::OrderFilled {
            order, commission, ..
        } if !commission.is_zero() => portfolio
            .owner_of_order(order)
            .map(|who| ledger::commission(id, record.at, who, currency_of(who), commission))
            .transpose()?,
        Event::FundsDeposited {
            account, amount, ..
        } => Some(ledger::deposit(
            id,
            record.at,
            account,
            currency_of(account),
            amount,
        )),
        Event::FundsWithdrawn {
            account, amount, ..
        } => Some(ledger::withdraw(
            id,
            record.at,
            account,
            currency_of(account),
            amount,
        )),
        // Two client books and no house book: nothing is created, so the
        // platform is neither richer nor poorer. The currency is read from
        // the sending account because the engine refused the transfer
        // unless both hold the same one.
        Event::FundsTransferred {
            from, to, amount, ..
        } => Some(ledger::transfer(
            id,
            record.at,
            from,
            to,
            currency_of(from),
            amount,
        )?),
        // Both pots are the client's, so this is a transfer rather than
        // money arriving: nothing is created and no house book moves.
        Event::InsuranceSet { account, .. } => match applied.insured {
            Some((_, moved)) if moved.is_zero() => None,
            Some((_, moved)) => Some(ledger::insure(
                id,
                record.at,
                account,
                currency_of(account),
                moved.neg()?,
            )?),
            None => None,
        },
        Event::CreditSet { account, .. } => match applied.credit_moved {
            // A bonus set to what it already was moves nothing, and a
            // transaction with two zero legs is noise in the books.
            Some((_, moved)) if moved.is_zero() => None,
            Some((_, moved)) if moved.is_negative() => Some(ledger::revoke_credit(
                id,
                record.at,
                account,
                currency_of(account),
                moved.neg()?,
            )),
            Some((_, moved)) => Some(ledger::grant_credit(
                id,
                record.at,
                account,
                currency_of(account),
                moved,
            )),
            None => None,
        },
        Event::TradeAdjusted {
            account, amount, ..
        } => Some(
            ledger::adjust(id, record.at, account, currency_of(account), amount)
                .map_err(|e| anyhow::anyhow!("building an adjustment: {e}"))?,
        ),
        _ => None,
    };

    if let Some(tx) = tx {
        ledger
            .post(&tx)
            .map_err(|e| anyhow::anyhow!("posting {} to the books: {e}", record.seq.0))?;
        return Ok(());
    }

    // What closing cost rides in the same transaction as what it made,
    // because one journal record gets one transaction id — the ledger
    // refuses a second, and that guard is what makes a replay safe.
    //
    // Only a hedged close has a charge here; a netting close is an opposing
    // order and its charge rides on `OrderFilled` above, reaching the same
    // book by the other road.
    let charge = match record.event {
        Event::PositionClosed { commission, .. } => commission,
        _ => Money::ZERO,
    };

    // A close: trading profit, its financing, and the charge, in one
    // transaction.
    if let Some((account, realised)) = applied.realised {
        if realised.is_zero() && charge.is_zero() {
            // Opening a position realises nothing. No postings, no noise.
            return Ok(());
        }
        let tx = ledger::close_position(
            id,
            record.at,
            account,
            currency_of(account),
            realised.profit,
            realised.swap,
            charge,
        )
        .map_err(|e| anyhow::anyhow!("building a close for {}: {e}", record.seq.0))?;
        ledger
            .post(&tx)
            .map_err(|e| anyhow::anyhow!("posting {} to the books: {e}", record.seq.0))?;
    }
    Ok(())
}

/// Turns a journaled sign-in into a line of the activity log.
fn activity_entry(record: &domain::Record) -> Option<ActivityDto> {
    let Event::SessionChanged {
        user,
        what,
        address,
    } = record.event
    else {
        return None;
    };
    Some(ActivityDto {
        seq: record.seq.0,
        at: record.at.0 / 1_000_000,
        user: user.0,
        login: None,
        what: match what {
            domain::SessionEvent::Opened => "signedIn",
            domain::SessionEvent::Resumed => "resumed",
            domain::SessionEvent::Refused => "refused",
            domain::SessionEvent::Closed => "signedOut",
        },
        address: address_str(address),
    })
}

/// Renders sixteen address bytes as text, IPv4-mapped addresses as IPv4.
fn address_str(bytes: [u8; 16]) -> String {
    if bytes == [0u8; 16] {
        return String::new();
    }
    let addr = std::net::Ipv6Addr::from(bytes);
    match addr.to_ipv4_mapped() {
        Some(v4) => v4.to_string(),
        None => addr.to_string(),
    }
}

/// Packs a socket address into the sixteen bytes the journal carries.
pub fn address_bytes(addr: Option<std::net::SocketAddr>) -> [u8; 16] {
    match addr.map(|a| a.ip()) {
        Some(std::net::IpAddr::V4(v4)) => v4.to_ipv6_mapped().octets(),
        Some(std::net::IpAddr::V6(v6)) => v6.octets(),
        None => [0u8; 16],
    }
}

/// An amendment reason in the terminal's words.
pub fn amend_reason_str(reason: domain::AmendReason) -> &'static str {
    use domain::AmendReason as R;
    match reason {
        R::BadPrice => "badPrice",
        R::DealingError => "dealingError",
        R::FeeReturned => "feeReturned",
        R::Goodwill => "goodwill",
        R::Other => "other",
    }
}

/// Parses the wire's reason. Unknown reasons are refused rather than filed
/// as "other": a reason the gateway does not know is a terminal out of step.
pub fn amend_reason_from(text: &str) -> Option<domain::AmendReason> {
    use domain::AmendReason as R;
    match text {
        "badPrice" => Some(R::BadPrice),
        "dealingError" => Some(R::DealingError),
        "feeReturned" => Some(R::FeeReturned),
        "goodwill" => Some(R::Goodwill),
        "other" => Some(R::Other),
        _ => None,
    }
}

/// Turns a journaled event into an audit line, or `None` if nobody did it.
///
/// Only events that carry an actor appear. A quote, a rollover and a stop-out
/// are the engine's own doing and belong in a different kind of log; what an
/// audit answers is "who decided this", and for those the answer is nobody.
fn audit_entry(record: &domain::Record) -> Option<AuditDto> {
    let (by, account, action, detail) = match record.event {
        Event::AccountOpened { account, .. } => {
            (UserId::SYSTEM, account, "accountOpened", String::new())
        }
        Event::AccountOwnerSet { account, owner } => (
            UserId::SYSTEM,
            account,
            "ownerSet",
            format!("user {}", owner.0),
        ),
        Event::FundsDeposited {
            account,
            amount,
            by,
        } => (by, account, "deposit", money_str(amount)),
        Event::FundsWithdrawn {
            account,
            amount,
            by,
        } => (by, account, "withdrawal", money_str(amount)),
        // Recorded against the account it left, naming where it went. One
        // line, because it was one decision.
        Event::FundsTransferred {
            from,
            to,
            amount,
            by,
        } => (
            by,
            from,
            "transfer",
            format!("{} to account {}", money_str(amount), to.0),
        ),
        Event::CreditSet {
            account,
            amount,
            by,
        } => (by, account, "creditSet", money_str(amount)),
        Event::AccountSuspended { account, by } => (by, account, "suspended", String::new()),
        Event::AccountReinstated { account, by } => (by, account, "reinstated", String::new()),
        Event::TradeAdjusted {
            account,
            order,
            amount,
            reason,
            by,
        } => (
            by,
            account,
            "adjustment",
            format!(
                "{} on #{} — {}",
                money_str(amount),
                order.0,
                amend_reason_str(reason)
            ),
        ),
        Event::PositionCorrected {
            account,
            order,
            from,
            to,
            reason,
            by,
        } => (
            by,
            account,
            "priceCorrected",
            format!(
                "#{} {} → {} — {}",
                order.0,
                price_str(from),
                price_str(to),
                amend_reason_str(reason)
            ),
        ),
        // An order is in the audit only when somebody other than the engine
        // placed it; whether that was the holder or an administrator is the
        // part worth recording, and `by` says which.
        Event::OrderAccepted {
            by,
            order,
            account,
            side,
            qty,
            instrument,
            ..
        } => (
            by,
            account,
            "order",
            format!(
                "#{} {} {} on {}",
                order.0,
                side_str(side),
                qty_str(qty),
                instrument.0
            ),
        ),
        Event::PendingOrderPlaced {
            by,
            order,
            account,
            side,
            qty,
            instrument,
            trigger_price,
            ..
        } => (
            by,
            account,
            "restingOrder",
            format!(
                "#{} {} {} on {} at {}",
                order.0,
                side_str(side),
                qty_str(qty),
                instrument.0,
                price_str(trigger_price)
            ),
        ),
        _ => return None,
    };
    Some(AuditDto {
        seq: record.seq.0,
        at: record.at.0 / 1_000_000,
        by: by.0,
        by_login: None,
        account: account.0,
        action,
        detail,
    })
}

impl Engine {
    /// Opens the journal, replays it, and lists anything missing.
    ///
    /// Restarting the gateway therefore keeps balances and open positions:
    /// they were never stored, they are re-derived from the event stream.
    ///
    /// The replay is streamed one segment at a time rather than loaded whole,
    /// so startup memory is bounded by the segment size no matter how long the
    /// journal has been running.
    pub fn open(
        dir: &Path,
        base: &str,
        config: EngineConfig,
        tx: broadcast::Sender<Broadcast>,
    ) -> Result<Self> {
        let journal = SegmentedJournal::open_with(dir, base, config.segment_bytes)
            .context("opening the journal")?;
        let report = journal.scan_report();
        if report.truncated_bytes > 0 {
            eprintln!(
                "journal: repaired a torn tail in the active segment, discarded {} bytes",
                report.truncated_bytes
            );
        }

        // A snapshot, if there is a usable one: state as of some sequence,
        // so only the tail after it has to be folded. It is derived from the
        // journal and never authoritative — delete every snapshot and the
        // gateway comes up with the same state, only slower.
        let restored = snapshot::read(dir, base, domain::Currency::USD).and_then(|stored| {
            snapshot::restore(&stored, domain::Currency::USD)
                .map(|(portfolio, ledger)| (stored.through, portfolio, ledger))
        });
        let (resume_after, mut portfolio, mut ledger) = match restored {
            Some((through, portfolio, ledger)) => {
                println!("engine: resuming from a snapshot taken through event {through}");
                (through, portfolio, ledger)
            }
            None => (0, Portfolio::new(), Ledger::new()),
        };
        // Bars are rebuilt from the journal's own quotes, with the times they
        // were journaled at — so a restart draws the same chart rather than
        // one stamped with the moment of the replay.
        let mut candles = Candles::new();
        let mut audit: Vec<AuditDto> = Vec::new();
        let mut activity: Vec<ActivityDto> = Vec::new();
        let mut statement: Vec<(AccountId, StatementDto)> = Vec::new();
        let mut folded = 0u64;
        for record in Replay::open(dir, base).context("opening the replay")? {
            let record = record.context("reading a journaled event")?;
            // Everything the snapshot covered is already in the state. Quotes
            // are still read, because bars are not snapshotted and a chart
            // with no history is worse than one that costs a little startup.
            if record.seq.0 <= resume_after {
                if let Event::QuoteTicked {
                    instrument,
                    bid,
                    ask,
                } = record.event
                {
                    candles.observe(instrument, bid, ask, record.at.0);
                }
                continue;
            }
            let applied = portfolio
                .apply(&record)
                .context("folding a journaled event")?;
            post_to_books(&mut ledger, &portfolio, &record, &applied)
                .context("folding a journaled event into the books")?;
            if let Event::QuoteTicked {
                instrument,
                bid,
                ask,
            } = record.event
            {
                candles.observe(instrument, bid, ask, record.at.0);
            }
            for line in statement_lines(&record, &applied, &portfolio) {
                statement.push(line);
                if statement.len() > STATEMENT_WINDOW {
                    statement.remove(0);
                }
            }
            if let Some(entry) = audit_entry(&record) {
                audit.push(entry);
                if audit.len() > AUDIT_WINDOW {
                    audit.remove(0);
                }
            }
            if let Some(entry) = activity_entry(&record) {
                activity.push(entry);
                if activity.len() > AUDIT_WINDOW {
                    activity.remove(0);
                }
            }
            folded += 1;
        }
        let segment_count = journal.segments().map(|s| s.len()).unwrap_or(1);
        println!("engine: replayed {folded} events across {segment_count} segment(s)");

        // Conversion routes are configuration, not journaled state: they say
        // which pair prices which currency, and the rate itself always comes
        // from that pair's live quote.
        for (currency, how) in conversions() {
            portfolio.market.route(currency, how);
        }

        let listings = catalogue();
        let max_quote_age = listings
            .iter()
            .map(|l| (l.terms.instrument.id, l.max_quote_age))
            .collect::<BTreeMap<_, _>>();
        let notes = listings
            .iter()
            .filter_map(|l| l.note.map(|n| (l.terms.instrument.id, n)))
            .collect::<BTreeMap<_, _>>();
        let swap_terms = listings
            .iter()
            .map(|l| (l.terms.instrument.id, l.swap))
            .collect::<BTreeMap<_, _>>();
        let terms = listings
            .into_iter()
            .map(|l| (l.terms.instrument.id, l.terms))
            .collect::<BTreeMap<_, _>>();

        // Order ids continue past anything already journaled.
        let next_order = FIRST_ORDER_ID.0.max(report.next_seq.0 + FIRST_ORDER_ID.0);

        let mut engine = Engine {
            portfolio,
            resting: PendingBook::new(),
            journal,
            policy: MarginPolicy::default(),
            terms,
            next_order,
            tx,
            dirty_quotes: BTreeMap::new(),
            last_journaled: BTreeMap::new(),
            dirty_accounts: BTreeSet::new(),
            outbox: Vec::new(),
            decision_pending: false,
            last_sync: Instant::now(),
            durability: config.durability,
            swap_terms,
            quote_at: BTreeMap::new(),
            quote_source: BTreeMap::new(),
            max_quote_age,
            notes,
            rollover_hour: config.rollover_hour,
            rollover_blocked: false,
            journal_dir: dir.to_path_buf(),
            journal_base: base.to_owned(),
            snapshot_through: resume_after,
            snapshot_every: config.snapshot_every,
            candles,
            ledger,
            statement,
            activity,
            audit,
        };
        engine.bootstrap()?;
        Ok(engine)
    }

    /// Writes the events a fresh journal is missing.
    ///
    /// Accounts are deliberately not among them. One is opened the first time
    /// its owner signs in, so an account always has a person attached to it —
    /// an unowned account is one nobody can be held to.
    fn bootstrap(&mut self) -> Result<()> {
        let missing: Vec<_> = self
            .terms
            .values()
            .filter(|t| self.portfolio.instrument(t.instrument.id).is_none())
            .map(|t| t.instrument)
            .collect();
        for instrument in missing {
            self.emit(Event::InstrumentListed { instrument })?;
        }
        if self.portfolio.last_rollover().is_none() {
            // Nothing was open before this journal existed, so there is no
            // history to charge. Record today's rollover as done, or the first
            // check would try to catch up on every day since 1970.
            let today = rollover::latest_due(Self::now().0, self.rollover_hour)
                .context("computing the current trading day")?;
            self.emit(Event::RolloverCompleted { day: today })?;
        }
        self.journal.sync()?;
        self.decision_pending = false;
        self.last_sync = Instant::now();
        Ok(())
    }

    /// A timestamp thirty-one days old, for backdating a demo account in a
    /// test. Here rather than in the test module so it reads from the same
    /// clock the expiry check does.
    #[cfg(test)]
    fn thirty_one_days_ago() -> Timestamp {
        Timestamp(Self::now().0 - 31 * 24 * 60 * 60 * 1_000_000_000)
    }

    /// Wall-clock time. This is the boundary where it enters the system: the
    /// value is journaled, so a replay uses the recorded one and never asks
    /// the clock again.
    fn now() -> Timestamp {
        Timestamp(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0),
        )
    }

    /// Journals an event, folds it into the portfolio, and posts the books.
    fn emit(&mut self, event: Event) -> Result<()> {
        let record = self.journal.append(Self::now(), event)?;
        self.decision_pending |= event.must_be_durable();
        let applied = self.portfolio.apply(&record)?;
        post_to_books(&mut self.ledger, &self.portfolio, &record, &applied)?;
        self.remember_money(&record, &applied);
        self.remember(&record);
        Ok(())
    }

    /// Adds a money movement to the statement window.
    fn remember_money(&mut self, record: &domain::Record, applied: &risk::Applied) {
        for entry in statement_lines(record, applied, &self.portfolio) {
            self.statement.push(entry);
            if self.statement.len() > STATEMENT_WINDOW {
                self.statement.remove(0);
            }
        }
    }

    /// Adds an event to the audit and activity windows, if it belongs there.
    fn remember(&mut self, record: &domain::Record) {
        if let Some(entry) = audit_entry(record) {
            self.audit.push(entry);
            if self.audit.len() > AUDIT_WINDOW {
                self.audit.remove(0);
            }
        }
        if let Some(entry) = activity_entry(record) {
            self.activity.push(entry);
            if self.activity.len() > AUDIT_WINDOW {
                self.activity.remove(0);
            }
        }
    }

    /// Journals an event that has already been applied. See the module note.
    ///
    /// The portfolio is not folded again — the caller already changed it — but
    /// the books still must be, and with the realisation the caller got back.
    /// Passing it in rather than recomputing is what keeps the live path and
    /// the replay path posting the same transaction.
    fn append_realised(&mut self, event: Event, applied: risk::Applied) -> Result<()> {
        let record = self.journal.append(Self::now(), event)?;
        self.decision_pending |= event.must_be_durable();
        post_to_books(&mut self.ledger, &self.portfolio, &record, &applied)?;
        self.remember_money(&record, &applied);
        self.remember(&record);
        Ok(())
    }

    /// Journals an event that moved no money and has already been applied.
    fn append(&mut self, event: Event) -> Result<()> {
        self.append_realised(event, risk::Applied::default())
    }

    /// Queues a message for one account's sessions. Never sends directly.
    fn announce_to(&mut self, account: AccountId, message: ServerMessage) {
        self.outbox.push(Broadcast {
            to: Some(account),
            message,
        });
    }

    /// Queues a message every session should see — prices, and only prices.
    fn announce_all(&mut self, message: ServerMessage) {
        self.outbox.push(Broadcast { to: None, message });
    }

    /// Marks an account's figures as needing a fresh portfolio broadcast.
    fn touch(&mut self, account: AccountId) {
        self.dirty_accounts.insert(account);
    }

    /// Writes a snapshot when enough has happened since the last one.
    ///
    /// After the sync, never before: a snapshot of state whose events are not
    /// yet on disk would, after a crash, describe a history the journal does
    /// not contain. It is also best effort — a snapshot that cannot be
    /// written costs startup time, not correctness, so it is reported and
    /// shrugged off.
    fn snapshot_if_due(&mut self) {
        if self.snapshot_every == 0 {
            return;
        }
        let through = self.journal.next_seq().0.saturating_sub(1);
        if through < self.snapshot_through.saturating_add(self.snapshot_every) {
            return;
        }
        let stored = snapshot::capture(&self.portfolio, &self.ledger, domain::Seq(through));
        match snapshot::write(&self.journal_dir, &self.journal_base, &stored) {
            Ok(()) => {
                self.snapshot_through = through;
                println!("engine: snapshot written through event {through}");
            }
            Err(err) => eprintln!("engine: could not write a snapshot: {err:#}"),
        }
    }

    /// Makes queued decisions durable, then tells everyone about them.
    ///
    /// The order inside this function is the whole guarantee: sync first,
    /// release second. `now` is passed in rather than read so the cadence can
    /// be tested without sleeping.
    fn commit_at(&mut self, now: Instant) -> Result<()> {
        let since = now.saturating_duration_since(self.last_sync);
        if self
            .durability
            .should_sync(self.decision_pending, self.journal.is_dirty(), since)
        {
            self.journal.sync()?;
            self.decision_pending = false;
            self.last_sync = now;
            self.snapshot_if_due();
        }

        // Nothing past this line may run before the sync above has returned.
        for message in self.outbox.drain(..) {
            let _ = self.tx.send(message);
        }
        Ok(())
    }

    /// The accounts a session may act on, opening one if a client has none.
    fn accounts_for(&mut self, user: Option<UserId>) -> Result<Vec<AccountSummaryDto>> {
        let ids = match user {
            Some(user) => {
                let mut ids = self.portfolio.accounts_of(user);
                if ids.is_empty() {
                    ids.push(self.open_account(user)?);
                }
                ids
            }
            // An administrator sees the book, not one person's holding.
            None => self.portfolio.account_ids().collect(),
        };

        let market = &self.portfolio.market;
        let mut out = Vec::new();
        for id in ids {
            let Some(account) = self.portfolio.account(id) else {
                continue;
            };
            out.push(AccountSummaryDto {
                id: id.0,
                currency: account.currency.to_string(),
                balance: money_str(account.balance),
                // Equity needs prices, and a market that has not quoted yet
                // is not an error — the balance is still the balance.
                equity: money_str(account.equity(market).unwrap_or(account.balance)),
                owner: self.portfolio.owner_of(id).map(|u| u.0),
                live: account.live,
                demo_expires_at: account.demo_expires_at().map(|t| t.0 / 1_000_000),
            });
        }
        Ok(out)
    }

    /// Opens an account for a user and funds it as a demo.
    ///
    /// The deposit is the demo platform's, not a real one: funding through
    /// the ledger, with a double-entry record and an operator behind it,
    /// arrives with the funds screens. What matters here is the order —
    /// opened, owned, then funded — so an account is never fundable before
    /// anyone is responsible for it.
    fn open_account(&mut self, user: UserId) -> Result<AccountId> {
        let id = self.portfolio.next_account_id(FIRST_ACCOUNT);
        self.emit(Event::AccountOpened {
            account: id,
            currency: domain::Currency::USD,
            leverage: LEVERAGE,
        })?;
        self.emit(Event::AccountOwnerSet {
            account: id,
            owner: user,
        })?;
        self.emit(Event::FundsDeposited {
            account: id,
            amount: money::Money::parse(OPENING_BALANCE)?,
            // The platform's own demo funding, not an operator's decision.
            by: UserId::SYSTEM,
        })?;
        self.emit(Event::CreditSet {
            account: id,
            amount: money::Money::parse(OPENING_CREDIT)?,
            by: UserId::SYSTEM,
        })?;
        println!("engine: opened account {} for user {}", id.0, user.0);
        Ok(id)
    }

    /// Whether this login already holds a demo account with time left.
    ///
    /// One demo per login, and only one at a time. A practice account is a
    /// trial, and a trial somebody can take again the moment it ends — or
    /// five of at once — is not a trial, it is an unlimited free product
    /// with an expiry date on each copy.
    ///
    /// Expired ones do not count. The thirty days running out is the end of
    /// that trial, and whether a client may start another is a commercial
    /// decision the desk makes by renewing the old one or letting them open
    /// a new one; what this refuses is holding two at once.
    ///
    /// Live accounts are not limited here. A client may hold several of
    /// those — they are sub-accounts of a funded relationship, and the
    /// money in them is already theirs.
    fn has_a_running_demo(&self, user: UserId) -> bool {
        let now = Self::now();
        self.portfolio
            .accounts_of(user)
            .into_iter()
            .filter_map(|id| self.portfolio.account(id))
            .any(|a| !a.live && !a.demo_expired(now))
    }

    /// Runs until the command channel closes.
    pub async fn run(mut self, mut rx: mpsc::Receiver<Command>) {
        while let Some(command) = rx.recv().await {
            if let Err(err) = self.handle(command) {
                // A failure here is a bug, not a client problem. Report it and
                // keep serving rather than taking every session down.
                eprintln!("engine: {err:#}");
            }
            // Commit even after a failure. A handler that journaled two events
            // and then failed on the third has still made two decisions, and
            // they must be durable and announced like any others.
            if let Err(err) = self.commit_at(Instant::now()) {
                // A failed fsync means durability can no longer be promised.
                // Continuing would mean confirming trades that may not survive
                // a restart, so the engine stops instead.
                eprintln!("engine: cannot sync the journal, stopping: {err:#}");
                return;
            }
        }
        let _ = self.journal.sync();
    }

    fn handle(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Quote {
                instrument,
                bid,
                ask,
                source,
                markup,
                commission,
            } => self.on_quote(instrument, bid, ask, source, markup, commission),
            Command::Place {
                by,
                account,
                instrument,
                side,
                kind,
                qty,
                price,
                limit_price,
            } => self.on_place(
                by,
                account,
                ProposedOrder {
                    instrument,
                    side,
                    kind,
                    qty,
                    price,
                    // Admission has to afford the commission too: an order
                    // whose margin fits but whose charge does not would
                    // open and leave the balance short.
                    commission: commission_on(&self.portfolio.market, instrument, qty)?,
                },
                limit_price,
            ),
            Command::SetProtection {
                by,
                account,
                ticket,
                stop_loss,
                take_profit,
            } => self.on_set_protection(by, account, ticket, stop_loss, take_profit),
            Command::Close {
                by,
                account,
                ticket,
                qty,
            } => self.on_close(by, account, ticket, qty),
            Command::CancelOrder {
                by,
                account,
                ticket,
            } => self.on_cancel(by, account, ticket),
            Command::Administer {
                by,
                account,
                action,
                reply,
            } => {
                let outcome = self.on_administer(by, account, action)?;
                let _ = reply.send(outcome);
                Ok(())
            }
            Command::Amend {
                by,
                account,
                ticket,
                amendment,
                reason,
                reply,
            } => {
                let outcome = self.on_amend(by, account, ticket, amendment, reason)?;
                let _ = reply.send(outcome);
                Ok(())
            }
            Command::Overview { reply } => {
                let _ = reply.send(self.overview()?);
                Ok(())
            }
            Command::NoteSession {
                user,
                what,
                address,
            } => self.emit(Event::SessionChanged {
                user,
                what,
                address,
            }),
            Command::Statement {
                account,
                limit,
                reply,
            } => {
                let lines = self
                    .statement
                    .iter()
                    .rev()
                    .filter(|(id, _)| *id == account)
                    .take(limit)
                    .map(|(_, line)| line.clone())
                    .collect::<Vec<_>>();
                let _ = reply.send(lines);
                Ok(())
            }
            Command::Chart {
                instrument,
                seconds,
                limit,
                reply,
            } => {
                let _ = reply.send(self.candles.series(instrument, seconds, limit));
                Ok(())
            }
            Command::Activity { limit, reply } => {
                let tail = self
                    .activity
                    .iter()
                    .rev()
                    .take(limit)
                    .cloned()
                    .collect::<Vec<_>>();
                let _ = reply.send(tail);
                Ok(())
            }
            Command::Books { account, reply } => {
                let _ = reply.send(self.books(account)?);
                Ok(())
            }
            Command::Audit { limit, reply } => {
                let tail = self
                    .audit
                    .iter()
                    .rev()
                    .take(limit)
                    .cloned()
                    .collect::<Vec<_>>();
                let _ = reply.send(tail);
                Ok(())
            }
            Command::Flush => self.on_flush(),
            Command::Snapshot { account, reply } => {
                let _ = reply.send(Box::new(self.snapshot(account)?));
                Ok(())
            }
            Command::OpenAccount { user, live, reply } => {
                let outcome = if self.portfolio.accounts_of(user).len() >= ACCOUNTS_PER_USER {
                    Err("that is as many accounts as one login may hold")
                } else if !live && self.has_a_running_demo(user) {
                    Err("you already have a demo account")
                } else {
                    let id = self.open_account(user)?;
                    if live {
                        // Attributed to the user: they asked for it, and the
                        // desk approved them beforehand.
                        self.emit(Event::AccountClassified {
                            account: id,
                            live: true,
                            by: user,
                        })?;
                    }
                    Ok(id)
                };
                let _ = reply.send(outcome);
                Ok(())
            }
            Command::Accounts { user, reply } => {
                let accounts = self.accounts_for(user)?;
                let _ = reply.send(accounts);
                Ok(())
            }
        }
    }

    /// A new price from a feed.
    ///
    /// `bid` and `ask` are the feed's. `markup` is what the house adds to
    /// each side, and it is applied *here* — once, on the way in — so that
    /// everything downstream reads one number: what the client is shown,
    /// fills at, is stopped out at, and has their open positions valued
    /// against. A client must never deal at a price they were not shown.
    fn on_quote(
        &mut self,
        instrument: InstrumentId,
        bid: Price,
        ask: Price,
        source: Source,
        markup: Price,
        commission: Money,
    ) -> Result<()> {
        if self.quote_source.get(&instrument) != Some(&source) {
            // Attribution for every fill that follows: the most recent change
            // before a fill names the feed that priced it.
            self.append(Event::PriceSourceChanged {
                instrument,
                source: source.0,
            })?;
            self.quote_source.insert(instrument, source);
        }
        self.quote_at.insert(instrument, Instant::now());
        self.portfolio
            .market
            .quote_marked(instrument, bid, ask, markup);
        self.portfolio.market.charge(instrument, commission);

        // From here on, the client's price. Reading it back rather than
        // recomputing it means there is exactly one place the widening
        // happens, and a markup the market refused — one that would take
        // the whole bid — cannot leak into a chart or a stop.
        let (bid, ask) = self.portfolio.market.top(instrument).unwrap_or((bid, ask));

        // Every tick, not only the journaled ones: a bar's high and low are
        // the extremes the market actually printed, and a quarter-second
        // throttle would quietly shave them off.
        self.candles.observe(instrument, bid, ask, Self::now().0);
        self.dirty_quotes.insert(instrument, (bid, ask));
        // A price moves the figures of every account holding it, and the
        // conversion rate of every account holding anything priced in its
        // quote currency — so rather than guess, every account with a
        // position is re-valued.
        let holders: Vec<AccountId> = self
            .portfolio
            .account_ids()
            .filter(|id| self.portfolio.account(*id).is_some_and(|a| !a.is_flat()))
            .collect();
        for id in holders {
            self.touch(id);
        }

        let due = self
            .last_journaled
            .get(&instrument)
            .is_none_or(|t| t.elapsed() >= QUOTE_JOURNAL_INTERVAL);
        if due {
            self.last_journaled.insert(instrument, Instant::now());
            self.append(Event::QuoteTicked {
                instrument,
                bid,
                ask,
            })?;
        }

        self.fire_resting(instrument, bid, ask)?;
        // Before the risk sweep: a stop loss is the trader's own instruction
        // and must be honoured before the engine decides the account needs
        // protecting from itself.
        self.fire_protection(instrument, bid, ask)?;
        self.evaluate_every_account()
    }

    /// Activates whatever this tick reached.
    ///
    /// Margin is checked here, not when the order was placed — see the
    /// `pending` crate. An order the account can no longer afford is refused
    /// and removed, which is a journaled decision like any other.
    fn fire_resting(&mut self, instrument: InstrumentId, bid: Price, ask: Price) -> Result<()> {
        let hits = self.resting.activated(instrument, bid, ask);
        if hits.is_empty() {
            return Ok(());
        }

        for hit in hits {
            let order = hit.order();
            self.emit(Event::PendingOrderTriggered {
                order: order.order,
                price: hit.price(),
            })?;
            self.announce_to(
                order.account,
                ServerMessage::OrderTriggered {
                    ticket: order.order.0,
                    price: price_str(hit.price()),
                },
            );

            match hit {
                Activation::BecomesLimit { .. } => {
                    // The stop was hit; it now rests as a limit and keeps its
                    // ticket. Nothing executes yet.
                    self.resting.convert_to_limit(order.order);
                }
                Activation::Fill { price, .. } => {
                    self.resting.cancel(order.order);

                    let Some(terms) = self.terms.get(&order.instrument).copied() else {
                        continue;
                    };
                    let proposed = ProposedOrder {
                        instrument: order.instrument,
                        side: order.side,
                        kind: OrderType::Market,
                        qty: order.qty,
                        price: Price::ZERO,
                        commission: commission_on(
                            &self.portfolio.market,
                            order.instrument,
                            order.qty,
                        )?,
                    };

                    if let Err(reason) =
                        self.portfolio
                            .check(order.account, &terms, proposed, self.policy)
                    {
                        self.emit(Event::OrderRejected {
                            order: order.order,
                            reason,
                        })?;
                        self.announce_to(
                            order.account,
                            ServerMessage::Rejected {
                                reason: reason_str(reason),
                            },
                        );
                        continue;
                    }

                    let Some(rate) = self.portfolio.market.rate(order.instrument) else {
                        // The conversion pair went dark between the trigger
                        // and the fill. Refuse rather than pick a rate.
                        self.emit(Event::OrderRejected {
                            order: order.order,
                            reason: RejectReason::NoConversionRate,
                        })?;
                        self.announce_to(
                            order.account,
                            ServerMessage::Rejected {
                                reason: reason_str(RejectReason::NoConversionRate),
                            },
                        );
                        continue;
                    };
                    self.emit(Event::OrderFilled {
                        order: order.order,
                        instrument: order.instrument,
                        side: order.side,
                        qty: order.qty,
                        price,
                        rate,
                        // A resting order triggers on the client's price
                        // like everything else, so it earns the same
                        // markup a market order would have.
                        markup: self.portfolio.market.markup_of(order.instrument),
                        commission: commission_on(
                            &self.portfolio.market,
                            order.instrument,
                            order.qty,
                        )?,
                    })?;
                    self.announce_to(
                        order.account,
                        ServerMessage::Filled {
                            ticket: order.order.0,
                            instrument: order.instrument.0,
                            side: side_str(order.side),
                            qty: qty_str(order.qty),
                            price: price_str(price),
                        },
                    );
                    self.touch(order.account);
                }
            }
        }
        Ok(())
    }

    /// Closes whatever this tick's stop losses and take profits reached.
    fn fire_protection(&mut self, instrument: InstrumentId, bid: Price, ask: Price) -> Result<()> {
        // Collected first: the close mutates the account these were read from.
        let mut hits: Vec<(AccountId, risk::ProtectionHit)> = Vec::new();
        for id in self.portfolio.account_ids() {
            if let Some(account) = self.portfolio.account(id) {
                for hit in risk::protection_hits(account, instrument, bid, ask) {
                    hits.push((id, hit));
                }
            }
        }
        if hits.is_empty() {
            return Ok(());
        }

        for (account, hit) in hits {
            let Some(rate) = self.portfolio.market.rate(instrument) else {
                // Without a rate the loss cannot be put into the account's
                // currency. Leave the position open rather than realise a
                // number nobody can check; the next tick tries again.
                eprintln!(
                    "engine: protection on ticket {} is waiting for a conversion rate",
                    hit.ticket.0
                );
                continue;
            };

            // Closed here and journaled after, like a stop-out: the engine
            // has already acted, and `apply` must not act a second time.
            let Some(holder) = self.portfolio.account_mut(account) else {
                continue;
            };
            let realised = holder.close_lot(
                instrument,
                hit.ticket,
                hit.qty,
                hit.price,
                rate,
                money::Money::ZERO,
            )?;
            self.append_realised(
                Event::ProtectionTriggered {
                    account,
                    instrument,
                    order: hit.ticket,
                    qty: hit.qty,
                    price: hit.price,
                    rate,
                    stop_loss: hit.stop_loss,
                },
                risk::Applied {
                    realised: Some((account, realised)),
                    ..risk::Applied::default()
                },
            )?;
            self.announce_to(
                account,
                ServerMessage::Protected {
                    ticket: hit.ticket.0,
                    instrument: instrument.0,
                    kind: if hit.stop_loss {
                        "stopLoss"
                    } else {
                        "takeProfit"
                    },
                    qty: qty_str(hit.qty),
                    price: price_str(hit.price),
                },
            );
            self.touch(account);
        }
        Ok(())
    }

    /// Attaches, moves or removes a position's protection.
    fn on_set_protection(
        &mut self,
        by: UserId,
        account: AccountId,
        ticket: OrderId,
        stop_loss: Price,
        take_profit: Price,
    ) -> Result<()> {
        // The parcel must be on this account: a ticket is a number anyone can
        // type, and protecting — or un-protecting — somebody else's position
        // is not something to allow by naming it.
        let found = self.portfolio.account(account).and_then(|a| {
            a.open_instruments().find_map(|i| {
                a.lots(i)
                    .iter()
                    .find(|l| l.ticket == ticket)
                    .map(|l| (i, l.side))
            })
        });
        let Some((instrument, side)) = found else {
            self.announce_to(
                account,
                ServerMessage::Unauthorized {
                    detail: "that position is not on this account",
                },
            );
            return Ok(());
        };

        let Some(terms) = self.terms.get(&instrument).copied() else {
            return Ok(());
        };
        let Some(close) = self.portfolio.market.close_price(instrument, side) else {
            self.announce_to(
                account,
                ServerMessage::Rejected {
                    reason: reason_str(RejectReason::MarketClosed),
                },
            );
            return Ok(());
        };

        if let Err(reason) = risk::check_protection(
            side,
            close,
            stop_loss,
            take_profit,
            terms.instrument.tick_size,
        ) {
            self.announce_to(
                account,
                ServerMessage::Rejected {
                    reason: reason_str(reason),
                },
            );
            return Ok(());
        }

        self.emit(Event::ProtectionSet {
            account,
            order: ticket,
            stop_loss,
            take_profit,
            by,
        })?;
        self.announce_to(
            account,
            ServerMessage::ProtectionSet {
                ticket: ticket.0,
                stop_loss: (!stop_loss.is_zero()).then(|| price_str(stop_loss)),
                take_profit: (!take_profit.is_zero()).then(|| price_str(take_profit)),
            },
        );
        self.touch(account);
        self.on_flush()
    }

    /// Closes one named parcel on a hedging account.
    ///
    /// The same prices and the same conversion rate a market order would get —
    /// it *is* a market order, one that names what it is closing.
    fn close_by_ticket(
        &mut self,
        by: UserId,
        account: AccountId,
        instrument: InstrumentId,
        ticket: OrderId,
        side: Side,
        qty: Qty,
    ) -> Result<()> {
        if !self.quote_is_fresh(instrument) {
            self.announce_to(
                account,
                ServerMessage::Rejected {
                    reason: reason_str(RejectReason::MarketClosed),
                },
            );
            return Ok(());
        }
        // A long closes by selling into the bid, so the closing side is the
        // opposite of the position's.
        let Some(price) = self
            .portfolio
            .market
            .open_price(instrument, side.opposite())
        else {
            self.announce_to(
                account,
                ServerMessage::Rejected {
                    reason: reason_str(RejectReason::MarketClosed),
                },
            );
            return Ok(());
        };
        let Some(rate) = self.portfolio.market.rate(instrument) else {
            self.announce_to(
                account,
                ServerMessage::Rejected {
                    reason: reason_str(RejectReason::NoConversionRate),
                },
            );
            return Ok(());
        };

        // Charged here as it is on a fill. Until this existed, a hedged
        // close was free and a netting one was not, which made the charge
        // depend on a setting that has nothing to do with pricing.
        let commission = commission_on(&self.portfolio.market, instrument, qty)?;

        let Some(holder) = self.portfolio.account_mut(account) else {
            return Ok(());
        };
        let realised = holder.close_lot(instrument, ticket, qty, price, rate, commission)?;
        self.append_realised(
            Event::PositionClosed {
                account,
                instrument,
                order: ticket,
                qty,
                price,
                rate,
                commission,
                by,
            },
            risk::Applied {
                realised: Some((account, realised)),
                ..risk::Applied::default()
            },
        )?;
        self.announce_to(
            account,
            ServerMessage::Filled {
                ticket: ticket.0,
                instrument: instrument.0,
                side: side_str(side.opposite()),
                qty: qty_str(qty),
                price: price_str(price),
            },
        );
        self.touch(account);
        self.evaluate_risk(account)?;
        self.on_flush()
    }

    /// Withdraws a resting order at the trader's request.
    fn on_cancel(&mut self, _by: UserId, account: AccountId, ticket: OrderId) -> Result<()> {
        // Checked before the cancel, not after: a ticket on somebody else's
        // book must not be removable by naming it. The session has already
        // decided this caller may act on `account`; this is the other half —
        // that the ticket is on it.
        if self.resting.get(ticket).map(|o| o.account) != Some(account) {
            self.announce_to(
                account,
                ServerMessage::Unauthorized {
                    detail: "that order is not on this account",
                },
            );
            return Ok(());
        }
        let Some(order) = self.resting.cancel(ticket) else {
            return Ok(());
        };
        self.emit(Event::OrderCancelled {
            order: ticket,
            unfilled: order.qty,
        })?;
        self.announce_to(account, ServerMessage::OrderCancelled { ticket: ticket.0 });
        self.touch(account);
        self.on_flush()
    }

    /// Re-checks every account that holds something.
    ///
    /// A price move is not account-shaped: one tick can call one account and
    /// stop out another, so the sweep is over all of them. A flat account is
    /// skipped because nothing about it can have changed.
    fn evaluate_every_account(&mut self) -> Result<()> {
        let holders: Vec<AccountId> = self
            .portfolio
            .account_ids()
            .filter(|id| self.portfolio.account(*id).is_some_and(|a| !a.is_flat()))
            .collect();
        for account in holders {
            self.evaluate_risk(account)?;
        }
        Ok(())
    }

    /// Re-checks one account and journals whatever the risk engine decided.
    fn evaluate_risk(&mut self, account: AccountId) -> Result<()> {
        let outcome = self.portfolio.evaluate(account, self.policy)?;
        if outcome.is_quiet() {
            return Ok(());
        }

        if let Some(level_bp) = outcome.call_bp {
            self.append(Event::MarginCallIssued {
                account,
                margin_level_bp: level_bp,
            })?;
            self.announce_to(account, ServerMessage::MarginCall { level_bp });
        }

        for closed in outcome.stopped_out {
            // `append`, not `emit`: evaluate already closed this position.
            self.append_realised(
                Event::PositionStoppedOut {
                    account,
                    instrument: closed.instrument,
                    // The parcel the risk engine chose, so replay closes the
                    // same one rather than the oldest.
                    order: closed.ticket,
                    qty: closed.qty,
                    price: closed.price,
                    // The rate the loss was realised at, not at replay.
                    rate: closed.rate,
                },
                risk::Applied {
                    realised: Some((account, closed.realised)),
                    ..risk::Applied::default()
                },
            )?;
            self.announce_to(
                account,
                ServerMessage::StoppedOut {
                    instrument: closed.instrument.0,
                    qty: qty_str(closed.qty),
                    price: price_str(closed.price),
                },
            );
        }
        self.touch(account);
        Ok(())
    }

    /// Admits one order on one account, at somebody's request.
    ///
    /// Takes the order as a [`ProposedOrder`] rather than six parameters: the
    /// instrument, side, kind, size and price are one thing — an order — and
    /// passing them separately is how a size ends up in a price.
    fn on_place(
        &mut self,
        by: UserId,
        account: AccountId,
        proposed: ProposedOrder,
        limit_price: Price,
    ) -> Result<()> {
        let ProposedOrder {
            instrument,
            side,
            kind,
            qty,
            price,
            // Read from the market at the fill, not from the proposal: a
            // resting order placed this morning is charged what the
            // instrument charges when it triggers.
            commission: _,
        } = proposed;

        if self.portfolio.account(account).is_none() {
            self.announce_to(
                account,
                ServerMessage::Unauthorized {
                    detail: "no such account",
                },
            );
            return Ok(());
        }
        let Some(terms) = self.terms.get(&instrument).copied() else {
            self.announce_to(
                account,
                ServerMessage::Rejected {
                    reason: reason_str(RejectReason::UnknownInstrument),
                },
            );
            return Ok(());
        };

        let ticket = OrderId(self.next_order);
        self.next_order += 1;

        if kind != OrderType::Market {
            return self.rest_order(by, account, ticket, terms, proposed, limit_price);
        }

        // A market order fills at the last quote, so the last quote had better
        // be current. An FX feed goes silent on Friday night; filling a Monday
        // morning order at Friday's close would hand the trader — or the house
        // — a weekend gap for free. Refuse instead, and let them retry.
        if !self.quote_is_fresh(instrument) {
            self.emit(Event::OrderRejected {
                order: ticket,
                reason: RejectReason::MarketClosed,
            })?;
            self.announce_to(
                account,
                ServerMessage::Rejected {
                    reason: reason_str(RejectReason::MarketClosed),
                },
            );
            return Ok(());
        }

        // A demo account past its thirty days may still close what it holds —
        // trapping a trader in a position is not what an expiry is for — but
        // it may not open anything new.
        if self
            .portfolio
            .account(account)
            .is_some_and(|a| a.demo_expired(Self::now()))
        {
            let opening = match self.portfolio.account(account) {
                Some(a) => risk::margin_needed(a, &self.portfolio.market, proposed, self.policy)
                    .map(|needed| !needed.is_zero())
                    .unwrap_or(true),
                None => true,
            };
            if opening {
                self.emit(Event::OrderRejected {
                    order: ticket,
                    reason: RejectReason::DemoExpired,
                })?;
                self.announce_to(
                    account,
                    ServerMessage::Rejected {
                        reason: reason_str(RejectReason::DemoExpired),
                    },
                );
                return Ok(());
            }
        }

        if let Err(reason) = self.portfolio.check(account, &terms, proposed, self.policy) {
            self.emit(Event::OrderRejected {
                order: ticket,
                reason,
            })?;
            self.announce_to(
                account,
                ServerMessage::Rejected {
                    reason: reason_str(reason),
                },
            );
            return Ok(());
        }

        // B-book: the house is the counterparty, so a market order fills at
        // the price the client would open at. Client-versus-client
        // internalisation is what the `book` crate is for, and arrives in P5.
        let Some(fill) = self.portfolio.market.open_price(instrument, side) else {
            self.announce_to(
                account,
                ServerMessage::Rejected {
                    reason: reason_str(RejectReason::MarketClosed),
                },
            );
            return Ok(());
        };

        // What the instrument's quote currency is worth in the account's, at
        // this moment. Journaled with the fill, because the profit this
        // position eventually realises is recomputed on replay and must come
        // out the same. `check` has already refused an instrument with no
        // rate, so this is belt and braces rather than the live path.
        let Some(rate) = self.portfolio.market.rate(instrument) else {
            self.announce_to(
                account,
                ServerMessage::Rejected {
                    reason: reason_str(RejectReason::NoConversionRate),
                },
            );
            return Ok(());
        };

        self.emit(Event::OrderAccepted {
            by,
            order: ticket,
            account,
            instrument,
            side,
            kind,
            qty,
            price,
            tif: domain::TimeInForce::ImmediateOrCancel,
        })?;
        self.emit(Event::OrderFilled {
            order: ticket,
            instrument,
            side,
            qty,
            price: fill,
            rate,
            // What the house earned on this trade. `fill` already includes
            // it — the client paid it — so this is the record, not a
            // second charge.
            markup: self.portfolio.market.markup_of(instrument),
            // Commission is the opposite of the markup: it leaves the
            // balance now rather than being inside the price.
            commission: commission_on(&self.portfolio.market, instrument, qty)?,
        })?;

        self.announce_to(
            account,
            ServerMessage::Filled {
                ticket: ticket.0,
                instrument: instrument.0,
                side: side_str(side),
                qty: qty_str(qty),
                price: price_str(fill),
            },
        );
        self.touch(account);
        self.evaluate_risk(account)?;
        self.on_flush()
    }

    /// Accepts a limit, stop or stop-limit onto the resting book.
    ///
    /// Only the order's shape is validated here. Margin is deliberately not
    /// reserved: a stop parked far from the market would otherwise lock up an
    /// account for an order that may never fire.
    fn rest_order(
        &mut self,
        by: UserId,
        account: AccountId,
        ticket: OrderId,
        terms: InstrumentTerms,
        proposed: ProposedOrder,
        limit_price: Price,
    ) -> Result<()> {
        // The price a resting order carries *is* its trigger: a limit rests at
        // its limit, a stop at its stop. Naming it here rather than passing it
        // twice keeps the two from ever disagreeing.
        let trigger_price = proposed.price;
        if let Err(reason) = self.portfolio.check_shape(account, &terms, proposed) {
            self.emit(Event::OrderRejected {
                order: ticket,
                reason,
            })?;
            self.announce_to(
                account,
                ServerMessage::Rejected {
                    reason: reason_str(reason),
                },
            );
            return Ok(());
        }

        // A stop-limit rests at its limit once the stop is reached. A limit
        // of zero means the terminal did not name one, and the stop itself is
        // the limit — no slippage tolerance, which is the safe reading of
        // silence. Anything else must be on the right side of the stop: a buy
        // stop-limit's limit at or above its stop, and the reverse for a sell,
        // or the order could never fill.
        let limit_price = if proposed.kind == OrderType::StopLimit {
            if limit_price.is_zero() {
                trigger_price
            } else {
                let sane = match proposed.side {
                    Side::Buy => limit_price >= trigger_price,
                    Side::Sell => limit_price <= trigger_price,
                };
                if !sane || !price_on_tick(limit_price, terms.instrument.tick_size) {
                    self.announce_to(
                        account,
                        ServerMessage::Rejected {
                            reason: reason_str(RejectReason::InvalidPrice),
                        },
                    );
                    return Ok(());
                }
                limit_price
            }
        } else {
            Price::ZERO
        };

        if self
            .resting
            .place(
                ticket,
                account,
                proposed.instrument,
                proposed.side,
                proposed.kind,
                proposed.qty,
                trigger_price,
                limit_price,
                domain::TimeInForce::GoodTilCancelled,
            )
            .is_err()
        {
            self.announce_to(
                account,
                ServerMessage::Rejected {
                    reason: reason_str(RejectReason::InvalidPrice),
                },
            );
            return Ok(());
        }

        self.emit(Event::PendingOrderPlaced {
            by,
            order: ticket,
            account,
            instrument: proposed.instrument,
            side: proposed.side,
            kind: proposed.kind,
            qty: proposed.qty,
            trigger_price,
            limit_price,
            tif: domain::TimeInForce::GoodTilCancelled,
        })?;
        self.announce_to(account, ServerMessage::OrderPlaced { ticket: ticket.0 });
        self.touch(account);
        self.on_flush()
    }

    fn on_close(
        &mut self,
        by: UserId,
        account: AccountId,
        ticket: OrderId,
        asked: Qty,
    ) -> Result<()> {
        let Some(holder) = self.portfolio.account(account) else {
            self.announce_to(
                account,
                ServerMessage::Unauthorized {
                    detail: "no such account",
                },
            );
            return Ok(());
        };
        // Find the parcel by ticket, on this account only. A ticket on
        // another account is not found here, and the answer is the same
        // whether it exists elsewhere or not at all.
        let found = holder.open_instruments().find_map(|instrument| {
            holder
                .lots(instrument)
                .iter()
                .find(|l| l.ticket == ticket)
                .map(|l| (instrument, l.side, l.qty))
        });
        let Some((instrument, side, qty)) = found else {
            self.announce_to(
                account,
                ServerMessage::Unauthorized {
                    detail: "that position is not on this account",
                },
            );
            return Ok(());
        };

        // How much of it is going. Zero means all, which is what every
        // terminal meant before it could say otherwise.
        let closing = if asked.is_zero() { qty } else { asked };
        if let Some(why) = self.closable(instrument, qty, closing) {
            self.announce_to(account, ServerMessage::Rejected { reason: why });
            return Ok(());
        }

        // On a hedging account the ticket is the whole point: an opposing
        // order would open a third position rather than close this one.
        if self.portfolio.account(account).is_some_and(|a| a.hedging) {
            return self.close_by_ticket(by, account, instrument, ticket, side, closing);
        }

        // Netting: closing is just an opposing market order, and it needs no
        // margin.
        self.on_place(
            by,
            account,
            ProposedOrder {
                instrument,
                side: side.opposite(),
                kind: OrderType::Market,
                qty: closing,
                price: Price::ZERO,
                // Closing is charged like any other fill — a broker that
                // charges to open and not to close is a broker with half a
                // commission model.
                commission: commission_on(&self.portfolio.market, instrument, closing)?,
            },
            Price::ZERO,
        )
    }

    /// Whether `closing` may be taken off a parcel of `held`.
    ///
    /// # Why a remainder has rules
    ///
    /// Closing part of a position leaves the rest of it open, and the rest
    /// has to be a position the platform can still deal with. A remainder
    /// below the minimum size, or off the step grid, is one no ordinary
    /// order can ever close — the trader would be stuck with it and would
    /// have to ask the desk to clear it by hand. So the size closed must
    /// sit on the grid, and what is left must either be nothing or a size
    /// that could have been opened in the first place.
    ///
    /// The maximum is deliberately not checked: it bounds what may be
    /// *opened*, and a parcel already larger than it — opened before the
    /// limit changed — must still be closeable.
    fn closable(&self, instrument: InstrumentId, held: Qty, closing: Qty) -> Option<&'static str> {
        let Some(terms) = self.terms.get(&instrument) else {
            return Some(reason_str(RejectReason::UnknownInstrument));
        };
        if closing <= Qty::ZERO || closing > held {
            return Some(reason_str(RejectReason::InvalidQuantity));
        }
        // The whole parcel always goes, whatever its size and whatever the
        // step is today: a position that cannot be closed is not a position
        // anybody should be left holding.
        if closing == held {
            return None;
        }
        if !multiple_of(closing, terms.qty_step) {
            return Some(reason_str(RejectReason::InvalidQuantity));
        }
        let rest = held.sub(closing).unwrap_or(Qty::ZERO);
        if rest < terms.min_qty || !multiple_of(rest, terms.qty_step) {
            return Some(reason_str(RejectReason::InvalidQuantity));
        }
        None
    }

    /// Charges every open position for each rollover due at `now_nanos`.
    ///
    /// Idempotent by construction: each day ends with `RolloverCompleted`, and
    /// only days after the last completed one are ever charged, so a restart,
    /// a second call, or a tick that lands twice in the same second cannot
    /// charge a night twice.
    fn run_rollovers_at(&mut self, now_nanos: i64) -> Result<()> {
        let Some(last) = self.portfolio.last_rollover() else {
            return Ok(());
        };
        let due = match rollover::due_days(now_nanos, last, self.rollover_hour) {
            Ok(days) => days,
            Err(RolloverError::TooManyMissed { missed }) => {
                if !self.rollover_blocked {
                    self.rollover_blocked = true;
                    eprintln!(
                        "engine: {missed} rollovers are overdue — more than the engine will post \
                         unattended. Financing is paused until an operator resolves it."
                    );
                }
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };

        for day in due {
            // Work out every charge before writing any, so the engine never
            // holds a borrow of the portfolio while it journals.
            let mut charges = Vec::new();
            let mut unconvertible = None;
            for account_id in self.portfolio.account_ids().collect::<Vec<_>>() {
                let Some(account) = self.portfolio.account(account_id) else {
                    continue;
                };
                for instrument in account.open_instruments() {
                    let terms = self
                        .swap_terms
                        .get(&instrument)
                        .copied()
                        .unwrap_or(SwapTerms::NONE);
                    let nights = terms.nights(day);
                    for lot in account.lots(instrument) {
                        // Financing is valued where the position would close,
                        // like everything else. With no quote — a delisted
                        // instrument — fall back to the entry price rather than
                        // leaving the whole rollover unable to complete.
                        let mark = self
                            .portfolio
                            .market
                            .close_price(instrument, lot.side)
                            .unwrap_or(lot.open_price);
                        let amount = rollover::charge(lot.side, lot.qty, mark, &terms, nights)?;
                        // Financing is a percentage of notional, so it comes
                        // out in the instrument's quote currency. It is
                        // converted here, once, at tonight's rate: a charge is
                        // a decision taken on one night, and the amount that
                        // reaches the journal is the amount in the account's
                        // currency — which is what makes it replay exactly.
                        let Some(rate) = self.portfolio.market.rate(instrument) else {
                            unconvertible = Some(instrument);
                            continue;
                        };
                        let amount = amount.convert_per(rate)?;
                        if !amount.is_zero() {
                            charges.push((account_id, instrument, lot.ticket, amount));
                        }
                    }
                }
            }

            // A night is charged in full or not at all. Writing the charges we
            // could compute and marking the day complete would quietly let a
            // position off a night's financing; writing them *without* marking
            // the day complete would charge the rest of the book twice on the
            // retry. So the whole day waits, and the next flush tries again
            // once the conversion pair is quoted.
            if let Some(instrument) = unconvertible {
                if !self.rollover_blocked {
                    self.rollover_blocked = true;
                    eprintln!(
                        "rollover: day {day} is waiting — instrument {} has no conversion rate, \
                         and a night is charged in full or not at all",
                        instrument.0
                    );
                }
                return Ok(());
            }
            self.rollover_blocked = false;

            // Each account hears its own financing, not the desk's total.
            let mut totals: BTreeMap<AccountId, money::Money> = BTreeMap::new();
            for (account, instrument, order, amount) in charges {
                self.emit(Event::SwapCharged {
                    account,
                    instrument,
                    order,
                    amount,
                })?;
                let running = totals.entry(account).or_insert(Money::ZERO);
                *running = running.add(amount)?;
            }
            self.emit(Event::RolloverCompleted { day })?;
            // Every account with a position is told the night closed, even
            // one charged nothing: "rollover complete, nothing held" is an
            // answer, and silence is not.
            let holders: Vec<AccountId> = self
                .portfolio
                .account_ids()
                .filter(|id| self.portfolio.account(*id).is_some_and(|a| !a.is_flat()))
                .collect();
            for account in holders {
                let total = totals.get(&account).copied().unwrap_or(Money::ZERO);
                self.announce_to(
                    account,
                    ServerMessage::Rollover {
                        day,
                        total: money_str(total),
                    },
                );
                self.touch(account);
            }
        }
        Ok(())
    }

    /// Moves money, sets a bonus, or stops an account, at an administrator's
    /// request.
    ///
    /// Every refusal is a sentence an operator can act on rather than a
    /// silent no-op: these are decisions somebody will be asked about.
    fn on_administer(
        &mut self,
        by: UserId,
        account: AccountId,
        action: AdminAction,
    ) -> Result<AdminOutcome> {
        if self.portfolio.account(account).is_none() {
            return Ok(AdminOutcome::Refused("no such account"));
        }

        let event = match action {
            AdminAction::Deposit(amount) => {
                if amount <= Money::ZERO {
                    return Ok(AdminOutcome::Refused("a deposit must be positive"));
                }
                Event::FundsDeposited {
                    account,
                    amount,
                    by,
                }
            }
            AdminAction::Withdraw(amount) => {
                if amount <= Money::ZERO {
                    return Ok(AdminOutcome::Refused("a withdrawal must be positive"));
                }
                // Free margin, not balance: paying out money that is holding
                // a position open would stop the account out on the way to
                // the bank, and the trader would be right to ask why.
                let free = self
                    .portfolio
                    .account(account)
                    .map(|a| a.free_margin(&self.portfolio.market, self.policy))
                    .transpose()?
                    .unwrap_or(Money::ZERO);
                if amount > free {
                    return Ok(AdminOutcome::Refused(
                        "more than the account's free margin — close positions first",
                    ));
                }
                // And cash, not equity. Free margin counts the bonus, because
                // a bonus supports margin — but a bonus is not the client's to
                // take out, and a broker that lets it be withdrawn is paying
                // out its own promotion. `withdrawable` is cash only.
                let cash = ledger::withdrawable(&self.ledger, account, self.currency_of(account));
                if amount > cash {
                    return Ok(AdminOutcome::Refused(
                        "more than the account's cash — the rest is non-withdrawable bonus",
                    ));
                }
                Event::FundsWithdrawn {
                    account,
                    amount,
                    by,
                }
            }
            AdminAction::SetCredit(amount) => {
                if amount.is_negative() {
                    return Ok(AdminOutcome::Refused("a bonus cannot be negative"));
                }
                Event::CreditSet {
                    account,
                    amount,
                    by,
                }
            }
            AdminAction::SetInsurance(amount) => {
                if amount.is_negative() {
                    return Ok(AdminOutcome::Refused("insurance cannot be negative"));
                }
                // It comes out of the balance, so there has to be one. A
                // client cannot set aside money they do not have, and
                // letting them would be an overdraft by another name.
                let held = self.portfolio.account(account);
                let moving = held
                    .map(|a| amount.sub(a.insurance))
                    .transpose()
                    .unwrap_or(None)
                    .unwrap_or(money::Money::ZERO);
                if held.is_some_and(|a| moving > a.balance) {
                    return Ok(AdminOutcome::Refused(
                        "there is not that much in the balance to set aside",
                    ));
                }
                Event::InsuranceSet {
                    account,
                    amount,
                    by,
                }
            }
            AdminAction::Transfer { to, amount } => {
                if amount <= money::Money::ZERO {
                    return Ok(AdminOutcome::Refused("a transfer must be positive"));
                }
                if to == account {
                    return Ok(AdminOutcome::Refused(
                        "that is the same account \u{2014} nothing would move",
                    ));
                }
                let Some(source) = self.portfolio.account(account) else {
                    return Ok(AdminOutcome::Refused("no such account to send from"));
                };
                let Some(target) = self.portfolio.account(to) else {
                    return Ok(AdminOutcome::Refused("no such account to send to"));
                };
                // Practice money must never reach a live account, and real
                // money must never be spent practising. This is the refusal
                // that matters most here: everything else on this list
                // costs somebody a day, and this one would put invented
                // figures into a balance a client can withdraw.
                if source.live != target.live {
                    return Ok(AdminOutcome::Refused(
                        "a demo account and a live one hold different kinds of money, and it does not cross",
                    ));
                }
                // No rate exists to apply, and applying one of 1.0 silently
                // would be worse than refusing.
                if source.currency != target.currency {
                    return Ok(AdminOutcome::Refused(
                        "the two accounts hold different currencies, and this platform does not convert",
                    ));
                }
                if !target.tradeable {
                    return Ok(AdminOutcome::Refused(
                        "that account is suspended \u{2014} reinstate it before sending money into it",
                    ));
                }
                // Free margin, then cash, exactly as a withdrawal is
                // judged. Money holding a position up is not available to
                // move, and the bonus is not the client's to move at all.
                let free = source.free_margin(&self.portfolio.market, self.policy)?;
                if amount > free {
                    return Ok(AdminOutcome::Refused(
                        "more than the account's free margin \u{2014} close positions first",
                    ));
                }
                let cash = ledger::withdrawable(&self.ledger, account, self.currency_of(account));
                if amount > cash {
                    return Ok(AdminOutcome::Refused(
                        "more than the account's cash \u{2014} the rest is non-withdrawable bonus",
                    ));
                }
                Event::FundsTransferred {
                    from: account,
                    to,
                    amount,
                    by,
                }
            }
            AdminAction::SetHedging(hedging) => {
                // Only on a flat account. Changing the mode under an open
                // position would change what closing it means halfway
                // through, and a trader is entitled to the rules they opened
                // under.
                let flat = self
                    .portfolio
                    .account(account)
                    .is_some_and(risk::Account::is_flat);
                if !flat {
                    return Ok(AdminOutcome::Refused(
                        "close the account's positions first — the mode changes what a close means",
                    ));
                }
                Event::AccountModeSet {
                    account,
                    hedging,
                    by,
                }
            }
            AdminAction::SetLive(live) => {
                // Only on a flat account, and for the same reason as the
                // netting mode: what an open position *is* should not change
                // under the trader holding it.
                let flat = self
                    .portfolio
                    .account(account)
                    .is_some_and(risk::Account::is_flat);
                if !flat {
                    return Ok(AdminOutcome::Refused("close the account's positions first"));
                }
                Event::AccountClassified { account, live, by }
            }
            AdminAction::RenewDemo => {
                if self.portfolio.account(account).is_some_and(|a| a.live) {
                    return Ok(AdminOutcome::Refused("a live account does not expire"));
                }
                Event::AccountRenewed { account, by }
            }
            AdminAction::Suspend => Event::AccountSuspended { account, by },
            AdminAction::Reinstate => Event::AccountReinstated { account, by },
        };

        self.emit(event)?;
        self.touch(account);
        // The trader sees the change as it happens, not on their next tick.
        self.on_flush()?;
        Ok(AdminOutcome::Done)
    }

    /// Puts a trade right, and tells the client it happened.
    ///
    /// Nothing here edits history. An adjustment is a new event that moves
    /// money with a reason attached; a price correction changes what an open
    /// position will realise from now on and is journaled with the price it
    /// replaced. The original fill stays exactly as it happened either way.
    fn on_amend(
        &mut self,
        by: UserId,
        account: AccountId,
        ticket: OrderId,
        amendment: Amendment,
        reason: domain::AmendReason,
    ) -> Result<AdminOutcome> {
        if self.portfolio.account(account).is_none() {
            return Ok(AdminOutcome::Refused("no such account"));
        }

        match amendment {
            Amendment::Adjust(amount) => {
                if amount.is_zero() {
                    return Ok(AdminOutcome::Refused("an adjustment of nothing is nothing"));
                }
                // A debit may not take money the account does not have: an
                // adjustment that overdraws is a loss the client never
                // agreed to, and the books would show cash below zero.
                if amount.is_negative() {
                    let cash =
                        ledger::withdrawable(&self.ledger, account, self.currency_of(account));
                    if amount.neg()? > cash {
                        return Ok(AdminOutcome::Refused(
                            "more than the account's cash — it would leave it overdrawn",
                        ));
                    }
                }
                self.emit(Event::TradeAdjusted {
                    account,
                    order: ticket,
                    amount,
                    reason,
                    by,
                })?;
                self.announce_to(
                    account,
                    ServerMessage::Amended {
                        ticket: ticket.0,
                        kind: "adjust",
                        amount: Some(money_str(amount)),
                        from: None,
                        to: None,
                        reason: amend_reason_str(reason),
                    },
                );
            }
            Amendment::CorrectOpenPrice(price) => {
                // Only an open parcel, and only on this account. A closed
                // trade is already in the balance; putting that right is an
                // adjustment, which says in money what was wrong.
                let found = self.portfolio.account(account).and_then(|a| {
                    a.open_instruments().find_map(|i| {
                        a.lots(i)
                            .iter()
                            .find(|l| l.ticket == ticket)
                            .map(|l| (i, l.open_price))
                    })
                });
                let Some((instrument, was)) = found else {
                    return Ok(AdminOutcome::Refused(
                        "no open position with that ticket on this account",
                    ));
                };
                let Some(terms) = self.terms.get(&instrument).copied() else {
                    return Ok(AdminOutcome::Refused("that instrument is not listed"));
                };
                if price <= Price::ZERO || !price_on_tick(price, terms.instrument.tick_size) {
                    return Ok(AdminOutcome::Refused(
                        "not a price on this instrument's tick grid",
                    ));
                }

                self.emit(Event::PositionCorrected {
                    account,
                    order: ticket,
                    from: was,
                    to: price,
                    reason,
                    by,
                })?;
                self.announce_to(
                    account,
                    ServerMessage::Amended {
                        ticket: ticket.0,
                        kind: "correctPrice",
                        amount: None,
                        from: Some(price_str(was)),
                        to: Some(price_str(price)),
                        reason: amend_reason_str(reason),
                    },
                );
            }
        }

        self.touch(account);
        // A corrected entry price changes what the position is worth, so the
        // account is re-checked before anyone is told the new figures.
        self.evaluate_risk(account)?;
        self.on_flush()?;
        Ok(AdminOutcome::Done)
    }

    /// Every account, with the figures an administrator looks at first.
    fn overview(&self) -> Result<Vec<AdminAccountDto>> {
        let market = &self.portfolio.market;
        let mut out = Vec::new();
        for id in self.portfolio.account_ids() {
            let Some(a) = self.portfolio.account(id) else {
                continue;
            };
            let positions = a.open_instruments().map(|i| a.lots(i).len()).sum::<usize>();
            out.push(AdminAccountDto {
                id: id.0,
                owner: self.portfolio.owner_of(id).map(|u| u.0),
                // The engine knows ids, not names. The session fills this in.
                owner_login: None,
                currency: a.currency.to_string(),
                leverage: a.leverage,
                balance: money_str(a.balance),
                credit: money_str(a.credit),
                insurance: money_str(a.insurance),
                // An unquoted market is not a reason to show no book: the
                // balance is known even when equity is not.
                equity: money_str(a.equity(market).unwrap_or(a.balance)),
                used_margin: money_str(a.used_margin(market, self.policy).unwrap_or(Money::ZERO)),
                margin_level_bp: a.margin_level_bp(market, self.policy).unwrap_or(None),
                hedging: a.hedging,
                live: a.live,
                demo_expires_at: a.demo_expires_at().map(|t| t.0 / 1_000_000),
                positions,
                pending: self.resting.for_account(id).count(),
                tradeable: a.tradeable,
            });
        }
        Ok(out)
    }

    /// The currency an account is held in, defaulting to the house's.
    fn currency_of(&self, account: AccountId) -> domain::Currency {
        self.portfolio
            .account(account)
            .map_or_else(|| self.portfolio.market.settlement(), |a| a.currency)
    }

    /// The books: one line per pot, and the trial balance per currency.
    ///
    /// The trial balance is the invariant made visible — every currency must
    /// read zero. A non-zero line is our bug, and an operator seeing it is
    /// seeing it before an auditor does.
    fn books(&self, account: Option<AccountId>) -> Result<BooksDto> {
        let mut lines: Vec<BookLineDto> = Vec::new();
        for (ledger_account, amount) in self.ledger.balances() {
            if let Some(only) = account {
                if ledger_account.owner != only {
                    continue;
                }
            }
            lines.push(BookLineDto {
                owner: ledger_account.owner.0,
                house: ledger_account.owner.0 == 0,
                book: format!("{:?}", ledger_account.kind),
                currency: ledger_account.currency.to_string(),
                amount: money_str(*amount),
            });
        }
        let trial = self
            .ledger
            .trial_balance()
            .map_err(|e| anyhow::anyhow!("trial balance: {e}"))?
            .into_iter()
            .map(|(currency, residual)| TrialBalanceDto {
                currency: currency.to_string(),
                residual: money_str(residual),
            })
            .collect();
        Ok(BooksDto {
            lines,
            trial,
            transactions: self.ledger.transaction_count(),
        })
    }

    /// True when the instrument has a quote young enough to fill against.
    fn quote_is_fresh(&self, instrument: InstrumentId) -> bool {
        let max_age = self
            .max_quote_age
            .get(&instrument)
            .copied()
            .unwrap_or(Duration::from_secs(15));
        self.quote_at
            .get(&instrument)
            .is_some_and(|at| at.elapsed() <= max_age)
    }

    /// The instrument's conversion rate, or `"0"` when it has none.
    ///
    /// Zero rather than one: a terminal that treats a missing rate as parity
    /// would price a yen ticket as dollars, which is the mistake this whole
    /// path exists to prevent. Zero cannot be divided by, so it shows as no
    /// figure at all.
    fn rate_str(&self, instrument: InstrumentId) -> String {
        self.portfolio
            .market
            .rate(instrument)
            .map_or_else(|| "0".to_owned(), price_str)
    }

    fn source_name(&self, instrument: InstrumentId) -> &'static str {
        self.quote_source.get(&instrument).map_or("", |s| s.name())
    }

    fn on_flush(&mut self) -> Result<()> {
        self.run_rollovers_at(Self::now().0)?;
        if !self.dirty_quotes.is_empty() {
            let quotes = self
                .dirty_quotes
                .iter()
                .map(|(id, (bid, ask))| QuoteDto {
                    instrument: id.0,
                    bid: price_str(*bid),
                    ask: price_str(*ask),
                    source: self.source_name(*id),
                    rate: self.rate_str(*id),
                })
                .collect();
            self.dirty_quotes.clear();
            self.announce_all(ServerMessage::Quotes { quotes });
        }

        for id in std::mem::take(&mut self.dirty_accounts) {
            if self.portfolio.account(id).is_none() {
                continue;
            }
            let (account, positions) = self.portfolio_dto(id)?;
            self.announce_to(
                id,
                ServerMessage::Portfolio {
                    account,
                    positions,
                    pending: self.pending_dto(id),
                    seq: self.journal.next_seq().0,
                },
            );
        }
        Ok(())
    }

    /// Account figures and open positions, all computed by the engine.
    fn portfolio_dto(&self, id: AccountId) -> Result<(AccountDto, Vec<PositionDto>)> {
        let account = self
            .portfolio
            .account(id)
            .with_context(|| format!("account {} is not open", id.0))?;
        let market = &self.portfolio.market;

        let dto = AccountDto {
            id: account.id.0,
            currency: account.currency.to_string(),
            leverage: account.leverage,
            balance: money_str(account.balance),
            credit: money_str(account.credit),
            insurance: money_str(account.insurance),
            equity: money_str(account.equity(market)?),
            used_margin: money_str(account.used_margin(market, self.policy)?),
            free_margin: money_str(account.free_margin(market, self.policy)?),
            margin_level_bp: account.margin_level_bp(market, self.policy)?,
            hedging: account.hedging,
            live: account.live,
            demo_expires_at: account.demo_expires_at().map(|t| t.0 / 1_000_000),
        };

        let mut positions = Vec::new();
        for instrument in account.open_instruments() {
            for lot in account.lots(instrument) {
                let current = market.close_price(instrument, lot.side);
                let profit = match current {
                    Some(c) => money::pnl(lot.side, lot.open_price, c, lot.qty)?,
                    None => Money::ZERO,
                };
                positions.push(PositionDto {
                    ticket: lot.ticket.0,
                    instrument: instrument.0,
                    side: side_str(lot.side),
                    qty: qty_str(lot.qty),
                    open_price: price_str(lot.open_price),
                    current: current.map(price_str),
                    profit: money_str(profit),
                    swap: money_str(lot.swap),
                    stop_loss: (!lot.stop_loss.is_zero()).then(|| price_str(lot.stop_loss)),
                    take_profit: (!lot.take_profit.is_zero()).then(|| price_str(lot.take_profit)),
                });
            }
        }
        Ok((dto, positions))
    }

    /// Resting orders for one account.
    fn pending_dto(&self, id: AccountId) -> Vec<PendingDto> {
        self.resting
            .for_account(id)
            .map(|o| PendingDto {
                ticket: o.order.0,
                instrument: o.instrument.0,
                side: side_str(o.side),
                kind: kind_str(o.kind),
                qty: qty_str(o.qty),
                trigger_price: price_str(o.trigger_price),
                limit_price: (!o.limit_price.is_zero()).then(|| price_str(o.limit_price)),
            })
            .collect()
    }

    fn snapshot(&self, id: AccountId) -> Result<ServerMessage> {
        let (account, positions) = self.portfolio_dto(id)?;
        let instruments = self
            .terms
            .values()
            .map(|t| InstrumentDto {
                id: t.instrument.id.0,
                symbol: t.instrument.symbol.as_str().to_owned(),
                base: t.instrument.base.to_string(),
                quote: t.instrument.quote.to_string(),
                display_digits: t.instrument.display_digits,
                contract_size: qty_str(t.instrument.contract_size),
                tick_size: price_str(t.instrument.tick_size),
                min_qty: qty_str(t.min_qty),
                max_qty: qty_str(t.max_qty),
                qty_step: qty_str(t.qty_step),
                tradeable: t.tradeable,
                note: self.notes.get(&t.instrument.id).map(|n| (*n).to_owned()),
            })
            .collect();

        let quotes = self
            .terms
            .keys()
            .filter_map(|id| {
                self.portfolio.market.top(*id).map(|(bid, ask)| QuoteDto {
                    instrument: id.0,
                    bid: price_str(bid),
                    ask: price_str(ask),
                    source: self.source_name(*id),
                    rate: self.rate_str(*id),
                })
            })
            .collect();

        Ok(ServerMessage::Snapshot {
            instruments,
            account,
            positions,
            pending: self.pending_dto(id),
            quotes,
            seq: self.journal.next_seq().0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch journal directory, removed on drop.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!("feraro-engine-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).expect("scratch");
            Scratch(p)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn p(s: &str) -> Price {
        Price::parse(s).expect("price")
    }

    fn q(s: &str) -> Qty {
        Qty::parse(s).expect("qty")
    }

    const BTC: InstrumentId = InstrumentId(1);

    /// The user every test signs in as, and the account they get.
    const TRADER_USER: UserId = UserId(1);
    const TRADER: AccountId = FIRST_ACCOUNT;

    /// An engine with one funded account, a quoted BTC market, and nothing
    /// waiting to commit.
    fn engine(dir: &Path) -> (Engine, broadcast::Receiver<Broadcast>) {
        let (tx, rx) = broadcast::channel(256);
        let mut e = Engine::open(dir, "test", EngineConfig::default(), tx).expect("opens");
        // Accounts come into existence when their owner first signs in.
        e.accounts_for(Some(TRADER_USER)).expect("opens an account");
        e.handle(Command::Quote {
            instrument: BTC,
            bid: p("80000.00"),
            ask: p("80000.01"),
            source: Source::BINANCE,
            markup: Price::ZERO,
            commission: Money::ZERO,
        })
        .unwrap();
        let now = e.last_sync + Duration::from_secs(1);
        e.commit_at(now).unwrap();
        (e, rx)
    }

    /// Quotes BTC with a markup and a commission in force.
    fn priced(e: &mut Engine, markup: &str, commission: &str) {
        e.handle(Command::Quote {
            instrument: BTC,
            bid: p("80000.00"),
            ask: p("80000.01"),
            source: Source::BINANCE,
            markup: p(markup),
            commission: money::Money::parse(commission).unwrap(),
        })
        .unwrap();
    }

    /// Every message waiting, whoever it was addressed to.
    fn drain(rx: &mut broadcast::Receiver<Broadcast>) -> Vec<ServerMessage> {
        let mut out = Vec::new();
        while let Ok(b) = rx.try_recv() {
            out.push(b.message);
        }
        out
    }

    /// Messages addressed to one account only.
    fn drain_for(
        rx: &mut broadcast::Receiver<Broadcast>,
        account: AccountId,
    ) -> Vec<ServerMessage> {
        let mut out = Vec::new();
        while let Ok(b) = rx.try_recv() {
            if b.to == Some(account) {
                out.push(b.message);
            }
        }
        out
    }

    /// A market order of a chosen size, for the tests that care about size.
    fn place(e: &mut Engine, side: Side, qty: money::Qty) {
        e.handle(Command::Place {
            by: TRADER_USER,
            limit_price: Price::ZERO,
            account: TRADER,
            instrument: BTC,
            side,
            kind: OrderType::Market,
            qty,
            price: Price::ZERO,
        })
        .unwrap();
    }

    fn market_buy() -> Command {
        Command::Place {
            by: TRADER_USER,
            limit_price: Price::ZERO,
            account: TRADER,
            instrument: BTC,
            side: Side::Buy,
            kind: OrderType::Market,
            qty: q("0.01"),
            price: Price::ZERO,
        }
    }

    #[test]
    fn a_fill_is_not_announced_until_it_is_on_disk() {
        // The invariant this whole change exists for.
        let s = Scratch::new("durable-fill");
        let (mut e, mut rx) = engine(&s.0);
        drain(&mut rx);

        e.handle(market_buy()).unwrap();

        // Decided and written, but not yet durable — so nobody has heard.
        assert!(e.journal.is_dirty(), "the fill is written but not synced");
        assert!(e.decision_pending);
        assert!(
            drain(&mut rx).is_empty(),
            "a client must not hear about a fill that is not on disk"
        );

        // Commit immediately — no interval has passed, and it must not matter.
        e.commit_at(e.last_sync).unwrap();

        assert!(!e.journal.is_dirty(), "a decision forces the sync");
        let heard = drain(&mut rx);
        assert!(
            heard
                .iter()
                .any(|m| matches!(m, ServerMessage::Filled { .. })),
            "and only then is the fill announced"
        );
    }

    #[test]
    fn a_rejection_is_a_decision_too() {
        let s = Scratch::new("durable-reject");
        let (mut e, mut rx) = engine(&s.0);
        drain(&mut rx);

        e.handle(Command::Place {
            by: TRADER_USER,
            limit_price: Price::ZERO,
            account: TRADER,
            instrument: BTC,
            side: Side::Buy,
            kind: OrderType::Market,
            qty: q("0.0001"), // below the instrument's minimum
            price: Price::ZERO,
        })
        .unwrap();

        assert!(
            e.decision_pending,
            "an OrderRejected must be durable before it is announced"
        );
        assert!(drain(&mut rx).is_empty());

        e.commit_at(e.last_sync).unwrap();
        assert!(!e.journal.is_dirty());
        assert!(drain(&mut rx)
            .iter()
            .any(|m| matches!(m, ServerMessage::Rejected { .. })));
    }

    #[test]
    fn quotes_alone_wait_for_the_interval() {
        let s = Scratch::new("durable-quotes");
        let (mut e, mut rx) = engine(&s.0);
        drain(&mut rx);

        // A tick on an instrument not yet journaled this interval is written.
        e.handle(Command::Quote {
            instrument: InstrumentId(2),
            bid: p("2600.00"),
            ask: p("2600.01"),
            source: Source::BINANCE,
            markup: Price::ZERO,
            commission: Money::ZERO,
        })
        .unwrap();
        e.handle(Command::Flush).unwrap();
        assert!(e.journal.is_dirty());
        assert!(!e.decision_pending, "a quote is not a decision");

        // Inside the interval: not synced, but quotes are still broadcast —
        // prices do not wait for durability.
        let start = e.last_sync;
        e.commit_at(start + Duration::from_millis(50)).unwrap();
        assert!(e.journal.is_dirty(), "no sync inside the interval");
        assert!(
            drain(&mut rx)
                .iter()
                .any(|m| matches!(m, ServerMessage::Quotes { .. })),
            "a quote does not wait for the disk"
        );

        // At the interval: synced, so the window really is bounded.
        e.commit_at(start + e.durability.interval).unwrap();
        assert!(!e.journal.is_dirty());
    }

    #[test]
    fn two_events_from_one_order_share_one_sync() {
        // OrderAccepted and OrderFilled are both decisions. Syncing between
        // them would double the fsync cost of every trade for no safety gain,
        // because neither is announced until both are down.
        let s = Scratch::new("durable-group");
        let (mut e, mut rx) = engine(&s.0);
        drain(&mut rx);

        let seq_before = e.journal.next_seq();
        e.handle(market_buy()).unwrap();
        let written = e.journal.next_seq().0 - seq_before.0;
        assert!(written >= 2, "accepted and filled were both journaled");
        assert!(e.journal.is_dirty(), "and neither was synced on its own");

        e.commit_at(e.last_sync).unwrap();
        assert!(!e.journal.is_dirty());
    }

    #[test]
    fn the_outbox_is_empty_after_every_commit() {
        let s = Scratch::new("durable-outbox");
        let (mut e, _rx) = engine(&s.0);

        e.handle(market_buy()).unwrap();
        assert!(!e.outbox.is_empty());
        e.commit_at(e.last_sync).unwrap();
        assert!(
            e.outbox.is_empty(),
            "nothing may be left waiting for a later command"
        );
    }
    // ── Rollover ────────────────────────────────────────────────────

    /// The first rollover moment after the engine's bootstrap marker.
    fn next_rollover(e: &Engine) -> i64 {
        let last = e.portfolio.last_rollover().expect("bootstrap records one");
        i64::from(last + 1) * rollover::NANOS_PER_DAY
            + i64::from(e.rollover_hour) * rollover::NANOS_PER_HOUR
            + 1_000_000_000
    }

    fn open_btc(e: &mut Engine) {
        e.handle(Command::Place {
            by: TRADER_USER,
            limit_price: Price::ZERO,
            account: TRADER,
            instrument: BTC,
            side: Side::Buy,
            kind: OrderType::Market,
            qty: q("1"),
            price: Price::ZERO,
        })
        .unwrap();
        e.commit_at(e.last_sync).unwrap();
    }

    fn btc_swap(e: &Engine) -> money::Money {
        e.portfolio.account(TRADER).unwrap().lots(BTC)[0].swap
    }

    #[test]
    fn a_fresh_journal_does_not_charge_for_history() {
        let s = Scratch::new("roll-fresh");
        let (mut e, _rx) = engine(&s.0);
        open_btc(&mut e);

        // Right now, with the bootstrap marker at today, nothing is due.
        e.run_rollovers_at(Engine::now().0).unwrap();
        assert!(btc_swap(&e).is_zero(), "no night has passed yet");
    }

    #[test]
    fn crossing_a_rollover_charges_each_open_position_once() {
        let s = Scratch::new("roll-once");
        let (mut e, mut rx) = engine(&s.0);
        open_btc(&mut e);
        drain(&mut rx);

        let t = next_rollover(&e);
        e.run_rollovers_at(t).unwrap();
        let after_first = btc_swap(&e);
        assert!(after_first < Money::ZERO, "a long in crypto pays financing");

        // One BTC near 80,000 at −10% a year is roughly −21.92 a night. The
        // live price moves, so assert a band — in raw integers, not floats.
        let raw = after_first.raw();
        assert!(
            (-24_000_000_000..=-20_000_000_000).contains(&raw),
            "unexpected charge {}",
            after_first.to_display_string(2).unwrap()
        );

        // Running the same moment again charges nothing more.
        e.run_rollovers_at(t).unwrap();
        e.run_rollovers_at(t + 60 * 1_000_000_000).unwrap();
        assert_eq!(btc_swap(&e), after_first, "a night is charged once");

        e.commit_at(e.last_sync).unwrap();
        assert!(drain(&mut rx)
            .iter()
            .any(|m| matches!(m, ServerMessage::Rollover { .. })));
    }

    #[test]
    fn a_restart_after_a_rollover_does_not_charge_it_again() {
        // The case the RolloverCompleted marker exists for.
        let s = Scratch::new("roll-restart");
        let t;
        let after_first;
        {
            let (mut e, _rx) = engine(&s.0);
            open_btc(&mut e);
            t = next_rollover(&e);
            e.run_rollovers_at(t).unwrap();
            e.commit_at(e.last_sync).unwrap();
            after_first = btc_swap(&e);
        }

        let (tx, _rx) = broadcast::channel(64);
        let mut reopened =
            Engine::open(&s.0, "test", EngineConfig::default(), tx).expect("reopens");
        assert_eq!(
            btc_swap(&reopened),
            after_first,
            "replay restored the accrued swap"
        );

        reopened
            .run_rollovers_at(t + 5 * 60 * 1_000_000_000)
            .unwrap();
        assert_eq!(
            btc_swap(&reopened),
            after_first,
            "a gateway restarted at 22:05 must not charge 22:00 twice"
        );
    }

    #[test]
    fn missed_nights_are_caught_up_one_by_one() {
        let s = Scratch::new("roll-catchup");
        let (mut e, _rx) = engine(&s.0);
        open_btc(&mut e);

        let one = {
            let t = next_rollover(&e);
            e.run_rollovers_at(t).unwrap();
            btc_swap(&e)
        };
        // Three more days pass with the gateway down.
        let later = next_rollover(&e) + 2 * rollover::NANOS_PER_DAY;
        e.run_rollovers_at(later).unwrap();

        let four = btc_swap(&e);
        // Four nights, within a few per cent for price moving between them.
        let (one, four) = (one.raw().abs(), four.raw().abs());
        assert!(
            four * 10 >= one * 39 && four * 10 <= one * 41,
            "expected four nights: one night {one}, total {four}"
        );
    }

    #[test]
    fn a_month_away_pauses_financing_instead_of_guessing() {
        let s = Scratch::new("roll-month");
        let (mut e, _rx) = engine(&s.0);
        open_btc(&mut e);

        let far = next_rollover(&e) + 40 * rollover::NANOS_PER_DAY;
        e.run_rollovers_at(far).unwrap();

        assert!(btc_swap(&e).is_zero(), "nothing posted unattended");
        assert!(e.rollover_blocked);
    }

    #[test]
    fn swap_rides_along_on_the_position_report() {
        let s = Scratch::new("roll-dto");
        let (mut e, _rx) = engine(&s.0);
        open_btc(&mut e);
        e.run_rollovers_at(next_rollover(&e)).unwrap();

        let (_, positions) = e.portfolio_dto(TRADER).unwrap();
        let swap = &positions[0].swap;
        assert!(swap.starts_with('-'), "reported swap {swap}");
    }
    // ── Price sources ───────────────────────────────────────────────

    #[test]
    fn a_market_order_on_a_stale_price_is_refused() {
        let s = Scratch::new("stale-refuse");
        let (mut e, mut rx) = engine(&s.0);
        drain(&mut rx);

        // The feed went quiet a minute ago.
        let long_ago = Instant::now()
            .checked_sub(Duration::from_secs(60))
            .expect("the clock has been running for a minute");
        e.quote_at.insert(BTC, long_ago);

        e.handle(market_buy()).unwrap();
        e.commit_at(e.last_sync).unwrap();

        let heard = drain(&mut rx);
        assert!(
            heard.iter().any(|m| matches!(
                m,
                ServerMessage::Rejected { reason } if *reason == "marketClosed"
            )),
            "no fill on a minute-old price"
        );
        assert!(!heard
            .iter()
            .any(|m| matches!(m, ServerMessage::Filled { .. })));
        assert!(e.portfolio.account(TRADER).unwrap().is_flat());
    }

    #[test]
    fn a_fresh_quote_makes_the_same_order_fill() {
        let s = Scratch::new("stale-recover");
        let (mut e, mut rx) = engine(&s.0);
        let long_ago = Instant::now()
            .checked_sub(Duration::from_secs(60))
            .expect("the clock has been running for a minute");
        e.quote_at.insert(BTC, long_ago);

        // The feed comes back.
        e.handle(Command::Quote {
            instrument: BTC,
            bid: p("80010.00"),
            ask: p("80010.01"),
            source: Source::KRAKEN,
            markup: Price::ZERO,
            commission: Money::ZERO,
        })
        .unwrap();
        drain(&mut rx);

        e.handle(market_buy()).unwrap();
        e.commit_at(e.last_sync).unwrap();
        assert!(drain(&mut rx)
            .iter()
            .any(|m| matches!(m, ServerMessage::Filled { .. })));
    }

    #[test]
    fn a_change_of_source_is_journaled_once_not_on_every_tick() {
        let s = Scratch::new("source-change");
        let (mut e, _rx) = engine(&s.0);
        assert_eq!(e.quote_source.get(&BTC), Some(&Source::BINANCE));

        let before = e.journal.next_seq();
        for i in 0..5 {
            e.handle(Command::Quote {
                instrument: BTC,
                bid: p("80000.00"),
                ask: p(&format!("8000{i}.01")),
                source: Source::BINANCE,
                markup: Price::ZERO,
                commission: Money::ZERO,
            })
            .unwrap();
        }
        let same_source_writes = e.journal.next_seq().0 - before.0;

        e.handle(Command::Quote {
            instrument: BTC,
            bid: p("80000.00"),
            ask: p("80000.01"),
            source: Source::KRAKEN,
            markup: Price::ZERO,
            commission: Money::ZERO,
        })
        .unwrap();
        let after_switch = e.journal.next_seq().0 - before.0;

        assert_eq!(
            after_switch - same_source_writes,
            1,
            "the switch to Kraken is one attribution event"
        );
        assert_eq!(e.quote_source.get(&BTC), Some(&Source::KRAKEN));
    }

    const USDJPY: InstrumentId = InstrumentId(30);

    /// Quotes USDJPY at 147.500 / 147.508 — which both prices it and, being
    /// the conversion pair for yen, makes yen convertible.
    fn quote_usdjpy(e: &mut Engine) {
        e.handle(Command::Quote {
            instrument: USDJPY,
            bid: p("147.500"),
            ask: p("147.508"),
            source: Source::TWELVE_DATA,
            markup: Price::ZERO,
            commission: Money::ZERO,
        })
        .unwrap();
    }

    #[test]
    fn a_yen_quoted_order_fills_and_books_dollars() {
        let s = Scratch::new("yen-fill");
        let (mut e, mut rx) = engine(&s.0);
        quote_usdjpy(&mut e);
        drain(&mut rx);

        // 1,000 units is a hundredth of a lot: 147,508 yen of notional.
        e.handle(Command::Place {
            by: TRADER_USER,
            limit_price: Price::ZERO,
            account: TRADER,
            instrument: USDJPY,
            side: Side::Buy,
            kind: OrderType::Market,
            qty: q("1000"),
            price: Price::ZERO,
        })
        .unwrap();
        e.commit_at(e.last_sync).unwrap();
        assert!(
            drain(&mut rx)
                .iter()
                .any(|m| matches!(m, ServerMessage::Filled { .. })),
            "a routed currency is tradeable"
        );

        // Margin: 147,508 yen ÷ 100 leverage = 1,475.08 yen, which at 147.504
        // is 10.00 dollars. Unconverted it would read as 1,475 — a hundred
        // and fifty times the real figure, which is the bug this proves gone.
        let account = e.portfolio.account(TRADER).unwrap();
        let used = account.used_margin(&e.portfolio.market, e.policy).unwrap();
        assert_eq!(used.to_display_string(2).unwrap(), "10.00");

        // The account figures the browser is sent are dollars too. Margin is
        // marked at the bid, 147.500, and converted at the mid, 147.504 —
        // hence a shade under ten rather than exactly ten.
        let (dto, positions) = e.portfolio_dto(TRADER).unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(dto.used_margin, "9.999728821");
    }

    #[test]
    fn a_yen_position_closes_for_a_dollar_amount() {
        let s = Scratch::new("yen-close");
        let (mut e, mut rx) = engine(&s.0);
        quote_usdjpy(&mut e);
        e.handle(Command::Place {
            by: TRADER_USER,
            limit_price: Price::ZERO,
            account: TRADER,
            instrument: USDJPY,
            side: Side::Buy,
            kind: OrderType::Market,
            qty: q("1000000"),
            price: Price::ZERO,
        })
        .unwrap();
        let opened = e.portfolio.account(TRADER).unwrap().balance;
        drain(&mut rx);

        // The yen weakens a full point: the long is up 1,000,000 × 0.992 yen
        // on the bid = 992,000 yen, which near 148.5 is about 6,680 dollars.
        e.handle(Command::Quote {
            instrument: USDJPY,
            bid: p("148.500"),
            ask: p("148.508"),
            source: Source::TWELVE_DATA,
            markup: Price::ZERO,
            commission: Money::ZERO,
        })
        .unwrap();
        let ticket = e.portfolio.account(TRADER).unwrap().lots(USDJPY)[0].ticket;
        e.handle(Command::Close {
            by: TRADER_USER,
            account: TRADER,
            ticket,
            qty: Qty::ZERO,
        })
        .unwrap();
        e.commit_at(e.last_sync).unwrap();

        let realised = e
            .portfolio
            .account(TRADER)
            .unwrap()
            .balance
            .sub(opened)
            .unwrap();
        assert_eq!(realised.to_display_string(2).unwrap(), "6679.95");
        assert!(
            realised < money::Money::parse("10000").unwrap(),
            "992,000 yen is not 992,000 dollars"
        );
    }

    // ── Several accounts on one engine ──────────────────────────────────

    const SECOND_USER: UserId = UserId(2);
    const SECOND: AccountId = AccountId(FIRST_ACCOUNT.0 + 1);

    /// Signs a second user in, giving them their own account.
    fn second_account(e: &mut Engine) -> AccountId {
        let accounts = e.accounts_for(Some(SECOND_USER)).expect("opens");
        assert_eq!(accounts.len(), 1, "a user gets exactly one starter account");
        AccountId(accounts[0].id)
    }

    #[test]
    fn each_user_gets_their_own_account_and_keeps_it() {
        let s = Scratch::new("two-accounts");
        let (mut e, _rx) = engine(&s.0);
        assert_eq!(second_account(&mut e), SECOND);

        // Signing in again finds the same account rather than opening another.
        assert_eq!(
            e.accounts_for(Some(SECOND_USER)).unwrap()[0].id,
            SECOND.0,
            "a second sign-in must not open a second account"
        );
        assert_eq!(e.portfolio.owner_of(SECOND), Some(SECOND_USER));
        assert_eq!(e.portfolio.owner_of(TRADER), Some(TRADER_USER));

        // An administrator sees both; each client sees one.
        assert_eq!(e.accounts_for(None).unwrap().len(), 2);
        assert_eq!(e.accounts_for(Some(TRADER_USER)).unwrap().len(), 1);
    }

    #[test]
    fn one_accounts_trading_does_not_touch_anothers_money() {
        let s = Scratch::new("isolated-money");
        let (mut e, mut rx) = engine(&s.0);
        second_account(&mut e);
        let before = e.portfolio.account(SECOND).unwrap().balance;
        drain(&mut rx);

        e.handle(market_buy()).unwrap();
        e.commit_at(e.last_sync).unwrap();

        let (first, _) = e.portfolio_dto(TRADER).unwrap();
        let (second, positions) = e.portfolio_dto(SECOND).unwrap();
        assert_ne!(
            first.used_margin, "0.000000000",
            "the trader is in a position"
        );
        assert_eq!(second.used_margin, "0.000000000");
        assert!(positions.is_empty());
        assert_eq!(e.portfolio.account(SECOND).unwrap().balance, before);
    }

    #[test]
    fn a_fill_is_announced_to_the_account_that_traded_and_no_other() {
        let s = Scratch::new("isolated-events");
        let (mut e, mut rx) = engine(&s.0);
        second_account(&mut e);
        drain(&mut rx);

        e.handle(market_buy()).unwrap();
        e.commit_at(e.last_sync).unwrap();

        let mut theirs = rx.resubscribe();
        let to_trader = drain_for(&mut rx, TRADER);
        assert!(to_trader
            .iter()
            .any(|m| matches!(m, ServerMessage::Filled { .. })));
        // Nothing addressed to the other account at all: not the fill, and
        // not a portfolio it did not ask for.
        assert!(
            drain_for(&mut theirs, SECOND).is_empty(),
            "another client's fill must not reach this session"
        );
    }

    #[test]
    fn prices_go_to_everyone_and_portfolios_do_not() {
        let s = Scratch::new("routing");
        let (mut e, mut rx) = engine(&s.0);
        second_account(&mut e);
        e.handle(market_buy()).unwrap();
        e.handle(Command::Flush).unwrap();
        e.commit_at(e.last_sync).unwrap();
        drain(&mut rx);

        e.handle(Command::Quote {
            instrument: BTC,
            bid: p("80100.00"),
            ask: p("80100.01"),
            source: Source::BINANCE,
            markup: Price::ZERO,
            commission: Money::ZERO,
        })
        .unwrap();
        e.handle(Command::Flush).unwrap();
        e.commit_at(e.last_sync).unwrap();

        let mut addressed_to = Vec::new();
        let mut quotes_for_everyone = 0;
        while let Ok(b) = rx.try_recv() {
            match (&b.message, b.to) {
                (ServerMessage::Quotes { .. }, None) => quotes_for_everyone += 1,
                (ServerMessage::Quotes { .. }, Some(_)) => {
                    panic!("a price was addressed to one account")
                }
                (ServerMessage::Portfolio { .. }, to) => addressed_to.push(to),
                _ => {}
            }
        }
        assert!(quotes_for_everyone >= 1);
        assert!(
            addressed_to.iter().all(|to| *to == Some(TRADER)),
            "only the account holding a position has figures to re-send: {addressed_to:?}"
        );
    }

    #[test]
    fn a_position_cannot_be_closed_from_another_account() {
        let s = Scratch::new("cross-close");
        let (mut e, mut rx) = engine(&s.0);
        second_account(&mut e);
        e.handle(market_buy()).unwrap();
        e.commit_at(e.last_sync).unwrap();
        let ticket = e.portfolio.account(TRADER).unwrap().lots(BTC)[0].ticket;
        drain(&mut rx);

        // The other account names a ticket it can see the number of — which is
        // all an attacker ever has — and must get nowhere.
        e.handle(Command::Close {
            by: TRADER_USER,
            account: SECOND,
            ticket,
            qty: Qty::ZERO,
        })
        .unwrap();
        e.commit_at(e.last_sync).unwrap();

        assert!(drain(&mut rx)
            .iter()
            .any(|m| matches!(m, ServerMessage::Unauthorized { .. })));
        assert_eq!(
            e.portfolio.account(TRADER).unwrap().lots(BTC).len(),
            1,
            "the position is still open"
        );
    }

    #[test]
    fn a_resting_order_cannot_be_cancelled_from_another_account() {
        let s = Scratch::new("cross-cancel");
        let (mut e, mut rx) = engine(&s.0);
        second_account(&mut e);
        e.handle(Command::Place {
            by: TRADER_USER,
            limit_price: Price::ZERO,
            account: TRADER,
            instrument: BTC,
            side: Side::Buy,
            kind: OrderType::Limit,
            qty: q("0.01"),
            price: p("70000.00"),
        })
        .unwrap();
        e.commit_at(e.last_sync).unwrap();
        let resting = e.resting.all().next().expect("one resting order").order;
        drain(&mut rx);

        e.handle(Command::CancelOrder {
            by: TRADER_USER,
            account: SECOND,
            ticket: resting,
        })
        .unwrap();
        e.commit_at(e.last_sync).unwrap();
        assert_eq!(e.resting.len(), 1, "it is still on the book");

        // Its owner can, of course.
        e.handle(Command::CancelOrder {
            by: TRADER_USER,
            account: TRADER,
            ticket: resting,
        })
        .unwrap();
        e.commit_at(e.last_sync).unwrap();
        assert_eq!(e.resting.len(), 0);
    }

    #[test]
    fn a_resting_order_fills_on_its_own_accounts_margin() {
        let s = Scratch::new("resting-owner");
        let (mut e, mut rx) = engine(&s.0);
        second_account(&mut e);

        // The second account rests a buy limit below the market.
        e.handle(Command::Place {
            by: TRADER_USER,
            limit_price: Price::ZERO,
            account: SECOND,
            instrument: BTC,
            side: Side::Buy,
            kind: OrderType::Limit,
            qty: q("0.01"),
            price: p("79000.00"),
        })
        .unwrap();
        e.commit_at(e.last_sync).unwrap();
        drain(&mut rx);

        // The market comes to it.
        e.handle(Command::Quote {
            instrument: BTC,
            bid: p("78990.00"),
            ask: p("78999.00"),
            source: Source::BINANCE,
            markup: Price::ZERO,
            commission: Money::ZERO,
        })
        .unwrap();
        e.commit_at(e.last_sync).unwrap();

        assert_eq!(
            e.portfolio.account(SECOND).unwrap().lots(BTC).len(),
            1,
            "the fill lands on the account that placed it"
        );
        assert!(e.portfolio.account(TRADER).unwrap().is_flat());
    }

    #[test]
    fn accounts_and_their_owners_survive_a_restart() {
        let s = Scratch::new("owners-replay");
        {
            let (tx, _rx) = broadcast::channel(256);
            let mut e = Engine::open(&s.0, "test", EngineConfig::default(), tx).expect("opens");
            e.accounts_for(Some(TRADER_USER)).unwrap();
            e.accounts_for(Some(SECOND_USER)).unwrap();
        }

        let (tx, _rx) = broadcast::channel(256);
        let reopened = Engine::open(&s.0, "test", EngineConfig::default(), tx).expect("reopens");
        assert_eq!(reopened.portfolio.owner_of(TRADER), Some(TRADER_USER));
        assert_eq!(reopened.portfolio.owner_of(SECOND), Some(SECOND_USER));
        assert_eq!(
            reopened.portfolio.accounts_of(SECOND_USER),
            vec![SECOND],
            "ownership is replayed, not re-guessed"
        );
    }

    #[test]
    fn a_stop_out_on_one_account_leaves_the_other_alone() {
        let s = Scratch::new("isolated-stopout");
        let (mut e, mut rx) = engine(&s.0);
        second_account(&mut e);

        // Both accounts go long the same size.
        for account in [TRADER, SECOND] {
            e.handle(Command::Place {
                by: TRADER_USER,
                limit_price: Price::ZERO,
                account,
                instrument: BTC,
                side: Side::Buy,
                kind: OrderType::Market,
                qty: q("0.30"),
                price: Price::ZERO,
            })
            .unwrap();
        }
        // Leave the second account with almost nothing behind the position,
        // so the next tick puts it past its stop-out and the first nowhere
        // near one. Both hold the same instrument at the same size: what
        // separates them is only their own money.
        if let Some(a) = e.portfolio.account_mut(SECOND) {
            a.balance = money::Money::parse("10.00").unwrap();
            a.credit = Money::ZERO;
        }
        e.commit_at(e.last_sync).unwrap();
        drain(&mut rx);

        e.handle(Command::Quote {
            instrument: BTC,
            bid: p("79000.00"),
            ask: p("79000.01"),
            source: Source::BINANCE,
            markup: Price::ZERO,
            commission: Money::ZERO,
        })
        .unwrap();
        e.commit_at(e.last_sync).unwrap();

        assert!(
            e.portfolio.account(SECOND).unwrap().is_flat(),
            "the account past its stop-out is closed out"
        );
        assert_eq!(
            e.portfolio.account(TRADER).unwrap().lots(BTC).len(),
            1,
            "the account that is still funded keeps its position"
        );
    }

    // ── Stop loss and take profit ───────────────────────────────────────

    /// Buys 0.01 BTC and returns the ticket.
    fn open_long(e: &mut Engine) -> OrderId {
        e.handle(market_buy()).unwrap();
        e.portfolio.account(TRADER).unwrap().lots(BTC)[0].ticket
    }

    fn set_protection(e: &mut Engine, ticket: OrderId, sl: Price, tp: Price) -> Result<()> {
        e.handle(Command::SetProtection {
            by: TRADER_USER,
            account: TRADER,
            ticket,
            stop_loss: sl,
            take_profit: tp,
        })
    }

    fn tick(e: &mut Engine, bid: &str, ask: &str) {
        e.handle(Command::Quote {
            instrument: BTC,
            bid: p(bid),
            ask: p(ask),
            source: Source::BINANCE,
            markup: Price::ZERO,
            commission: Money::ZERO,
        })
        .unwrap();
    }

    #[test]
    fn a_stop_loss_closes_the_position_and_says_which_level_fired() {
        let s = Scratch::new("stop-loss");
        let (mut e, mut rx) = engine(&s.0);
        let ticket = open_long(&mut e);
        set_protection(&mut e, ticket, p("79000.00"), p("82000.00")).unwrap();
        drain(&mut rx);

        // Through the stop.
        tick(&mut e, "78900.00", "78910.00");
        e.commit_at(e.last_sync).unwrap();

        let heard = drain(&mut rx);
        let protected = heard
            .iter()
            .find_map(|m| match m {
                ServerMessage::Protected {
                    ticket: t,
                    kind,
                    price,
                    ..
                } => Some((*t, *kind, price.clone())),
                _ => None,
            })
            .expect("the trader is told their stop fired");
        assert_eq!(protected.0, ticket.0);
        assert_eq!(protected.1, "stopLoss");
        // At the bid the market actually offered, not at the level asked for.
        assert_eq!(protected.2, "78900.000000000");
        assert!(e.portfolio.account(TRADER).unwrap().is_flat());

        // And it is not reported as a stop-out, which is a different thing.
        assert!(!heard
            .iter()
            .any(|m| matches!(m, ServerMessage::StoppedOut { .. })));
    }

    #[test]
    fn a_take_profit_closes_the_position_at_a_profit() {
        let s = Scratch::new("take-profit");
        let (mut e, mut rx) = engine(&s.0);
        let ticket = open_long(&mut e);
        let opened = e.portfolio.account(TRADER).unwrap().balance;
        set_protection(&mut e, ticket, p("79000.00"), p("82000.00")).unwrap();
        drain(&mut rx);

        tick(&mut e, "82000.00", "82010.00");
        e.commit_at(e.last_sync).unwrap();

        assert!(drain(&mut rx).iter().any(|m| matches!(
            m,
            ServerMessage::Protected { kind, .. } if *kind == "takeProfit"
        )));
        let gained = e
            .portfolio
            .account(TRADER)
            .unwrap()
            .balance
            .sub(opened)
            .unwrap();
        // 0.01 BTC from 80,000.01 to 82,000 is 19.9999 — a profit, and the
        // trader keeps the improvement over their level.
        assert_eq!(gained.to_display_string(2).unwrap(), "20.00");
    }

    #[test]
    fn protection_on_the_wrong_side_is_refused_and_nothing_is_set() {
        let s = Scratch::new("protection-refused");
        let (mut e, mut rx) = engine(&s.0);
        let ticket = open_long(&mut e);
        drain(&mut rx);

        // A stop above a long.
        set_protection(&mut e, ticket, p("81000.00"), Price::ZERO).unwrap();
        e.commit_at(e.last_sync).unwrap();
        assert!(drain(&mut rx).iter().any(|m| matches!(
            m,
            ServerMessage::Rejected { reason } if *reason == "invalidPrice"
        )));
        let lot = e.portfolio.account(TRADER).unwrap().lots(BTC)[0];
        assert_eq!(lot.stop_loss, Price::ZERO, "nothing was set");

        // A ticket on no account of this session's.
        set_protection(&mut e, OrderId(999_999), p("79000.00"), Price::ZERO).unwrap();
        e.commit_at(e.last_sync).unwrap();
        assert!(drain(&mut rx)
            .iter()
            .any(|m| matches!(m, ServerMessage::Unauthorized { .. })));
    }

    #[test]
    fn protection_can_be_moved_and_taken_off() {
        let s = Scratch::new("protection-moved");
        let (mut e, mut rx) = engine(&s.0);
        let ticket = open_long(&mut e);

        set_protection(&mut e, ticket, p("79000.00"), Price::ZERO).unwrap();
        set_protection(&mut e, ticket, p("79500.00"), p("83000.00")).unwrap();
        let lot = e.portfolio.account(TRADER).unwrap().lots(BTC)[0];
        assert_eq!(lot.stop_loss, p("79500.00"));
        assert_eq!(lot.take_profit, p("83000.00"));

        // Zero clears both, and the old stop no longer fires.
        set_protection(&mut e, ticket, Price::ZERO, Price::ZERO).unwrap();
        drain(&mut rx);
        tick(&mut e, "79000.00", "79010.00");
        e.commit_at(e.last_sync).unwrap();
        assert!(!drain(&mut rx)
            .iter()
            .any(|m| matches!(m, ServerMessage::Protected { .. })));
        assert_eq!(e.portfolio.account(TRADER).unwrap().lots(BTC).len(), 1);
    }

    #[test]
    fn the_blotter_shows_the_levels_it_is_protected_at() {
        let s = Scratch::new("protection-dto");
        let (mut e, _rx) = engine(&s.0);
        let ticket = open_long(&mut e);
        set_protection(&mut e, ticket, p("79000.00"), p("82000.00")).unwrap();

        let (_, positions) = e.portfolio_dto(TRADER).unwrap();
        assert_eq!(positions[0].stop_loss.as_deref(), Some("79000.000000000"));
        assert_eq!(positions[0].take_profit.as_deref(), Some("82000.000000000"));

        set_protection(&mut e, ticket, Price::ZERO, Price::ZERO).unwrap();
        let (_, positions) = e.portfolio_dto(TRADER).unwrap();
        assert_eq!(positions[0].stop_loss, None, "none is absent, not zero");
    }

    #[test]
    fn a_protected_close_replays_to_the_same_balance() {
        let s = Scratch::new("protection-replay");
        let live = {
            let (mut e, _rx) = engine(&s.0);
            let ticket = open_long(&mut e);
            set_protection(&mut e, ticket, p("79000.00"), Price::ZERO).unwrap();
            tick(&mut e, "78900.00", "78910.00");
            e.commit_at(e.last_sync).unwrap();
            assert!(e.portfolio.account(TRADER).unwrap().is_flat());
            e.portfolio.account(TRADER).unwrap().balance
        };

        let (tx, _rx) = broadcast::channel(256);
        let reopened = Engine::open(&s.0, "test", EngineConfig::default(), tx).expect("reopens");
        let account = reopened.portfolio.account(TRADER).unwrap();
        assert_eq!(account.balance, live);
        assert!(account.is_flat(), "the close replays, it does not reopen");
    }

    #[test]
    fn protection_only_closes_the_parcel_it_belongs_to() {
        let s = Scratch::new("protection-parcel");
        let (mut e, mut rx) = engine(&s.0);
        let first = open_long(&mut e);
        // A second parcel, protected lower down.
        e.handle(market_buy()).unwrap();
        let second = e.portfolio.account(TRADER).unwrap().lots(BTC)[1].ticket;
        set_protection(&mut e, first, p("79900.00"), Price::ZERO).unwrap();
        set_protection(&mut e, second, p("79000.00"), Price::ZERO).unwrap();
        drain(&mut rx);

        // A move that reaches the first parcel's stop only.
        tick(&mut e, "79800.00", "79810.00");
        e.commit_at(e.last_sync).unwrap();

        let left = e.portfolio.account(TRADER).unwrap().lots(BTC).to_vec();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].ticket, second, "the other parcel is untouched");
        assert_eq!(left[0].stop_loss, p("79000.00"), "and keeps its own stop");
    }

    #[test]
    fn a_stop_limit_rests_at_the_limit_the_terminal_asked_for() {
        let s = Scratch::new("stop-limit");
        let (mut e, mut rx) = engine(&s.0);
        drain(&mut rx);

        // Buy stop at 81,000, resting as a limit at 81,100 once touched:
        // a hundred dollars of slippage tolerance, and no more.
        e.handle(Command::Place {
            by: TRADER_USER,
            account: TRADER,
            instrument: BTC,
            side: Side::Buy,
            kind: OrderType::StopLimit,
            qty: q("0.01"),
            price: p("81000.00"),
            limit_price: p("81100.00"),
        })
        .unwrap();
        let resting = e.resting.all().next().expect("one resting order");
        assert_eq!(resting.trigger_price, p("81000.00"));
        assert_eq!(resting.limit_price, p("81100.00"));

        // A limit below a buy stop could never fill, so it is refused.
        e.handle(Command::Place {
            by: TRADER_USER,
            account: TRADER,
            instrument: BTC,
            side: Side::Buy,
            kind: OrderType::StopLimit,
            qty: q("0.01"),
            price: p("81000.00"),
            limit_price: p("80500.00"),
        })
        .unwrap();
        e.commit_at(e.last_sync).unwrap();
        assert!(drain(&mut rx).iter().any(|m| matches!(
            m,
            ServerMessage::Rejected { reason } if *reason == "invalidPrice"
        )));
        assert_eq!(e.resting.len(), 1, "the refused order did not rest");
    }

    // ── Account lifecycle: demo, live and sub-accounts ──────────────────

    /// Opens another account for a user the way a client's request does.
    fn open_for(
        e: &mut Engine,
        user: UserId,
        live: bool,
    ) -> std::result::Result<AccountId, String> {
        let (tx, rx) = oneshot::channel();
        e.handle(Command::OpenAccount {
            user,
            live,
            reply: tx,
        })
        .unwrap();
        rx.blocking_recv()
            .expect("the engine answers")
            .map_err(str::to_owned)
    }

    #[test]
    fn one_demo_account_per_login_and_only_one_at_a_time() {
        let s = Scratch::new("one-demo");
        let (mut e, _rx) = engine(&s.0);
        // The first came from signing in, and it has time left on it.
        assert_eq!(e.portfolio.accounts_of(TRADER_USER).len(), 1);

        let again = open_for(&mut e, TRADER_USER, false);
        assert_eq!(
            again,
            Err("you already have a demo account".to_owned()),
            "a trial somebody can take five of at once is not a trial"
        );
        assert_eq!(
            e.portfolio.accounts_of(TRADER_USER).len(),
            1,
            "the refusal opened nothing"
        );
    }

    #[test]
    fn a_demo_that_has_run_out_does_not_block_another() {
        let s = Scratch::new("expired-demo");
        let (mut e, _rx) = engine(&s.0);
        // Age the one they signed in with past its thirty days.
        if let Some(a) = e.portfolio.account_mut(TRADER) {
            a.clock_started = Engine::thirty_one_days_ago();
        }

        let fresh =
            open_for(&mut e, TRADER_USER, false).expect("the trial ended, so another may start");
        assert_ne!(fresh, TRADER);
        assert_eq!(balance_of(&e, fresh), "25000.00");
        books_agree(&e);

        // And now that one is running, so it blocks the next.
        assert!(open_for(&mut e, TRADER_USER, false).is_err());
    }

    #[test]
    fn live_accounts_are_not_limited_to_one() {
        let s = Scratch::new("sub-accounts");
        let (mut e, _rx) = engine(&s.0);
        // A demo is running, and a live account opens beside it: the rule
        // is about trials, not about how many accounts a funded client has.
        let first = open_for(&mut e, TRADER_USER, true).expect("opens");
        let second = open_for(&mut e, TRADER_USER, true).expect("opens");
        assert_ne!(first, second);
        assert!(e.portfolio.account(first).expect("exists").live);
        assert_eq!(e.portfolio.owner_of(second), Some(TRADER_USER));
        assert_eq!(balance_of(&e, second), "25000.00");
        books_agree(&e);

        while e.portfolio.accounts_of(TRADER_USER).len() < ACCOUNTS_PER_USER {
            open_for(&mut e, TRADER_USER, true).expect("opens");
        }
        assert!(
            open_for(&mut e, TRADER_USER, true).is_err(),
            "a script must not be able to open ten thousand of them"
        );
    }

    #[test]
    fn an_account_is_demo_until_somebody_says_otherwise() {
        let s = Scratch::new("demo-default");
        let (mut e, _rx) = engine(&s.0);
        let account = e.portfolio.account(TRADER).unwrap();
        assert!(!account.live, "demo unless deliberately made live");
        assert!(
            account.demo_expires_at().is_some(),
            "and it has an expiry, which a live account does not"
        );

        // A client may ask for a live one; the engine records it as live.
        let live = open_for(&mut e, SECOND_USER, true).expect("opens");
        assert!(e.portfolio.account(live).unwrap().live);
        assert_eq!(
            e.portfolio.account(live).unwrap().demo_expires_at(),
            None,
            "live accounts do not expire"
        );
    }

    #[test]
    fn a_demo_account_stops_opening_positions_after_thirty_days() {
        let s = Scratch::new("demo-expiry");
        let (mut e, mut rx) = engine(&s.0);

        // Backdate the clock past the thirty days.
        let long_ago = Engine::thirty_one_days_ago();
        if let Some(a) = e.portfolio.account_mut(TRADER) {
            a.clock_started = long_ago;
        }
        drain(&mut rx);

        e.handle(market_buy()).unwrap();
        e.commit_at(e.last_sync).unwrap();
        assert!(
            drain(&mut rx).iter().any(|m| matches!(
                m,
                ServerMessage::Rejected { reason } if *reason == "demoExpired"
            )),
            "and says which refusal it is, because the remedy differs"
        );
        assert!(e.portfolio.account(TRADER).unwrap().is_flat());

        // Renewing starts the clock again, and the same order goes through.
        assert_eq!(
            administer(&mut e, TRADER, AdminAction::RenewDemo),
            AdminOutcome::Done
        );
        e.handle(market_buy()).unwrap();
        assert_eq!(e.portfolio.account(TRADER).unwrap().lots(BTC).len(), 1);
    }

    #[test]
    fn an_expired_demo_can_still_close_what_it_holds() {
        // An expiry is a clock running out, not a sanction: trapping a trader
        // in a position is not what it is for.
        let s = Scratch::new("demo-expiry-close");
        let (mut e, _rx) = engine(&s.0);
        let ticket = open_long(&mut e);
        if let Some(a) = e.portfolio.account_mut(TRADER) {
            a.clock_started = Engine::thirty_one_days_ago();
        }

        e.handle(Command::Close {
            by: TRADER_USER,
            account: TRADER,
            ticket,
            qty: Qty::ZERO,
        })
        .unwrap();
        assert!(
            e.portfolio.account(TRADER).unwrap().is_flat(),
            "closing is always allowed"
        );
        books_agree(&e);
    }

    #[test]
    fn a_live_account_cannot_be_renewed_and_does_not_expire() {
        let s = Scratch::new("live-no-expiry");
        let (mut e, _rx) = engine(&s.0);
        administer(&mut e, TRADER, AdminAction::SetLive(true));
        assert!(matches!(
            administer(&mut e, TRADER, AdminAction::RenewDemo),
            AdminOutcome::Refused(_)
        ));
        assert!(!e
            .portfolio
            .account(TRADER)
            .unwrap()
            .demo_expired(Engine::now()));
    }

    #[test]
    fn the_demo_clock_is_dated_by_the_journal_and_replays() {
        let s = Scratch::new("demo-clock-replay");
        let expected = {
            let (mut e, _rx) = engine(&s.0);
            administer(&mut e, TRADER, AdminAction::RenewDemo);
            e.commit_at(e.last_sync).unwrap();
            e.portfolio.account(TRADER).unwrap().clock_started
        };

        let (tx, _rx) = broadcast::channel(256);
        let reopened = Engine::open(&s.0, "test", EngineConfig::default(), tx).expect("reopens");
        assert_eq!(
            reopened.portfolio.account(TRADER).unwrap().clock_started,
            expected,
            "an account is dated the way it was dated the first time"
        );
        assert!(expected.0 > 0);
    }

    // ── Snapshots ───────────────────────────────────────────────────────

    /// Everything a snapshot has to reproduce, in one comparable value.
    fn fingerprint(e: &Engine) -> String {
        let mut out = String::new();
        for id in e.portfolio.account_ids() {
            let a = e.portfolio.account(id).unwrap();
            out.push_str(&format!(
                "{}|{}|{}|{}|{}|{}|{:?}|",
                id.0,
                a.balance.to_decimal_string(),
                a.credit.to_decimal_string(),
                a.tradeable,
                a.hedging,
                a.called,
                e.portfolio.owner_of(id).map(|u| u.0),
            ));
            for instrument in a.open_instruments() {
                for lot in a.lots(instrument) {
                    out.push_str(&format!(
                        "{}:{}:{:?}:{}:{}:{}:{}:{};",
                        instrument.0,
                        lot.ticket.0,
                        lot.side,
                        lot.qty.to_decimal_string(),
                        lot.open_price.to_decimal_string(),
                        lot.stop_loss.to_decimal_string(),
                        lot.take_profit.to_decimal_string(),
                        lot.swap.to_decimal_string(),
                    ));
                }
            }
            out.push('\n');
        }
        out.push_str(&format!("rollover={:?}\n", e.portfolio.last_rollover()));
        for (account, amount) in e.ledger.export() {
            out.push_str(&format!("{account}={}\n", amount.to_decimal_string()));
        }
        out
    }

    /// Does everything the engine knows how to do, in one journal.
    fn busy_journal(e: &mut Engine) {
        second_account(&mut *e);
        administer(e, TRADER, AdminAction::Deposit(m("500.00")));
        administer(e, TRADER, AdminAction::SetCredit(m("250.00")));
        administer(e, SECOND, AdminAction::Withdraw(m("1000.00")));
        administer(e, SECOND, AdminAction::Suspend);
        administer(e, SECOND, AdminAction::Reinstate);

        // A trade closed at a profit, a stop loss, and a position left open
        // with protection and financing on it.
        let first = open_long(e);
        tick(e, "81000.00", "81000.01");
        e.handle(Command::Close {
            by: TRADER_USER,
            account: TRADER,
            ticket: first,
            qty: Qty::ZERO,
        })
        .unwrap();

        let second = open_long(e);
        set_protection(e, second, p("79000.00"), Price::ZERO).unwrap();
        tick(e, "78900.00", "78910.00");

        let third = open_long(e);
        set_protection(e, third, p("70000.00"), p("90000.00")).unwrap();
        e.emit(Event::SwapCharged {
            account: TRADER,
            instrument: BTC,
            order: third,
            amount: m("-1.25"),
        })
        .unwrap();
        amend(e, TRADER, third, Amendment::Adjust(m("3.00")));
        amend(e, TRADER, third, Amendment::CorrectOpenPrice(p("78800.00")));

        // A resting order, and one cancelled.
        e.handle(Command::Place {
            by: TRADER_USER,
            account: TRADER,
            instrument: BTC,
            side: Side::Buy,
            kind: OrderType::Limit,
            qty: q("0.01"),
            price: p("70000.00"),
            limit_price: Price::ZERO,
        })
        .unwrap();
        let resting = e.resting.all().next().unwrap().order;
        e.handle(Command::CancelOrder {
            by: TRADER_USER,
            account: TRADER,
            ticket: resting,
        })
        .unwrap();

        note(e, TRADER_USER, domain::SessionEvent::Opened, [0u8; 16]);
        e.commit_at(e.last_sync).unwrap();
    }

    #[test]
    fn a_snapshot_and_the_tail_reach_the_state_a_full_replay_does() {
        // The only property a snapshot needs, and the only one worth testing:
        // starting from it must be indistinguishable from reading everything.
        let s = Scratch::new("snapshot-equivalence");
        let full = {
            let (mut e, _rx) = engine(&s.0);
            busy_journal(&mut e);
            fingerprint(&e)
        };

        // Reopened with no snapshot: a full replay, which is the reference.
        let (tx, _rx) = broadcast::channel(256);
        let replayed = Engine::open(&s.0, "test", EngineConfig::default(), tx).expect("reopens");
        assert_eq!(fingerprint(&replayed), full, "the reference must hold");

        // Now write a snapshot covering everything so far, and reopen. The
        // tail is empty, so this proves the restore path on its own.
        let through = replayed.journal.next_seq().0 - 1;
        let stored = snapshot::capture(&replayed.portfolio, &replayed.ledger, domain::Seq(through));
        snapshot::write(&s.0, "test", &stored).expect("writes");

        let (tx, _rx) = broadcast::channel(256);
        let restored = Engine::open(&s.0, "test", EngineConfig::default(), tx).expect("reopens");
        assert_eq!(
            fingerprint(&restored),
            full,
            "a snapshot must restore exactly what the journal folds to"
        );
        books_agree(&restored);
    }

    #[test]
    fn a_tail_on_top_of_a_snapshot_is_applied_once_and_only_once() {
        let s = Scratch::new("snapshot-tail");
        let (through, expected) = {
            let (mut e, _rx) = engine(&s.0);
            busy_journal(&mut e);
            let through = e.journal.next_seq().0 - 1;
            let stored = snapshot::capture(&e.portfolio, &e.ledger, domain::Seq(through));
            snapshot::write(&s.0, "test", &stored).expect("writes");

            // More happens after the snapshot was taken.
            administer(&mut e, TRADER, AdminAction::Deposit(m("77.00")));
            let ticket = open_long(&mut e);
            tick(&mut e, "81500.00", "81500.01");
            e.handle(Command::Close {
                by: TRADER_USER,
                account: TRADER,
                ticket,
                qty: Qty::ZERO,
            })
            .unwrap();
            e.commit_at(e.last_sync).unwrap();
            (through, fingerprint(&e))
        };

        let (tx, _rx) = broadcast::channel(256);
        let restored = Engine::open(&s.0, "test", EngineConfig::default(), tx).expect("reopens");
        assert_eq!(
            fingerprint(&restored),
            expected,
            "the tail is folded onto the snapshot, and the books are not double-posted"
        );
        books_agree(&restored);
        assert!(through > 0);
    }

    #[test]
    fn an_unusable_snapshot_costs_time_and_not_correctness() {
        let s = Scratch::new("snapshot-broken");
        let expected = {
            let (mut e, _rx) = engine(&s.0);
            busy_journal(&mut e);
            fingerprint(&e)
        };

        // Half a file, which is what a crash mid-write would leave if the
        // write were not atomic.
        std::fs::write(
            snapshot::path_for(&s.0, "test"),
            r#"{"version":1,"through":9,"settlement":"US"#,
        )
        .unwrap();

        let (tx, _rx) = broadcast::channel(256);
        let reopened = Engine::open(&s.0, "test", EngineConfig::default(), tx).expect("reopens");
        assert_eq!(
            fingerprint(&reopened),
            expected,
            "an unreadable snapshot is ignored and the journal is read in full"
        );
    }

    #[test]
    fn a_snapshot_is_written_on_the_cadence_and_not_before() {
        let s = Scratch::new("snapshot-cadence");
        let (tx, _rx) = broadcast::channel(256);
        // Every five events, so the cadence is reachable in a test. The
        // default is ten thousand, which a test journal never gets near.
        let config = EngineConfig {
            snapshot_every: 5,
            ..EngineConfig::default()
        };
        let mut e = Engine::open(&s.0, "test", config, tx).expect("opens");
        let path = snapshot::path_for(&s.0, "test");
        assert!(!path.exists(), "nothing written before anything happened");

        e.accounts_for(Some(TRADER_USER)).unwrap();
        e.handle(market_buy()).unwrap();
        e.commit_at(e.last_sync + Duration::from_secs(1)).unwrap();
        assert!(path.exists(), "past five events, one is written");

        let first = snapshot::read(&s.0, "test", domain::Currency::USD).expect("reads");
        assert!(first.through > 0);

        // And it moves forward rather than being written once and forgotten.
        for _ in 0..8 {
            e.handle(market_buy()).unwrap();
        }
        e.commit_at(e.last_sync + Duration::from_secs(2)).unwrap();
        let second = snapshot::read(&s.0, "test", domain::Currency::USD).expect("reads");
        assert!(
            second.through > first.through,
            "the snapshot keeps up with the journal"
        );

        // Turned off, nothing further is written.
        let (tx, _rx) = broadcast::channel(256);
        let off = EngineConfig {
            snapshot_every: 0,
            ..EngineConfig::default()
        };
        let mut e = Engine::open(&s.0, "test", off, tx).expect("reopens");
        let before = std::fs::metadata(&path).unwrap().len();
        for _ in 0..20 {
            e.handle(market_buy()).unwrap();
        }
        e.commit_at(e.last_sync + Duration::from_secs(3)).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            before,
            "zero means off"
        );
    }

    // ── The account statement ───────────────────────────────────────────

    /// The statement for one account, newest first.
    fn statement(e: &Engine, account: AccountId) -> Vec<StatementDto> {
        e.statement
            .iter()
            .rev()
            .filter(|(id, _)| *id == account)
            .map(|(_, line)| line.clone())
            .collect()
    }

    #[test]
    fn a_statement_shows_every_movement_and_what_it_left() {
        let s = Scratch::new("statement");
        let (mut e, _rx) = engine(&s.0);

        // Opening the account already put money in.
        let opening = statement(&e, TRADER);
        assert_eq!(
            opening.iter().map(|l| l.kind).collect::<Vec<_>>(),
            vec!["bonus", "deposit"],
            "newest first: the bonus was set after the deposit"
        );
        assert_eq!(opening[1].amount, "25000.000000000");
        assert_eq!(opening[1].balance, "25000.000000000");
        // A bonus is not cash, so it moves the statement without moving the
        // balance.
        assert_eq!(opening[0].amount, "500.000000000");
        assert_eq!(opening[0].balance, "25000.000000000");

        // A trade that opens moves nothing; closing it does.
        let ticket = open_long(&mut e);
        assert_eq!(
            statement(&e, TRADER).len(),
            2,
            "opening a position is not a movement"
        );
        tick(&mut e, "81000.00", "81000.01");
        e.handle(Command::Close {
            by: TRADER_USER,
            account: TRADER,
            ticket,
            qty: Qty::ZERO,
        })
        .unwrap();

        let after = statement(&e, TRADER);
        assert_eq!(after[0].kind, "trade");
        assert_eq!(after[0].ticket, Some(ticket.0 + 1), "the closing order's");
        assert_eq!(after[0].amount, "9.999900000");
        assert_eq!(after[0].balance, "25009.999900000");
    }

    #[test]
    fn every_way_money_moves_is_named_on_the_statement() {
        let s = Scratch::new("statement-kinds");
        let (mut e, _rx) = engine(&s.0);

        administer(&mut e, TRADER, AdminAction::Deposit(m("100.00")));
        administer(&mut e, TRADER, AdminAction::Withdraw(m("50.00")));
        amend(&mut e, TRADER, OrderId(0), Amendment::Adjust(m("5.00")));

        // Financing on an open position, then a stop loss that closes it.
        let ticket = open_long(&mut e);
        e.emit(Event::SwapCharged {
            account: TRADER,
            instrument: BTC,
            order: ticket,
            amount: m("-1.50"),
        })
        .unwrap();
        set_protection(&mut e, ticket, p("79000.00"), Price::ZERO).unwrap();
        tick(&mut e, "78900.00", "78910.00");

        let kinds: Vec<_> = statement(&e, TRADER)
            .iter()
            .map(|l| l.kind)
            .collect::<Vec<_>>();
        for expected in [
            "stopLoss",
            "financing",
            "adjustment",
            "withdrawal",
            "deposit",
            "bonus",
        ] {
            assert!(
                kinds.contains(&expected),
                "{expected} is missing: {kinds:?}"
            );
        }
        // Withdrawals are negative, deposits positive: a statement that does
        // not say which way the money went is a list of numbers.
        let lines = statement(&e, TRADER);
        let withdrawal = lines.iter().find(|l| l.kind == "withdrawal").unwrap();
        assert!(withdrawal.amount.starts_with('-'));
    }

    #[test]
    fn a_statement_is_one_account_s_own_and_replays_from_the_journal() {
        let s = Scratch::new("statement-replay");
        {
            let (mut e, _rx) = engine(&s.0);
            second_account(&mut e);
            administer(&mut e, TRADER, AdminAction::Deposit(m("111.00")));
            administer(&mut e, SECOND, AdminAction::Deposit(m("222.00")));
            e.commit_at(e.last_sync).unwrap();

            let mine = statement(&e, TRADER);
            assert!(mine.iter().all(|l| l.amount != "222.000000000"));
            assert_eq!(mine[0].amount, "111.000000000");
        }

        let (tx, _rx) = broadcast::channel(256);
        let reopened = Engine::open(&s.0, "test", EngineConfig::default(), tx).expect("reopens");
        let mine = statement(&reopened, TRADER);
        assert_eq!(mine[0].amount, "111.000000000");
        assert_eq!(mine[0].balance, "25111.000000000");
        let theirs = statement(&reopened, SECOND);
        assert_eq!(theirs[0].amount, "222.000000000");
    }

    // ── Hedging accounts ────────────────────────────────────────────────

    #[test]
    fn a_netting_account_closes_on_an_opposing_order() {
        let s = Scratch::new("netting");
        let (mut e, _rx) = engine(&s.0);
        let ticket = open_long(&mut e);
        e.handle(Command::Place {
            by: TRADER_USER,
            account: TRADER,
            instrument: BTC,
            side: Side::Sell,
            kind: OrderType::Market,
            qty: q("0.01"),
            price: Price::ZERO,
            limit_price: Price::ZERO,
        })
        .unwrap();
        assert!(
            e.portfolio.account(TRADER).unwrap().is_flat(),
            "netting is the default, and an opposing order nets"
        );
        let _ = ticket;
    }

    #[test]
    fn a_hedging_account_holds_both_sides_and_closes_by_ticket() {
        let s = Scratch::new("hedging");
        let (mut e, mut rx) = engine(&s.0);
        assert_eq!(
            administer(&mut e, TRADER, AdminAction::SetHedging(true)),
            AdminOutcome::Done
        );

        let long = open_long(&mut e);
        e.handle(Command::Place {
            by: TRADER_USER,
            account: TRADER,
            instrument: BTC,
            side: Side::Sell,
            kind: OrderType::Market,
            qty: q("0.01"),
            price: Price::ZERO,
            limit_price: Price::ZERO,
        })
        .unwrap();

        let lots = e.portfolio.account(TRADER).unwrap().lots(BTC).to_vec();
        assert_eq!(lots.len(), 2, "both sides are held");
        assert_eq!(lots[0].side, Side::Buy);
        assert_eq!(lots[1].side, Side::Sell);
        let short = lots[1].ticket;
        drain(&mut rx);

        // Closing names the ticket, and closes that one only.
        e.handle(Command::Close {
            by: TRADER_USER,
            account: TRADER,
            ticket: long,
            qty: Qty::ZERO,
        })
        .unwrap();
        e.commit_at(e.last_sync).unwrap();
        assert!(drain(&mut rx)
            .iter()
            .any(|m| matches!(m, ServerMessage::Filled { .. })));

        let left = e.portfolio.account(TRADER).unwrap().lots(BTC).to_vec();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].ticket, short, "the short is what remains");
        books_agree(&e);
    }

    #[test]
    fn a_hedge_ties_up_less_than_two_outright_positions() {
        let s = Scratch::new("hedging-margin");
        let (mut e, _rx) = engine(&s.0);
        administer(&mut e, TRADER, AdminAction::SetHedging(true));

        e.handle(Command::Place {
            by: TRADER_USER,
            account: TRADER,
            instrument: BTC,
            side: Side::Buy,
            kind: OrderType::Market,
            qty: q("1"),
            price: Price::ZERO,
            limit_price: Price::ZERO,
        })
        .unwrap();
        let one_side = e
            .portfolio
            .account(TRADER)
            .unwrap()
            .used_margin(&e.portfolio.market, e.policy)
            .unwrap();

        e.handle(Command::Place {
            by: TRADER_USER,
            account: TRADER,
            instrument: BTC,
            side: Side::Sell,
            kind: OrderType::Market,
            qty: q("1"),
            price: Price::ZERO,
            limit_price: Price::ZERO,
        })
        .unwrap();
        let hedged = e
            .portfolio
            .account(TRADER)
            .unwrap()
            .used_margin(&e.portfolio.market, e.policy)
            .unwrap();

        assert!(
            hedged < one_side,
            "a hedge ties up less than the single position it hedges: {} vs {}",
            hedged.to_display_string(2).unwrap(),
            one_side.to_display_string(2).unwrap()
        );
        assert!(
            hedged > Money::ZERO,
            "and not nothing: both legs still cross a spread to close"
        );
    }

    #[test]
    fn the_mode_cannot_change_under_an_open_position() {
        let s = Scratch::new("hedging-mode");
        let (mut e, _rx) = engine(&s.0);
        open_long(&mut e);
        assert!(
            matches!(
                administer(&mut e, TRADER, AdminAction::SetHedging(true)),
                AdminOutcome::Refused(_)
            ),
            "a trader is entitled to the rules they opened under"
        );

        let ticket = e.portfolio.account(TRADER).unwrap().lots(BTC)[0].ticket;
        e.handle(Command::Close {
            by: TRADER_USER,
            account: TRADER,
            ticket,
            qty: Qty::ZERO,
        })
        .unwrap();
        assert_eq!(
            administer(&mut e, TRADER, AdminAction::SetHedging(true)),
            AdminOutcome::Done,
            "flat, it can change"
        );
    }

    #[test]
    fn a_hedging_account_replays_to_the_same_two_positions() {
        let s = Scratch::new("hedging-replay");
        let expected = {
            let (mut e, _rx) = engine(&s.0);
            administer(&mut e, TRADER, AdminAction::SetHedging(true));
            open_long(&mut e);
            e.handle(Command::Place {
                by: TRADER_USER,
                account: TRADER,
                instrument: BTC,
                side: Side::Sell,
                kind: OrderType::Market,
                qty: q("0.02"),
                price: Price::ZERO,
                limit_price: Price::ZERO,
            })
            .unwrap();
            e.commit_at(e.last_sync).unwrap();
            e.portfolio.account(TRADER).unwrap().lots(BTC).to_vec()
        };

        let (tx, _rx) = broadcast::channel(256);
        let reopened = Engine::open(&s.0, "test", EngineConfig::default(), tx).expect("reopens");
        let account = reopened.portfolio.account(TRADER).unwrap();
        assert!(account.hedging, "the mode is journaled, not configured");
        assert_eq!(
            account.lots(BTC),
            expected.as_slice(),
            "and an opposing order replays as an opening, not a close"
        );
        books_agree(&reopened);
    }

    // ── The client activity log ─────────────────────────────────────────

    fn note(e: &mut Engine, user: UserId, what: domain::SessionEvent, address: [u8; 16]) {
        e.handle(Command::NoteSession {
            user,
            what,
            address,
        })
        .unwrap();
    }

    /// The activity window, oldest first — the order it was recorded in.
    fn activity(e: &Engine) -> Vec<ActivityDto> {
        e.activity.clone()
    }

    #[test]
    fn sign_ins_refusals_and_sign_outs_are_all_recorded() {
        let s = Scratch::new("activity");
        let (mut e, _rx) = engine(&s.0);
        let local = address_bytes("127.0.0.1:51000".parse().ok());
        let elsewhere = address_bytes("203.0.113.7:4000".parse().ok());

        note(&mut e, TRADER_USER, domain::SessionEvent::Opened, local);
        note(
            &mut e,
            UserId::SYSTEM,
            domain::SessionEvent::Refused,
            elsewhere,
        );
        note(&mut e, TRADER_USER, domain::SessionEvent::Resumed, local);
        note(&mut e, TRADER_USER, domain::SessionEvent::Closed, local);

        let log = activity(&e);
        assert_eq!(
            log.iter().map(|l| l.what).collect::<Vec<_>>(),
            vec!["signedIn", "refused", "resumed", "signedOut"]
        );
        assert_eq!(log[0].address, "127.0.0.1", "an IPv4 address reads as one");
        assert_eq!(log[1].address, "203.0.113.7");
        assert_eq!(
            log[1].user, 0,
            "a refused attempt names no user, because it proved nothing about who it was"
        );
        // Logins are not the engine's to know; the session joins them on.
        assert!(log.iter().all(|l| l.login.is_none()));
    }

    #[test]
    fn a_sign_in_waits_for_the_next_sync_and_does_not_hold_up_a_login() {
        let s = Scratch::new("activity-durability");
        let (mut e, _rx) = engine(&s.0);
        e.decision_pending = false;
        note(&mut e, TRADER_USER, domain::SessionEvent::Opened, [0u8; 16]);
        assert!(
            !e.decision_pending,
            "a sign-in is a record, not a decision somebody is waiting on"
        );
        assert!(e.journal.is_dirty(), "but it is in the journal");
        // An address the gateway could not read is empty, not "::".
        assert_eq!(activity(&e)[0].address, "");
    }

    #[test]
    fn the_activity_log_replays_from_the_journal() {
        let s = Scratch::new("activity-replay");
        {
            let (mut e, _rx) = engine(&s.0);
            note(
                &mut e,
                TRADER_USER,
                domain::SessionEvent::Opened,
                address_bytes("10.0.0.5:9000".parse().ok()),
            );
            note(
                &mut e,
                UserId::SYSTEM,
                domain::SessionEvent::Refused,
                address_bytes("[2001:db8::1]:9000".parse().ok()),
            );
            e.commit_at(e.last_sync).unwrap();
        }

        let (tx, _rx) = broadcast::channel(256);
        let reopened = Engine::open(&s.0, "test", EngineConfig::default(), tx).expect("reopens");
        let log = activity(&reopened);
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].address, "10.0.0.5");
        assert_eq!(log[1].address, "2001:db8::1", "and IPv6 reads as IPv6");
    }

    // ── Putting a trade right ───────────────────────────────────────────

    fn amend(
        e: &mut Engine,
        account: AccountId,
        ticket: OrderId,
        amendment: Amendment,
    ) -> AdminOutcome {
        let (tx, rx) = oneshot::channel();
        e.handle(Command::Amend {
            by: ADMIN_USER,
            account,
            ticket,
            amendment,
            reason: domain::AmendReason::BadPrice,
            reply: tx,
        })
        .unwrap();
        rx.blocking_recv().expect("the engine answers")
    }

    #[test]
    fn an_adjustment_moves_money_and_tells_the_client() {
        let s = Scratch::new("amend-adjust");
        let (mut e, mut rx) = engine(&s.0);
        let ticket = open_long(&mut e);
        drain(&mut rx);

        assert_eq!(
            amend(&mut e, TRADER, ticket, Amendment::Adjust(m("12.50"))),
            AdminOutcome::Done
        );
        e.commit_at(e.last_sync).unwrap();

        // The client hears about it, not only the administrator who did it.
        let heard = drain_for(&mut rx, TRADER);
        let amended = heard
            .iter()
            .find_map(|msg| match msg {
                ServerMessage::Amended {
                    ticket: t,
                    kind,
                    amount,
                    reason,
                    ..
                } => Some((*t, *kind, amount.clone(), *reason)),
                _ => None,
            })
            .expect("the client is told");
        assert_eq!(amended.0, ticket.0);
        assert_eq!(amended.1, "adjust");
        assert_eq!(amended.2.as_deref(), Some("12.500000000"));
        assert_eq!(amended.3, "badPrice");

        assert_eq!(balance_of(&e, TRADER), "25012.50");
        books_agree(&e);
        // And in its own book, never mixed into trading profit.
        assert_eq!(
            house(&e, ledger::AccountKind::Adjustment)
                .to_display_string(2)
                .unwrap(),
            "-12.50"
        );
        assert_eq!(house(&e, ledger::AccountKind::RealisedPnl), Money::ZERO);
    }

    #[test]
    fn an_adjustment_cannot_overdraw_an_account_or_move_nothing() {
        let s = Scratch::new("amend-limits");
        let (mut e, _rx) = engine(&s.0);
        assert!(matches!(
            amend(&mut e, TRADER, OrderId(0), Amendment::Adjust(m("0"))),
            AdminOutcome::Refused(_)
        ));
        // 25,000 cash and a 500 bonus: a 25,400 debit would eat the bonus and
        // leave the books showing cash below zero.
        assert!(matches!(
            amend(
                &mut e,
                TRADER,
                OrderId(0),
                Amendment::Adjust(m("-25400.00"))
            ),
            AdminOutcome::Refused(_)
        ));
        assert!(matches!(
            amend(
                &mut e,
                AccountId(4004),
                OrderId(0),
                Amendment::Adjust(m("5.00"))
            ),
            AdminOutcome::Refused(_)
        ));
        assert_eq!(balance_of(&e, TRADER), "25000.00");

        // A debit within the cash is allowed: a fee taken in error, returned.
        assert_eq!(
            amend(&mut e, TRADER, OrderId(0), Amendment::Adjust(m("-40.00"))),
            AdminOutcome::Done
        );
        assert_eq!(balance_of(&e, TRADER), "24960.00");
        books_agree(&e);
    }

    #[test]
    fn a_corrected_entry_price_changes_what_the_position_will_realise() {
        let s = Scratch::new("amend-price");
        let (mut e, mut rx) = engine(&s.0);
        let ticket = open_long(&mut e);
        let opened_at = e.portfolio.account(TRADER).unwrap().lots(BTC)[0].open_price;
        let balance_before = balance_of(&e, TRADER);
        drain(&mut rx);

        // Filled at 80,000.01; it should have been 79,900.
        assert_eq!(
            amend(
                &mut e,
                TRADER,
                ticket,
                Amendment::CorrectOpenPrice(p("79900.00"))
            ),
            AdminOutcome::Done
        );
        e.commit_at(e.last_sync).unwrap();

        let lot = e.portfolio.account(TRADER).unwrap().lots(BTC)[0];
        assert_eq!(lot.open_price, p("79900.00"));
        assert_ne!(lot.open_price, opened_at);
        // No money has moved: nothing was realised.
        assert_eq!(balance_of(&e, TRADER), balance_before);
        books_agree(&e);

        // The client is told what it was and what it is.
        assert!(drain_for(&mut rx, TRADER).iter().any(|msg| matches!(
            msg,
            ServerMessage::Amended { kind, from, to, .. }
                if *kind == "correctPrice"
                    && from.as_deref() == Some("80000.010000000")
                    && to.as_deref() == Some("79900.000000000")
        )));

        // And closing now realises against the corrected price.
        e.handle(Command::Close {
            by: TRADER_USER,
            account: TRADER,
            ticket,
            qty: Qty::ZERO,
        })
        .unwrap();
        let gained = e
            .portfolio
            .account(TRADER)
            .unwrap()
            .balance
            .sub(m("25000.00"))
            .unwrap();
        assert_eq!(gained.to_display_string(2).unwrap(), "1.00");
        books_agree(&e);
    }

    #[test]
    fn only_an_open_position_on_this_account_can_be_corrected() {
        let s = Scratch::new("amend-price-refused");
        let (mut e, _rx) = engine(&s.0);
        second_account(&mut e);
        let ticket = open_long(&mut e);

        // Another account's ticket.
        assert!(matches!(
            amend(
                &mut e,
                SECOND,
                ticket,
                Amendment::CorrectOpenPrice(p("79900.00"))
            ),
            AdminOutcome::Refused(_)
        ));
        // Off the tick grid, and not a price at all.
        assert!(matches!(
            amend(
                &mut e,
                TRADER,
                ticket,
                Amendment::CorrectOpenPrice(p("79900.005"))
            ),
            AdminOutcome::Refused(_)
        ));
        assert!(matches!(
            amend(
                &mut e,
                TRADER,
                ticket,
                Amendment::CorrectOpenPrice(Price::ZERO)
            ),
            AdminOutcome::Refused(_)
        ));

        // A closed trade cannot be corrected: it is already in the balance,
        // and putting that right is an adjustment.
        e.handle(Command::Close {
            by: TRADER_USER,
            account: TRADER,
            ticket,
            qty: Qty::ZERO,
        })
        .unwrap();
        assert!(matches!(
            amend(
                &mut e,
                TRADER,
                ticket,
                Amendment::CorrectOpenPrice(p("79900.00"))
            ),
            AdminOutcome::Refused(_)
        ));
    }

    #[test]
    fn amendments_are_in_the_audit_and_replay_from_the_journal() {
        let s = Scratch::new("amend-replay");
        let live = {
            let (mut e, _rx) = engine(&s.0);
            let ticket = open_long(&mut e);
            amend(&mut e, TRADER, ticket, Amendment::Adjust(m("12.50")));
            amend(
                &mut e,
                TRADER,
                ticket,
                Amendment::CorrectOpenPrice(p("79900.00")),
            );
            e.commit_at(e.last_sync).unwrap();

            let lines: Vec<_> = e
                .audit
                .iter()
                .filter(|l| l.action == "adjustment" || l.action == "priceCorrected")
                .cloned()
                .collect();
            assert_eq!(lines.len(), 2);
            assert!(lines.iter().all(|l| l.by == ADMIN_USER.0));
            assert!(lines[0].detail.contains("badPrice"));
            assert!(lines[1].detail.contains("79900"));

            (
                e.portfolio.account(TRADER).unwrap().balance,
                e.portfolio.account(TRADER).unwrap().lots(BTC)[0].open_price,
            )
        };

        let (tx, _rx) = broadcast::channel(256);
        let reopened = Engine::open(&s.0, "test", EngineConfig::default(), tx).expect("reopens");
        let account = reopened.portfolio.account(TRADER).unwrap();
        assert_eq!(account.balance, live.0, "the adjustment replays");
        assert_eq!(
            account.lots(BTC)[0].open_price,
            live.1,
            "and so does the corrected price"
        );
        assert_eq!(
            reopened
                .audit
                .iter()
                .filter(|l| l.action == "adjustment" || l.action == "priceCorrected")
                .count(),
            2,
            "an amendment is permanent: it is in the journal for ever"
        );
        books_agree(&reopened);
    }

    // ── The double-entry books ──────────────────────────────────────────

    /// What the books say an account's withdrawable cash is.
    fn cash(e: &Engine, account: AccountId) -> money::Money {
        ledger::withdrawable(&e.ledger, account, domain::Currency::USD)
    }

    fn bonus(e: &Engine, account: AccountId) -> money::Money {
        e.ledger.balance(&ledger::LedgerAccount::client(
            account,
            ledger::AccountKind::Bonus,
            domain::Currency::USD,
        ))
    }

    fn house(e: &Engine, kind: ledger::AccountKind) -> money::Money {
        e.ledger
            .balance(&ledger::LedgerAccount::house(kind, domain::Currency::USD))
    }

    /// The two things that must always be true of the books.
    #[test]
    fn commission_leaves_the_balance_and_lands_in_the_house_book() {
        let s = Scratch::new("commission");
        let (mut e, _rx) = engine(&s.0);
        priced(&mut e, "0", "2.50");

        let before = balance_of(&e, TRADER);
        place(&mut e, Side::Buy, q("1"));

        // One unit at 2.50 a unit.
        let after = balance_of(&e, TRADER);
        assert_ne!(before, after, "the charge left the balance");
        let moved = money::Money::parse(&before)
            .unwrap()
            .sub(money::Money::parse(&after).unwrap())
            .unwrap();
        assert_eq!(moved.to_display_string(2).unwrap(), "2.50");

        // And it is in the books, not merely missing from the balance.
        books_agree(&e);
    }

    #[test]
    fn commission_scales_with_size() {
        let s = Scratch::new("commission-size");
        let (mut e, _rx) = engine(&s.0);
        priced(&mut e, "0", "2.50");

        let before = balance_of(&e, TRADER);
        place(&mut e, Side::Buy, q("0.40"));
        let after = balance_of(&e, TRADER);
        let moved = money::Money::parse(&before)
            .unwrap()
            .sub(money::Money::parse(&after).unwrap())
            .unwrap();
        assert_eq!(moved.to_display_string(2).unwrap(), "1.00");
        books_agree(&e);
    }

    #[test]
    fn no_commission_takes_nothing() {
        let s = Scratch::new("commission-none");
        let (mut e, _rx) = engine(&s.0);
        priced(&mut e, "0", "0");

        let before = balance_of(&e, TRADER);
        place(&mut e, Side::Buy, q("1"));
        assert_eq!(
            balance_of(&e, TRADER),
            before,
            "nothing charged, nothing taken"
        );
        books_agree(&e);
    }

    #[test]
    fn a_charged_fill_replays_to_the_same_balance() {
        // The whole point of carrying it on the event: a restart must not
        // give the money back.
        let s = Scratch::new("commission-replay");
        let after = {
            let (mut e, _rx) = engine(&s.0);
            priced(&mut e, "0", "2.50");
            place(&mut e, Side::Buy, q("1"));
            let now = e.last_sync + Duration::from_secs(1);
            e.commit_at(now).unwrap();
            balance_of(&e, TRADER)
        };

        let (again, _rx) = engine(&s.0);
        assert_eq!(balance_of(&again, TRADER), after, "replay charged it once");
        books_agree(&again);
    }

    #[test]
    fn insurance_comes_out_of_the_balance_and_goes_back() {
        let s = Scratch::new("insurance");
        let (mut e, _rx) = engine(&s.0);
        let before = balance_of(&e, TRADER);

        administer(&mut e, TRADER, AdminAction::SetInsurance(m("1000.00")));
        let after = balance_of(&e, TRADER);
        let moved = money::Money::parse(&before)
            .unwrap()
            .sub(money::Money::parse(&after).unwrap())
            .unwrap();
        assert_eq!(moved.to_display_string(2).unwrap(), "1000.00");
        assert_eq!(e.portfolio.account(TRADER).unwrap().insurance, m("1000.00"));
        books_agree(&e);

        // Releasing it puts it back. Nothing was created or destroyed:
        // both pots are the client's.
        administer(
            &mut e,
            TRADER,
            AdminAction::SetInsurance(money::Money::ZERO),
        );
        assert_eq!(balance_of(&e, TRADER), before);
        books_agree(&e);
    }

    #[test]
    fn money_cannot_be_set_aside_that_is_not_there() {
        // An overdraft by another name.
        let s = Scratch::new("insurance-over");
        let (mut e, _rx) = engine(&s.0);
        let before = balance_of(&e, TRADER);

        let outcome = administer(&mut e, TRADER, AdminAction::SetInsurance(m("999999.00")));
        assert!(matches!(outcome, AdminOutcome::Refused(_)), "{outcome:?}");
        assert_eq!(balance_of(&e, TRADER), before, "nothing moved");
    }

    #[test]
    fn insurance_is_out_of_the_markets_reach() {
        // The whole point of the pot: it does not support margin, so
        // setting money aside *reduces* what can be traded.
        let s = Scratch::new("insurance-margin");
        let (mut e, _rx) = engine(&s.0);
        let free_before = e
            .portfolio
            .account(TRADER)
            .unwrap()
            .free_margin(&e.portfolio.market, e.policy)
            .unwrap();

        administer(&mut e, TRADER, AdminAction::SetInsurance(m("1000.00")));
        let free_after = e
            .portfolio
            .account(TRADER)
            .unwrap()
            .free_margin(&e.portfolio.market, e.policy)
            .unwrap();

        let fell = free_before.sub(free_after).unwrap();
        assert_eq!(
            fell.to_display_string(2).unwrap(),
            "1000.00",
            "money set aside must not still be tradeable"
        );
    }

    #[test]
    fn insurance_survives_a_restart() {
        let s = Scratch::new("insurance-replay");
        let (held, balance) = {
            let (mut e, _rx) = engine(&s.0);
            administer(&mut e, TRADER, AdminAction::SetInsurance(m("750.00")));
            let now = e.last_sync + Duration::from_secs(1);
            e.commit_at(now).unwrap();
            (
                e.portfolio.account(TRADER).unwrap().insurance,
                balance_of(&e, TRADER),
            )
        };
        let (again, _rx) = engine(&s.0);
        assert_eq!(again.portfolio.account(TRADER).unwrap().insurance, held);
        assert_eq!(balance_of(&again, TRADER), balance);
        books_agree(&again);
    }

    // ── Transfers ─────────────────────────────────────────────────────

    #[test]
    fn money_moves_between_two_accounts_and_the_platform_is_no_richer() {
        let s = Scratch::new("transfer");
        let (mut e, _rx) = engine(&s.0);
        let other = second_account(&mut e);
        // Both the client's, so the transfer is between accounts one person
        // holds \u2014 which is the case this exists for.
        administer(&mut e, other, AdminAction::SetLive(false));

        let from_before = money::Money::parse(&balance_of(&e, TRADER)).unwrap();
        let to_before = money::Money::parse(&balance_of(&e, other)).unwrap();

        let outcome = administer(
            &mut e,
            TRADER,
            AdminAction::Transfer {
                to: other,
                amount: m("1500.00"),
            },
        );
        assert!(matches!(outcome, AdminOutcome::Done), "{outcome:?}");

        let from_after = money::Money::parse(&balance_of(&e, TRADER)).unwrap();
        let to_after = money::Money::parse(&balance_of(&e, other)).unwrap();
        assert_eq!(from_before.sub(from_after).unwrap(), m("1500.00"));
        assert_eq!(to_after.sub(to_before).unwrap(), m("1500.00"));
        // The two sides are the same figure, so the pair of balances sums to
        // what it did before. Nothing was created.
        assert_eq!(
            from_after.add(to_after).unwrap(),
            from_before.add(to_before).unwrap()
        );
        books_agree(&e);
    }

    #[test]
    fn a_transfer_is_one_record_so_a_replay_cannot_land_between_the_legs() {
        // The whole reason this is one event. If it were a withdrawal
        // followed by a deposit, a journal truncated between them would
        // replay to a state where the money had left and arrived nowhere \u2014
        // and the journal is the database, so that state would be the truth.
        let s = Scratch::new("transfer-replay");
        let other;
        let (sum_before, sum_after, seq_count) = {
            let (mut e, _rx) = engine(&s.0);
            other = second_account(&mut e);
            administer(&mut e, other, AdminAction::SetLive(false));
            let before = money::Money::parse(&balance_of(&e, TRADER))
                .unwrap()
                .add(money::Money::parse(&balance_of(&e, other)).unwrap())
                .unwrap();
            let at = e.journal.next_seq().0;
            administer(
                &mut e,
                TRADER,
                AdminAction::Transfer {
                    to: other,
                    amount: m("2000.00"),
                },
            );
            let after = money::Money::parse(&balance_of(&e, TRADER))
                .unwrap()
                .add(money::Money::parse(&balance_of(&e, other)).unwrap())
                .unwrap();
            (before, after, e.journal.next_seq().0 - at)
        };
        assert_eq!(sum_before, sum_after);
        assert_eq!(seq_count, 1, "a transfer is exactly one journal record");

        // And it replays to the same place.
        let (again, _rx) = engine(&s.0);
        let replayed = money::Money::parse(&balance_of(&again, TRADER))
            .unwrap()
            .add(money::Money::parse(&balance_of(&again, other)).unwrap())
            .unwrap();
        assert_eq!(replayed, sum_after);
        books_agree(&again);
    }

    #[test]
    fn practice_money_never_reaches_a_live_account() {
        // The refusal that matters most: everything else on the list costs
        // somebody a day, and this one would put invented figures into a
        // balance a client can withdraw.
        let s = Scratch::new("transfer-demo");
        let (mut e, _rx) = engine(&s.0);
        let other = second_account(&mut e);
        administer(&mut e, other, AdminAction::SetLive(true));

        let before = balance_of(&e, other);
        let outcome = administer(
            &mut e,
            TRADER,
            AdminAction::Transfer {
                to: other,
                amount: m("100.00"),
            },
        );
        assert!(matches!(outcome, AdminOutcome::Refused(_)), "{outcome:?}");
        assert_eq!(balance_of(&e, other), before, "nothing moved");

        // And not the other way either.
        let back = administer(
            &mut e,
            other,
            AdminAction::Transfer {
                to: TRADER,
                amount: m("100.00"),
            },
        );
        assert!(matches!(back, AdminOutcome::Refused(_)), "{back:?}");
        books_agree(&e);
    }

    #[test]
    fn a_transfer_is_refused_for_the_same_reasons_a_withdrawal_is() {
        let s = Scratch::new("transfer-refusals");
        let (mut e, _rx) = engine(&s.0);
        let other = second_account(&mut e);
        administer(&mut e, other, AdminAction::SetLive(false));
        let before = balance_of(&e, TRADER);

        for (what, action) in [
            (
                "zero",
                AdminAction::Transfer {
                    to: other,
                    amount: money::Money::ZERO,
                },
            ),
            (
                "itself",
                AdminAction::Transfer {
                    to: TRADER,
                    amount: m("10.00"),
                },
            ),
            (
                "an account that does not exist",
                AdminAction::Transfer {
                    to: AccountId(999_999),
                    amount: m("10.00"),
                },
            ),
            (
                "more than is there",
                AdminAction::Transfer {
                    to: other,
                    amount: m("9999999.00"),
                },
            ),
        ] {
            let outcome = administer(&mut e, TRADER, action);
            assert!(
                matches!(outcome, AdminOutcome::Refused(_)),
                "{what} went through: {outcome:?}"
            );
            assert_eq!(balance_of(&e, TRADER), before, "{what} moved money");
        }
        books_agree(&e);
    }

    #[test]
    fn a_bonus_cannot_be_moved_onto_another_account() {
        // A bonus supports margin but is not the client's. Letting it move
        // would let a client park it where the condition that earned it does
        // not apply, and then withdraw from there.
        let s = Scratch::new("transfer-bonus");
        let (mut e, _rx) = engine(&s.0);
        let other = second_account(&mut e);
        administer(&mut e, other, AdminAction::SetLive(false));

        // Everything in cash goes out, leaving only bonus behind.
        let cash_here = cash(&e, TRADER);
        administer(&mut e, TRADER, AdminAction::Withdraw(cash_here));
        administer(&mut e, TRADER, AdminAction::SetCredit(m("500.00")));

        let outcome = administer(
            &mut e,
            TRADER,
            AdminAction::Transfer {
                to: other,
                amount: m("100.00"),
            },
        );
        assert!(matches!(outcome, AdminOutcome::Refused(_)), "{outcome:?}");
        books_agree(&e);
    }

    #[test]
    fn both_sides_of_a_transfer_are_on_the_statement_at_the_same_moment() {
        let s = Scratch::new("transfer-statement");
        let (mut e, _rx) = engine(&s.0);
        let other = second_account(&mut e);
        administer(&mut e, other, AdminAction::SetLive(false));

        administer(
            &mut e,
            TRADER,
            AdminAction::Transfer {
                to: other,
                amount: m("300.00"),
            },
        );

        let out = e
            .statement
            .iter()
            .rev()
            .find(|(id, l)| *id == TRADER && l.kind == "transferOut")
            .expect("the sending account has a line");
        let into = e
            .statement
            .iter()
            .rev()
            .find(|(id, l)| *id == other && l.kind == "transferIn")
            .expect("the receiving account has a line");

        // One event, so one sequence number. A statement that showed the two
        // legs at different moments would invite somebody to go looking for
        // the money in between.
        assert_eq!(out.1.seq, into.1.seq);
        assert!(out.1.amount.starts_with('-'), "{}", out.1.amount);
        assert!(!into.1.amount.starts_with('-'), "{}", into.1.amount);
    }

    // ── Partial closes ────────────────────────────────────────────────

    /// Closes `qty` of `ticket`, or all of it when zero.
    fn close(e: &mut Engine, ticket: OrderId, qty: Qty) -> Result<()> {
        e.handle(Command::Close {
            by: TRADER_USER,
            account: TRADER,
            ticket,
            qty,
        })
    }

    /// What is left of one parcel, or none if it has gone.
    fn parcel(e: &Engine, ticket: OrderId) -> Option<Qty> {
        let a = e.portfolio.account(TRADER)?;
        a.open_instruments()
            .flat_map(|i| a.lots(i).to_vec())
            .find(|l| l.ticket == ticket)
            .map(|l| l.qty)
    }

    #[test]
    fn closing_part_of_a_hedged_position_leaves_the_rest_open() {
        let s = Scratch::new("partial-hedged");
        let (mut e, _rx) = engine(&s.0);
        administer(&mut e, TRADER, AdminAction::SetHedging(true));
        place(&mut e, Side::Buy, q("1"));
        let ticket = e.portfolio.account(TRADER).unwrap().lots(BTC)[0].ticket;

        close(&mut e, ticket, q("0.400")).unwrap();
        assert_eq!(
            parcel(&e, ticket),
            Some(q("0.600")),
            "the remainder keeps the ticket"
        );
        books_agree(&e);

        // And the rest goes.
        close(&mut e, ticket, Qty::ZERO).unwrap();
        assert_eq!(parcel(&e, ticket), None);
        books_agree(&e);
    }

    #[test]
    fn closing_part_of_a_netting_position_sells_only_that_much() {
        // On a netting account a close is an opposing order, so a partial
        // close is a smaller one. The parcel shrinks the same way.
        let s = Scratch::new("partial-netting");
        let (mut e, _rx) = engine(&s.0);
        place(&mut e, Side::Buy, q("1"));
        let ticket = e.portfolio.account(TRADER).unwrap().lots(BTC)[0].ticket;

        close(&mut e, ticket, q("0.250")).unwrap();
        let left = e.portfolio.account(TRADER).unwrap().lots(BTC);
        assert_eq!(left.len(), 1, "not a second position");
        assert_eq!(left[0].qty, q("0.750"));
        books_agree(&e);
    }

    #[test]
    fn a_partial_close_pays_out_its_share_and_leaves_the_rest_running() {
        let s = Scratch::new("partial-profit");
        let (mut e, _rx) = engine(&s.0);
        administer(&mut e, TRADER, AdminAction::SetHedging(true));
        place(&mut e, Side::Buy, q("1"));
        let ticket = e.portfolio.account(TRADER).unwrap().lots(BTC)[0].ticket;

        // Up a thousand a coin.
        tick(&mut e, "81000.00", "81000.01");
        let before = money::Money::parse(&balance_of(&e, TRADER)).unwrap();
        close(&mut e, ticket, q("0.400")).unwrap();
        let after = money::Money::parse(&balance_of(&e, TRADER)).unwrap();

        // 0.4 at 1,000 up, against the bid.
        assert_eq!(
            after.sub(before).unwrap().to_display_string(2).unwrap(),
            "400.00"
        );
        // The rest is still in, so the profit on it is still floating
        // rather than realised \u2014 which is the whole point of taking part
        // off instead of all of it.
        assert_eq!(parcel(&e, ticket), Some(q("0.600")));
        books_agree(&e);
    }

    #[test]
    fn a_close_off_the_step_grid_is_refused() {
        // A size that is not a multiple of the step leaves a remainder
        // that is not either, and that remainder is one no ordinary order
        // can ever close — the trader would have to ask the desk to clear
        // it by hand.
        //
        // The companion rule in `closable`, that what is left must be at
        // least the minimum size, cannot fire for anything in today's
        // catalogue: every instrument's step equals its minimum, so a
        // multiple of the step is always at least the minimum. It is there
        // for when the desk can set the two apart, which is what symbol
        // administration will allow.
        let s = Scratch::new("partial-remainder");
        let (mut e, mut rx) = engine(&s.0);
        administer(&mut e, TRADER, AdminAction::SetHedging(true));
        place(&mut e, Side::Buy, q("0.002"));
        let ticket = e.portfolio.account(TRADER).unwrap().lots(BTC)[0].ticket;
        let _ = drain(&mut rx);

        // BTC's minimum is 0.001 and its step is 0.001, so this would
        // leave 0.0005 behind.
        close(&mut e, ticket, q("0.0015")).unwrap();
        e.commit_at(e.last_sync).unwrap();
        let said = drain(&mut rx);
        assert!(
            said.iter().any(|m| matches!(
                m,
                ServerMessage::Rejected {
                    reason: "invalidQuantity"
                }
            )),
            "{said:?}"
        );
        assert_eq!(parcel(&e, ticket), Some(q("0.002")), "nothing moved");
    }

    #[test]
    fn a_close_bigger_than_the_position_is_refused_rather_than_clamped() {
        // The engine's fold clamps, because a replay has nobody to tell.
        // A live trader does, and a silent clamp is how somebody closes a
        // whole position believing they took half off.
        let s = Scratch::new("partial-too-big");
        let (mut e, mut rx) = engine(&s.0);
        administer(&mut e, TRADER, AdminAction::SetHedging(true));
        place(&mut e, Side::Buy, q("0.100"));
        let ticket = e.portfolio.account(TRADER).unwrap().lots(BTC)[0].ticket;
        let _ = drain(&mut rx);

        close(&mut e, ticket, q("0.500")).unwrap();
        e.commit_at(e.last_sync).unwrap();
        assert!(drain(&mut rx).iter().any(|m| matches!(
            m,
            ServerMessage::Rejected {
                reason: "invalidQuantity"
            }
        )));
        assert_eq!(parcel(&e, ticket), Some(q("0.100")), "nothing moved");
    }

    #[test]
    fn the_whole_parcel_always_closes_whatever_the_step_is() {
        // A position that cannot be closed is not a position anybody
        // should be left holding \u2014 including one opened before a step or
        // a minimum changed under it.
        let s = Scratch::new("partial-whole");
        let (mut e, _rx) = engine(&s.0);
        administer(&mut e, TRADER, AdminAction::SetHedging(true));
        place(&mut e, Side::Buy, q("0.001"));
        let ticket = e.portfolio.account(TRADER).unwrap().lots(BTC)[0].ticket;

        close(&mut e, ticket, q("0.001")).unwrap();
        assert_eq!(parcel(&e, ticket), None);
        books_agree(&e);
    }

    #[test]
    fn a_partial_close_survives_a_restart_as_the_size_it_was() {
        let s = Scratch::new("partial-replay");
        let ticket;
        {
            let (mut e, _rx) = engine(&s.0);
            administer(&mut e, TRADER, AdminAction::SetHedging(true));
            place(&mut e, Side::Buy, q("1"));
            ticket = e.portfolio.account(TRADER).unwrap().lots(BTC)[0].ticket;
            close(&mut e, ticket, q("0.400")).unwrap();
            e.commit_at(e.last_sync).unwrap();
            assert_eq!(parcel(&e, ticket), Some(q("0.600")));
        }
        // The fold has to honour the size on the event. Before this it
        // threw the size away and closed the whole parcel, so a replay
        // would have lost the remainder.
        let (again, _rx) = engine(&s.0);
        assert_eq!(parcel(&again, ticket), Some(q("0.600")));
        books_agree(&again);
    }

    #[test]
    fn closing_is_charged_on_a_hedging_account_as_it_is_on_a_netting_one() {
        // It was not. The hedged path bypassed the fill, and
        // `PositionClosed` had nowhere to put a charge \u2014 so whether a
        // close cost anything depended on a setting that has nothing to do
        // with pricing.
        let s = Scratch::new("close-commission");
        let (mut e, _rx) = engine(&s.0);
        administer(&mut e, TRADER, AdminAction::SetHedging(true));
        priced(&mut e, "0", "2.50");
        place(&mut e, Side::Buy, q("1"));
        let ticket = e.portfolio.account(TRADER).unwrap().lots(BTC)[0].ticket;

        let before = money::Money::parse(&balance_of(&e, TRADER)).unwrap();
        close(&mut e, ticket, Qty::ZERO).unwrap();
        let after = money::Money::parse(&balance_of(&e, TRADER)).unwrap();

        // 2.50 of commission, and a penny of spread on one coin: the
        // position opened at the ask and closed into the bid, as every
        // round trip does.
        assert_eq!(
            before.sub(after).unwrap().to_display_string(2).unwrap(),
            "2.51"
        );
        // And it is in the house book, not merely missing from the balance.
        books_agree(&e);
    }

    #[test]
    fn a_partial_close_is_charged_for_what_it_closed_and_no_more() {
        let s = Scratch::new("close-commission-part");
        let (mut e, _rx) = engine(&s.0);
        administer(&mut e, TRADER, AdminAction::SetHedging(true));
        priced(&mut e, "0", "2.50");
        place(&mut e, Side::Buy, q("1"));
        let ticket = e.portfolio.account(TRADER).unwrap().lots(BTC)[0].ticket;

        let before = money::Money::parse(&balance_of(&e, TRADER)).unwrap();
        close(&mut e, ticket, q("0.400")).unwrap();
        let after = money::Money::parse(&balance_of(&e, TRADER)).unwrap();
        assert_eq!(
            before.sub(after).unwrap().to_display_string(2).unwrap(),
            "1.00"
        );
        books_agree(&e);
    }

    #[test]
    fn a_stop_out_and_a_stop_loss_are_still_free() {
        // The engine closed those, not the trader. Charging somebody for
        // the platform liquidating them is adding insult to a loss.
        let s = Scratch::new("close-commission-forced");
        let (mut e, _rx) = engine(&s.0);
        administer(&mut e, TRADER, AdminAction::SetHedging(true));
        priced(&mut e, "0", "2.50");
        place(&mut e, Side::Buy, q("1"));
        let ticket = e.portfolio.account(TRADER).unwrap().lots(BTC)[0].ticket;
        set_protection(&mut e, ticket, p("79000.00"), Price::ZERO).unwrap();

        let before = money::Money::parse(&balance_of(&e, TRADER)).unwrap();
        // Through the stop.
        tick(&mut e, "78900.00", "78900.01");
        let after = money::Money::parse(&balance_of(&e, TRADER)).unwrap();
        assert_eq!(parcel(&e, ticket), None, "the stop fired");

        // Bought at 80,000.01 and stopped out at 78,900.00: 1,100.01 on
        // one coin, and not a penny of commission on top of it.
        assert_eq!(
            before.sub(after).unwrap().to_display_string(2).unwrap(),
            "1100.01"
        );
        books_agree(&e);
    }

    fn books_agree(e: &Engine) {
        e.ledger
            .assert_balanced()
            .expect("every currency nets to zero");
        for id in e.portfolio.account_ids() {
            let account = e.portfolio.account(id).unwrap();
            assert_eq!(
                cash(e, id),
                account.balance,
                "the books and the balance disagree about account {}",
                id.0
            );
            assert_eq!(
                bonus(e, id),
                account.credit,
                "the books and the bonus disagree about account {}",
                id.0
            );
        }
    }

    #[test]
    fn opening_an_account_posts_its_funding_to_the_books() {
        let s = Scratch::new("books-opening");
        let (e, _rx) = engine(&s.0);
        books_agree(&e);

        assert_eq!(cash(&e, TRADER).to_display_string(2).unwrap(), "25000.00");
        assert_eq!(bonus(&e, TRADER).to_display_string(2).unwrap(), "500.00");
        // The other side of each: money came from the payment provider, and
        // the bonus is the house's expense.
        assert_eq!(
            house(&e, ledger::AccountKind::PspClearing)
                .to_display_string(2)
                .unwrap(),
            "-25000.00"
        );
        assert_eq!(
            house(&e, ledger::AccountKind::Bonus)
                .to_display_string(2)
                .unwrap(),
            "-500.00"
        );
    }

    #[test]
    fn a_closed_trade_moves_money_between_the_client_and_the_house() {
        let s = Scratch::new("books-trade");
        let (mut e, _rx) = engine(&s.0);
        e.handle(market_buy()).unwrap();
        books_agree(&e);
        // Opening realises nothing, so the books have not moved.
        assert_eq!(house(&e, ledger::AccountKind::RealisedPnl), Money::ZERO);

        // The market rises and the position is closed at a profit.
        tick(&mut e, "81000.00", "81000.01");
        let ticket = e.portfolio.account(TRADER).unwrap().lots(BTC)[0].ticket;
        e.handle(Command::Close {
            by: TRADER_USER,
            account: TRADER,
            ticket,
            qty: Qty::ZERO,
        })
        .unwrap();
        books_agree(&e);

        // 0.01 BTC from 80,000.01 to 81,000 is 9.9999 to the client, out of
        // the house's trading book. A B-book's profit is the client's loss and
        // the reverse, which is exactly what two legs say.
        let gained = cash(&e, TRADER).sub(m("25000.00")).unwrap();
        assert_eq!(gained.to_display_string(2).unwrap(), "10.00");
        assert_eq!(
            house(&e, ledger::AccountKind::RealisedPnl),
            gained.neg().unwrap()
        );
    }

    #[test]
    fn financing_is_posted_to_its_own_book_not_to_trading_profit() {
        let s = Scratch::new("books-swap");
        let (mut e, _rx) = engine(&s.0);
        e.handle(market_buy()).unwrap();

        // Charge a night's financing onto the open parcel.
        let ticket = e.portfolio.account(TRADER).unwrap().lots(BTC)[0].ticket;
        e.emit(Event::SwapCharged {
            account: TRADER,
            instrument: BTC,
            order: ticket,
            amount: m("-2.00"),
        })
        .unwrap();
        // Accrued, not yet realised: the books have not moved.
        assert_eq!(cash(&e, TRADER).to_display_string(2).unwrap(), "25000.00");
        books_agree(&e);

        // Close flat on price, so the only thing realised is the financing.
        e.handle(Command::Close {
            by: TRADER_USER,
            account: TRADER,
            ticket,
            qty: Qty::ZERO,
        })
        .unwrap();
        books_agree(&e);

        assert_eq!(
            house(&e, ledger::AccountKind::Swap)
                .to_display_string(2)
                .unwrap(),
            "2.00",
            "the financing the broker earned is readable on its own"
        );
        // And the price loss — the spread — is in the trading book, not mixed
        // in with the financing.
        let trading = house(&e, ledger::AccountKind::RealisedPnl);
        assert!(trading > Money::ZERO, "the spread is the house's");
        assert!(trading < m("1.00"));
    }

    #[test]
    fn a_bonus_is_posted_as_a_difference_not_as_a_total() {
        let s = Scratch::new("books-bonus");
        let (mut e, _rx) = engine(&s.0);
        administer(&mut e, TRADER, AdminAction::SetCredit(m("750.00")));
        books_agree(&e);
        assert_eq!(bonus(&e, TRADER).to_display_string(2).unwrap(), "750.00");

        // Down again: the books move by the difference, and setting it to what
        // it already is moves nothing at all.
        let before = e.ledger.transaction_count();
        administer(&mut e, TRADER, AdminAction::SetCredit(m("100.00")));
        administer(&mut e, TRADER, AdminAction::SetCredit(m("100.00")));
        books_agree(&e);
        assert_eq!(bonus(&e, TRADER).to_display_string(2).unwrap(), "100.00");
        assert_eq!(
            e.ledger.transaction_count(),
            before + 1,
            "a bonus set to what it already was is not a transaction"
        );
    }

    #[test]
    fn a_withdrawal_cannot_pay_out_the_bonus() {
        let s = Scratch::new("books-bonus-withdrawal");
        let (mut e, _rx) = engine(&s.0);
        // 25,000 cash and a 500 bonus: free margin is 25,500, cash is 25,000.
        // Free margin alone would let all 25,500 out, and 500 of it is the
        // house's promotion.
        assert!(matches!(
            administer(&mut e, TRADER, AdminAction::Withdraw(m("25400.00"))),
            AdminOutcome::Refused(_)
        ));
        assert_eq!(
            administer(&mut e, TRADER, AdminAction::Withdraw(m("25000.00"))),
            AdminOutcome::Done,
            "all the cash can go"
        );
        books_agree(&e);
        assert_eq!(cash(&e, TRADER), Money::ZERO);
        assert_eq!(
            bonus(&e, TRADER).to_display_string(2).unwrap(),
            "500.00",
            "the bonus stays where it is"
        );
    }

    #[test]
    fn the_books_a_restart_rebuilds_are_the_books_that_were_live() {
        let s = Scratch::new("books-replay");
        let live = {
            let (mut e, _rx) = engine(&s.0);
            second_account(&mut e);
            administer(&mut e, TRADER, AdminAction::Deposit(m("1000.00")));
            administer(&mut e, TRADER, AdminAction::SetCredit(m("250.00")));
            // Taken out through the books, not by poking the balance: an
            // account that is nearly empty has to have *become* nearly empty
            // the way a real one would, or the books are right to disagree.
            administer(&mut e, SECOND, AdminAction::Withdraw(m("24900.00")));

            // A trade on each account, one closed by its stop loss and one
            // stopped out, so every realisation path is exercised.
            e.handle(market_buy()).unwrap();
            e.handle(Command::Place {
                by: UserId(2),
                account: SECOND,
                instrument: BTC,
                side: Side::Buy,
                kind: OrderType::Market,
                qty: q("0.30"),
                price: Price::ZERO,
                limit_price: Price::ZERO,
            })
            .unwrap();

            let ticket = e.portfolio.account(TRADER).unwrap().lots(BTC)[0].ticket;
            set_protection(&mut e, ticket, p("78000.00"), Price::ZERO).unwrap();

            // Down enough to take out the small account and trip the stop.
            tick(&mut e, "79000.00", "79000.01");
            tick(&mut e, "77900.00", "77900.01");
            e.commit_at(e.last_sync).unwrap();
            assert!(
                e.portfolio.account(SECOND).unwrap().is_flat(),
                "the small account was stopped out"
            );
            assert!(
                e.portfolio.account(TRADER).unwrap().is_flat(),
                "and the stop loss closed the other"
            );

            books_agree(&e);
            e.ledger
                .balances()
                .map(|(a, m)| (*a, *m))
                .collect::<Vec<_>>()
        };

        let (tx, _rx) = broadcast::channel(256);
        let reopened = Engine::open(&s.0, "test", EngineConfig::default(), tx).expect("reopens");
        let replayed = reopened
            .ledger
            .balances()
            .map(|(a, m)| (*a, *m))
            .collect::<Vec<_>>();
        assert_eq!(replayed, live, "the books must replay exactly");
        books_agree(&reopened);
    }

    #[test]
    fn the_trial_balance_is_zero_in_every_currency_and_says_so() {
        let s = Scratch::new("books-trial");
        let (mut e, _rx) = engine(&s.0);
        administer(&mut e, TRADER, AdminAction::Deposit(m("100.00")));
        e.handle(market_buy()).unwrap();
        tick(&mut e, "81000.00", "81000.01");
        let ticket = e.portfolio.account(TRADER).unwrap().lots(BTC)[0].ticket;
        e.handle(Command::Close {
            by: TRADER_USER,
            account: TRADER,
            ticket,
            qty: Qty::ZERO,
        })
        .unwrap();

        let books = e.books(None).unwrap();
        assert!(books.transactions >= 4);
        assert!(!books.lines.is_empty());
        for line in &books.trial {
            assert_eq!(
                line.residual, "0.000000000",
                "{} does not balance",
                line.currency
            );
        }
        // One account's view holds only that account's pots.
        let mine = e.books(Some(TRADER)).unwrap();
        assert!(mine.lines.iter().all(|l| l.owner == TRADER.0 && !l.house));
    }

    // ── Administration ──────────────────────────────────────────────────

    const ADMIN_USER: UserId = UserId(9);

    fn m(s: &str) -> money::Money {
        money::Money::parse(s).expect("money")
    }

    /// Runs an administrator's action and returns what came back.
    fn administer(e: &mut Engine, account: AccountId, action: AdminAction) -> AdminOutcome {
        let (tx, rx) = oneshot::channel();
        e.handle(Command::Administer {
            by: ADMIN_USER,
            account,
            action,
            reply: tx,
        })
        .unwrap();
        rx.blocking_recv().expect("the engine answers")
    }

    fn balance_of(e: &Engine, account: AccountId) -> String {
        e.portfolio
            .account(account)
            .unwrap()
            .balance
            .to_display_string(2)
            .unwrap()
    }

    #[test]
    fn a_deposit_lands_in_the_balance_and_names_who_posted_it() {
        let s = Scratch::new("admin-deposit");
        let (mut e, mut rx) = engine(&s.0);
        drain(&mut rx);

        assert_eq!(
            administer(&mut e, TRADER, AdminAction::Deposit(m("2500.00"))),
            AdminOutcome::Done
        );
        assert_eq!(balance_of(&e, TRADER), "27500.00");

        // The trader is told without waiting for their next tick.
        e.commit_at(e.last_sync).unwrap();
        assert!(drain_for(&mut rx, TRADER)
            .iter()
            .any(|msg| matches!(msg, ServerMessage::Portfolio { .. })));

        // And the audit says who did it.
        let line = e.audit.last().expect("an audit line");
        assert_eq!(line.action, "deposit");
        assert_eq!(line.by, ADMIN_USER.0);
        assert_eq!(line.account, TRADER.0);
        assert_eq!(line.detail, "2500.000000000");
    }

    #[test]
    fn a_withdrawal_cannot_take_money_that_is_holding_a_position_open() {
        let s = Scratch::new("admin-withdraw");
        let (mut e, _rx) = engine(&s.0);

        // 25,000 and a 500 bonus, with nothing open: free margin is 25,500.
        assert!(matches!(
            administer(&mut e, TRADER, AdminAction::Withdraw(m("30000.00"))),
            AdminOutcome::Refused(_)
        ));
        assert_eq!(balance_of(&e, TRADER), "25000.00");

        // Open something large enough that most of the account is tied up.
        e.handle(Command::Place {
            by: TRADER_USER,
            limit_price: Price::ZERO,
            account: TRADER,
            instrument: BTC,
            side: Side::Buy,
            kind: OrderType::Market,
            // The listing caps a single BTC order at ten coins.
            qty: q("10"),
            price: Price::ZERO,
        })
        .unwrap();
        let free = e
            .portfolio
            .account(TRADER)
            .unwrap()
            .free_margin(&e.portfolio.market, e.policy)
            .unwrap();
        assert!(free < m("25000.00"), "margin is tied up");

        assert!(
            matches!(
                administer(&mut e, TRADER, AdminAction::Withdraw(m("20000.00"))),
                AdminOutcome::Refused(_)
            ),
            "paying out margin would stop the account out on the way to the bank"
        );
        // What is genuinely free can still be paid out.
        assert_eq!(
            administer(&mut e, TRADER, AdminAction::Withdraw(m("100.00"))),
            AdminOutcome::Done
        );
        assert_eq!(balance_of(&e, TRADER), "24900.00");
    }

    #[test]
    fn nonsense_amounts_are_refused_with_a_reason() {
        let s = Scratch::new("admin-nonsense");
        let (mut e, _rx) = engine(&s.0);
        for action in [
            AdminAction::Deposit(m("0")),
            AdminAction::Deposit(m("-50.00")),
            AdminAction::Withdraw(m("-50.00")),
            AdminAction::SetCredit(m("-1.00")),
        ] {
            assert!(
                matches!(administer(&mut e, TRADER, action), AdminOutcome::Refused(_)),
                "{action:?} must be refused"
            );
        }
        assert!(matches!(
            administer(&mut e, AccountId(4004), AdminAction::Deposit(m("10.00"))),
            AdminOutcome::Refused(_)
        ));
        assert_eq!(balance_of(&e, TRADER), "25000.00");
    }

    #[test]
    fn a_bonus_moves_equity_without_touching_the_balance() {
        let s = Scratch::new("admin-credit");
        let (mut e, _rx) = engine(&s.0);
        assert_eq!(
            administer(&mut e, TRADER, AdminAction::SetCredit(m("1000.00"))),
            AdminOutcome::Done
        );
        let (dto, _) = e.portfolio_dto(TRADER).unwrap();
        assert_eq!(dto.balance, "25000.000000000");
        assert_eq!(dto.credit, "1000.000000000");
        assert_eq!(dto.equity, "26000.000000000");
    }

    #[test]
    fn a_suspended_account_is_refused_a_new_order() {
        let s = Scratch::new("admin-suspend");
        let (mut e, mut rx) = engine(&s.0);
        assert_eq!(
            administer(&mut e, TRADER, AdminAction::Suspend),
            AdminOutcome::Done
        );
        drain(&mut rx);

        e.handle(market_buy()).unwrap();
        e.commit_at(e.last_sync).unwrap();
        assert!(drain(&mut rx).iter().any(|msg| matches!(
            msg,
            ServerMessage::Rejected { reason } if *reason == "accountNotTradeable"
        )));
        assert!(e.portfolio.account(TRADER).unwrap().is_flat());

        // Reinstated, the same order goes through.
        assert_eq!(
            administer(&mut e, TRADER, AdminAction::Reinstate),
            AdminOutcome::Done
        );
        e.handle(market_buy()).unwrap();
        e.commit_at(e.last_sync).unwrap();
        assert_eq!(e.portfolio.account(TRADER).unwrap().lots(BTC).len(), 1);
    }

    #[test]
    fn the_overview_shows_every_account_with_its_figures() {
        let s = Scratch::new("admin-overview");
        let (mut e, _rx) = engine(&s.0);
        second_account(&mut e);
        e.handle(market_buy()).unwrap();

        let book = e.overview().unwrap();
        assert_eq!(book.len(), 2);
        let first = book.iter().find(|a| a.id == TRADER.0).unwrap();
        assert_eq!(first.owner, Some(TRADER_USER.0));
        assert_eq!(first.positions, 1);
        assert_eq!(first.credit, "500.000000000");
        assert!(first.tradeable);
        assert!(first.margin_level_bp.is_some());

        let second = book.iter().find(|a| a.id == SECOND.0).unwrap();
        assert_eq!(second.positions, 0);
        assert_eq!(second.margin_level_bp, None, "nothing open, no level");
        // Logins are not the engine's to know.
        assert_eq!(first.owner_login, None);
    }

    #[test]
    fn the_audit_records_who_placed_an_order_and_survives_a_restart() {
        let s = Scratch::new("admin-audit");
        let ticket = {
            let (mut e, _rx) = engine(&s.0);
            // The holder trades for themselves...
            e.handle(market_buy()).unwrap();
            // ... and an administrator trades on their account.
            e.handle(Command::Place {
                by: ADMIN_USER,
                limit_price: Price::ZERO,
                account: TRADER,
                instrument: BTC,
                side: Side::Buy,
                kind: OrderType::Market,
                qty: q("0.02"),
                price: Price::ZERO,
            })
            .unwrap();
            administer(&mut e, TRADER, AdminAction::Deposit(m("100.00")));

            let orders: Vec<AuditDto> = e
                .audit
                .iter()
                .filter(|l| l.action == "order")
                .cloned()
                .collect();
            assert_eq!(orders.len(), 2);
            assert_eq!(orders[0].by, TRADER_USER.0);
            assert_eq!(
                orders[1].by, ADMIN_USER.0,
                "an order placed for a client is attributed to who placed it"
            );
            e.commit_at(e.last_sync).unwrap();
            orders[1].seq
        };

        // Replay rebuilds the trail from the journal.
        let (tx, _rx) = broadcast::channel(256);
        let reopened = Engine::open(&s.0, "test", EngineConfig::default(), tx).expect("reopens");
        let replayed: Vec<_> = reopened
            .audit
            .iter()
            .filter(|l| l.by == ADMIN_USER.0)
            .collect();
        assert_eq!(
            replayed.len(),
            2,
            "the administrator's order and deposit both come back"
        );
        assert!(replayed.iter().any(|l| l.seq == ticket));
        // The platform's own funding is attributed to nobody in particular.
        assert!(reopened
            .audit
            .iter()
            .any(|l| l.action == "deposit" && l.by == UserId::SYSTEM.0));
    }

    #[test]
    fn the_audit_window_is_bounded_and_keeps_the_newest() {
        let s = Scratch::new("admin-audit-window");
        let (mut e, _rx) = engine(&s.0);
        for i in 0..(AUDIT_WINDOW + 20) {
            administer(&mut e, TRADER, AdminAction::SetCredit(m("1.00")));
            if i == 0 {
                assert!(!e.audit.is_empty());
            }
        }
        assert_eq!(e.audit.len(), AUDIT_WINDOW, "the window does not grow");
        let last = e.audit.last().unwrap();
        assert!(
            last.seq > e.audit.first().unwrap().seq,
            "and it holds the newest, not the oldest"
        );
    }

    #[test]
    fn a_restart_reproduces_a_yen_balance_from_the_journal() {
        // The whole reason the rate is in the event. The engine closes a yen
        // position at one rate; the journal is replayed later, when the rate
        // has moved a long way; the balance must be the same to the last
        // place. Recomputing the profit at today's rate would not be.
        let s = Scratch::new("yen-replay");
        let live_balance = {
            let (tx, mut rx) = broadcast::channel(256);
            let mut e = Engine::open(&s.0, "test", EngineConfig::default(), tx).expect("opens");
            e.accounts_for(Some(TRADER_USER)).expect("opens an account");
            quote_usdjpy(&mut e);
            e.handle(Command::Place {
                by: TRADER_USER,
                limit_price: Price::ZERO,
                account: TRADER,
                instrument: USDJPY,
                side: Side::Buy,
                kind: OrderType::Market,
                qty: q("1000000"),
                price: Price::ZERO,
            })
            .unwrap();
            e.handle(Command::Quote {
                instrument: USDJPY,
                bid: p("148.500"),
                ask: p("148.508"),
                source: Source::TWELVE_DATA,
                markup: Price::ZERO,
                commission: Money::ZERO,
            })
            .unwrap();
            let ticket = e.portfolio.account(TRADER).unwrap().lots(USDJPY)[0].ticket;
            e.handle(Command::Close {
                by: TRADER_USER,
                account: TRADER,
                ticket,
                qty: Qty::ZERO,
            })
            .unwrap();
            e.commit_at(e.last_sync).unwrap();
            drain(&mut rx);
            e.portfolio.account(TRADER).unwrap().balance
        };

        // Reopen and replay. Nothing quotes USDJPY this time, so the only
        // rate available is the one in the journal — which is the point.
        let (tx, _rx) = broadcast::channel(256);
        let reopened = Engine::open(&s.0, "test", EngineConfig::default(), tx).expect("reopens");
        let replayed = reopened.portfolio.account(TRADER).unwrap().balance;
        assert_eq!(replayed, live_balance);
        assert!(reopened.portfolio.account(TRADER).unwrap().is_flat());
        // And it is the converted figure, not the yen one: 992,000 yen of
        // profit on a 25,000 dollar demo account is 6,679.95 dollars. If
        // either side had treated the rate as one, this would read 1,017,000.
        assert_eq!(replayed.to_display_string(2).unwrap(), "31679.95");
    }

    #[test]
    fn the_snapshot_says_which_instruments_are_view_only_and_why() {
        let s = Scratch::new("snapshot-notes");
        let (e, _rx) = engine(&s.0);
        let ServerMessage::Snapshot {
            instruments,
            quotes,
            ..
        } = e.snapshot(TRADER).unwrap()
        else {
            panic!("expected a snapshot");
        };
        let jpy = instruments.iter().find(|i| i.symbol == "USDJPY").unwrap();
        assert!(jpy.tradeable, "a routed currency trades");
        assert_eq!(jpy.note, None);
        let eur = instruments.iter().find(|i| i.symbol == "EURUSD").unwrap();
        assert!(eur.tradeable && eur.note.is_none());

        let btc = quotes.iter().find(|q| q.instrument == 1).unwrap();
        assert_eq!(btc.source, "binance");
        assert_eq!(btc.rate, "1.000000000", "a dollar is a dollar");
    }
}
