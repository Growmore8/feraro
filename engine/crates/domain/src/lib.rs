//! Instruments, accounts, orders and the event vocabulary of the engine.
//!
//! # Invariant P0-2 — events are the only truth
//!
//! Engine state is never stored. It is a fold over the [`Event`] stream. Any
//! fact not expressible as an event does not exist, and any state not
//! reachable by replaying events from genesis is a bug.
//!
//! That constraint is what buys us replay in CI, a defensible audit trail, and
//! crash recovery that does not depend on a database being consistent at the
//! moment the power went out.
//!
//! The wire encoding lives here rather than in the journal because the byte
//! layout *is* part of the domain contract. It is hand-written and explicitly
//! versioned: a regulator asking us to reproduce a trading day from 2029 must
//! get the same bytes back, which rules out a derive macro whose output can
//! shift between releases of a dependency.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic)]

use core::fmt;
/// Re-exported so consumers of the event vocabulary do not have to reach past
/// it into `money` for the types its events are built on.
pub use money::{Money, Price, Qty, Side};

/// Version of the event wire format. Bumping this requires a migration path
/// for every journal segment already on disk.
pub const WIRE_VERSION: u16 = 1;

macro_rules! id_type {
    ($name:ident, $inner:ty, $doc:literal) => {
        #[doc = $doc]
        #[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        pub struct $name(pub $inner);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}#{}", stringify!($name), self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }
    };
}

id_type!(AccountId, u64, "Identifies a trading account.");
impl UserId {
    /// The platform rather than a person.
    ///
    /// Used where an event was not somebody's decision: the opening balance
    /// of a demo account, and every attributed event written before the
    /// gateway recorded an actor. Ids issued to people start at one, so this
    /// can never collide with one.
    pub const SYSTEM: UserId = UserId(0);

    /// True when a person took this action, rather than the platform.
    pub const fn is_person(self) -> bool {
        self.0 != 0
    }
}

id_type!(
    UserId,
    u64,
    "Identifies a person who signs in.\n\nSeparate from [`AccountId`] because the two are not the same thing: a person may hold several accounts, a dealer acts on accounts they do not own, and an account outlives the login that opened it. Credentials are *not* journaled — see [`Event::AccountOwnerSet`]."
);
id_type!(OrderId, u64, "Identifies a single client order.");
id_type!(InstrumentId, u32, "Identifies a listed instrument.");
id_type!(
    Seq,
    u64,
    "Position of a record in the journal. Gapless and strictly increasing."
);

/// Nanoseconds since the Unix epoch, UTC.
///
/// Engine time is supplied by the caller and journaled, never read from the
/// system clock inside the matching loop. A replay must produce identical
/// timestamps, and `SystemTime::now()` cannot promise that.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp(pub i64);

impl fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Timestamp({}ns)", self.0)
    }
}

/// An ISO 4217 currency code packed into four bytes.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Currency(u32);

impl Currency {
    /// Builds a currency from a three-letter code.
    pub const fn new(code: &[u8; 3]) -> Self {
        Currency((code[0] as u32) << 16 | (code[1] as u32) << 8 | (code[2] as u32))
    }

    /// The three-letter code.
    pub const fn code(self) -> [u8; 3] {
        [
            ((self.0 >> 16) & 0xFF) as u8,
            ((self.0 >> 8) & 0xFF) as u8,
            (self.0 & 0xFF) as u8,
        ]
    }

    /// Raw packed representation, for the wire format.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Rebuilds from the packed representation.
    pub const fn from_raw(raw: u32) -> Self {
        Currency(raw)
    }

    /// United States dollar.
    pub const USD: Currency = Currency::new(b"USD");
    /// Euro.
    pub const EUR: Currency = Currency::new(b"EUR");
    /// Pound sterling.
    pub const GBP: Currency = Currency::new(b"GBP");
    /// Japanese yen.
    pub const JPY: Currency = Currency::new(b"JPY");
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = self.code();
        f.write_str(core::str::from_utf8(&c).unwrap_or("???"))
    }
}

impl fmt::Debug for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

/// A trading symbol, at most sixteen bytes, space padded on the wire.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Symbol([u8; 16]);

impl Symbol {
    /// Builds a symbol, truncating past sixteen bytes.
    pub fn new(s: &str) -> Self {
        let mut buf = [b' '; 16];
        let bytes = s.as_bytes();
        let n = bytes.len().min(16);
        buf[..n].copy_from_slice(&bytes[..n]);
        Symbol(buf)
    }

    /// The symbol without its padding.
    pub fn as_str(&self) -> &str {
        let end = self.0.iter().rposition(|&b| b != b' ').map_or(0, |i| i + 1);
        core::str::from_utf8(&self.0[..end]).unwrap_or("")
    }

    /// Raw padded bytes, for the wire format.
    pub const fn raw(&self) -> [u8; 16] {
        self.0
    }

    /// Rebuilds from raw padded bytes.
    pub const fn from_raw(raw: [u8; 16]) -> Self {
        Symbol(raw)
    }
}

impl Default for Symbol {
    fn default() -> Self {
        Symbol([b' '; 16])
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Symbol({})", self.as_str())
    }
}

/// A listed instrument and its contract terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instrument {
    /// Engine-assigned identity.
    pub id: InstrumentId,
    /// Display symbol, for example `EURUSD`.
    pub symbol: Symbol,
    /// Currency being bought or sold.
    pub base: Currency,
    /// Currency the price is quoted in.
    pub quote: Currency,
    /// Decimal places shown to traders — five for FX majors, three for JPY
    /// pairs, two for index CFDs.
    pub display_digits: u8,
    /// Base units in one standard lot. `100_000` for FX majors.
    pub contract_size: Qty,
    /// Smallest price increment the venue accepts.
    pub tick_size: Price,
}

/// How an order should be priced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OrderType {
    /// Fill immediately at whatever the book offers.
    Market,
    /// Rest on the book until the limit price or better is available.
    Limit,
    /// Become a market order once the stop price trades.
    Stop,
    /// Become a limit order once the stop price trades.
    StopLimit,
}

/// How long an order stays live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimeInForce {
    /// Rest until cancelled.
    GoodTilCancelled,
    /// Fill what is available now, cancel the remainder.
    ImmediateOrCancel,
    /// Fill entirely now or cancel entirely.
    FillOrKill,
    /// Cancel at the end of the trading day.
    Day,
}

/// Why the engine refused an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RejectReason {
    /// Free margin does not cover the position.
    InsufficientMargin,
    /// The instrument is not listed or not tradeable right now.
    UnknownInstrument,
    /// Outside the instrument's trading session.
    MarketClosed,
    /// Quantity below minimum, above maximum, or off the step.
    InvalidQuantity,
    /// Price is not on the instrument's tick grid.
    InvalidPrice,
    /// A fill-or-kill order could not be filled in full.
    CouldNotFillInFull,
    /// The account is closed, suspended or read-only.
    AccountNotTradeable,
    /// A demo account past its thirty days.
    ///
    /// Separate from [`AccountNotTradeable`](Self::AccountNotTradeable)
    /// because the remedy is different and the trader should be told which it
    /// is: a suspension is the desk's decision about them, an expiry is a
    /// clock they can restart by asking.
    DemoExpired,
    /// The order id has already been used.
    DuplicateOrderId,
    /// The instrument is priced in a currency the account is not held in, and
    /// the rate needed to convert between them is missing or stale.
    ///
    /// Distinct from [`MarketClosed`](Self::MarketClosed) on purpose: the
    /// instrument itself is quoted and tradeable, and nothing is wrong with
    /// the order. A trader told "market closed" while the price ticks in front
    /// of them has been told something false.
    NoConversionRate,
}

/// Why a trade was adjusted or corrected.
///
/// A fixed list rather than free text, and journaled: "the operator typed
/// something" is not an answer anybody can audit, and a reason that cannot be
/// counted cannot be reported on. Free notes belong in a support system,
/// attached to the ticket, not in the trade history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AmendReason {
    /// The price filled at was not the market's — a feed glitch, a stale
    /// quote, a crossed book.
    BadPrice,
    /// A mistake in dealing: wrong size, wrong side, wrong account.
    DealingError,
    /// A charge being returned: swap, commission, a fee taken in error.
    FeeReturned,
    /// A payment to the client that is not a correction of anything.
    Goodwill,
    /// Anything else. Expected to be rare; a reason nobody can name is a
    /// reason worth asking about.
    Other,
}

/// What happened to a sign-in attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionEvent {
    /// Signed in with a password.
    Opened,
    /// Took up an existing session with its token.
    Resumed,
    /// Credentials were not accepted.
    Refused,
    /// Signed out, or the socket closed.
    Closed,
}

