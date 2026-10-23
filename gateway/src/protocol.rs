//! The wire format between the browser and the gateway.
//!
//! # Every money value crosses as a decimal string
//!
//! Not as a JSON number. `serde_json` would happily serialise an `i128` raw
//! value, and the browser's `JSON.parse` would happily turn it into a double —
//! silently, and only wrongly at magnitudes nobody tests. Sending
//! `"78389.03000000"` means the browser's `price()` parses it into a `bigint`
//! by exactly the same rules the engine used to produce it.
//!
//! This is the same discipline the exchange feeds happen to follow, and it is
//! not a coincidence: it is the only way two languages agree about money.

use domain::{AccountId, InstrumentId, OrderId, OrderType, RejectReason, Side};
use money::{Money, Price, Qty};
use serde::{Deserialize, Serialize};

/// Renders a price the way the browser expects to parse it.
pub fn price_str(p: Price) -> String {
    p.to_decimal_string()
}

/// Renders a quantity.
pub fn qty_str(q: Qty) -> String {
    q.to_decimal_string()
}

/// Renders money.
pub fn money_str(m: Money) -> String {
    m.to_decimal_string()
}

/// Lowercase names matching the TypeScript unions in `web/src/lib/domain.ts`.
pub fn side_str(s: Side) -> &'static str {
    match s {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

/// Parses the browser's side.
pub fn side_from(s: &str) -> Option<Side> {
    match s {
        "buy" => Some(Side::Buy),
        "sell" => Some(Side::Sell),
        _ => None,
    }
}

/// Parses the browser's order type.
pub fn kind_from(s: &str) -> Option<OrderType> {
    match s {
        "market" => Some(OrderType::Market),
        "limit" => Some(OrderType::Limit),
        "stop" => Some(OrderType::Stop),
        "stopLimit" => Some(OrderType::StopLimit),
        _ => None,
    }
}

/// Order-type names matching the TypeScript union.
pub fn kind_str(k: OrderType) -> &'static str {
    match k {
        OrderType::Market => "market",
        OrderType::Limit => "limit",
        OrderType::Stop => "stop",
        OrderType::StopLimit => "stopLimit",
    }
}

/// The `RejectReason` variant names the terminal already has copy for.
pub fn reason_str(r: RejectReason) -> &'static str {
    match r {
        RejectReason::InsufficientMargin => "insufficientMargin",
        RejectReason::UnknownInstrument => "unknownInstrument",
        RejectReason::MarketClosed => "marketClosed",
        RejectReason::InvalidQuantity => "invalidQuantity",
        RejectReason::InvalidPrice => "invalidPrice",
        RejectReason::CouldNotFillInFull => "couldNotFillInFull",
        RejectReason::AccountNotTradeable => "accountNotTradeable",
        RejectReason::DuplicateOrderId => "duplicateOrderId",
        RejectReason::NoConversionRate => "noConversionRate",
        RejectReason::DemoExpired => "demoExpired",
    }
}

// ── Browser to gateway ────────────────────────────────────────────

/// What a terminal can ask the gateway to do.
///
/// Everything but [`Login`](Self::Login) and [`Resume`](Self::Resume) needs a
/// signed-in session; the gateway drops the rest until there is one, rather
/// than answering them.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ClientMessage {
    /// Open another account for the signed-in user.
    ///
    /// A client may have several — a demo to practise on, a live one, and
    /// further ones to keep strategies apart. A live one needs identity
    /// checks approved first.
    #[serde(rename_all = "camelCase")]
    OpenAccount {
        /// True for live, false for a demo account.
        #[serde(default)]
        live: bool,
    },
    /// Ask the desk to verify the signed-in client's identity.
    RequestVerification,
    /// Administrator: move a client's identity checks along. `kyc` is
    /// `submitted`, `approved`, `rejected` or `none`.
    #[serde(rename_all = "camelCase")]
    AdminSetKyc { user: u64, kyc: String },
    /// Administrator: put a client in a manager's book, or take them out of
    /// one. A null manager unassigns.
    #[serde(rename_all = "camelCase")]
    AdminAssignManager { user: u64, manager: Option<u64> },
    /// Administrator: promote a login to manager, or back to client.
    #[serde(rename_all = "camelCase")]
    AdminSetRole { user: u64, role: String },
    /// How a client may pay money in. Clients see the ones switched on;
    /// the desk sees all of them.
    PaymentMethods,
    /// Client: "I have sent you this much, by this method."
    ///
    /// A claim, not a payment. Nothing is credited until somebody at the
    /// desk sees the money and deposits it.
    #[serde(rename_all = "camelCase")]
    DepositRequested {
        method: u64,
        amount: String,
        #[serde(default)]
        reference: String,
    },
    /// Client: "Please send me this much, to one of my saved places."
    ///
    /// Also a request, not a payment: the money leaves when the desk has
    /// sent it and says so.
    #[serde(rename_all = "camelCase")]
    WithdrawalRequested {
        /// One of the client's own payout methods.
        payout: u64,
        amount: String,
        #[serde(default)]
        note: String,
    },
    /// Where this client's money may be sent.
    PayoutMethods,
    /// Client: save somewhere to be paid. A null id adds a new one.
    #[serde(rename_all = "camelCase")]
    SavePayoutMethod {
        #[serde(default)]
        id: Option<u64>,
        rail: String,
        label: String,
        address: String,
        #[serde(default)]
        note: String,
        #[serde(default)]
        preferred: bool,
    },
    /// Client: forget one nothing has been sent to.
    #[serde(rename_all = "camelCase")]
    ForgetPayoutMethod { id: u64 },
    /// Administrator: move money on an account without the client asking —
    /// a bonus granted, insurance set aside, either taken back.
    ///
    /// It joins the same queue so that every movement is approved the same
    /// way and appears in one place.
    #[serde(rename_all = "camelCase")]
    AdminRaise {
        kind: String,
        account: u64,
        amount: String,
        #[serde(default)]
        note: String,
    },
    /// Administrator: add a way to pay, or change one. A null id adds.
    #[serde(rename_all = "camelCase")]
    AdminPutPaymentMethod {
        #[serde(default)]
        id: Option<u64>,
        rail: String,
        label: String,
        address: String,
        #[serde(default)]
        note: String,
        enabled: bool,
    },
    /// Administrator: remove one nobody has paid to.
    #[serde(rename_all = "camelCase")]
    AdminRemovePaymentMethod { id: u64 },
    /// Desk: the deposit claims waiting to be checked.
    AdminDepositRequests,
    /// What the house adds to each feed price, per instrument.
    Markups,
    /// Administrator: set one. Zero removes it.
    #[serde(rename_all = "camelCase")]
    AdminSetMarkup { instrument: u32, markup: String },
    /// Administrator: what opening one unit costs. Zero removes it.
    #[serde(rename_all = "camelCase")]
    AdminSetCommission { instrument: u32, commission: String },
    /// Desk: approve or refuse any request in the queue.
    ///
    /// Named for what it is now that the queue holds more than deposits:
    /// a decision on a request, whatever kind it is.
    #[serde(rename_all = "camelCase")]
    AdminSettle {
        id: u64,
        approve: bool,
        #[serde(default)]
        detail: String,
    },
    /// Desk: "I have looked, and the money is there" — or it is not.
    ///
    /// Crediting is a separate, journaled deposit. This only closes the
    /// claim, so the two can never disagree about who authorised what.
    #[serde(rename_all = "camelCase")]
    AdminSettleDeposit {
        id: u64,
        credit: bool,
        #[serde(default)]
        detail: String,
    },
    /// Desk: open another account for a client who already has one.
    ///
    /// The same thing the client could do themselves, done for them —
    /// which is how a desk opens a sub-account over the phone.
    #[serde(rename_all = "camelCase")]
    AdminOpenAccount { user: u64, live: bool },
    /// Sign in with a password.
    #[serde(rename_all = "camelCase")]
    Login { login: String, password: String },
    /// Sign in with a token from an earlier `Login` on this gateway.
    #[serde(rename_all = "camelCase")]
    Resume { token: String },
    /// Act on another account from now on — one of the session's own for a
    /// client, every account on the book for an administrator.
    #[serde(rename_all = "camelCase")]
    SelectAccount { account: u64 },
    /// Send the full state. Sent once on connect.
    Hello,
    /// Every movement on the account this session is acting for.
    #[serde(rename_all = "camelCase")]
    Statement {
        #[serde(default)]
        limit: usize,
    },
    /// Bars for a chart, from what the gateway has seen.
    #[serde(rename_all = "camelCase")]
    Candles {
        instrument: u32,
        /// `1m`, `5m`, `15m`, `1h`, `4h` or `1d`.
        interval: String,
        #[serde(default)]
        limit: usize,
    },
    /// Administrator: every account with its figures.
    AdminOverview,
    /// Administrator: the client activity log, newest first.
    #[serde(rename_all = "camelCase")]
    AdminActivity {
        #[serde(default)]
        limit: usize,
    },
    /// Administrator: the books, for one account or for the whole house.
    #[serde(rename_all = "camelCase")]
    AdminBooks {
        /// `0` or absent for every account and the house.
        #[serde(default)]
        account: u64,
    },
    /// Administrator: the most recent attributed actions.
    #[serde(rename_all = "camelCase")]
    AdminAudit {
        /// How many lines, newest first. Capped by the gateway.
        #[serde(default)]
        limit: usize,
    },
    /// Administrator: move money, set a bonus, or stop an account.
    ///
    /// One message for all five because they are one decision with five
    /// shapes. `kind` is `deposit`, `withdraw`, `credit`, `suspend` or
    /// `reinstate`; `amount` is a decimal string, ignored by the last two.
    #[serde(rename_all = "camelCase")]
    AdminAction {
        account: u64,
        kind: String,
        #[serde(default)]
        amount: String,
    },
    /// Administrator: put a trade right.
    ///
    /// `kind` is `adjust` — money onto or off the account, with the ticket it
    /// concerns — or `correctPrice`, which changes what an *open* position is
    /// treated as having opened at. `reason` is one of `badPrice`,
    /// `dealingError`, `feeReturned`, `goodwill`, `other`.
    #[serde(rename_all = "camelCase")]
    AdminAmend {
        account: u64,
        ticket: u64,
        kind: String,
        reason: String,
        /// Signed decimal string for `adjust`; the new price for
        /// `correctPrice`.
        #[serde(default)]
        value: String,
    },
    /// Administrator: create a client login.
    #[serde(rename_all = "camelCase")]
    AdminCreateUser { login: String, password: String },
    /// Administrator: set someone's password, their own included.
    #[serde(rename_all = "camelCase")]
    AdminSetPassword { user: u64, password: String },
    /// Place an order.
    #[serde(rename_all = "camelCase")]
    PlaceOrder {
        instrument: u32,
        side: String,
        kind: String,
        /// Decimal string, in base units.
        qty: String,
        /// Decimal string. Ignored for a market order.
        #[serde(default)]
        price: String,
        /// Where a stop-limit rests once its stop is reached. Empty means
        /// "at the stop", which is no slippage tolerance at all.
        #[serde(default)]
        limit_price: String,
    },
    /// Attach, move or remove a position's stop loss and take profit.
    ///
    /// Both are absolute prices, and an empty string means none. Sending one
    /// and omitting the other removes the omitted one: the message says what
    /// the protection *is*, not what to change about it, so a terminal can
    /// never half-apply it.
    #[serde(rename_all = "camelCase")]
    SetProtection {
        ticket: u64,
        #[serde(default)]
        stop_loss: String,
        #[serde(default)]
        take_profit: String,
    },
    /// Close an open position in full.
    #[serde(rename_all = "camelCase")]
    ClosePosition { ticket: u64 },
    /// Withdraw a resting order.
    #[serde(rename_all = "camelCase")]
    CancelOrder { ticket: u64 },
}