/// Everything that can happen, and the only way state changes.
///
/// Ordering of variants is load-bearing only in that each carries a stable
/// wire tag; adding a variant means adding a new tag, never renumbering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A trading account came into existence.
    AccountOpened {
        /// The new account.
        account: AccountId,
        /// Currency all balances on it are denominated in.
        currency: Currency,
        /// Leverage as a ratio denominator — `100` means 1:100.
        leverage: u32,
    },
    /// An account was put in a user's hands.
    ///
    /// # Why ownership is journaled and credentials are not
    ///
    /// Who may trade an account decides what every later order means, so it
    /// belongs in the same ordered history as the orders themselves: replaying
    /// the journal must reproduce not just the balances but who they belong
    /// to.
    ///
    /// A password hash is the opposite. It is rotated, it is reset, and a user
    /// who leaves is entitled to have it erased — none of which an append-only
    /// log can honour. Credentials live in the gateway's user store, where
    /// they can be changed and deleted; only the link between a user and an
    /// account is recorded here, by id.
    AccountOwnerSet {
        /// The account.
        account: AccountId,
        /// Who holds it.
        owner: UserId,
    },
    /// Cleared funds arrived.
    FundsDeposited {
        /// Account credited.
        account: AccountId,
        /// Amount, in the account currency.
        amount: Money,
        /// Who posted it, or [`UserId::SYSTEM`] for the platform itself.
        ///
        /// Money moving into an account is somebody's decision, and an audit
        /// that cannot name them is not an audit. Journaled on the event
        /// rather than alongside it so the two can never come apart.
        by: UserId,
    },
    /// Funds left the account.
    FundsWithdrawn {
        /// Account debited.
        account: AccountId,
        /// Amount, in the account currency.
        amount: Money,
        /// Who posted it. See [`Event::FundsDeposited`].
        by: UserId,
    },
    /// Non-withdrawable bonus was set to an amount.
    ///
    /// Absolute, not a delta: an operator decides what the bonus *is*, and a
    /// delta cannot be read without knowing what it was applied to. Credit
    /// supports margin and never becomes withdrawable cash — see the `ledger`
    /// crate for why those are different things.
    CreditSet {
        /// Account whose bonus changed.
        account: AccountId,
        /// The bonus from now on.
        amount: Money,
        /// Who set it.
        by: UserId,
    },
    /// Money set aside on an account against losses.
    ///
    /// # Why this is not the bonus
    ///
    /// A bonus is the house's money lent to a trader: it counts toward
    /// margin and may be taken back. Insurance is the trader's own, set
    /// aside so it cannot be traded away — it does *not* count toward
    /// margin, and nothing the market does can consume it.
    ///
    /// Absolute like [`Event::CreditSet`], and for the same reason: an
    /// operator decides what the figure *is*, and a delta cannot be read
    /// without knowing what it was applied to.
    InsuranceSet {
        /// Account whose insurance changed.
        account: AccountId,
        /// The insurance from now on.
        amount: Money,
        /// Who set it.
        by: UserId,
    },
    /// Trading on an account was stopped.
    ///
    /// Open positions stay open and keep being valued: suspension refuses new
    /// orders, it does not liquidate. Closing is still allowed, because
    /// trapping a trader in a position is not a sanction anyone intends.
    AccountSuspended {
        /// Account suspended.
        account: AccountId,
        /// Who suspended it.
        by: UserId,
    },
    /// Trading on an account was allowed again.
    AccountReinstated {
        /// Account reinstated.
        account: AccountId,
        /// Who reinstated it.
        by: UserId,
    },
    /// An instrument became tradeable.
    InstrumentListed {
        /// Full contract terms.
        instrument: Instrument,
    },
    /// A new top-of-book quote arrived from the aggregator.
    QuoteTicked {
        /// Instrument quoted.
        instrument: InstrumentId,
        /// Best bid.
        bid: Price,
        /// Best ask.
        ask: Price,
    },
    /// An order passed validation and risk checks.
    OrderAccepted {
        /// Who placed it: the account holder, or an administrator acting on
        /// the account. [`UserId::SYSTEM`] on journals written before the
        /// gateway had sign-ins.
        ///
        /// An order placed on a client's behalf and one they placed
        /// themselves are the same trade with different answers to "who
        /// decided this", and the second answer is the one disputes turn on.
        by: UserId,
        /// Client order identity.
        order: OrderId,
        /// Owning account.
        account: AccountId,
        /// Instrument traded.
        instrument: InstrumentId,
        /// Direction.
        side: Side,
        /// Pricing behaviour.
        kind: OrderType,
        /// Size in base units.
        qty: Qty,
        /// Limit or stop price. Zero for a market order.
        price: Price,
        /// Lifetime.
        tif: TimeInForce,
    },
    /// An order was refused before reaching the book.
    OrderRejected {
        /// The refused order.
        order: OrderId,
        /// Why.
        reason: RejectReason,
    },
    /// A resting order was withdrawn.
    OrderCancelled {
        /// The withdrawn order.
        order: OrderId,
        /// Size that never traded.
        unfilled: Qty,
    },
    /// An order traded, in whole or in part.
    OrderFilled {
        /// The order that traded.
        order: OrderId,
        /// Instrument traded.
        instrument: InstrumentId,
        /// Direction of the aggressing order.
        side: Side,
        /// Size that traded on this fill.
        qty: Qty,
        /// Price it traded at.
        price: Price,
        /// Units of the instrument's quote currency per one unit of the
        /// account's currency, at the moment of the fill. One when they are
        /// the same currency.
        ///
        /// # Why the rate is in the event
        ///
        /// Profit on a close is *recomputed* during replay from the lots and
        /// this fill's price — it is not stored. For a yen-quoted instrument
        /// in a dollar account, the profit also depends on USDJPY at the
        /// moment of the close, and that is not reproducible later: quotes are
        /// journaled on a throttle, so the rate the engine used may never have
        /// been written down. Carrying it here keeps replay exact, which is
        /// invariant P0-2.
        rate: Price,
        /// What the house added to the feed's price to get `price`, per
        /// unit, as a positive number on whichever side the client dealt.
        ///
        /// # Why the markup is in the event
        ///
        /// `price` is what the client got, and that is the only price the
        /// account's money is computed from. But the difference between it
        /// and the price the market was actually showing is the platform's
        /// earnings on the trade, and a broker has to be able to total
        /// that — per day, per symbol, per client — long after the quote
        /// that produced it has gone.
        ///
        /// Quotes are not journaled, so it cannot be recovered later by
        /// looking: the feed's price at that instant was never written
        /// down. Nor can it be derived from the markup *setting*, because
        /// settings change and the old value is not kept. So it is carried
        /// on the fill, where it is a fact about one trade rather than a
        /// reading of configuration that has since moved.
        ///
        /// Zero on a fill from a build that had no markup, and zero
        /// whenever none is charged.
        markup: Price,
        /// Commission taken from the account's balance for this fill, in
        /// the account's currency.
        ///
        /// # Why it is not the same as the markup beside it
        ///
        /// The markup is *inside* `price` — the client paid it by dealing
        /// at a worse number, and it reaches the house when the position
        /// closes. Commission is money leaving the balance the moment the
        /// trade is done, whatever happens to the position afterwards. One
        /// is a price, the other is a payment, and the books post them to
        /// different places.
        ///
        /// Zero on a fill from a build that charged none.
        commission: Money,
    },
    /// Margin level fell below the call threshold.
    MarginCallIssued {
        /// Account at risk.
        account: AccountId,
        /// Margin level in basis points — `10_000` is 100%.
        margin_level_bp: i64,
    },
    /// An order was accepted but rests until a price condition is met.
    ///
    /// A separate variant from [`Event::OrderAccepted`] rather than an extra
    /// field on it: a stop-limit needs two prices, and adding a field to an
    /// existing variant would change the bytes every journal already on disk
    /// was written with. New behaviour takes a new tag.
    PendingOrderPlaced {
        /// Who placed it. See [`Event::OrderAccepted`].
        by: UserId,
        /// Client order identity.
        order: OrderId,
        /// Owning account.
        account: AccountId,
        /// Instrument traded.
        instrument: InstrumentId,
        /// Direction.
        side: Side,
        /// `Limit`, `Stop` or `StopLimit`. A market order never rests.
        kind: OrderType,
        /// Size in base units.
        qty: Qty,
        /// The price that activates it — the limit for a limit order, the
        /// stop for a stop or stop-limit.
        trigger_price: Price,
        /// Where a stop-limit rests once triggered. Zero for the others.
        limit_price: Price,
        /// Lifetime.
        tif: TimeInForce,
    },
    /// A resting order's price condition was met.
    ///
    /// What happens next — a fill, a rejection on margin, or conversion into a
    /// resting limit — is journaled separately, because each is a different
    /// decision the engine took.
    PendingOrderTriggered {
        /// The order that activated.
        order: OrderId,
        /// The market price that activated it.
        price: Price,
    },
    /// Overnight financing accrued on one open position.
    ///
    /// Accrues *on the position* rather than landing in the balance, which is
    /// the MetaTrader convention traders expect: it shows in the Swap column,
    /// moves equity, and is realised into the balance when the position closes.
    SwapCharged {
        /// Account holding the position.
        account: AccountId,
        /// Instrument held.
        instrument: InstrumentId,
        /// The ticket of the position charged.
        order: OrderId,
        /// Signed amount. Negative is a charge, positive a credit.
        amount: Money,
    },
    /// Every position has been charged for one trading day's rollover.
    ///
    /// The marker is what makes rollover idempotent. Without it, a gateway
    /// restarted at 22:05 would find positions open past 22:00 and charge them
    /// a second time.
    RolloverCompleted {
        /// Days since the Unix epoch of the rollover that finished.
        day: u32,
    },
    /// The feed an instrument is priced from changed.
    ///
    /// Recorded so every fill can be attributed to the source that priced it:
    /// the fill carries its price, and the most recent change before it names
    /// where that price came from. A trader disputing a fill is owed that
    /// answer, and "whichever feed was up at the time" is not one.
    PriceSourceChanged {
        /// Instrument re-sourced.
        instrument: InstrumentId,
        /// The feed now pricing it. Numbering is the gateway's, and stable.
        source: u8,
    },
    /// The engine force-closed a position to protect the account.
    PositionStoppedOut {
        /// Account closed out.
        account: AccountId,
        /// Instrument closed.
        instrument: InstrumentId,
        /// The parcel closed.
        ///
        /// A stop-out picks the *largest loser*, which is not necessarily the
        /// oldest parcel. Without the ticket, replay would close the front of
        /// the queue instead and reach a different book from the one that was
        /// live — so this is the ticket that was actually closed.
        /// [`OrderId`]`(0)` on journals from before it was recorded, and those
        /// replay oldest-first, which is what they did at the time.
        order: OrderId,
        /// Size closed.
        qty: Qty,
        /// Price the close executed at.
        price: Price,
        /// Conversion rate used to realise the loss, as on
        /// [`Event::OrderFilled`].
        rate: Price,
    },
    /// A stop loss or take profit was attached to an open position, moved, or
    /// taken off.
    ///
    /// Both prices are absolute, and [`Price::ZERO`] means "none" — the same
    /// convention as a stop-limit's unused limit price. Protection belongs to
    /// a *parcel*, not to an instrument: two lots of the same symbol opened at
    /// different prices are protected at different prices.
    ProtectionSet {
        /// Account holding the position.
        account: AccountId,
        /// The parcel's ticket.
        order: OrderId,
        /// Closes the position when the market reaches it. Zero for none.
        stop_loss: Price,
        /// Takes the profit when the market reaches it. Zero for none.
        take_profit: Price,
        /// Who set it: the holder, or an administrator acting for them.
        by: UserId,
    },
    /// A demo account's thirty days were started again.
    ///
    /// Demo accounts expire so that a practice account cannot quietly become
    /// a permanent one with stale prices and forgotten positions. Renewing is
    /// a decision somebody takes, and it is journaled with their name on it
    /// like every other.
    AccountRenewed {
        /// Account renewed.
        account: AccountId,
        /// Who renewed it.
        by: UserId,
    },
    /// An account was classified as demo or live.
    ///
    /// Every account starts as demo, because an account that has not been
    /// deliberately made live should never be one. The distinction is
    /// journaled rather than configured: it decides whether the money in it
    /// is somebody's, and that is not a question to answer from a settings
    /// file.
    AccountClassified {
        /// Account affected.
        account: AccountId,
        /// True for live, false for demo.
        live: bool,
        /// Who classified it.
        by: UserId,
    },
    /// An account changed between netting and hedging.
    ///
    /// **Netting** is the default and what most brokers offer: an opposing
    /// order reduces or closes what is open, so an account holds at most one
    /// position per instrument. **Hedging** lets both sides be held at once,
    /// as MetaTrader 5 does, and a position is then closed by naming its
    /// ticket rather than by trading against it.
    ///
    /// Journaled because it changes what an order *means*: the same sell, on
    /// the same account, either closes a long or opens a short. Replay has to
    /// know which.
    AccountModeSet {
        /// Account affected.
        account: AccountId,
        /// True for hedging, false for netting.
        hedging: bool,
        /// Who changed it.
        by: UserId,
    },
    /// A position was closed at its holder's request, by ticket.
    ///
    /// Only a hedging account needs this: on a netting account a close *is*
    /// an opposing order, and the fill is the record. Here the ticket says
    /// which of several positions was closed, and nothing has to be inferred
    /// from the order they were opened in.
    PositionClosed {
        /// Account holding it.
        account: AccountId,
        /// Instrument closed.
        instrument: InstrumentId,
        /// The parcel closed.
        order: OrderId,
        /// Size closed — the whole parcel.
        qty: Qty,
        /// Price it closed at.
        price: Price,
        /// Conversion rate used, as on [`Event::OrderFilled`].
        rate: Price,
        /// Who closed it: the holder, or an administrator acting for them.
        by: UserId,
    },
    /// Somebody tried to sign in, or stopped being signed in.
    ///
    /// # Why a sign-in is in the trade journal
    ///
    /// Because the questions asked about it are the questions asked about
    /// trades: who, when, from where, and in what order relative to
    /// everything else. A separate log would have to be correlated with this
    /// one by timestamp, and timestamps from two writers do not order
    /// reliably — the journal sequence does.
    ///
    /// It is deliberately small: a user, an outcome and an address. No tokens,
    /// no password material, nothing that could sign anybody in if the journal
    /// were read by someone who should not have it.
    SessionChanged {
        /// Who, or [`UserId::SYSTEM`] when the login was not even a user.
        user: UserId,
        /// What happened.
        what: SessionEvent,
        /// Where from, as sixteen bytes: an IPv6 address, or an IPv4 one
        /// mapped into IPv6. Zero when the gateway could not tell.
        ///
        /// Personal data, and kept because a trading platform is asked to
        /// know it — a sign-in from a new country is how account takeover is
        /// noticed. It is not shown to other clients.
        address: [u8; 16],
    },
    /// Money was moved onto an account to put a trade right.
    ///
    /// # Why an amendment is an event and not an edit
    ///
    /// The journal is append-only, so a filled trade cannot be rewritten and
    /// this does not try: the original fill stays exactly as it happened and
    /// the correction sits beside it, with the amount, the reason, the ticket
    /// it concerns and the administrator who made it. The client's balance
    /// moves; the client is told; the history shows both.
    ///
    /// Editing a filled trade in place is the single most abusable thing a
    /// B-book can do — a losing trade quietly becoming a winning one, a
    /// stop-out un-happening — and no amount of policy makes a mutable trade
    /// history auditable. An adjustment that is visible, attributed and
    /// permanent can be questioned. A rewritten fill cannot even be noticed.
    TradeAdjusted {
        /// Account credited or debited.
        account: AccountId,
        /// The trade this concerns. [`OrderId`]`(0)` for an adjustment that
        /// is not about a particular trade.
        order: OrderId,
        /// Signed, in the account's currency. Positive pays the client.
        amount: Money,
        /// Why.
        reason: AmendReason,
        /// Who made it.
        by: UserId,
    },
    /// An open position's entry price was corrected.
    ///
    /// Changes what the position will realise from now on; it moves no money
    /// by itself, because nothing has been realised yet. Only an *open*
    /// parcel can be corrected — a closed one is already in the balance, and
    /// putting that right is a [`Event::TradeAdjusted`].
    PositionCorrected {
        /// Account holding it.
        account: AccountId,
        /// The parcel.
        order: OrderId,
        /// What it was opened at, recorded so the change can be read without
        /// replaying the whole journal to find the original fill.
        from: Price,
        /// What it is treated as having opened at.
        to: Price,
        /// Why.
        reason: AmendReason,
        /// Who made it.
        by: UserId,
    },
    /// A stop loss or take profit was reached and the position was closed.
    ///
    /// Distinct from [`Event::PositionStoppedOut`], which is the engine
    /// protecting *itself* from an account that can no longer pay. This is the
    /// engine doing what the trader asked, and a trader reading their history
    /// is owed the difference.
    ProtectionTriggered {
        /// Account holding the position.
        account: AccountId,
        /// Instrument closed.
        instrument: InstrumentId,
        /// The parcel closed.
        order: OrderId,
        /// Size closed — always the whole parcel.
        qty: Qty,
        /// Price it closed at. A stop can slip past its level; that is what a
        /// stop is.
        price: Price,
        /// Conversion rate used, as on [`Event::OrderFilled`].
        rate: Price,
        /// True for a stop loss, false for a take profit.
        stop_loss: bool,
    },
}

impl Event {
    /// True for events that must be on durable storage before anyone is told
    /// they happened.
    ///
    /// That is every event except a quote. A fill, a rejection, a stop-out, a
    /// deposit — each is a decision the engine took, and a trader who has been
    /// told "filled" must never find, after a power cut, that the engine has no
    /// record of the trade. A quote is different: it arrives hundreds of times
    /// a second, the next one supersedes it, and losing the last few after a
    /// crash changes no balance, because every decision that used a price
    /// journaled that price itself.
    ///
    /// Written as an exhaustive match rather than `!matches!(QuoteTicked)` on
    /// purpose. Adding a variant must force someone to decide which side it
    /// falls on, not silently default to durable or not.
    pub const fn must_be_durable(&self) -> bool {
        match self {
            // Attribution rather than a decision. A switch lost in a crash is
            // re-recorded on the first quote after restart, because the
            // aggregator starts with no active source and records whichever it
            // picks.
            Event::QuoteTicked { .. }
            | Event::PriceSourceChanged { .. }
            | Event::SessionChanged { .. } => false,
            Event::AccountOpened { .. }
            | Event::FundsDeposited { .. }
            | Event::FundsWithdrawn { .. }
            | Event::InstrumentListed { .. }
            | Event::AccountOwnerSet { .. }
            | Event::CreditSet { .. }
            | Event::InsuranceSet { .. }
            | Event::AccountSuspended { .. }
            | Event::AccountReinstated { .. }
            | Event::OrderAccepted { .. }
            | Event::OrderRejected { .. }
            | Event::OrderCancelled { .. }
            | Event::OrderFilled { .. }
            | Event::PendingOrderPlaced { .. }
            | Event::PendingOrderTriggered { .. }
            | Event::MarginCallIssued { .. }
            | Event::SwapCharged { .. }
            | Event::RolloverCompleted { .. }
            | Event::PositionStoppedOut { .. }
            | Event::ProtectionSet { .. }
            | Event::ProtectionTriggered { .. }
            // A sign-in is a record, not a decision anybody is waiting on. It
            // rides the next sync like a quote: losing the last few in a crash
            // changes no balance, and blocking a sign-in on an fsync would
            // make the slowest disk the login latency.
            | Event::TradeAdjusted { .. }
            | Event::PositionCorrected { .. }
            | Event::AccountModeSet { .. }
            | Event::AccountClassified { .. }
            | Event::AccountRenewed { .. }
            | Event::PositionClosed { .. } => true,
        }
    }