// ── Gateway to browser ────────────────────────────────────────────

/// An instrument as the terminal lists it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstrumentDto {
    pub id: u32,
    pub symbol: String,
    pub base: String,
    pub quote: String,
    pub display_digits: u8,
    pub contract_size: String,
    pub tick_size: String,
    pub min_qty: String,
    pub max_qty: String,
    pub qty_step: String,
    /// False when orders are refused, for the reason in `note`.
    pub tradeable: bool,
    pub note: Option<String>,
}

/// Account figures. Every one computed by the engine, not the browser.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountDto {
    pub id: u64,
    pub currency: String,
    pub leverage: u32,
    pub balance: String,
    pub credit: String,
    pub equity: String,
    pub used_margin: String,
    pub free_margin: String,
    /// Basis points, or null when nothing is open.
    pub margin_level_bp: Option<i64>,
    /// True when both sides of an instrument may be held at once. The ticket
    /// shows it, because it changes what a sell on a long position does.
    pub hedging: bool,
    /// True when the money in it is somebody's. The terminal says so, because
    /// a trader should never be unsure which account they are in.
    pub live: bool,
    /// When this demo account expires, in epoch milliseconds. Absent when it
    /// is live and so does not.
    pub demo_expires_at: Option<i64>,
}

/// An account as it appears in a picker, rather than as the one being traded.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSummaryDto {
    pub id: u64,
    pub currency: String,
    pub balance: String,
    pub equity: String,
    /// The user who holds it, when the journal records one.
    pub owner: Option<u64>,
    /// True for live, false for demo — how the client tree is grouped.
    pub live: bool,
    /// When a demo account expires, in epoch milliseconds.
    pub demo_expires_at: Option<i64>,
}