    /// Stable wire tag. Never reuse or renumber these.
    pub const fn tag(&self) -> u8 {
        match self {
            Event::AccountOpened { .. } => 0x01,
            Event::AccountOwnerSet { .. } => 0x04,
            // `0x02` and `0x03` are the same decisions without an actor on
            // them, from before the gateway knew who was signed in. They
            // still decode, as the platform's own; they are never written.
            Event::FundsDeposited { .. } => 0x05,
            Event::FundsWithdrawn { .. } => 0x06,
            Event::CreditSet { .. } => 0x07,
            Event::InsuranceSet { .. } => 0x50,
            Event::AccountSuspended { .. } => 0x08,
            Event::AccountReinstated { .. } => 0x09,
            Event::InstrumentListed { .. } => 0x10,
            Event::QuoteTicked { .. } => 0x20,
            // `0x30` and `0x34` are the actor-free encodings. As above.
            Event::OrderAccepted { .. } => 0x36,
            Event::OrderRejected { .. } => 0x31,
            Event::OrderCancelled { .. } => 0x32,
            // A fill now carries the conversion rate it used. `0x33` is the
            // same decision without one, written before the engine could
            // trade an instrument quoted in anything but the account's
            // currency — it still decodes, with a rate of one, which is
            // exactly what it was. It is never written again.
            // A fill now carries the markup the house earned on it as well
            // as the conversion rate. `0x46` is the same event without it
            // and still decodes, with a markup of zero.
            // A fill now carries commission as well as the markup and the
            // conversion rate. `0x4E` is the same event without it, and
            // `0x46` without either; both still decode.
            Event::OrderFilled { .. } => 0x4F,
            Event::PendingOrderPlaced { .. } => 0x37,
            Event::PendingOrderTriggered { .. } => 0x35,
            Event::MarginCallIssued { .. } => 0x40,
            // `0x41` is the rate-free encoding and `0x47` the one without a
            // ticket. Both are still read; neither is written.
            Event::PositionStoppedOut { .. } => 0x48,
            Event::ProtectionSet { .. } => 0x49,
            Event::ProtectionTriggered { .. } => 0x4A,
            Event::SessionChanged { .. } => 0x0A,
            Event::AccountModeSet { .. } => 0x0B,
            Event::AccountClassified { .. } => 0x0C,
            Event::AccountRenewed { .. } => 0x0D,
            Event::PositionClosed { .. } => 0x4D,
            Event::TradeAdjusted { .. } => 0x4B,
            Event::PositionCorrected { .. } => 0x4C,
            Event::SwapCharged { .. } => 0x42,
            Event::RolloverCompleted { .. } => 0x43,
            Event::PriceSourceChanged { .. } => 0x44,
        }
    }
}

/// One journaled event with its assigned position and time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    /// Gapless position in the stream.
    pub seq: Seq,
    /// When the engine accepted it.
    pub at: Timestamp,
    /// What happened.
    pub event: Event,
}

// ---------------------------------------------------------------------------
// Wire codec
// ---------------------------------------------------------------------------

/// A frame that could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    /// The frame ended mid-field.
    Truncated,
    /// An event tag this build does not know.
    UnknownTag(u8),
    /// An enum discriminant outside its valid range.
    InvalidDiscriminant,
    /// Bytes remained after decoding a complete record.
    TrailingBytes,
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::Truncated => f.write_str("frame truncated"),
            CodecError::UnknownTag(t) => write!(f, "unknown event tag 0x{t:02X}"),
            CodecError::InvalidDiscriminant => f.write_str("invalid enum discriminant"),
            CodecError::TrailingBytes => f.write_str("trailing bytes after record"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Little-endian append-only byte writer.
#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    /// A writer over an empty buffer.
    pub fn new() -> Self {
        Writer { buf: Vec::new() }
    }

    /// Consumes the writer, yielding the encoded bytes.
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }

    /// Bytes written so far.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// True when nothing has been written.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn i128(&mut self, v: i128) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes16(&mut self, v: [u8; 16]) {
        self.buf.extend_from_slice(&v);
    }
    fn price(&mut self, v: Price) {
        self.i64(v.raw());
    }
    fn qty(&mut self, v: Qty) {
        self.i64(v.raw());
    }
    fn money(&mut self, v: Money) {
        self.i128(v.raw());
    }
    fn side(&mut self, v: Side) {
        self.u8(match v {
            Side::Buy => 0,
            Side::Sell => 1,
        });
    }
    fn currency(&mut self, v: Currency) {
        self.u32(v.raw());
    }
}

/// Little-endian byte reader that never panics on short input.
#[derive(Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// A reader over `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    /// Bytes not yet consumed.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        if self.remaining() < n {
            return Err(CodecError::Truncated);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, CodecError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32, CodecError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Result<u64, CodecError> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }
    fn i64(&mut self) -> Result<i64, CodecError> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(i64::from_le_bytes(a))
    }
    fn i128(&mut self) -> Result<i128, CodecError> {
        let b = self.take(16)?;
        let mut a = [0u8; 16];
        a.copy_from_slice(b);
        Ok(i128::from_le_bytes(a))
    }
    fn bytes16(&mut self) -> Result<[u8; 16], CodecError> {
        let b = self.take(16)?;
        let mut a = [0u8; 16];
        a.copy_from_slice(b);
        Ok(a)
    }
    fn price(&mut self) -> Result<Price, CodecError> {
        Ok(Price::from_raw(self.i64()?))
    }
    fn qty(&mut self) -> Result<Qty, CodecError> {
        Ok(Qty::from_raw(self.i64()?))
    }
    fn money(&mut self) -> Result<Money, CodecError> {
        Ok(Money::from_raw(self.i128()?))
    }
    fn side(&mut self) -> Result<Side, CodecError> {
        match self.u8()? {
            0 => Ok(Side::Buy),
            1 => Ok(Side::Sell),
            _ => Err(CodecError::InvalidDiscriminant),
        }
    }
    fn currency(&mut self) -> Result<Currency, CodecError> {
        Ok(Currency::from_raw(self.u32()?))
    }
}

const fn order_type_tag(t: OrderType) -> u8 {
    match t {
        OrderType::Market => 0,
        OrderType::Limit => 1,
        OrderType::Stop => 2,
        OrderType::StopLimit => 3,
    }
}

fn order_type_from(tag: u8) -> Result<OrderType, CodecError> {
    match tag {
        0 => Ok(OrderType::Market),
        1 => Ok(OrderType::Limit),
        2 => Ok(OrderType::Stop),
        3 => Ok(OrderType::StopLimit),
        _ => Err(CodecError::InvalidDiscriminant),
    }
}

const fn tif_tag(t: TimeInForce) -> u8 {
    match t {
        TimeInForce::GoodTilCancelled => 0,
        TimeInForce::ImmediateOrCancel => 1,
        TimeInForce::FillOrKill => 2,
        TimeInForce::Day => 3,
    }
}

fn tif_from(tag: u8) -> Result<TimeInForce, CodecError> {
    match tag {
        0 => Ok(TimeInForce::GoodTilCancelled),
        1 => Ok(TimeInForce::ImmediateOrCancel),
        2 => Ok(TimeInForce::FillOrKill),
        3 => Ok(TimeInForce::Day),
        _ => Err(CodecError::InvalidDiscriminant),
    }
}

const fn session_tag(e: SessionEvent) -> u8 {
    match e {
        SessionEvent::Opened => 0,
        SessionEvent::Resumed => 1,
        SessionEvent::Refused => 2,
        SessionEvent::Closed => 3,
    }
}

fn session_from(tag: u8) -> Result<SessionEvent, CodecError> {
    match tag {
        0 => Ok(SessionEvent::Opened),
        1 => Ok(SessionEvent::Resumed),
        2 => Ok(SessionEvent::Refused),
        3 => Ok(SessionEvent::Closed),
        _ => Err(CodecError::InvalidDiscriminant),
    }
}

const fn amend_tag(r: AmendReason) -> u8 {
    match r {
        AmendReason::BadPrice => 0,
        AmendReason::DealingError => 1,
        AmendReason::FeeReturned => 2,
        AmendReason::Goodwill => 3,
        AmendReason::Other => 4,
    }
}

fn amend_from(tag: u8) -> Result<AmendReason, CodecError> {
    match tag {
        0 => Ok(AmendReason::BadPrice),
        1 => Ok(AmendReason::DealingError),
        2 => Ok(AmendReason::FeeReturned),
        3 => Ok(AmendReason::Goodwill),
        4 => Ok(AmendReason::Other),
        _ => Err(CodecError::InvalidDiscriminant),
    }
}

const fn reject_tag(r: RejectReason) -> u8 {
    match r {
        RejectReason::InsufficientMargin => 0,
        RejectReason::UnknownInstrument => 1,
        RejectReason::MarketClosed => 2,
        RejectReason::InvalidQuantity => 3,
        RejectReason::InvalidPrice => 4,
        RejectReason::CouldNotFillInFull => 5,
        RejectReason::AccountNotTradeable => 6,
        RejectReason::DuplicateOrderId => 7,
        RejectReason::NoConversionRate => 8,
        RejectReason::DemoExpired => 9,
    }
}

fn reject_from(tag: u8) -> Result<RejectReason, CodecError> {
    match tag {
        0 => Ok(RejectReason::InsufficientMargin),
        1 => Ok(RejectReason::UnknownInstrument),
        2 => Ok(RejectReason::MarketClosed),
        3 => Ok(RejectReason::InvalidQuantity),
        4 => Ok(RejectReason::InvalidPrice),
        5 => Ok(RejectReason::CouldNotFillInFull),
        6 => Ok(RejectReason::AccountNotTradeable),
        7 => Ok(RejectReason::DuplicateOrderId),
        8 => Ok(RejectReason::NoConversionRate),
        9 => Ok(RejectReason::DemoExpired),
        _ => Err(CodecError::InvalidDiscriminant),
    }
}

/// Encodes a record into a self-describing frame body.
///
/// The frame header — length and checksum — is the journal's business, not
/// the domain's.
pub fn encode(record: &Record) -> Vec<u8> {
    let mut w = Writer::new();
    w.u16(WIRE_VERSION);
    w.u64(record.seq.0);
    w.i64(record.at.0);
    w.u8(record.event.tag());

    match record.event {
        Event::AccountOpened {
            account,
            currency,
            leverage,
        } => {
            w.u64(account.0);
            w.currency(currency);
            w.u32(leverage);
        }
        Event::AccountOwnerSet { account, owner } => {
            w.u64(account.0);
            w.u64(owner.0);
        }
        Event::FundsDeposited {
            account,
            amount,
            by,
        }
        | Event::FundsWithdrawn {
            account,
            amount,
            by,
        }
        | Event::InsuranceSet {
            account,
            amount,
            by,
        }
        | Event::CreditSet {
            account,
            amount,
            by,
        } => {
            w.u64(account.0);
            w.money(amount);
            w.u64(by.0);
        }
        Event::AccountSuspended { account, by } | Event::AccountReinstated { account, by } => {
            w.u64(account.0);
            w.u64(by.0);
        }
        Event::InstrumentListed { instrument } => {
            w.u32(instrument.id.0);
            w.bytes16(instrument.symbol.raw());
            w.currency(instrument.base);
            w.currency(instrument.quote);
            w.u8(instrument.display_digits);
            w.qty(instrument.contract_size);
            w.price(instrument.tick_size);
        }
        Event::QuoteTicked {
            instrument,
            bid,
            ask,
        } => {
            w.u32(instrument.0);
            w.price(bid);
            w.price(ask);
        }
        Event::OrderAccepted {
            by,
            order,
            account,
            instrument,
            side,
            kind,
            qty,
            price,
            tif,
        } => {
            w.u64(by.0);
            w.u64(order.0);
            w.u64(account.0);
            w.u32(instrument.0);
            w.side(side);
            w.u8(order_type_tag(kind));
            w.qty(qty);
            w.price(price);
            w.u8(tif_tag(tif));
        }
        Event::OrderRejected { order, reason } => {
            w.u64(order.0);
            w.u8(reject_tag(reason));
        }
        Event::OrderCancelled { order, unfilled } => {
            w.u64(order.0);
            w.qty(unfilled);
        }
        Event::OrderFilled {
            order,
            instrument,
            side,
            qty,
            price,
            rate,
            markup,
            commission,
        } => {
            w.u64(order.0);
            w.u32(instrument.0);
            w.side(side);
            w.qty(qty);
            w.price(price);
            w.price(rate);
            w.price(markup);
            w.money(commission);
        }
        Event::PendingOrderPlaced {
            by,
            order,
            account,
            instrument,
            side,
            kind,
            qty,
            trigger_price,
            limit_price,
            tif,
        } => {
            w.u64(by.0);
            w.u64(order.0);
            w.u64(account.0);
            w.u32(instrument.0);
            w.side(side);
            w.u8(order_type_tag(kind));
            w.qty(qty);
            w.price(trigger_price);
            w.price(limit_price);
            w.u8(tif_tag(tif));
        }
        Event::PendingOrderTriggered { order, price } => {
            w.u64(order.0);
            w.price(price);
        }
        Event::MarginCallIssued {
            account,
            margin_level_bp,
        } => {
            w.u64(account.0);
            w.i64(margin_level_bp);
        }
        Event::SwapCharged {
            account,
            instrument,
            order,
            amount,
        } => {
            w.u64(account.0);
            w.u32(instrument.0);
            w.u64(order.0);
            w.money(amount);
        }
        Event::RolloverCompleted { day } => {
            w.u32(day);
        }
        Event::PriceSourceChanged { instrument, source } => {
            w.u32(instrument.0);
            w.u8(source);
        }
        Event::PositionStoppedOut {
            account,
            instrument,
            order,
            qty,
            price,
            rate,
        } => {
            w.u64(account.0);
            w.u32(instrument.0);
            w.u64(order.0);
            w.qty(qty);
            w.price(price);
            w.price(rate);
        }
        Event::AccountModeSet {
            account,
            hedging,
            by,
        } => {
            w.u64(account.0);
            w.u8(u8::from(hedging));
            w.u64(by.0);
        }
        Event::AccountClassified { account, live, by } => {
            w.u64(account.0);
            w.u8(u8::from(live));
            w.u64(by.0);
        }
        Event::AccountRenewed { account, by } => {
            w.u64(account.0);
            w.u64(by.0);
        }
        Event::PositionClosed {
            account,
            instrument,
            order,
            qty,
            price,
            rate,
            by,
        } => {
            w.u64(account.0);
            w.u32(instrument.0);
            w.u64(order.0);
            w.qty(qty);
            w.price(price);
            w.price(rate);
            w.u64(by.0);
        }
        Event::SessionChanged {
            user,
            what,
            address,
        } => {
            w.u64(user.0);
            w.u8(session_tag(what));
            w.bytes16(address);
        }
        Event::TradeAdjusted {
            account,
            order,
            amount,
            reason,
            by,
        } => {
            w.u64(account.0);
            w.u64(order.0);
            w.money(amount);
            w.u8(amend_tag(reason));
            w.u64(by.0);
        }
        Event::PositionCorrected {
            account,
            order,
            from,
            to,
            reason,
            by,
        } => {
            w.u64(account.0);
            w.u64(order.0);
            w.price(from);
            w.price(to);
            w.u8(amend_tag(reason));
            w.u64(by.0);
        }
        Event::ProtectionSet {
            account,
            order,
            stop_loss,
            take_profit,
            by,
        } => {
            w.u64(account.0);
            w.u64(order.0);
            w.price(stop_loss);
            w.price(take_profit);
            w.u64(by.0);
        }
        Event::ProtectionTriggered {
            account,
            instrument,
            order,
            qty,
            price,
            rate,
            stop_loss,
        } => {
            w.u64(account.0);
            w.u32(instrument.0);
            w.u64(order.0);
            w.qty(qty);
            w.price(price);
            w.price(rate);
            w.u8(u8::from(stop_loss));
        }
    }
    w.finish()
}