/// One account as the administrator's book lists it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminAccountDto {
    pub id: u64,
    /// The user who holds it, if the journal records one.
    pub owner: Option<u64>,
    /// Filled in by the session, which knows logins; the engine does not.
    pub owner_login: Option<String>,
    pub currency: String,
    pub leverage: u32,
    pub balance: String,
    pub credit: String,
    pub equity: String,
    pub used_margin: String,
    pub margin_level_bp: Option<i64>,
    /// How many open parcels and resting orders it has.
    pub positions: usize,
    pub pending: usize,
    /// False when the account is suspended.
    pub tradeable: bool,
    /// True when both sides of an instrument may be held at once.
    pub hedging: bool,
    /// True when the money in it is somebody's. Demo until said otherwise.
    pub live: bool,
    /// When a demo account stops being able to open positions, in epoch
    /// milliseconds. Absent for a live account.
    pub demo_expires_at: Option<i64>,
}

/// One login, as the administrator's people list shows it.
///
/// The login *is* the email address: one person, one address, one set of
/// accounts under it.
///
/// No hash, no salt and no token: nothing here could be used to sign in as
/// anybody, because a screen does not need that to do its job.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminUserDto {
    pub id: u64,
    pub login: String,
    /// `client`, `manager` or `admin`.
    pub role: &'static str,
    /// `none`, `submitted`, `approved` or `rejected`.
    pub kyc: &'static str,
    /// The manager whose book this client sits in, when one has been
    /// assigned.
    pub manager: Option<u64>,
    /// That manager's login, joined here so the screen need not look it up.
    pub manager_login: Option<String>,
}

/// What the house adds to one instrument's price.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkupDto {
    pub instrument: u32,
    pub symbol: String,
    /// Added to each side, so the spread widens by twice it.
    pub markup: String,
    /// Charged per unit of quantity when a position opens or closes.
    pub commission: String,
    /// The widened spread as it stands, in the instrument's own points, so
    /// the desk can see what a client is actually quoted.
    pub spread: Option<String>,
}

/// A way to pay money in, as the desk has set it up.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentMethodDto {
    pub id: u64,
    /// `crypto`, `upi` or `bank`.
    pub rail: &'static str,
    pub label: String,
    pub address: String,
    pub note: String,
    pub enabled: bool,
}

/// A client's claim that they have paid.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DepositRequestDto {
    pub id: u64,
    /// `deposit`, `withdrawal`, `bonusIn`, `bonusOut`, `insuranceIn` or
    /// `insuranceOut`.
    pub kind: &'static str,
    /// Which pot it moves: `balance`, `bonus` or `insurance`.
    pub pot: &'static str,
    pub account: u64,
    pub user: u64,
    /// Their login, joined by the session.
    pub login: Option<String>,
    pub method: u64,
    /// What that method is called, joined so the screen need not look.
    pub method_label: String,
    pub rail: &'static str,
    /// What the client says they sent. A claim, not an agreed figure.
    pub amount: String,
    pub reference: String,
    /// Epoch milliseconds.
    pub at: i64,
    /// `pending`, `credited` or `refused`.
    pub stage: &'static str,
    pub settled_by: Option<u64>,
    pub detail: String,
}