/// Decodes a frame body produced by [`encode`].
pub fn decode(bytes: &[u8]) -> Result<Record, CodecError> {
    let mut r = Reader::new(bytes);
    let version = r.u16()?;
    if version != WIRE_VERSION {
        return Err(CodecError::InvalidDiscriminant);
    }
    let seq = Seq(r.u64()?);
    let at = Timestamp(r.i64()?);
    let tag = r.u8()?;

    let event = match tag {
        0x01 => Event::AccountOpened {
            account: AccountId(r.u64()?),
            currency: r.currency()?,
            leverage: r.u32()?,
        },
        0x04 => Event::AccountOwnerSet {
            account: AccountId(r.u64()?),
            owner: UserId(r.u64()?),
        },
        // Legacy: funds with nobody's name on them.
        0x02 => Event::FundsDeposited {
            account: AccountId(r.u64()?),
            amount: r.money()?,
            by: UserId::SYSTEM,
        },
        0x03 => Event::FundsWithdrawn {
            account: AccountId(r.u64()?),
            amount: r.money()?,
            by: UserId::SYSTEM,
        },
        0x05 => Event::FundsDeposited {
            account: AccountId(r.u64()?),
            amount: r.money()?,
            by: UserId(r.u64()?),
        },
        0x06 => Event::FundsWithdrawn {
            account: AccountId(r.u64()?),
            amount: r.money()?,
            by: UserId(r.u64()?),
        },
        0x07 => Event::CreditSet {
            account: AccountId(r.u64()?),
            amount: r.money()?,
            by: UserId(r.u64()?),
        },
        0x08 => Event::AccountSuspended {
            account: AccountId(r.u64()?),
            by: UserId(r.u64()?),
        },
        0x09 => Event::AccountReinstated {
            account: AccountId(r.u64()?),
            by: UserId(r.u64()?),
        },
        0x10 => Event::InstrumentListed {
            instrument: Instrument {
                id: InstrumentId(r.u32()?),
                symbol: Symbol::from_raw(r.bytes16()?),
                base: r.currency()?,
                quote: r.currency()?,
                display_digits: r.u8()?,
                contract_size: r.qty()?,
                tick_size: r.price()?,
            },
        },
        0x20 => Event::QuoteTicked {
            instrument: InstrumentId(r.u32()?),
            bid: r.price()?,
            ask: r.price()?,
        },
        // Legacy: an order with no actor. It was the account holder's.
        0x30 => Event::OrderAccepted {
            by: UserId::SYSTEM,
            order: OrderId(r.u64()?),
            account: AccountId(r.u64()?),
            instrument: InstrumentId(r.u32()?),
            side: r.side()?,
            kind: order_type_from(r.u8()?)?,
            qty: r.qty()?,
            price: r.price()?,
            tif: tif_from(r.u8()?)?,
        },
        0x31 => Event::OrderRejected {
            order: OrderId(r.u64()?),
            reason: reject_from(r.u8()?)?,
        },
        0x32 => Event::OrderCancelled {
            order: OrderId(r.u64()?),
            unfilled: r.qty()?,
        },
        // Legacy: a fill from before conversion existed. Every instrument the
        // engine could trade then was quoted in the account's currency, so a
        // rate of one is not a default — it is what was used.
        0x33 => Event::OrderFilled {
            order: OrderId(r.u64()?),
            instrument: InstrumentId(r.u32()?),
            side: r.side()?,
            qty: r.qty()?,
            price: r.price()?,
            rate: Price::ONE,
            markup: Price::ZERO,
            commission: Money::ZERO,
        },
        // A fill with a conversion rate but no markup: the shape before
        // the house charged one.
        0x46 => Event::OrderFilled {
            order: OrderId(r.u64()?),
            instrument: InstrumentId(r.u32()?),
            side: r.side()?,
            qty: r.qty()?,
            price: r.price()?,
            rate: r.price()?,
            markup: Price::ZERO,
            commission: Money::ZERO,
        },
        // A fill with a markup but no commission: the shape before the
        // house charged one.
        0x4E => Event::OrderFilled {
            order: OrderId(r.u64()?),
            instrument: InstrumentId(r.u32()?),
            side: r.side()?,
            qty: r.qty()?,
            price: r.price()?,
            rate: r.price()?,
            markup: r.price()?,
            commission: Money::ZERO,
        },
        0x50 => Event::InsuranceSet {
            account: AccountId(r.u64()?),
            amount: r.money()?,
            by: UserId(r.u64()?),
        },
        0x4F => Event::OrderFilled {
            order: OrderId(r.u64()?),
            instrument: InstrumentId(r.u32()?),
            side: r.side()?,
            qty: r.qty()?,
            price: r.price()?,
            rate: r.price()?,
            markup: r.price()?,
            commission: r.money()?,
        },
        0x34 => Event::PendingOrderPlaced {
            by: UserId::SYSTEM,
            order: OrderId(r.u64()?),
            account: AccountId(r.u64()?),
            instrument: InstrumentId(r.u32()?),
            side: r.side()?,
            kind: order_type_from(r.u8()?)?,
            qty: r.qty()?,
            trigger_price: r.price()?,
            limit_price: r.price()?,
            tif: tif_from(r.u8()?)?,
        },
        0x36 => Event::OrderAccepted {
            by: UserId(r.u64()?),
            order: OrderId(r.u64()?),
            account: AccountId(r.u64()?),
            instrument: InstrumentId(r.u32()?),
            side: r.side()?,
            kind: order_type_from(r.u8()?)?,
            qty: r.qty()?,
            price: r.price()?,
            tif: tif_from(r.u8()?)?,
        },
        0x37 => Event::PendingOrderPlaced {
            by: UserId(r.u64()?),
            order: OrderId(r.u64()?),
            account: AccountId(r.u64()?),
            instrument: InstrumentId(r.u32()?),
            side: r.side()?,
            kind: order_type_from(r.u8()?)?,
            qty: r.qty()?,
            trigger_price: r.price()?,
            limit_price: r.price()?,
            tif: tif_from(r.u8()?)?,
        },
        0x35 => Event::PendingOrderTriggered {
            order: OrderId(r.u64()?),
            price: r.price()?,
        },
        0x40 => Event::MarginCallIssued {
            account: AccountId(r.u64()?),
            margin_level_bp: r.i64()?,
        },
        0x42 => Event::SwapCharged {
            account: AccountId(r.u64()?),
            instrument: InstrumentId(r.u32()?),
            order: OrderId(r.u64()?),
            amount: r.money()?,
        },
        0x43 => Event::RolloverCompleted { day: r.u32()? },
        0x44 => Event::PriceSourceChanged {
            instrument: InstrumentId(r.u32()?),
            source: r.u8()?,
        },
        // Legacy, as `0x33`: no rate, and no ticket either.
        0x41 => Event::PositionStoppedOut {
            account: AccountId(r.u64()?),
            instrument: InstrumentId(r.u32()?),
            order: OrderId(0),
            qty: r.qty()?,
            price: r.price()?,
            rate: Price::ONE,
        },
        0x47 => Event::PositionStoppedOut {
            account: AccountId(r.u64()?),
            instrument: InstrumentId(r.u32()?),
            order: OrderId(0),
            qty: r.qty()?,
            price: r.price()?,
            rate: r.price()?,
        },
        0x48 => Event::PositionStoppedOut {
            account: AccountId(r.u64()?),
            instrument: InstrumentId(r.u32()?),
            order: OrderId(r.u64()?),
            qty: r.qty()?,
            price: r.price()?,
            rate: r.price()?,
        },
        0x0D => Event::AccountRenewed {
            account: AccountId(r.u64()?),
            by: UserId(r.u64()?),
        },
        0x0C => Event::AccountClassified {
            account: AccountId(r.u64()?),
            live: r.u8()? != 0,
            by: UserId(r.u64()?),
        },
        0x0B => Event::AccountModeSet {
            account: AccountId(r.u64()?),
            hedging: r.u8()? != 0,
            by: UserId(r.u64()?),
        },
        0x4D => Event::PositionClosed {
            account: AccountId(r.u64()?),
            instrument: InstrumentId(r.u32()?),
            order: OrderId(r.u64()?),
            qty: r.qty()?,
            price: r.price()?,
            rate: r.price()?,
            by: UserId(r.u64()?),
        },
        0x0A => Event::SessionChanged {
            user: UserId(r.u64()?),
            what: session_from(r.u8()?)?,
            address: r.bytes16()?,
        },
        0x4B => Event::TradeAdjusted {
            account: AccountId(r.u64()?),
            order: OrderId(r.u64()?),
            amount: r.money()?,
            reason: amend_from(r.u8()?)?,
            by: UserId(r.u64()?),
        },
        0x4C => Event::PositionCorrected {
            account: AccountId(r.u64()?),
            order: OrderId(r.u64()?),
            from: r.price()?,
            to: r.price()?,
            reason: amend_from(r.u8()?)?,
            by: UserId(r.u64()?),
        },
        0x49 => Event::ProtectionSet {
            account: AccountId(r.u64()?),
            order: OrderId(r.u64()?),
            stop_loss: r.price()?,
            take_profit: r.price()?,
            by: UserId(r.u64()?),
        },
        0x4A => Event::ProtectionTriggered {
            account: AccountId(r.u64()?),
            instrument: InstrumentId(r.u32()?),
            order: OrderId(r.u64()?),
            qty: r.qty()?,
            price: r.price()?,
            rate: r.price()?,
            stop_loss: r.u8()? != 0,
        },
        other => return Err(CodecError::UnknownTag(other)),
    };

    if r.remaining() != 0 {
        return Err(CodecError::TrailingBytes);
    }
    Ok(Record { seq, at, event })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eurusd() -> Instrument {
        Instrument {
            id: InstrumentId(1),
            symbol: Symbol::new("EURUSD"),
            base: Currency::EUR,
            quote: Currency::USD,
            display_digits: 5,
            contract_size: Qty::parse("100000").unwrap(),
            tick_size: Price::parse("0.00001").unwrap(),
        }
    }

    fn sample_events() -> Vec<Event> {
        vec![
            Event::AccountOpened {
                account: AccountId(1001),
                currency: Currency::USD,
                leverage: 100,
            },
            Event::FundsDeposited {
                account: AccountId(1001),
                amount: money::Money::parse("25000.00").unwrap(),
                by: UserId::SYSTEM,
            },
            Event::FundsWithdrawn {
                account: AccountId(1001),
                amount: money::Money::parse("500.00").unwrap(),
                by: UserId::SYSTEM,
            },
            Event::InstrumentListed {
                instrument: eurusd(),
            },
            Event::QuoteTicked {
                instrument: InstrumentId(1),
                bid: Price::parse("1.08530").unwrap(),
                ask: Price::parse("1.08532").unwrap(),
            },
            Event::OrderAccepted {
                order: OrderId(7),
                account: AccountId(1001),
                instrument: InstrumentId(1),
                side: Side::Buy,
                kind: OrderType::Limit,
                qty: Qty::parse("100000").unwrap(),
                price: Price::parse("1.08500").unwrap(),
                tif: TimeInForce::GoodTilCancelled,
                by: UserId::SYSTEM,
            },
            Event::AccountOwnerSet {
                account: AccountId(1001),
                owner: UserId(7),
            },
            Event::OrderRejected {
                order: OrderId(8),
                reason: RejectReason::InsufficientMargin,
            },
            Event::OrderCancelled {
                order: OrderId(7),
                unfilled: Qty::parse("40000").unwrap(),
            },
            Event::OrderFilled {
                order: OrderId(7),
                instrument: InstrumentId(1),
                side: Side::Buy,
                qty: Qty::parse("60000").unwrap(),
                price: Price::parse("1.08500").unwrap(),
                rate: Price::ONE,
                markup: Price::ZERO,
                commission: Money::ZERO,
            },
            Event::PendingOrderPlaced {
                order: OrderId(9),
                account: AccountId(1001),
                instrument: InstrumentId(1),
                side: Side::Buy,
                kind: OrderType::StopLimit,
                qty: Qty::parse("50000").unwrap(),
                trigger_price: Price::parse("1.09000").unwrap(),
                limit_price: Price::parse("1.09050").unwrap(),
                tif: TimeInForce::GoodTilCancelled,
                by: UserId::SYSTEM,
            },
            Event::PendingOrderTriggered {
                order: OrderId(9),
                price: Price::parse("1.09000").unwrap(),
            },
            Event::MarginCallIssued {
                account: AccountId(1001),
                margin_level_bp: 7_500,
            },
            Event::PositionStoppedOut {
                account: AccountId(1001),
                instrument: InstrumentId(1),
                order: OrderId(7),
                qty: Qty::parse("60000").unwrap(),
                price: Price::parse("1.08110").unwrap(),
                rate: Price::parse("147.504").unwrap(),
            },
            Event::AccountRenewed {
                account: AccountId(1001),
                by: UserId(1),
            },
            Event::AccountClassified {
                account: AccountId(1001),
                live: true,
                by: UserId(1),
            },
            Event::AccountModeSet {
                account: AccountId(1001),
                hedging: true,
                by: UserId(1),
            },
            Event::PositionClosed {
                account: AccountId(1001),
                instrument: InstrumentId(1),
                order: OrderId(7),
                qty: Qty::parse("60000").unwrap(),
                price: Price::parse("1.08500").unwrap(),
                rate: Price::ONE,
                by: UserId(2),
            },
            Event::SessionChanged {
                user: UserId(2),
                what: SessionEvent::Opened,
                address: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF, 127, 0, 0, 1],
            },
            Event::TradeAdjusted {
                account: AccountId(1001),
                order: OrderId(7),
                amount: Money::parse("12.50").unwrap(),
                reason: AmendReason::BadPrice,
                by: UserId(1),
            },
            Event::PositionCorrected {
                account: AccountId(1001),
                order: OrderId(7),
                from: Price::parse("1.08500").unwrap(),
                to: Price::parse("1.08450").unwrap(),
                reason: AmendReason::DealingError,
                by: UserId(1),
            },
            Event::ProtectionSet {
                account: AccountId(1001),
                order: OrderId(7),
                stop_loss: Price::parse("1.07500").unwrap(),
                take_profit: Price::parse("1.09500").unwrap(),
                by: UserId(2),
            },
            Event::ProtectionTriggered {
                account: AccountId(1001),
                instrument: InstrumentId(1),
                order: OrderId(7),
                qty: Qty::parse("60000").unwrap(),
                price: Price::parse("1.07490").unwrap(),
                rate: Price::ONE,
                stop_loss: true,
            },
            Event::SwapCharged {
                account: AccountId(1001),
                instrument: InstrumentId(1),
                order: OrderId(7),
                amount: Money::parse("-4.26").unwrap(),
            },
            Event::RolloverCompleted { day: 20_700 },
            Event::PriceSourceChanged {
                instrument: InstrumentId(20),
                source: 3,
            },
        ]
    }

    #[test]
    fn every_event_survives_a_round_trip() {
        for (i, event) in sample_events().into_iter().enumerate() {
            let record = Record {
                seq: Seq(i as u64),
                at: Timestamp(1_725_000_000_000_000_000 + i as i64),
                event,
            };
            let bytes = encode(&record);
            let back = decode(&bytes).expect("decodes");
            assert_eq!(record, back, "round trip failed for {event:?}");
        }
    }

    #[test]
    fn only_quotes_attribution_and_sign_ins_may_wait_for_the_next_sync() {
        for event in sample_events() {
            // A quote is superseded by the next one; a source change is
            // re-recorded on the first quote after a restart; a sign-in is a
            // record nobody is waiting on. Everything else is a decision
            // somebody was told about, and must be on disk before they were.
            let expected = !matches!(
                event,
                Event::QuoteTicked { .. }
                    | Event::PriceSourceChanged { .. }
                    | Event::SessionChanged { .. }
            );
            assert_eq!(
                event.must_be_durable(),
                expected,
                "{event:?} is on the wrong side of the durability line"
            );
        }
        // And the samples really do include both sides, or the loop proves
        // nothing.
        let events = sample_events();
        assert!(events.iter().any(|e| !e.must_be_durable()));
        assert!(events.iter().any(Event::must_be_durable));
    }

    #[test]
    fn every_event_tag_is_distinct() {
        let mut tags: Vec<u8> = sample_events().iter().map(Event::tag).collect();
        let before = tags.len();
        tags.sort_unstable();
        tags.dedup();
        assert_eq!(tags.len(), before, "two events share a wire tag");
    }

    #[test]
    fn encoding_is_byte_stable() {
        // A regulator reproducing a 2029 trading day must get these exact
        // bytes. If this assertion ever changes, WIRE_VERSION must change too.
        let record = Record {
            seq: Seq(42),
            at: Timestamp(1_725_000_000_000_000_000),
            event: Event::OrderFilled {
                order: OrderId(7),
                instrument: InstrumentId(1),
                side: Side::Buy,
                qty: Qty::parse("60000").unwrap(),
                price: Price::parse("1.08500").unwrap(),
                rate: Price::ONE,
                markup: Price::ZERO,
                commission: Money::ZERO,
            },
        };
        let bytes = encode(&record);
        // ... + price + rate + markup + commission.
        // ... + price + rate + markup, then commission, which is a Money
        // and so crosses as sixteen bytes where a Price takes eight.
        assert_eq!(bytes.len(), 2 + 8 + 8 + 1 + 8 + 4 + 1 + 8 + 8 + 8 + 8 + 16);
        assert_eq!(&bytes[..2], &WIRE_VERSION.to_le_bytes());
        assert_eq!(bytes[18], 0x4F);
    }

    #[test]
    fn a_fill_written_before_conversion_existed_still_replays() {
        // Tag 0x33 is a fill with no rate on it. The engine no longer writes
        // one, and a journal full of them must still fold to the same state —
        // everything it could trade was quoted in the account's currency, so
        // the rate it used was one.
        let legacy = Event::OrderFilled {
            order: OrderId(7),
            instrument: InstrumentId(1),
            side: Side::Buy,
            qty: Qty::parse("60000").unwrap(),
            price: Price::parse("1.08500").unwrap(),
            rate: Price::ONE,
            markup: Price::ZERO,
            commission: Money::ZERO,
        };
        let mut bytes = encode(&Record {
            seq: Seq(42),
            at: Timestamp(0),
            event: legacy,
        });
        // Rewrite it as the oldest tag would have: 0x33, with none of the
        // rate, the markup or the commission on the end.
        bytes[18] = 0x33;
        bytes.truncate(bytes.len() - 32);
        assert_eq!(decode(&bytes).expect("legacy fill decodes").event, legacy);

        // A stop-out from before either the rate or the ticket was recorded.
        // Both come back as what they were: a rate of one, and no ticket,
        // which replay reads as "close oldest-first, as it did then".
        let legacy_stop = Event::PositionStoppedOut {
            account: AccountId(1001),
            instrument: InstrumentId(1),
            order: OrderId(0),
            qty: Qty::parse("60000").unwrap(),
            price: Price::parse("1.08110").unwrap(),
            rate: Price::ONE,
        };
        let mut bytes = encode(&Record {
            seq: Seq(43),
            at: Timestamp(0),
            event: legacy_stop,
        });
        // Drop the ticket (8) and the rate (8), and stamp the oldest tag.
        bytes[18] = 0x41;
        let ticket_at = 19 + 8 + 4;
        bytes.drain(ticket_at..ticket_at + 8);
        bytes.truncate(bytes.len() - 8);
        assert_eq!(
            decode(&bytes).expect("legacy stop-out decodes").event,
            legacy_stop
        );

        // And the encoding in between: a rate, but still no ticket.
        let mut bytes = encode(&Record {
            seq: Seq(44),
            at: Timestamp(0),
            event: Event::PositionStoppedOut {
                account: AccountId(1001),
                instrument: InstrumentId(1),
                order: OrderId(0),
                qty: Qty::parse("60000").unwrap(),
                price: Price::parse("1.08110").unwrap(),
                rate: Price::parse("147.504").unwrap(),
            },
        });
        bytes[18] = 0x47;
        bytes.drain(ticket_at..ticket_at + 8);
        let Event::PositionStoppedOut { order, rate, .. } =
            decode(&bytes).expect("0x47 decodes").event
        else {
            panic!("expected a stop-out");
        };
        assert_eq!(order, OrderId(0));
        assert_eq!(rate, Price::parse("147.504").unwrap());
    }

    #[test]
    fn a_fill_written_before_markup_existed_still_replays() {
        // Tag 0x46 is a fill with a conversion rate but no markup: the
        // shape before the house charged one. A journal full of them must
        // fold to the same state, with nothing earned on the spread —
        // because nothing was.
        let before = Event::OrderFilled {
            order: OrderId(7),
            instrument: InstrumentId(1),
            side: Side::Buy,
            qty: Qty::parse("60000").unwrap(),
            price: Price::parse("1.08500").unwrap(),
            rate: Price::parse("147.500").unwrap(),
            markup: Price::ZERO,
            commission: Money::ZERO,
        };
        let mut bytes = encode(&Record {
            seq: Seq(9),
            at: Timestamp(0),
            event: before,
        });
        // Drop the markup and the commission from the end.
        bytes[18] = 0x46;
        bytes.truncate(bytes.len() - 24);
        assert_eq!(
            decode(&bytes).expect("a fill without markup decodes").event,
            before
        );
    }

    #[test]
    fn a_fill_written_before_commission_existed_still_replays() {
        // Tag 0x4E is a fill with a markup but no commission: the shape
        // before the house charged one. It must fold to the same state,
        // with nothing taken from the balance — because nothing was.
        let before = Event::OrderFilled {
            order: OrderId(7),
            instrument: InstrumentId(1),
            side: Side::Buy,
            qty: Qty::parse("60000").unwrap(),
            price: Price::parse("1.08500").unwrap(),
            rate: Price::ONE,
            markup: Price::parse("0.00002").unwrap(),
            commission: Money::ZERO,
        };
        let mut bytes = encode(&Record {
            seq: Seq(11),
            at: Timestamp(0),
            event: before,
        });
        bytes[18] = 0x4E;
        bytes.truncate(bytes.len() - 16);
        assert_eq!(
            decode(&bytes)
                .expect("a fill without commission decodes")
                .event,
            before
        );
    }

    #[test]
    fn a_commission_survives_the_wire_to_the_last_place() {
        // It left somebody's balance, so it has to come back exactly.
        let charged = Money::parse("3.070000007").unwrap();
        let record = Record {
            seq: Seq(2),
            at: Timestamp(0),
            event: Event::OrderFilled {
                order: OrderId(1),
                instrument: InstrumentId(1),
                side: Side::Buy,
                qty: Qty::parse("1000").unwrap(),
                price: Price::parse("1.08500").unwrap(),
                rate: Price::ONE,
                markup: Price::ZERO,
                commission: charged,
            },
        };
        let back = decode(&encode(&record)).expect("a charged fill decodes");
        let Event::OrderFilled { commission, .. } = back.event else {
            panic!("expected a fill");
        };
        assert_eq!(commission, charged);
    }

    #[test]
    fn a_markup_survives_the_wire_to_the_last_place() {
        // What the house earned is read back off the journal to total it,
        // so it has to cross with every digit intact — the same reason the
        // conversion rate does.
        let markup = Price::parse("0.000120007").unwrap();
        let record = Record {
            seq: Seq(1),
            at: Timestamp(0),
            event: Event::OrderFilled {
                order: OrderId(1),
                instrument: InstrumentId(30),
                side: Side::Sell,
                qty: Qty::parse("1000").unwrap(),
                price: Price::parse("147.500").unwrap(),
                rate: Price::ONE,
                markup,
                commission: Money::ZERO,
            },
        };
        let back = decode(&encode(&record)).expect("a marked-up fill decodes");
        let Event::OrderFilled { markup: got, .. } = back.event else {
            panic!("expected a fill");
        };
        assert_eq!(got, markup);
    }

    #[test]
    fn a_conversion_rate_survives_the_wire_to_the_last_place() {
        // The rate is what makes a replayed yen P&L exact, so it has to cross
        // with every digit intact.
        let rate = Price::parse("147.504999999").unwrap();
        let record = Record {
            seq: Seq(1),
            at: Timestamp(0),
            event: Event::OrderFilled {
                order: OrderId(1),
                instrument: InstrumentId(30),
                side: Side::Sell,
                qty: Qty::parse("1000").unwrap(),
                price: Price::parse("147.500").unwrap(),
                rate,
                markup: Price::ZERO,
                commission: Money::ZERO,
            },
        };
        let Event::OrderFilled { rate: back, .. } = decode(&encode(&record)).unwrap().event else {
            panic!("expected a fill");
        };
        assert_eq!(back, rate);
    }

    #[test]
    fn a_truncated_frame_is_an_error_not_a_panic() {
        let record = Record {
            seq: Seq(1),
            at: Timestamp(0),
            event: Event::MarginCallIssued {
                account: AccountId(1),
                margin_level_bp: 5_000,
            },
        };
        let bytes = encode(&record);
        for cut in 0..bytes.len() {
            assert!(
                decode(&bytes[..cut]).is_err(),
                "expected error at cut {cut}"
            );
        }
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let record = Record {
            seq: Seq(1),
            at: Timestamp(0),
            event: Event::OrderCancelled {
                order: OrderId(3),
                unfilled: Qty::ZERO,
            },
        };
        let mut bytes = encode(&record);
        bytes.push(0xFF);
        assert_eq!(decode(&bytes), Err(CodecError::TrailingBytes));
    }

    #[test]
    fn an_unknown_tag_is_reported_with_its_value() {
        let mut bytes = encode(&Record {
            seq: Seq(1),
            at: Timestamp(0),
            event: Event::OrderCancelled {
                order: OrderId(3),
                unfilled: Qty::ZERO,
            },
        });
        bytes[18] = 0x7E;
        assert_eq!(decode(&bytes), Err(CodecError::UnknownTag(0x7E)));
    }

    #[test]
    fn symbols_pad_and_trim_cleanly() {
        assert_eq!(Symbol::new("EURUSD").as_str(), "EURUSD");
        assert_eq!(Symbol::new("").as_str(), "");
        assert_eq!(Symbol::new("XAUUSD").raw()[6], b' ');
        let long = Symbol::new("ABCDEFGHIJKLMNOPQRSTUV");
        assert_eq!(long.as_str().len(), 16);
    }

    #[test]
    fn currencies_pack_and_unpack() {
        assert_eq!(Currency::USD.to_string(), "USD");
        assert_eq!(Currency::JPY.code(), *b"JPY");
        assert_eq!(Currency::from_raw(Currency::EUR.raw()), Currency::EUR);
        assert_ne!(Currency::EUR, Currency::USD);
    }
}