/// Where one client wants money sent.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PayoutMethodDto {
    pub id: u64,
    pub rail: &'static str,
    pub label: String,
    pub address: String,
    pub note: String,
    pub preferred: bool,
}

/// One line of an account statement: money in, money out, and what it left.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatementDto {
    /// Journal position, which is also the order it happened in.
    pub seq: u64,
    /// Epoch milliseconds.
    pub at: i64,
    /// `deposit`, `withdrawal`, `bonus`, `trade`, `financing`, `stopOut`,
    /// `stopLoss`, `takeProfit` or `adjustment`.
    pub kind: &'static str,
    /// The ticket it concerns, when it concerns one.
    pub ticket: Option<u64>,
    /// Signed, in the account's currency.
    pub amount: String,
    /// The balance after it. What makes a statement a statement.
    pub balance: String,
}

/// One line of the client activity log.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityDto {
    pub seq: u64,
    /// Epoch milliseconds.
    pub at: i64,
    pub user: u64,
    /// Their login, filled in by the session.
    pub login: Option<String>,
    /// `signedIn`, `resumed`, `refused` or `signedOut`.
    pub what: &'static str,
    /// Where from, as text. Empty when the gateway could not tell.
    pub address: String,
}

/// One bar. Prices are mids, as decimal strings like every other price.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BarDto {
    /// Start of the bucket, epoch seconds.
    pub start: i64,
    pub open: String,
    pub high: String,
    pub low: String,
    pub close: String,
}

/// One pot in the double-entry books.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookLineDto {
    /// The trading account, or `0` for the house.
    pub owner: u64,
    pub house: bool,
    /// `ClientCash`, `Bonus`, `RealisedPnl`, `Swap`, `PspClearing` …
    pub book: String,
    pub currency: String,
    pub amount: String,
}

/// What a currency's books add up to. Must be zero.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrialBalanceDto {
    pub currency: String,
    /// Zero when the books balance, which is the only acceptable answer.
    pub residual: String,
}

/// The books, as the administrator reads them.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BooksDto {
    pub lines: Vec<BookLineDto>,
    pub trial: Vec<TrialBalanceDto>,
    /// How many transactions have been posted since the journal began.
    pub transactions: usize,
}

/// One line of the audit trail.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditDto {
    /// Journal position, which is also the order it happened in.
    pub seq: u64,
    /// Epoch milliseconds.
    pub at: i64,
    /// Who did it. `0` is the platform itself.
    pub by: u64,
    /// Their login, filled in by the session.
    pub by_login: Option<String>,
    pub account: u64,
    /// `deposit`, `withdrawal`, `creditSet`, `suspended`, `reinstated`,
    /// `order`, `restingOrder`, `accountOpened`, `ownerSet`.
    pub action: &'static str,
    /// The amount, the ticket, or whatever else the line needs.
    pub detail: String,
}

/// One open position.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionDto {
    pub ticket: u64,
    pub instrument: u32,
    pub side: &'static str,
    pub qty: String,
    pub open_price: String,
    /// Price it would close at now, if the instrument is quoted.
    pub current: Option<String>,
    /// Price profit only. Swap is reported separately so the trader can see
    /// what the market did and what financing cost, rather than one number.
    pub profit: String,
    /// Overnight financing accrued and not yet realised.
    pub swap: String,
    /// The level that closes it at a loss. Absent when there is none.
    pub stop_loss: Option<String>,
    /// The level that takes the profit. Absent when there is none.
    pub take_profit: Option<String>,
}

/// One order resting on the book, waiting for its price.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingDto {
    pub ticket: u64,
    pub instrument: u32,
    pub side: &'static str,
    pub kind: &'static str,
    pub qty: String,
    pub trigger_price: String,
    /// Where a stop-limit rests once triggered. Absent for the others.
    pub limit_price: Option<String>,
}

/// A top-of-book update.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteDto {
    pub instrument: u32,
    pub bid: String,
    pub ask: String,
    /// Which feed priced it: binance, kraken, twelvedata or finnhub.
    pub source: &'static str,
    /// Units of the instrument's quote currency per unit of the account's —
    /// `"1"` when they are the same. The terminal needs it to price a ticket
    /// in the account's currency before the order exists; every figure on a
    /// *filled* position is converted by the engine.
    pub rate: String,
}

/// Everything the gateway pushes.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServerMessage {
    /// Nothing will be answered until this connection signs in.
    ///
    /// Sent as soon as the socket opens, so the terminal shows a login form
    /// rather than an empty platform while it waits for a snapshot that is
    /// never coming.
    AuthRequired,
    /// How far the signed-in client's identity checks have got.
    #[serde(rename_all = "camelCase")]
    Verification {
        /// `none`, `submitted`, `approved` or `rejected`.
        kyc: &'static str,
        /// True when a live account may be opened.
        may_go_live: bool,
    },
    /// Signed in. Carries the token to resume with and what this user may see.
    #[serde(rename_all = "camelCase")]
    Welcome {
        /// Opaque, 256 bits, valid until it expires or the gateway restarts.
        token: String,
        user: u64,
        login: String,
        /// `client` or `admin`.
        role: &'static str,
        /// Accounts this session may act on. One for a client; every account
        /// on the book for an administrator.
        accounts: Vec<AccountSummaryDto>,
        /// The one it is acting on now. A `snapshot` for it follows.
        account: u64,
    },
    /// The credentials or token were not accepted.
    ///
    /// `reason` is `refused` for a wrong login or password — the two are not
    /// distinguished, because telling a stranger which half was right is
    /// telling them which logins exist — `tooManyAttempts` while a login is
    /// waiting out repeated failures, or `expired` for a dead token.
    #[serde(rename_all = "camelCase")]
    AuthFailed {
        reason: &'static str,
        /// Seconds to wait, when that is the answer.
        retry_in_seconds: Option<u64>,
    },
    /// The session asked for something it is not entitled to.
    ///
    /// Distinct from `rejected`: nothing is wrong with the request, it is the
    /// requester who is wrong. A client asking after an account that is not
    /// theirs gets this, and the same answer whether or not that account
    /// exists.
    #[serde(rename_all = "camelCase")]
    Unauthorized { detail: &'static str },
    /// Full state, sent once on connect.
    #[serde(rename_all = "camelCase")]
    Snapshot {
        instruments: Vec<InstrumentDto>,
        account: AccountDto,
        positions: Vec<PositionDto>,
        pending: Vec<PendingDto>,
        quotes: Vec<QuoteDto>,
        /// Journal position, so the terminal can show it.
        seq: u64,
    },
    /// An account statement, newest first.
    #[serde(rename_all = "camelCase")]
    Statement { entries: Vec<StatementDto> },
    /// Bars for one instrument and interval, oldest first.
    ///
    /// Possibly fewer than asked for, or none: the gateway serves what it has
    /// seen, and a fresh journal has seen nothing. The terminal says so
    /// rather than drawing a flat line.
    #[serde(rename_all = "camelCase")]
    Candles {
        instrument: u32,
        interval: String,
        bars: Vec<BarDto>,
    },
    /// A batch of quote changes.
    Quotes { quotes: Vec<QuoteDto> },
    /// Administrator: the book.
    #[serde(rename_all = "camelCase")]
    AdminBook {
        accounts: Vec<AdminAccountDto>,
        /// Logins the administrator can act on, for the new-client form and
        /// for showing who owns what.
        users: Vec<AdminUserDto>,
    },
    /// The ways money may be paid in.
    #[serde(rename_all = "camelCase")]
    PaymentMethods {
        methods: Vec<PaymentMethodDto>,
        /// This client's own claims, newest first. Empty for the desk,
        /// which asks for the queue instead.
        mine: Vec<DepositRequestDto>,
    },
    /// What the house adds to each price.
    #[serde(rename_all = "camelCase")]
    Markups { markups: Vec<MarkupDto> },
    /// Desk: everything waiting, newest first.
    #[serde(rename_all = "camelCase")]
    DepositRequests { requests: Vec<DepositRequestDto> },
    /// Where this client's money may be sent.
    #[serde(rename_all = "camelCase")]
    PayoutMethods { payouts: Vec<PayoutMethodDto> },
    /// Administrator: the audit trail, newest first.
    #[serde(rename_all = "camelCase")]
    AdminAudit { entries: Vec<AuditDto> },
    /// Administrator: who signed in, when, and from where.
    #[serde(rename_all = "camelCase")]
    AdminActivity { entries: Vec<ActivityDto> },
    /// Administrator: the double-entry books and their trial balance.
    #[serde(rename_all = "camelCase")]
    AdminBooks { books: BooksDto },
    /// Administrator: what came of the last action.
    #[serde(rename_all = "camelCase")]
    AdminResult {
        /// True when it went through.
        done: bool,
        /// What happened, or why it did not.
        detail: String,
    },
    /// Account and positions after something changed.
    #[serde(rename_all = "camelCase")]
    Portfolio {
        account: AccountDto,
        positions: Vec<PositionDto>,
        pending: Vec<PendingDto>,
        seq: u64,
    },
    /// An order was accepted and filled.
    #[serde(rename_all = "camelCase")]
    Filled {
        ticket: u64,
        instrument: u32,
        side: &'static str,
        qty: String,
        price: String,
    },
    /// An order was refused. `reason` matches the terminal's `RejectReason`.
    #[serde(rename_all = "camelCase")]
    Rejected { reason: &'static str },
    /// The account crossed the margin call level.
    #[serde(rename_all = "camelCase")]
    MarginCall { level_bp: i64 },
    /// A resting order was accepted.
    #[serde(rename_all = "camelCase")]
    OrderPlaced { ticket: u64 },
    /// A resting order was withdrawn, by the trader or by the engine.
    #[serde(rename_all = "camelCase")]
    OrderCancelled { ticket: u64 },
    /// A resting order's price was reached.
    #[serde(rename_all = "camelCase")]
    OrderTriggered { ticket: u64, price: String },
    /// A trading day's rollover finished.
    #[serde(rename_all = "camelCase")]
    Rollover {
        /// Days since the Unix epoch.
        day: u32,
        /// Net financing charged across the account's positions.
        total: String,
    },
    /// The engine force-closed a position.
    #[serde(rename_all = "camelCase")]
    StoppedOut {
        instrument: u32,
        qty: String,
        price: String,
    },
    /// A stop loss or take profit was reached and the position closed.
    ///
    /// Not a `stoppedOut`: that is the engine protecting itself from an
    /// account that can no longer pay, and this is the engine doing what the
    /// trader asked. A trader reading their notifications is owed the
    /// difference.
    #[serde(rename_all = "camelCase")]
    Protected {
        ticket: u64,
        instrument: u32,
        /// `stopLoss` or `takeProfit`.
        kind: &'static str,
        qty: String,
        price: String,
    },
    /// An administrator put a trade right, and the client is being told.
    ///
    /// Sent to the account it happened to, not only to the administrator who
    /// did it: a correction nobody tells the client about is a correction
    /// nobody can question.
    #[serde(rename_all = "camelCase")]
    Amended {
        /// The trade it concerns, or `0` when it concerns none.
        ticket: u64,
        /// `adjust` or `correctPrice`.
        kind: &'static str,
        /// Money moved, signed, for an adjustment.
        amount: Option<String>,
        /// Before and after, for a price correction.
        from: Option<String>,
        to: Option<String>,
        /// Why, in the words the terminal shows.
        reason: &'static str,
    },
    /// A position's protection was set, moved or removed.
    #[serde(rename_all = "camelCase")]
    ProtectionSet {
        ticket: u64,
        stop_loss: Option<String>,
        take_profit: Option<String>,
    },
}

/// The first account id the engine hands out.
///
/// Four digits because a trader reads it aloud; 1001 rather than 1 because an
/// account number that looks like a row id invites someone to treat it as one.
pub const FIRST_ACCOUNT: AccountId = AccountId(1001);

/// Order ids the gateway hands out start here.
pub const FIRST_ORDER_ID: OrderId = OrderId(100_000);

/// Instrument ids are `u32` on the wire and [`InstrumentId`] inside.
pub fn instrument_id(raw: u32) -> InstrumentId {
    InstrumentId(raw)
}
