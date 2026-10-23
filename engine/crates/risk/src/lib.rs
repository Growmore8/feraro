//! Account state, margin arithmetic, order admission and stop-out.
//!
//! # Invariant P1-1 — no order reaches the book the account cannot afford
//!
//! [`check_order`] runs before matching, on the same thread, against state the
//! same thread owns. That is what makes the check meaningful: two orders cannot
//! both pass against the same free balance, because there is no moment between
//! the check and the fill for a second order to slip through.
//!
//! # Invariant P1-2 — margin required is a difference, not a total
//!
//! The margin an order needs is `margin(after) - margin(before)`, floored at
//! zero. A closing order therefore needs nothing, which is the whole point: an
//! account already past its stop-out level must always be able to *reduce*
//! risk, and a naive "notional / leverage" check would refuse exactly that and
//! trap the trader in the position.
//!
//! # Invariant P1-3 — every figure on an account is in the account's currency
//!
//! An instrument's profit arises in its *quote* currency: a USDJPY position
//! makes and loses yen. An account holds one currency. So every amount that
//! crosses from an instrument to an account — profit, margin, swap — passes
//! through [`Market::rate`], and nothing else is allowed to reach a balance.
//!
//! The rate is the conversion pair's **mid**, and it is read at the moment the
//! amount is computed. For realised amounts that moment matters: the rate used
//! is written into [`Event::OrderFilled`] so replay reproduces the balance
//! exactly rather than re-deriving it from whatever the rate is later. See the
//! note on that event.
//!
//! An instrument whose quote currency has no route to the account's currency,
//! or whose conversion pair is unquoted, cannot be traded —
//! [`RejectReason::NoConversionRate`] — rather than being valued as if the
//! currencies were the same. Yen counted as dollars is a 150× error in the
//! trader's favour or the house's, depending on the sign.
//!
//! # What is deliberately not modelled yet
//!
//! - **Hedged margin relief.** A long and a short in the same instrument each
//!   take full margin here. Real brokers often charge one side. P5.
//! - **Cross-currency routes through a third pair.** A EUR-quoted instrument
//!   in a GBP account would need EURUSD and GBPUSD multiplied together. Each
//!   currency here routes through exactly one pair.
//! - **A conversion spread.** Converting at the mid costs the client nothing.
//!   Brokers usually take a markup; that is a fee, and a fee belongs in the
//!   ledger where it can be seen, not buried in a rate.
//! - **Swap accrual.** Rollover is a scheduled job, not part of admission.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic)]

use std::collections::BTreeMap;
use std::fmt;

use domain::{
    AccountId, Currency, Event, Instrument, InstrumentId, OrderId, OrderType, Record, RejectReason,
    Timestamp, UserId,
};
use money::{ArithError, Money, Price, Qty, Side};

/// Basis points in one hundred per cent.
pub const ONE_HUNDRED_PERCENT_BP: i64 = 10_000;

/// How long a demo account trades before it has to be renewed.
///
/// Thirty days, in nanoseconds. A practice account that never expires becomes
/// a permanent one: stale prices, forgotten positions, and a balance somebody
/// eventually mistakes for real.
pub const DEMO_DAYS: i64 = 30;
const DEMO_LIFETIME_NANOS: i64 = DEMO_DAYS * 24 * 60 * 60 * 1_000_000_000;

/// Something the risk layer could not compute or apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RiskError {
    /// Arithmetic went out of range.
    Arithmetic(ArithError),
    /// An event referred to an account that was never opened.
    UnknownAccount(AccountId),
    /// A fill arrived for an order the portfolio never saw accepted.
    UnknownOrder(OrderId),
    /// An instrument was valued with no quote on the book.
    NoQuote(InstrumentId),
    /// An instrument was referenced before it was listed.
    UnknownInstrument(InstrumentId),
    /// An amount could not be converted into the account's currency: the
    /// currency has no route, or the pair that prices it has no quote.
    NoRate(Currency),
    /// Something was given a price that cannot be one: zero or negative.
    NotAPrice(Price),
    /// An account is held in a currency the market does not settle in.
    ///
    /// Every conversion route targets one settlement currency. An account in
    /// another currency cannot be valued against it, and guessing is how a
    /// book ends up with two definitions of equity.
    CurrencyMismatch {
        /// What the account is held in.
        account: Currency,
        /// What the market converts to.
        settlement: Currency,
    },
}

impl fmt::Display for RiskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RiskError::Arithmetic(e) => write!(f, "risk arithmetic: {e}"),
            RiskError::UnknownAccount(a) => write!(f, "unknown account {a}"),
            RiskError::UnknownOrder(o) => write!(f, "fill for unknown order {o}"),
            RiskError::NoQuote(i) => write!(f, "no quote for instrument {i}"),
            RiskError::UnknownInstrument(i) => write!(f, "unknown instrument {i}"),
            RiskError::NoRate(c) => write!(f, "no conversion rate for {c}"),
            RiskError::NotAPrice(p) => write!(f, "{} is not a price", p.to_decimal_string()),
            RiskError::CurrencyMismatch {
                account,
                settlement,
            } => write!(
                f,
                "account held in {account}, market settles in {settlement}"
            ),
        }
    }
}

impl std::error::Error for RiskError {}

impl From<ArithError> for RiskError {
    fn from(e: ArithError) -> Self {
        RiskError::Arithmetic(e)
    }
}

/// Which price a position's margin is valued at.
///
/// Both are in real use. Locking margin at the open price is the MetaTrader
/// convention for FX; marking it to market is more conservative and usual for
/// crypto and CFDs, because a position that doubles in value should tie up
/// twice the margin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MarginBasis {
    /// Value margin at the price the position opened at.
    OpenPrice,
    /// Value margin at the current market price.
    #[default]
    MarketPrice,
}

/// The levels at which the engine warns and then intervenes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarginPolicy {
    /// How margin is valued.
    pub basis: MarginBasis,
    /// Below this margin level, a call is issued. 10,000 bp is 100%.
    pub call_level_bp: i64,
    /// Below this, the engine force-closes positions.
    pub stop_out_level_bp: i64,
    /// What the matched part of a hedged position still ties up, in basis
    /// points of **both its legs' margin**. `0` charges nothing for a hedge;
    /// `10_000` charges it as though it were not hedged at all; the default
    /// `2_500` charges a quarter of the pair, which is half of one leg.
    ///
    /// Not zero by default. A perfectly hedged pair cannot lose on price, but
    /// it is not riskless: both legs cross a spread to close, financing runs
    /// on both, and an instrument can gap out from under the pair while the
    /// account has nothing behind it. Brokers that charge nothing here are
    /// the ones that discover this on a Monday morning.
    pub hedged_bp: i64,
}

impl Default for MarginPolicy {
    fn default() -> Self {
        MarginPolicy {
            basis: MarginBasis::default(),
            call_level_bp: 10_000,
            stop_out_level_bp: 5_000,
            // A quarter of the matched pair — half of one leg. Enough that a
            // hedge is substantially cheaper than two outright positions,
            // which is the honest relief, and not so little that a fully
            // hedged account can hold size it could never close.
            hedged_bp: 2_500,
        }
    }
}

/// Trading limits for one instrument, beyond its contract terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstrumentTerms {
    /// Contract definition from the domain.
    pub instrument: Instrument,
    /// Smallest order the venue accepts.
    pub min_qty: Qty,
    /// Largest single order.
    pub max_qty: Qty,
    /// Order sizes must be a whole multiple of this.
    pub qty_step: Qty,
    /// False when the instrument is closed, close-only or delisted.
    pub tradeable: bool,
}

impl InstrumentTerms {
    /// Terms with permissive defaults, for tests and for listing.
    pub fn new(instrument: Instrument, min_qty: Qty, max_qty: Qty, qty_step: Qty) -> Self {
        InstrumentTerms {
            instrument,
            min_qty,
            max_qty,
            qty_step,
            tradeable: true,
        }
    }
}

/// How an amount in one currency reaches the settlement currency.
///
/// One hop, by design: a currency is priced by exactly one pair, and which
/// side of that pair it sits on decides whether the rate multiplies or divides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conversion {
    /// The pair is quoted *in this currency, per unit of settlement* — USDJPY
    /// for yen in a dollar book. Its price is the rate, used as it stands.
    PerSettlement(InstrumentId),
    /// The pair is quoted *in settlement, per unit of this currency* — EURUSD
    /// for a euro amount in a dollar book. The rate is its reciprocal, so the
    /// stored rate is one over the price.
    InSettlement(InstrumentId),
}

/// Top of book for every instrument the engine values against, and the rates
/// that turn their quote currencies into the settlement currency.
#[derive(Debug, Clone)]
pub struct Market {
    quotes: BTreeMap<InstrumentId, (Price, Price)>,
    /// What the house added to each side of the feed's price, per unit.
    ///
    /// # Why the market holds the client's price, not the feed's
    ///
    /// Everything downstream of here reads one price: what the client is
    /// shown, what they fill at, what triggers their stop, what values
    /// their open positions, and what the stop-out monitor watches. If the
    /// market held the feed's price and the markup were applied only at the
    /// fill, a client would deal at a price they never saw and a stop would
    /// fire at one they did — which is the kind of difference that ends in
    /// a complaint nobody can answer.
    ///
    /// So the widening happens once, on the way in, and this records how
    /// much it was so a fill can journal what the house earned.
    markups: BTreeMap<InstrumentId, Price>,
    /// Commission per unit of quantity, per instrument.
    commissions: BTreeMap<InstrumentId, Money>,
    /// The currency every account here is held in and every rate converts to.
    settlement: Currency,
    /// What each instrument is priced in, from its listing.
    quoted_in: BTreeMap<InstrumentId, Currency>,
    /// How to price each non-settlement currency.
    routes: BTreeMap<Currency, Conversion>,
}

impl Default for Market {
    fn default() -> Self {
        Market {
            quotes: BTreeMap::new(),
            markups: BTreeMap::new(),
            commissions: BTreeMap::new(),
            settlement: Currency::USD,
            quoted_in: BTreeMap::new(),
            routes: BTreeMap::new(),
        }
    }
}

impl Market {
    /// An empty market settling in US dollars.
    pub fn new() -> Self {
        Market::default()
    }

    /// An empty market settling in `settlement`.
    pub fn settling_in(settlement: Currency) -> Self {
        Market {
            settlement,
            ..Market::default()
        }
    }

    /// The currency this market's accounts are held in.
    pub fn settlement(&self) -> Currency {
        self.settlement
    }

    /// Records what an instrument is priced in. Called from its listing.
    pub fn list(&mut self, instrument: &Instrument) {
        self.quoted_in.insert(instrument.id, instrument.quote);
    }

    /// Routes a currency to the pair that prices it.
    ///
    /// Configuration, not state: the route says *which* instrument converts
    /// this currency, and the rate itself comes from that instrument's live
    /// quote. A route to an unquoted instrument is simply a currency that
    /// cannot be converted yet, which is a refusal and never an assumption.
    pub fn route(&mut self, currency: Currency, how: Conversion) {
        self.routes.insert(currency, how);
    }

    /// Units of `currency` per one unit of the settlement currency.
    ///
    /// One when they are the same currency — not as a fallback, but because
    /// that is the rate. `None` when the currency has no route or its pair is
    /// unquoted; the caller must refuse, never substitute.
    pub fn rate_for(&self, currency: Currency) -> Option<Price> {
        if currency == self.settlement {
            return Some(Price::ONE);
        }
        let mid = |instrument: InstrumentId| {
            let (bid, ask) = self.top(instrument)?;
            bid.midpoint(ask).ok().filter(|m| *m > Price::ZERO)
        };
        match self.routes.get(&currency)? {
            Conversion::PerSettlement(pair) => mid(*pair),
            // The pair gives settlement per unit of the currency; the caller
            // wants the other direction, and dividing by that price is how it
            // gets there — see `Money::convert_per`.
            Conversion::InSettlement(pair) => {
                let price = mid(*pair)?;
                Money::from_raw(i128::from(money::PRICE_ONE))
                    .convert_per(price)
                    .ok()
                    .and_then(|m| i64::try_from(m.raw()).ok())
                    .map(Price::from_raw)
                    .filter(|r| *r > Price::ZERO)
            }
        }
    }

    /// The rate for one instrument's quote currency.
    ///
    /// `None` for an instrument that was never listed, which is a programming
    /// error rather than a market condition — valuing a position in an unknown
    /// currency is not something to guess at.
    pub fn rate(&self, instrument: InstrumentId) -> Option<Price> {
        self.rate_for(*self.quoted_in.get(&instrument)?)
    }

    /// The currency an instrument is priced in, if it has been listed.
    pub fn quote_currency(&self, instrument: InstrumentId) -> Option<Currency> {
        self.quoted_in.get(&instrument).copied()
    }

    /// Converts an amount arising on `instrument` into the settlement currency.
    fn settle(&self, instrument: InstrumentId, amount: Money) -> Result<Money, RiskError> {
        // Nothing to convert, and nothing to refuse: zero is zero in every
        // currency. This keeps a flat account valuable while a conversion
        // pair is briefly unquoted.
        if amount.is_zero() {
            return Ok(Money::ZERO);
        }
        let currency = self
            .quote_currency(instrument)
            .ok_or(RiskError::UnknownInstrument(instrument))?;
        let rate = self.rate_for(currency).ok_or(RiskError::NoRate(currency))?;
        Ok(amount.convert_per(rate)?)
    }

    /// Every quote, for a snapshot.
    pub fn export_quotes(&self) -> Vec<(InstrumentId, Price, Price)> {
        self.quotes
            .iter()
            .map(|(i, (bid, ask))| (*i, *bid, *ask))
            .collect()
    }

    /// Records a new top of book, exactly as given.
    pub fn quote(&mut self, instrument: InstrumentId, bid: Price, ask: Price) {
        self.quotes.insert(instrument, (bid, ask));
        self.markups.remove(&instrument);
    }

    /// Records a top of book with the house's markup already applied.
    ///
    /// `markup` is added to each side, so the spread the client sees widens
    /// by twice it. Two things are refused rather than stored:
    ///
    /// * a markup that would drive the bid to zero or below — there is no
    ///   price at which the house may take the whole of a client's side;
    /// * one that overflows.
    ///
    /// Either means a setting was entered wrong, and the honest response is
    /// to quote the feed's price unchanged rather than an invented one.
    ///
    /// Note that no width of markup can cross the book: it moves the bid
    /// down and the ask up, so the two only ever move apart. The bid
    /// reaching zero is the real limit. Returns what was actually charged.
    pub fn quote_marked(
        &mut self,
        instrument: InstrumentId,
        bid: Price,
        ask: Price,
        markup: Price,
    ) -> Price {
        if markup <= Price::ZERO {
            self.quote(instrument, bid, ask);
            return Price::ZERO;
        }
        let (Ok(wide_bid), Ok(wide_ask)) = (bid.sub(markup), ask.add(markup)) else {
            self.quote(instrument, bid, ask);
            return Price::ZERO;
        };
        if wide_bid <= Price::ZERO || wide_bid >= wide_ask {
            self.quote(instrument, bid, ask);
            return Price::ZERO;
        }
        self.quotes.insert(instrument, (wide_bid, wide_ask));
        self.markups.insert(instrument, markup);
        markup
    }

    /// Commission per unit of quantity, in the settlement currency.
    pub fn commission_of(&self, instrument: InstrumentId) -> Money {
        self.commissions
            .get(&instrument)
            .copied()
            .unwrap_or(Money::ZERO)
    }

    /// Sets what this instrument charges to open, per unit of quantity.
    ///
    /// Kept beside the markup, because both arrive with the price and both
    /// are read at the moment of a fill — including a resting order
    /// filling hours later. One place to read means a market order and a
    /// limit order cannot be charged differently by accident.
    pub fn charge(&mut self, instrument: InstrumentId, per_unit: Money) {
        if per_unit <= Money::ZERO {
            self.commissions.remove(&instrument);
        } else {
            self.commissions.insert(instrument, per_unit);
        }
    }

    /// What the house is adding to each side of this instrument, or zero.
    pub fn markup_of(&self, instrument: InstrumentId) -> Price {
        self.markups
            .get(&instrument)
            .copied()
            .unwrap_or(Price::ZERO)
    }

    /// Best bid and ask, if the instrument is quoted.
    pub fn top(&self, instrument: InstrumentId) -> Option<(Price, Price)> {
        self.quotes.get(&instrument).copied()
    }

    /// The price a position of `side` would close at.
    ///
    /// A long closes by selling into the bid; a short closes by buying the ask.
    /// Valuing both at the mid would flatter every open position by half the
    /// spread, which across a book is not a rounding difference.
    pub fn close_price(&self, instrument: InstrumentId, side: Side) -> Option<Price> {
        self.top(instrument).map(|(bid, ask)| match side {
            Side::Buy => bid,
            Side::Sell => ask,
        })
    }

    /// The price an order of `side` would open at.
    pub fn open_price(&self, instrument: InstrumentId, side: Side) -> Option<Price> {
        self.top(instrument).map(|(bid, ask)| match side {
            Side::Buy => ask,
            Side::Sell => bid,
        })
    }
}

/// Money a close moved into the balance, split by where it came from.
///
/// The split is not cosmetic: trading profit and overnight financing are
/// different revenue to a broker and different costs to a trader, and the
/// ledger posts them to different books. Collapsing them would make the
/// financing a broker earned unreadable from its own accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Realised {
    /// Price profit, converted into the account's currency.
    pub profit: Money,
    /// Overnight financing that had accrued on what was closed.
    pub swap: Money,
}

impl Realised {
    /// What actually reached the balance.
    pub fn total(self) -> Result<Money, ArithError> {
        self.profit.add(self.swap)
    }

    fn add(self, other: Realised) -> Result<Realised, ArithError> {
        Ok(Realised {
            profit: self.profit.add(other.profit)?,
            swap: self.swap.add(other.swap)?,
        })
    }

    /// True when nothing moved.
    pub fn is_zero(self) -> bool {
        self.profit.is_zero() && self.swap.is_zero()
    }
}

/// One fill, as [`Account::fill`] takes it.
///
/// Named rather than positional, and not only to quiet a lint: `price` and
/// `rate` are both a [`Price`], sit next to each other, and mean completely
/// different things. A call that swapped them would compile, and every
/// figure downstream of it would be wrong in a way no test name would
/// point at.
#[derive(Debug, Clone, Copy)]
struct Fill {
    instrument: InstrumentId,
    ticket: OrderId,
    side: Side,
    qty: Qty,
    price: Price,
    /// Units of the instrument's quote currency per unit of the account's.
    /// One when they are the same.
    rate: Price,
    commission: Money,
}

/// One parcel of an open position, retained so closes are FIFO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lot {
    /// The order that opened it.
    pub ticket: OrderId,
    /// Direction.
    pub side: Side,
    /// Size still open.
    pub qty: Qty,
    /// Price it opened at.
    pub open_price: Price,
    /// Closes the position when the market reaches it. [`Price::ZERO`] for
    /// none.
    ///
    /// On the parcel rather than the instrument: two lots of the same symbol
    /// opened at different prices are protected at different prices, and
    /// collapsing them would move one trader's stop because another trader —
    /// or the same one — opened a second position.
    pub stop_loss: Price,
    /// Takes the profit when the market reaches it. Zero for none.
    pub take_profit: Price,
    /// Overnight financing accrued and not yet realised, **in the account's
    /// currency**.
    ///
    /// Converted when it was charged, not when the position closes: the charge
    /// is a decision taken on one night at that night's rate, and re-valuing
    /// it later would make a position's financing drift with the exchange rate
    /// after the fact.
    ///
    /// Moves equity while the position is open and lands in the balance only
    /// when it closes — in proportion, if it closes in part.
    pub swap: Money,
}

impl Lot {
    /// The share of accrued swap that belongs to `traded` of this lot.
    ///
    /// Closing the whole lot takes all of it exactly. Closing part takes a
    /// rounded share, and the caller keeps `swap - share` on what remains, so
    /// the two always sum back to the original: rounding moves a nano between
    /// the closed and open parts, it never loses one.
    fn swap_share(&self, traded: Qty) -> Result<Money, ArithError> {
        if traded == self.qty {
            return Ok(self.swap);
        }
        self.swap
            .mul_ratio(i128::from(traded.raw()), i128::from(self.qty.raw()))
    }
}

/// A trading account and everything it holds.
#[derive(Debug, Clone)]
pub struct Account {
    /// Identity.
    pub id: AccountId,
    /// Denomination of every balance here.
    pub currency: Currency,
    /// Ratio denominator — `100` means 1:100.
    pub leverage: u32,
    /// Withdrawable cash.
    pub balance: Money,
    /// Non-withdrawable bonus. Supports margin; see the `ledger` crate.
    pub credit: Money,
    /// The trader's own money, set aside so it cannot be traded away.
    ///
    /// Deliberately *not* part of equity or free margin: the point of it
    /// is that the market cannot reach it. It is still the client's — it
    /// returns to the balance when released — but while it sits here no
    /// position can be opened against it and no stop-out can consume it.
    pub insurance: Money,
    /// False when the account is suspended or read-only.
    pub tradeable: bool,
    /// True when both sides of an instrument may be held at once.
    ///
    /// On a netting account — the default — an opposing order reduces what is
    /// open. On a hedging one it opens a second position, and closing is by
    /// ticket. The two give different answers to the same order, which is why
    /// the mode is journaled rather than configured.
    pub hedging: bool,
    /// When the demo clock started: the account was opened or last renewed.
    ///
    /// Nanoseconds since the epoch, taken from the journaled record rather
    /// than the clock, so a replay dates an account the way it was dated the
    /// first time.
    pub clock_started: Timestamp,
    /// True when the money in it is somebody's.
    ///
    /// Demo until somebody says otherwise. The engine treats both the same —
    /// the arithmetic of a demo account is the arithmetic of a live one, or
    /// the demo is a lie — but the desk needs to know which is which, and so
    /// does anyone reading the books.
    pub live: bool,
    /// Whether a call has already been issued, so it is not raised repeatedly.
    pub called: bool,
    lots: BTreeMap<InstrumentId, Vec<Lot>>,
}

impl Account {
    /// A new account with no positions.
    pub fn new(id: AccountId, currency: Currency, leverage: u32, balance: Money) -> Self {
        Account {
            id,
            currency,
            leverage,
            balance,
            credit: Money::ZERO,
            insurance: Money::ZERO,
            tradeable: true,
            hedging: false,
            clock_started: Timestamp(0),
            live: false,
            called: false,
            lots: BTreeMap::new(),
        }
    }

    /// Open parcels for one instrument, oldest first.
    pub fn lots(&self, instrument: InstrumentId) -> &[Lot] {
        self.lots.get(&instrument).map_or(&[], Vec::as_slice)
    }

    /// Every instrument with something open.
    pub fn open_instruments(&self) -> impl Iterator<Item = InstrumentId> + '_ {
        self.lots
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, _)| *k)
    }

    /// True when nothing is open.
    pub fn is_flat(&self) -> bool {
        self.lots.values().all(Vec::is_empty)
    }

    /// When this demo account stops being able to open positions.
    ///
    /// `None` for a live account, and for a demo one whose clock was never
    /// stamped — a journal from before expiry existed, which is treated as
    /// not expiring rather than as expiring immediately.
    pub fn demo_expires_at(&self) -> Option<Timestamp> {
        if self.live || self.clock_started.0 == 0 {
            return None;
        }
        Some(Timestamp(
            self.clock_started.0.saturating_add(DEMO_LIFETIME_NANOS),
        ))
    }

    /// True when this is a demo account past its thirty days.
    pub fn demo_expired(&self, now: Timestamp) -> bool {
        self.demo_expires_at().is_some_and(|at| now.0 >= at.0)
    }

    /// Fails unless this account is held in the currency the market settles in.
    fn require_settlement(&self, market: &Market) -> Result<(), RiskError> {
        if self.currency == market.settlement() {
            return Ok(());
        }
        Err(RiskError::CurrencyMismatch {
            account: self.currency,
            settlement: market.settlement(),
        })
    }

    /// Unrealised profit across every open lot, including accrued swap.
    ///
    /// In the account's currency: each lot's profit arises in its instrument's
    /// quote currency and is converted on the way in. Accrued swap is already
    /// in the account's currency — it was converted when it was charged.
    pub fn floating(&self, market: &Market) -> Result<Money, RiskError> {
        self.require_settlement(market)?;
        let mut total = Money::ZERO;
        for (instrument, lots) in &self.lots {
            for lot in lots {
                total = total.add(self.lot_value(*instrument, lot, market)?)?;
            }
        }
        Ok(total)
    }

    /// What one lot would realise if it closed now, in the account's currency.
    ///
    /// Profit converted, plus the swap it has run up. This is the figure a
    /// stop-out ranks lots by, so it has to be comparable across instruments
    /// in different quote currencies — the whole reason it converts.
    fn lot_value(
        &self,
        instrument: InstrumentId,
        lot: &Lot,
        market: &Market,
    ) -> Result<Money, RiskError> {
        let close = market
            .close_price(instrument, lot.side)
            .ok_or(RiskError::NoQuote(instrument))?;
        let profit = money::pnl(lot.side, lot.open_price, close, lot.qty)?;
        Ok(market.settle(instrument, profit)?.add(lot.swap)?)
    }

    /// Swap accrued across every open lot and not yet realised.
    pub fn accrued_swap(&self) -> Result<Money, RiskError> {
        let mut total = Money::ZERO;
        for lot in self.lots.values().flatten() {
            total = total.add(lot.swap)?;
        }
        Ok(total)
    }

    /// Accrues a swap charge onto one open lot, in the account's currency.
    ///
    /// The caller converts: rollover computes the charge from notional, which
    /// is in the instrument's quote currency, and the amount journaled in
    /// [`Event::SwapCharged`] is already converted — which is what makes the
    /// charge reproducible on replay.
    ///
    /// An unknown ticket is an error rather than a charge to the balance:
    /// rollover and closing both run on the engine thread, so a charge for a
    /// lot that is not there means the journal and the book disagree.
    pub fn charge_swap(
        &mut self,
        instrument: InstrumentId,
        ticket: OrderId,
        amount: Money,
    ) -> Result<(), RiskError> {
        let lot = self
            .lots
            .get_mut(&instrument)
            .and_then(|lots| lots.iter_mut().find(|l| l.ticket == ticket))
            .ok_or(RiskError::UnknownOrder(ticket))?;
        lot.swap = lot.swap.add(amount)?;
        Ok(())
    }

    /// Attaches, moves or removes a parcel's stop loss and take profit.
    ///
    /// Zero clears. An unknown ticket is an error rather than a silent
    /// success: a trader who has set a stop and been told nothing went wrong
    /// is entitled to believe the stop exists.
    pub fn set_protection(
        &mut self,
        instrument: InstrumentId,
        ticket: OrderId,
        stop_loss: Price,
        take_profit: Price,
    ) -> Result<(), RiskError> {
        let lot = self
            .lots
            .get_mut(&instrument)
            .and_then(|lots| lots.iter_mut().find(|l| l.ticket == ticket))
            .ok_or(RiskError::UnknownOrder(ticket))?;
        lot.stop_loss = stop_loss;
        lot.take_profit = take_profit;
        Ok(())
    }

    /// Treats a parcel as having opened at a different price.
    ///
    /// Moves no money: nothing has been realised, so the only thing that
    /// changes is what this position *will* realise, and what it ties up as
    /// margin in the meantime. A closed parcel cannot be corrected — it is
    /// already in the balance, and putting that right is an adjustment.
    pub fn correct_open_price(
        &mut self,
        instrument: InstrumentId,
        ticket: OrderId,
        price: Price,
    ) -> Result<Price, RiskError> {
        if price <= Price::ZERO {
            return Err(RiskError::NotAPrice(price));
        }
        let lot = self
            .lots
            .get_mut(&instrument)
            .and_then(|lots| lots.iter_mut().find(|l| l.ticket == ticket))
            .ok_or(RiskError::UnknownOrder(ticket))?;
        let was = lot.open_price;
        lot.open_price = price;
        Ok(was)
    }

    /// Credits or debits the balance to put something right.
    ///
    /// Deliberately not a way to set a balance: an adjustment is a movement,
    /// and a movement is what the books record. There is no path here that
    /// makes a balance whatever somebody says it is.
    pub fn adjust(&mut self, amount: Money) -> Result<(), RiskError> {
        self.balance = self.balance.add(amount)?;
        Ok(())
    }

    /// Closes one named parcel in full, realising its profit and swap.
    ///
    /// Distinct from [`fill`](Self::fill), which closes oldest-first because
    /// that is what a client's closing *order* does. A stop-out and a stop
    /// loss both name the parcel they are closing, and closing a different
    /// one would realise a different number.
    pub fn close_lot(
        &mut self,
        instrument: InstrumentId,
        ticket: OrderId,
        qty: Qty,
        price: Price,
        rate: Price,
        commission: Money,
    ) -> Result<Realised, RiskError> {
        let lots = self
            .lots
            .get_mut(&instrument)
            .ok_or(RiskError::UnknownOrder(ticket))?;
        let index = lots
            .iter()
            .position(|l| l.ticket == ticket)
            .ok_or(RiskError::UnknownOrder(ticket))?;
        let lot = lots[index];

        // Zero means the whole parcel, which is what every record written
        // before partial closes existed meant by the size it carried.
        //
        // More than the parcel is clamped rather than refused, and only
        // here: the gateway refuses it, with a reason the trader reads. But
        // a *replay* has nobody to tell, and a journal that says "close
        // 0.2" of a 0.1 parcel has to fold to something — and the only
        // honest something is the parcel that is there.
        let closing = if qty.is_zero() || qty >= lot.qty {
            lot.qty
        } else {
            qty
        };

        let profit = money::pnl(lot.side, lot.open_price, price, closing)?;
        let share = lot.swap_share(closing)?;
        let realised = Realised {
            profit: profit.convert_per(rate)?,
            swap: share,
        };

        if closing == lot.qty {
            lots.remove(index);
        } else {
            // The remainder keeps the ticket, the side and the open price:
            // it is the same position, smaller. A trader who takes half off
            // is still in at the price they got, which is the whole reason
            // to close part rather than all and re-open.
            let rest = &mut lots[index];
            rest.qty = rest.qty.sub(closing)?;
            rest.swap = rest.swap.sub(share)?;
        }

        self.balance = self.balance.add(realised.total()?)?;
        // Deliberately not part of `Realised`: that is what the position
        // made, and a charge for closing it is not profit. It comes off the
        // balance beside the profit, exactly as it does on a fill.
        if !commission.is_zero() {
            self.balance = self.balance.sub(commission)?;
        }
        Ok(realised)
    }

    /// Balance plus credit plus unrealised profit.
    ///
    /// Credit counts here because bonus supports margin. It does not count
    /// toward withdrawable funds — that distinction lives in the `ledger`
    /// crate and the two must not be conflated.
    pub fn equity(&self, market: &Market) -> Result<Money, RiskError> {
        Ok(self.balance.add(self.credit)?.add(self.floating(market)?)?)
    }

    /// Margin currently tied up by open positions, in the account's currency.
    ///
    /// # Hedged positions are charged once, not twice
    ///
    /// A long and a short of the same size in the same instrument cannot both
    /// lose: whatever the market does, one gains what the other gives up. The
    /// pair's remaining risk is the *difference* between the two sides, so
    /// that is what it ties up — plus [`MarginPolicy::hedged_bp`] of the
    /// matched part, which is not zero because a hedge is not free: the two
    /// legs still cross a spread to close, and a weekend gap can move the
    /// instrument out from under both.
    ///
    /// Per instrument, never across them. Long EURUSD against short GBPUSD is
    /// *correlated*, not hedged, and charging it as hedged is how a broker
    /// discovers the difference on the one morning the correlation breaks.
    pub fn used_margin(&self, market: &Market, policy: MarginPolicy) -> Result<Money, RiskError> {
        self.require_settlement(market)?;
        let mut total = Money::ZERO;
        for (instrument, lots) in &self.lots {
            // Each side's margin, at the price the policy values it at.
            let mut long = Money::ZERO;
            let mut short = Money::ZERO;
            for lot in lots {
                let at = match policy.basis {
                    MarginBasis::OpenPrice => lot.open_price,
                    MarginBasis::MarketPrice => market
                        .close_price(*instrument, lot.side)
                        .ok_or(RiskError::NoQuote(*instrument))?,
                };
                // Notional is in the instrument's quote currency, so the
                // margin it implies is too, until it is converted.
                let margin = market.settle(*instrument, margin_for(at, lot.qty, self.leverage)?)?;
                match lot.side {
                    Side::Buy => long = long.add(margin)?,
                    Side::Sell => short = short.add(margin)?,
                }
            }

            // The matched part is both legs of the hedge, so the full charge
            // for it is twice the smaller side. `hedged_bp` is a fraction of
            // *that*, which makes 10,000 bp mean exactly "as if not hedged"
            // and 0 mean free.
            let matched = if long < short { long } else { short };
            let both_legs = matched.scale_by(2)?;
            let exposed = long.add(short)?.sub(both_legs)?;
            let on_hedge = both_legs.mul_ratio(i128::from(policy.hedged_bp), 10_000)?;
            total = total.add(exposed)?.add(on_hedge)?;
        }
        Ok(total)
    }

    /// Equity not tied up as margin.
    pub fn free_margin(&self, market: &Market, policy: MarginPolicy) -> Result<Money, RiskError> {
        Ok(self
            .equity(market)?
            .sub(self.used_margin(market, policy)?)?)
    }

    /// Margin level in basis points, or `None` when no margin is used.
    ///
    /// `None` is not infinity. It means the question does not apply, and every
    /// caller must decide what that means rather than being handed a number
    /// that compares badly against a threshold.
    pub fn margin_level_bp(
        &self,
        market: &Market,
        policy: MarginPolicy,
    ) -> Result<Option<i64>, RiskError> {
        let used = self.used_margin(market, policy)?;
        if used.is_zero() {
            return Ok(None);
        }
        let equity = self.equity(market)?;
        let scaled = equity
            .raw()
            .checked_mul(i128::from(ONE_HUNDRED_PERCENT_BP))
            .ok_or(ArithError::Overflow)?;
        let bp = scaled / used.raw();
        i64::try_from(bp)
            .map(Some)
            .map_err(|_| RiskError::Arithmetic(ArithError::Overflow))
    }

    /// Applies a fill, closing opposing lots FIFO before opening new size.
    ///
    /// Returns the profit realised by whatever it closed, in the account's
    /// currency, which the caller posts to the balance.
    ///
    /// `rate` is units of the instrument's quote currency per unit of the
    /// account's — one when they are the same. It is passed in rather than
    /// read from the market because this runs on replay too, where the rate
    /// must be the one the fill was taken at, not today's. It comes from
    /// [`Event::OrderFilled::rate`].
    /// Applies a fill, and takes the commission it cost.
    ///
    /// `commission` leaves the balance here rather than at the close,
    /// because that is when it is charged: a position opened and closed a
    /// second later still cost it. It is deliberately *not* part of
    /// `Realised` — that is what the position made, and a charge for
    /// opening it is not profit.
    fn fill(&mut self, fill: Fill) -> Result<Realised, RiskError> {
        let Fill {
            instrument,
            ticket,
            side,
            qty,
            price,
            rate,
            commission,
        } = fill;
        // Before any branch. A hedging account returns early a few lines
        // down, and a charge that only some accounts pay is a bug nobody
        // would find until a client asked why.
        if !commission.is_zero() {
            self.balance = self.balance.sub(commission)?;
        }

        let lots = self.lots.entry(instrument).or_default();
        let mut remaining = qty;
        let mut realised = Realised::default();

        // On a hedging account an opposing order is a new position, not a
        // close: the trader asked to be short as well as long, and netting
        // them off would be answering a question they did not ask.
        if self.hedging {
            lots.push(Lot {
                ticket,
                side,
                qty,
                open_price: price,
                stop_loss: Price::ZERO,
                take_profit: Price::ZERO,
                swap: Money::ZERO,
            });
            return Ok(realised);
        }

        // Close opposing parcels oldest first.
        while !remaining.is_zero() {
            let Some(front) = lots.first().copied() else {
                break;
            };
            if front.side == side {
                break;
            }
            let traded = if front.qty <= remaining {
                front.qty
            } else {
                remaining
            };
            let share = front.swap_share(traded)?;
            let profit = money::pnl(front.side, front.open_price, price, traded)?;
            realised = realised.add(Realised {
                profit: profit.convert_per(rate)?,
                // Swap was converted when it was charged, so it is taken as it
                // is rather than converted twice.
                swap: share,
            })?;
            remaining = remaining.sub(traded)?;

            if front.qty == traded {
                lots.remove(0);
            } else if let Some(first) = lots.first_mut() {
                first.qty = first.qty.sub(traded)?;
                first.swap = first.swap.sub(share)?;
            }
        }

        if !remaining.is_zero() {
            lots.push(Lot {
                ticket,
                side,
                qty: remaining,
                open_price: price,
                stop_loss: Price::ZERO,
                take_profit: Price::ZERO,
                swap: Money::ZERO,
            });
        }
        self.balance = self.balance.add(realised.total()?)?;
        Ok(realised)
    }
}

/// Why a parcel's protection fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtectionHit {
    /// The parcel to close.
    pub ticket: OrderId,
    /// Size — always the whole parcel.
    pub qty: Qty,
    /// Price it closes at: the side the position closes on, not the level the
    /// trader set. A stop can slip past its level in a fast market, and
    /// promising otherwise would mean owing the difference in a gap.
    pub price: Price,
    /// True for a stop loss, false for a take profit.
    pub stop_loss: bool,
}

/// Every parcel on one instrument whose protection this tick reached.
///
/// | Position | Closes on | Stop loss fires | Take profit fires |
/// |---|---|---|---|
/// | Long | the bid | `bid <= stop` | `bid >= target` |
/// | Short | the ask | `ask >= stop` | `ask <= target` |
///
/// Measured against the price the position would *execute* at, never the mid:
/// a long is closed by selling into the bid, so the bid is what its stop
/// watches. Using the mid would fire a stop the market never actually offered.
///
/// A tick that reaches both levels at once — a gap straddling them — fires the
/// stop loss. The alternative is paying out the better side of a move that
/// also blew through the worse one, on a price the account never traded.
pub fn protection_hits(
    account: &Account,
    instrument: InstrumentId,
    bid: Price,
    ask: Price,
) -> Vec<ProtectionHit> {
    let mut out = Vec::new();
    for lot in account.lots(instrument) {
        let (close, stop_hit, target_hit) = match lot.side {
            Side::Buy => (
                bid,
                lot.stop_loss > Price::ZERO && bid <= lot.stop_loss,
                lot.take_profit > Price::ZERO && bid >= lot.take_profit,
            ),
            Side::Sell => (
                ask,
                lot.stop_loss > Price::ZERO && ask >= lot.stop_loss,
                lot.take_profit > Price::ZERO && ask <= lot.take_profit,
            ),
        };
        if stop_hit || target_hit {
            out.push(ProtectionHit {
                ticket: lot.ticket,
                qty: lot.qty,
                price: close,
                stop_loss: stop_hit,
            });
        }
    }
    out
}

/// Whether a stop loss and take profit may be set on a parcel.
///
/// Both must sit on the wrong side of the market to be protection at all: a
/// stop loss *below* a long is a stop loss, and one above it is an order to
/// sell at a loss immediately, which no trader means. Refusing is kinder than
/// firing it on the next tick.
pub fn check_protection(
    side: Side,
    close: Price,
    stop_loss: Price,
    take_profit: Price,
    tick: Price,
) -> Result<(), RejectReason> {
    for level in [stop_loss, take_profit] {
        if level < Price::ZERO {
            return Err(RejectReason::InvalidPrice);
        }
        if level > Price::ZERO && !price_on_tick(level, tick) {
            return Err(RejectReason::InvalidPrice);
        }
    }
    let wrong_side = match side {
        Side::Buy => {
            (stop_loss > Price::ZERO && stop_loss >= close)
                || (take_profit > Price::ZERO && take_profit <= close)
        }
        Side::Sell => {
            (stop_loss > Price::ZERO && stop_loss <= close)
                || (take_profit > Price::ZERO && take_profit >= close)
        }
    };
    if wrong_side {
        return Err(RejectReason::InvalidPrice);
    }
    Ok(())
}

/// Margin one parcel ties up.
pub fn margin_for(price: Price, qty: Qty, leverage: u32) -> Result<Money, ArithError> {
    if leverage == 0 {
        return Err(ArithError::DivideByZero);
    }
    let notional = price.notional(qty)?;
    let raw = notional.raw() / i128::from(leverage);
    Ok(Money::from_raw(raw))
}

/// An order awaiting admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProposedOrder {
    /// Instrument traded.
    pub instrument: InstrumentId,
    /// Direction.
    pub side: Side,
    /// Pricing behaviour.
    pub kind: OrderType,
    /// Size in base units.
    pub qty: Qty,
    /// Limit or stop price. Ignored for a market order.
    pub price: Price,
    /// What opening this would cost in commission, in the account's
    /// currency.
    ///
    /// Part of the proposal rather than looked up, because the margin test
    /// has to include it: an order a trader can afford the margin for but
    /// not the commission would open and leave the balance short, and the
    /// first they would know is a figure that does not add up.
    pub commission: Money,
}

/// Decides whether an order may reach the book.
///
/// The margin test is the *incremental* one described in the module note: an
/// order that reduces exposure needs nothing, so an account deep past stop-out
/// can still close.
pub fn check_order(
    account: &Account,
    terms: &InstrumentTerms,
    market: &Market,
    order: ProposedOrder,
    policy: MarginPolicy,
) -> Result<(), RejectReason> {
    check_order_shape(account, terms, market, order)?;

    let needed = margin_needed(account, market, order, policy)
        .map_err(|_| RejectReason::InsufficientMargin)?;

    // An order that ties up no additional margin is ALWAYS admitted, whatever
    // the free margin is. This early return is the invariant, not an
    // optimisation: free margin is negative on an account past its stop-out
    // level, and `0 > -500` is true, so falling through to the comparison
    // below would refuse the one order that could save the trader.
    if needed.is_zero() {
        return Ok(());
    }

    let free = account
        .free_margin(market, policy)
        .map_err(|_| RejectReason::InsufficientMargin)?;
    if needed > free {
        return Err(RejectReason::InsufficientMargin);
    }
    Ok(())
}

/// Everything about an order that does not depend on the account's money.
///
/// Split out because the two checks happen at different moments for a resting
/// order: its shape is validated when the trader places it, but its margin
/// only when it triggers, possibly days later against a different balance.
/// See the `pending` crate for why margin is not reserved in between.
pub fn check_order_shape(
    account: &Account,
    terms: &InstrumentTerms,
    market: &Market,
    order: ProposedOrder,
) -> Result<(), RejectReason> {
    if !account.tradeable {
        return Err(RejectReason::AccountNotTradeable);
    }
    if terms.instrument.id != order.instrument {
        return Err(RejectReason::UnknownInstrument);
    }
    if !terms.tradeable {
        return Err(RejectReason::MarketClosed);
    }

    if order.qty <= Qty::ZERO
        || order.qty < terms.min_qty
        || order.qty > terms.max_qty
        || !is_multiple_of(order.qty, terms.qty_step)
    {
        return Err(RejectReason::InvalidQuantity);
    }

    let priced = matches!(
        order.kind,
        OrderType::Limit | OrderType::Stop | OrderType::StopLimit
    );
    if priced
        && (order.price <= Price::ZERO || !price_on_tick(order.price, terms.instrument.tick_size))
    {
        return Err(RejectReason::InvalidPrice);
    }

    if market.top(order.instrument).is_none() {
        return Err(RejectReason::MarketClosed);
    }

    // The instrument is quoted and tradeable, but if its quote currency
    // cannot be turned into the account's, no figure on the resulting
    // position — profit, margin, stop-out level — would mean anything.
    if market.rate(order.instrument).is_none() {
        return Err(RejectReason::NoConversionRate);
    }
    Ok(())
}

/// Extra margin the order would tie up, floored at zero.
pub fn margin_needed(
    account: &Account,
    market: &Market,
    order: ProposedOrder,
    policy: MarginPolicy,
) -> Result<Money, RiskError> {
    let before = account.used_margin(market, policy)?;

    let fill_price = market
        .open_price(order.instrument, order.side)
        .ok_or(RiskError::NoQuote(order.instrument))?;

    let mut hypothetical = account.clone();
    // The rate only affects the profit this hypothetical close would realise,
    // which margin does not depend on — but it must be a real rate, because a
    // zero would be a division by zero inside the fill.
    let rate = market.rate(order.instrument).ok_or_else(|| {
        match market.quote_currency(order.instrument) {
            Some(c) => RiskError::NoRate(c),
            None => RiskError::UnknownInstrument(order.instrument),
        }
    })?;
    hypothetical.fill(Fill {
        instrument: order.instrument,
        ticket: OrderId(0),
        side: order.side,
        qty: order.qty,
        price: fill_price,
        rate,
        commission: order.commission,
    })?;
    let after = hypothetical.used_margin(market, policy)?;

    Ok(if after > before {
        after.sub(before)?
    } else {
        Money::ZERO
    })
}

/// True when `value` is a whole multiple of `step`.
fn is_multiple_of(value: Qty, step: Qty) -> bool {
    if step.is_zero() {
        return true;
    }
    value.raw() % step.raw() == 0
}

/// True when `price` sits on the instrument's tick grid.
fn price_on_tick(price: Price, tick: Price) -> bool {
    if tick.is_zero() {
        return true;
    }
    price.raw() % tick.raw() == 0
}

/// One position the engine decided to force-close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopOut {
    /// Account closed out.
    pub account: AccountId,
    /// Instrument closed.
    pub instrument: InstrumentId,
    /// The parcel's ticket.
    pub ticket: OrderId,
    /// Size closed.
    pub qty: Qty,
    /// Price it closed at.
    pub price: Price,
    /// The conversion rate the loss was realised at. The caller must journal
    /// it, or replay will realise a different number.
    pub rate: Price,
    /// What the close moved into the balance, split for the ledger.
    pub realised: Realised,
}

/// What the risk engine decided after a price move.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RiskOutcome {
    /// A call to raise, when the account has just crossed the call level.
    pub call_bp: Option<i64>,
    /// Positions closed, in the order they were closed.
    pub stopped_out: Vec<StopOut>,
}

impl RiskOutcome {
    /// True when nothing needs doing.
    pub fn is_quiet(&self) -> bool {
        self.call_bp.is_none() && self.stopped_out.is_empty()
    }
}

/// Re-evaluates an account against the market, issuing calls and stopping out.
///
/// Closes the **largest loser first** and re-measures after each close, which
/// is the MetaTrader convention. Closing the smallest first can force several
/// liquidations where one would have done, and every one of them costs the
/// trader a spread.
///
/// Mutates the account: closed lots are removed and their loss realised.
pub fn evaluate(
    account: &mut Account,
    market: &Market,
    policy: MarginPolicy,
) -> Result<RiskOutcome, RiskError> {
    let mut outcome = RiskOutcome::default();

    let Some(level) = account.margin_level_bp(market, policy)? else {
        account.called = false;
        return Ok(outcome);
    };

    if level < policy.call_level_bp {
        // Latch, so a call is raised on the crossing rather than every tick.
        let first_crossing = !account.called;
        account.called = true;
        if first_crossing {
            outcome.call_bp = Some(level);
        }
    } else {
        account.called = false;
    }

    // Close, re-measure, repeat. `None` means nothing is open any more.
    while let Some(level) = account.margin_level_bp(market, policy)? {
        if level >= policy.stop_out_level_bp {
            break;
        }
        let Some((instrument, index)) = worst_lot(account, market)? else {
            break;
        };

        let lot = account.lots[&instrument][index];
        let price = market
            .close_price(instrument, lot.side)
            .ok_or(RiskError::NoQuote(instrument))?;
        let currency = market
            .quote_currency(instrument)
            .ok_or(RiskError::UnknownInstrument(instrument))?;
        let rate = market
            .rate_for(currency)
            .ok_or(RiskError::NoRate(currency))?;

        // By ticket, so the journaled event names the parcel and replay
        // closes the same one. `index` found it; `close_lot` is what the
        // replay path runs too.
        let _ = index;
        let realised =
            account.close_lot(instrument, lot.ticket, lot.qty, price, rate, Money::ZERO)?;

        outcome.stopped_out.push(StopOut {
            account: account.id,
            instrument,
            ticket: lot.ticket,
            qty: lot.qty,
            price,
            rate,
            realised,
        });
    }

    Ok(outcome)
}

/// The open parcel losing the most, by absolute P&L.
///
/// Measured in the account's currency, so a yen loss and a dollar loss are
/// compared as the same quantity rather than as bare numbers — ten thousand
/// yen is not a bigger loss than a hundred dollars.
fn worst_lot(
    account: &Account,
    market: &Market,
) -> Result<Option<(InstrumentId, usize)>, RiskError> {
    let mut worst: Option<(InstrumentId, usize, Money)> = None;
    for (instrument, lots) in &account.lots {
        for (i, lot) in lots.iter().enumerate() {
            // A position is losing what it would realise if closed now, and
            // that includes the financing it has run up.
            let pnl = account.lot_value(*instrument, lot, market)?;
            if worst.as_ref().is_none_or(|(_, _, w)| pnl < *w) {
                worst = Some((*instrument, i, pnl));
            }
        }
    }
    Ok(worst.map(|(i, idx, _)| (i, idx)))
}

/// Every account, folded from the journal.
///
/// This is the state described by invariant P0-2: nothing here is stored, all
/// of it is a fold over events, and replaying from genesis reproduces it.
#[derive(Debug, Clone, Default)]
pub struct Portfolio {
    accounts: BTreeMap<AccountId, Account>,
    instruments: BTreeMap<InstrumentId, Instrument>,
    /// Which account an accepted order belongs to. `OrderFilled` carries only
    /// the ticket, so the link has to be remembered from acceptance.
    order_owner: BTreeMap<OrderId, (AccountId, InstrumentId)>,
    /// Who holds each account, folded from `AccountOwnerSet`.
    owner: BTreeMap<AccountId, UserId>,
    /// Latest top of book, folded from quote events.
    pub market: Market,
    /// The last trading day whose rollover completed. `None` on a journal
    /// that has never run one.
    last_rollover: Option<u32>,
}

impl Portfolio {
    /// An empty portfolio.
    pub fn new() -> Self {
        Portfolio::default()
    }

    /// An account, if it has been opened.
    pub fn account(&self, id: AccountId) -> Option<&Account> {
        self.accounts.get(&id)
    }

    /// The last trading day whose rollover has completed.
    pub fn last_rollover(&self) -> Option<u32> {
        self.last_rollover
    }

    /// An account, mutably.
    pub fn account_mut(&mut self, id: AccountId) -> Option<&mut Account> {
        self.accounts.get_mut(&id)
    }

    /// Every account id.
    pub fn account_ids(&self) -> impl Iterator<Item = AccountId> + '_ {
        self.accounts.keys().copied()
    }

    /// Who holds an account, if anyone has been recorded.
    ///
    /// `None` is not "anyone may trade it": it is an account from before
    /// ownership was recorded, and the caller decides what that means. The
    /// gateway refuses it.
    /// Which account an order belongs to.
    ///
    /// The books need it: a commission posting has to name the account the
    /// money came out of, and `OrderFilled` carries the ticket rather than
    /// the account — the link is remembered from acceptance.
    pub fn owner_of_order(&self, order: OrderId) -> Option<AccountId> {
        self.order_owner.get(&order).map(|(account, _)| *account)
    }

    pub fn owner_of(&self, account: AccountId) -> Option<UserId> {
        self.owner.get(&account).copied()
    }

    /// Every account one user holds, in id order.
    pub fn accounts_of(&self, user: UserId) -> Vec<AccountId> {
        self.owner
            .iter()
            .filter(|(_, u)| **u == user)
            .map(|(a, _)| *a)
            .collect()
    }

    /// The lowest account id not yet used, from `first` upward.
    ///
    /// Allocation is a fold over the journal rather than a counter, so two
    /// gateways replaying the same journal agree about which id comes next.
    pub fn next_account_id(&self, first: AccountId) -> AccountId {
        let mut id = first;
        while self.accounts.contains_key(&id) {
            id = AccountId(id.0 + 1);
        }
        id
    }

    /// A listed instrument.
    pub fn instrument(&self, id: InstrumentId) -> Option<&Instrument> {
        self.instruments.get(&id)
    }

    /// Re-evaluates one account against the folded market.
    ///
    /// This exists on `Portfolio` rather than being called directly because
    /// `evaluate` needs `&mut Account` and `&Market` at the same time, and
    /// those are disjoint fields only from in here.
    ///
    /// # This mutates, and the caller must journal without re-applying
    ///
    /// A stop-out closes lots immediately, so the account is already changed
    /// when this returns. The caller writes the resulting
    /// [`Event::PositionStoppedOut`] to the journal but must NOT feed it back
    /// through [`apply`](Self::apply) — that would close a second lot.
    ///
    /// Replay takes the other path: it never calls this, and `apply` performs
    /// the close. Both routes reach the same state, which
    /// `a_stop_out_replays_to_the_same_state` asserts.
    pub fn evaluate(
        &mut self,
        id: AccountId,
        policy: MarginPolicy,
    ) -> Result<RiskOutcome, RiskError> {
        let market = &self.market;
        let account = self
            .accounts
            .get_mut(&id)
            .ok_or(RiskError::UnknownAccount(id))?;
        evaluate(account, market, policy)
    }

    /// Admission check for one account against the folded market.
    pub fn check(
        &self,
        id: AccountId,
        terms: &InstrumentTerms,
        order: ProposedOrder,
        policy: MarginPolicy,
    ) -> Result<(), RejectReason> {
        let Some(account) = self.accounts.get(&id) else {
            return Err(RejectReason::AccountNotTradeable);
        };
        check_order(account, terms, &self.market, order, policy)
    }

    /// Shape-only admission check, for an order that will rest.
    pub fn check_shape(
        &self,
        id: AccountId,
        terms: &InstrumentTerms,
        order: ProposedOrder,
    ) -> Result<(), RejectReason> {
        let Some(account) = self.accounts.get(&id) else {
            return Err(RejectReason::AccountNotTradeable);
        };
        check_order_shape(account, terms, &self.market, order)
    }

    /// Everything this portfolio holds, as plain data.
    pub fn export(&self) -> PortfolioState {
        PortfolioState {
            accounts: self
                .accounts
                .values()
                .map(|a| AccountState {
                    id: a.id,
                    currency: a.currency,
                    leverage: a.leverage,
                    balance: a.balance,
                    credit: a.credit,
                    insurance: a.insurance,
                    tradeable: a.tradeable,
                    hedging: a.hedging,
                    live: a.live,
                    clock_started: a.clock_started,
                    called: a.called,
                    lots: a
                        .lots
                        .iter()
                        .filter(|(_, lots)| !lots.is_empty())
                        .map(|(i, lots)| (*i, lots.clone()))
                        .collect(),
                })
                .collect(),
            instruments: self.instruments.values().copied().collect(),
            order_owner: self
                .order_owner
                .iter()
                .map(|(o, (a, i))| (*o, *a, *i))
                .collect(),
            owners: self.owner.iter().map(|(a, u)| (*a, *u)).collect(),
            last_rollover: self.last_rollover,
            quotes: self.market.export_quotes(),
        }
    }

    /// Rebuilds a portfolio from exported state.
    ///
    /// Conversion routes are *not* restored: they are configuration the
    /// gateway sets at startup, not state the journal produced, and reading
    /// them from a file would let a stale snapshot quietly change how money
    /// is converted.
    pub fn import(state: PortfolioState, settlement: Currency) -> Self {
        let mut market = Market::settling_in(settlement);
        for instrument in &state.instruments {
            market.list(instrument);
        }
        for (instrument, bid, ask) in &state.quotes {
            market.quote(*instrument, *bid, *ask);
        }

        let mut accounts = BTreeMap::new();
        for a in state.accounts {
            let mut account = Account::new(a.id, a.currency, a.leverage, a.balance);
            account.credit = a.credit;
            account.insurance = a.insurance;
            account.tradeable = a.tradeable;
            account.hedging = a.hedging;
            account.live = a.live;
            account.clock_started = a.clock_started;
            account.called = a.called;
            for (instrument, lots) in a.lots {
                account.lots.insert(instrument, lots);
            }
            accounts.insert(a.id, account);
        }

        Portfolio {
            accounts,
            instruments: state.instruments.into_iter().map(|i| (i.id, i)).collect(),
            order_owner: state
                .order_owner
                .into_iter()
                .map(|(o, a, i)| (o, (a, i)))
                .collect(),
            owner: state.owners.into_iter().collect(),
            market,
            last_rollover: state.last_rollover,
        }
    }

    /// Folds one journal record into the state, and says what money moved.
    ///
    /// The [`Applied`] summary is what lets the double-entry ledger be folded
    /// from the same events without re-deriving FIFO closes: the one place
    /// that knows what a fill realised is the code that applied it.
    pub fn apply(&mut self, record: &Record) -> Result<Applied, RiskError> {
        let mut applied = Applied::default();
        match record.event {
            Event::AccountOpened {
                account,
                currency,
                leverage,
            } => {
                self.accounts.entry(account).or_insert_with(|| {
                    let mut opened = Account::new(account, currency, leverage, Money::ZERO);
                    // The demo clock starts when the account is opened, dated
                    // by the journal rather than by the clock at replay.
                    opened.clock_started = record.at;
                    opened
                });
            }
            Event::AccountRenewed { account, .. } => {
                self.accounts
                    .get_mut(&account)
                    .ok_or(RiskError::UnknownAccount(account))?
                    .clock_started = record.at;
            }
            Event::AccountOwnerSet { account, owner } => {
                // Deliberately not conditional on the account existing: the
                // link is a fact about ids, and a journal that sets ownership
                // before opening the account replays to the same place.
                self.owner.insert(account, owner);
            }
            Event::FundsDeposited {
                account, amount, ..
            } => {
                let a = self
                    .accounts
                    .get_mut(&account)
                    .ok_or(RiskError::UnknownAccount(account))?;
                a.balance = a.balance.add(amount)?;
            }
            Event::FundsWithdrawn {
                account, amount, ..
            } => {
                let a = self
                    .accounts
                    .get_mut(&account)
                    .ok_or(RiskError::UnknownAccount(account))?;
                a.balance = a.balance.sub(amount)?;
            }
            Event::FundsTransferred {
                from, to, amount, ..
            } => {
                // Both sides are checked before either moves. A fold that
                // debited and then found the other account missing would
                // leave the money nowhere, which is the exact failure the
                // single event exists to prevent — and on a replay there is
                // nobody to tell.
                if !self.accounts.contains_key(&from) {
                    return Err(RiskError::UnknownAccount(from));
                }
                if !self.accounts.contains_key(&to) {
                    return Err(RiskError::UnknownAccount(to));
                }
                // Likewise the arithmetic: both are computed, and only then
                // are both written.
                let out = self.accounts[&from].balance.sub(amount)?;
                let into = self.accounts[&to].balance.add(amount)?;
                self.accounts
                    .get_mut(&from)
                    .ok_or(RiskError::UnknownAccount(from))?
                    .balance = out;
                self.accounts
                    .get_mut(&to)
                    .ok_or(RiskError::UnknownAccount(to))?
                    .balance = into;
            }
            Event::InsuranceSet {
                account, amount, ..
            } => {
                let before = self
                    .accounts
                    .get(&account)
                    .map_or(Money::ZERO, |a| a.insurance);
                let moved = amount.sub(before)?;
                applied.insured = Some((account, moved));
                let held = self
                    .accounts
                    .get_mut(&account)
                    .ok_or(RiskError::UnknownAccount(account))?;
                // Setting it aside takes it out of the balance; releasing
                // it puts it back. Both pots are the client's, so the two
                // moves are one transfer and nothing is created.
                held.balance = held.balance.sub(moved)?;
                held.insurance = amount;
            }
            Event::CreditSet {
                account, amount, ..
            } => {
                let before = self
                    .accounts
                    .get(&account)
                    .map_or(Money::ZERO, |a| a.credit);
                applied.credit_moved = Some((account, amount.sub(before)?));
                // Credit is state like any other: folded from the journal, so
                // a restart reproduces it. It used to be set directly on the
                // account at startup, which meant a bonus that survived until
                // the next restart and then quietly did not.
                self.accounts
                    .get_mut(&account)
                    .ok_or(RiskError::UnknownAccount(account))?
                    .credit = amount;
            }
            Event::AccountSuspended { account, .. } => {
                self.accounts
                    .get_mut(&account)
                    .ok_or(RiskError::UnknownAccount(account))?
                    .tradeable = false;
            }
            Event::AccountReinstated { account, .. } => {
                self.accounts
                    .get_mut(&account)
                    .ok_or(RiskError::UnknownAccount(account))?
                    .tradeable = true;
            }
            Event::InstrumentListed { instrument } => {
                self.instruments.insert(instrument.id, instrument);
                // The market needs its quote currency to value anything on it.
                self.market.list(&instrument);
            }
            Event::QuoteTicked {
                instrument,
                bid,
                ask,
            } => {
                self.market.quote(instrument, bid, ask);
            }
            Event::OrderAccepted {
                order,
                account,
                instrument,
                ..
            } => {
                self.order_owner.insert(order, (account, instrument));
            }
            Event::OrderFilled {
                order,
                instrument,
                side,
                qty,
                price,
                rate,
                // The markup is already inside `price` — the client paid it.
                // Posting it anywhere here would count it twice; it is
                // journaled so the house can total what it earned, which is
                // a report rather than a balance.
                markup: _,
                commission,
            } => {
                let (owner, _) = *self
                    .order_owner
                    .get(&order)
                    .ok_or(RiskError::UnknownOrder(order))?;
                let account = self
                    .accounts
                    .get_mut(&owner)
                    .ok_or(RiskError::UnknownAccount(owner))?;
                let realised = account.fill(Fill {
                    instrument,
                    ticket: order,
                    side,
                    qty,
                    price,
                    rate,
                    commission,
                })?;
                applied.realised = Some((owner, realised));
            }
            Event::PositionStoppedOut {
                account,
                instrument,
                order,
                qty,
                price,
                rate,
            } => {
                let a = self
                    .accounts
                    .get_mut(&account)
                    .ok_or(RiskError::UnknownAccount(account))?;
                let realised = if order.0 == 0 {
                    // A journal from before the ticket was recorded. Those
                    // stop-outs closed oldest-first, so that is what replaying
                    // them must do.
                    let side = a
                        .lots(instrument)
                        .first()
                        .map(|l| l.side.opposite())
                        .unwrap_or(Side::Sell);
                    // A stop-out closes; it charges no commission to do it.
                    a.fill(Fill {
                        instrument,
                        ticket: OrderId(0),
                        side,
                        qty,
                        price,
                        rate,
                        commission: Money::ZERO,
                    })?
                } else {
                    a.close_lot(instrument, order, qty, price, rate, Money::ZERO)?
                };
                applied.realised = Some((account, realised));
            }
            Event::TradeAdjusted {
                account, amount, ..
            } => {
                self.accounts
                    .get_mut(&account)
                    .ok_or(RiskError::UnknownAccount(account))?
                    .adjust(amount)?;
                // Reported as a realisation with no trading profit in it: the
                // books post it to their own adjustment account, and the
                // client's cash moves by exactly what the event says.
                applied.adjusted = Some((account, amount));
            }
            Event::PositionCorrected {
                account, order, to, ..
            } => {
                let a = self
                    .accounts
                    .get_mut(&account)
                    .ok_or(RiskError::UnknownAccount(account))?;
                let Some(instrument) = a
                    .open_instruments()
                    .find(|i| a.lots(*i).iter().any(|l| l.ticket == order))
                else {
                    return Err(RiskError::UnknownOrder(order));
                };
                a.correct_open_price(instrument, order, to)?;
            }
            Event::AccountClassified { account, live, .. } => {
                self.accounts
                    .get_mut(&account)
                    .ok_or(RiskError::UnknownAccount(account))?
                    .live = live;
            }
            Event::AccountModeSet {
                account, hedging, ..
            } => {
                self.accounts
                    .get_mut(&account)
                    .ok_or(RiskError::UnknownAccount(account))?
                    .hedging = hedging;
            }
            Event::PositionClosed {
                account,
                instrument,
                order,
                qty,
                price,
                rate,
                commission,
                ..
            } => {
                let realised = self
                    .accounts
                    .get_mut(&account)
                    .ok_or(RiskError::UnknownAccount(account))?
                    .close_lot(instrument, order, qty, price, rate, commission)?;
                applied.realised = Some((account, realised));
            }
            Event::ProtectionSet {
                account,
                order,
                stop_loss,
                take_profit,
                ..
            } => {
                let a = self
                    .accounts
                    .get_mut(&account)
                    .ok_or(RiskError::UnknownAccount(account))?;
                // The instrument is not on the event: a ticket identifies a
                // parcel on its own, and carrying the instrument too would be
                // a second source of truth to disagree with.
                let Some(instrument) = a
                    .open_instruments()
                    .find(|i| a.lots(*i).iter().any(|l| l.ticket == order))
                else {
                    return Err(RiskError::UnknownOrder(order));
                };
                a.set_protection(instrument, order, stop_loss, take_profit)?;
            }
            Event::ProtectionTriggered {
                account,
                instrument,
                order,
                qty,
                price,
                rate,
                ..
            } => {
                let realised = self
                    .accounts
                    .get_mut(&account)
                    .ok_or(RiskError::UnknownAccount(account))?
                    .close_lot(instrument, order, qty, price, rate, Money::ZERO)?;
                applied.realised = Some((account, realised));
            }
            Event::MarginCallIssued { account, .. } => {
                if let Some(a) = self.accounts.get_mut(&account) {
                    a.called = true;
                }
            }
            Event::PendingOrderPlaced {
                order,
                account,
                instrument,
                ..
            } => {
                // Remember the owner now. When this order eventually triggers
                // and fills, `OrderFilled` carries only the ticket, and the
                // fill would be orphaned without this.
                self.order_owner.insert(order, (account, instrument));
            }
            // Triggering changes no state on its own: the fill or the refusal
            // that follows is journaled as its own decision.
            Event::SwapCharged {
                account,
                instrument,
                order,
                amount,
            } => {
                self.accounts
                    .get_mut(&account)
                    .ok_or(RiskError::UnknownAccount(account))?
                    .charge_swap(instrument, order, amount)?;
            }
            Event::RolloverCompleted { day } => {
                self.last_rollover = Some(day);
            }
            // A sign-in changes no balance. It is in the journal to be read,
            // not to be folded.
            Event::SessionChanged { .. }
            | Event::PendingOrderTriggered { .. }
            | Event::PriceSourceChanged { .. }
            | Event::OrderRejected { .. }
            | Event::OrderCancelled { .. } => {}
        }
        Ok(applied)
    }
}

/// One account's whole state, as plain data.
///
/// # Why an export rather than a serialised format
///
/// Snapshots need engine state on disk, and this crate has no serialisation
/// library and should not acquire one: the journal's wire format is the only
/// thing here that must be byte-stable for ever. So the state is handed out as
/// plain fields and the *gateway* decides how to write it, which keeps the
/// decision about file formats where the files are.
///
/// Every field is exactly what the matching event folds into, so a round trip
/// through [`Portfolio::export`] and [`Portfolio::import`] is the identity —
/// which is the only property a snapshot needs and the one its test asserts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountState {
    pub id: AccountId,
    pub currency: Currency,
    pub leverage: u32,
    pub balance: Money,
    pub credit: Money,
    pub insurance: Money,
    pub tradeable: bool,
    pub hedging: bool,
    pub live: bool,
    pub clock_started: Timestamp,
    pub called: bool,
    /// Every open parcel, by instrument, oldest first — the order closes
    /// happen in, which is state and not presentation.
    pub lots: Vec<(InstrumentId, Vec<Lot>)>,
}

/// The whole portfolio, as plain data. See [`AccountState`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PortfolioState {
    pub accounts: Vec<AccountState>,
    pub instruments: Vec<Instrument>,
    /// Which account and instrument each accepted order belongs to.
    pub order_owner: Vec<(OrderId, AccountId, InstrumentId)>,
    pub owners: Vec<(AccountId, UserId)>,
    pub last_rollover: Option<u32>,
    /// Top of book per instrument. Quotes are state too: an account's equity
    /// the instant after a restart should be the equity the instant before.
    pub quotes: Vec<(InstrumentId, Price, Price)>,
}

/// What one applied event moved, for callers keeping their own books.
///
/// Everything here is already implied by the event and the state it was
/// applied to; it is reported rather than recomputed because the fold is the
/// only place that knows, and two implementations of a FIFO close are one
/// implementation too many.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Applied {
    /// An account whose balance changed, and by how much, split for the
    /// ledger's books.
    pub realised: Option<(AccountId, Realised)>,
    /// An account whose bonus changed, and the signed size of the change.
    pub credit_moved: Option<(AccountId, Money)>,
    /// An account whose insurance changed, and by how much. Positive is
    /// money moved out of the balance and set aside.
    pub insured: Option<(AccountId, Money)>,
    /// An account adjusted to put a trade right, and by how much.
    pub adjusted: Option<(AccountId, Money)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The feed says 100.00 / 100.10; the house charges two points a side.
    fn marked(markup: &str) -> (Market, Price) {
        let mut m = Market::new();
        let charged = m.quote_marked(
            InstrumentId(1),
            Price::parse("100.00").unwrap(),
            Price::parse("100.10").unwrap(),
            Price::parse(markup).unwrap(),
        );
        (m, charged)
    }

    #[test]
    fn a_markup_widens_both_sides_and_is_remembered() {
        let (m, charged) = marked("0.02");
        assert_eq!(charged, Price::parse("0.02").unwrap());
        // The client sells lower and buys higher than the feed.
        assert_eq!(
            m.top(InstrumentId(1)).unwrap().0,
            Price::parse("99.98").unwrap()
        );
        assert_eq!(
            m.top(InstrumentId(1)).unwrap().1,
            Price::parse("100.12").unwrap()
        );
        assert_eq!(m.markup_of(InstrumentId(1)), Price::parse("0.02").unwrap());
    }

    #[test]
    fn the_price_a_client_deals_at_is_the_one_they_are_shown() {
        // This is the whole reason the widening happens here rather than at
        // the fill: open, close and stop all read the same number.
        let (m, _) = marked("0.02");
        assert_eq!(
            m.open_price(InstrumentId(1), Side::Buy).unwrap(),
            Price::parse("100.12").unwrap()
        );
        assert_eq!(
            m.close_price(InstrumentId(1), Side::Buy).unwrap(),
            Price::parse("99.98").unwrap()
        );
    }

    #[test]
    fn no_markup_leaves_the_feed_alone() {
        let (m, charged) = marked("0");
        assert_eq!(charged, Price::ZERO);
        assert_eq!(
            m.top(InstrumentId(1)).unwrap().0,
            Price::parse("100.00").unwrap()
        );
        assert_eq!(m.markup_of(InstrumentId(1)), Price::ZERO);
    }

    #[test]
    fn widening_can_never_cross_the_book() {
        // Worth stating because it is why there is no "spread too wide"
        // refusal: the markup moves the bid down and the ask up, so however
        // large it is the two move apart. The only way this goes wrong is
        // the bid reaching zero, which is the test below.
        let mut m = Market::new();
        let charged = m.quote_marked(
            InstrumentId(2),
            Price::parse("1.00").unwrap(),
            Price::parse("1.00").unwrap(),
            Price::parse("0.60").unwrap(),
        );
        assert_eq!(charged, Price::parse("0.60").unwrap());
        let (bid, ask) = m.top(InstrumentId(2)).unwrap();
        assert_eq!(bid, Price::parse("0.40").unwrap());
        assert_eq!(ask, Price::parse("1.60").unwrap());
        assert!(bid < ask);
    }

    #[test]
    fn a_markup_that_would_take_the_whole_bid_is_refused() {
        // There is no price at which the house may take the entire client
        // side. A setting that does is a typo, and the feed's price stands.
        let mut m = Market::new();
        let charged = m.quote_marked(
            InstrumentId(1),
            Price::parse("0.50").unwrap(),
            Price::parse("0.60").unwrap(),
            Price::parse("0.50").unwrap(),
        );
        assert_eq!(charged, Price::ZERO);
        assert_eq!(
            m.top(InstrumentId(1)).unwrap().0,
            Price::parse("0.50").unwrap()
        );
        assert_eq!(m.markup_of(InstrumentId(1)), Price::ZERO);
    }

    #[test]
    fn turning_a_markup_off_clears_it() {
        let mut m = Market::new();
        m.quote_marked(
            InstrumentId(1),
            Price::parse("100.00").unwrap(),
            Price::parse("100.10").unwrap(),
            Price::parse("0.02").unwrap(),
        );
        // The next quote arrives with no markup set.
        m.quote(
            InstrumentId(1),
            Price::parse("100.00").unwrap(),
            Price::parse("100.10").unwrap(),
        );
        assert_eq!(m.markup_of(InstrumentId(1)), Price::ZERO);
        assert_eq!(
            m.top(InstrumentId(1)).unwrap().1,
            Price::parse("100.10").unwrap()
        );
    }
    use domain::{Seq, Symbol, Timestamp};

    /// Short constructors, so a test reads as the numbers it is about.
    fn m(s: &str) -> Money {
        Money::parse(s).expect("money")
    }
    fn p(s: &str) -> Price {
        Price::parse(s).expect("price")
    }
    fn q(s: &str) -> Qty {
        Qty::parse(s).expect("qty")
    }

    const BTC: InstrumentId = InstrumentId(1);
    const ETH: InstrumentId = InstrumentId(2);
    const TRADER: AccountId = AccountId(1001);

    fn btc() -> Instrument {
        Instrument {
            id: BTC,
            symbol: Symbol::new("BTCUSD"),
            base: Currency::new(b"BTC"),
            quote: Currency::USD,
            display_digits: 2,
            contract_size: q("1"),
            tick_size: p("0.01"),
        }
    }

    fn eth() -> Instrument {
        Instrument {
            id: ETH,
            symbol: Symbol::new("ETHUSD"),
            base: Currency::new(b"ETH"),
            quote: Currency::USD,
            display_digits: 2,
            contract_size: q("1"),
            tick_size: p("0.01"),
        }
    }

    fn terms() -> InstrumentTerms {
        InstrumentTerms::new(btc(), q("0.01"), q("100"), q("0.01"))
    }

    /// An account with 10,000 and a market at 80,000 / 80,010.
    fn setup() -> (Account, Market) {
        let mut market = Market::new();
        // Listing is what tells the market a currency, and a market that does
        // not know what an instrument is priced in refuses to value it.
        market.list(&btc());
        market.list(&eth());
        market.quote(BTC, p("80000.00"), p("80010.00"));
        market.quote(ETH, p("2600.00"), p("2601.00"));
        (
            Account::new(TRADER, Currency::USD, 100, m("10000.00")),
            market,
        )
    }

    #[test]
    fn a_flat_account_has_no_margin_level() {
        let (account, market) = setup();
        let policy = MarginPolicy::default();
        assert_eq!(account.used_margin(&market, policy).unwrap(), Money::ZERO);
        // Not infinity, not a huge number — the question does not apply.
        assert_eq!(account.margin_level_bp(&market, policy).unwrap(), None);
        assert!(account.is_flat());
    }

    #[test]
    fn margin_is_notional_over_leverage() {
        // 0.10 BTC at 80,010 is 8,001 notional; at 1:100 that is 80.01.
        let margin = margin_for(p("80010.00"), q("0.10"), 100).unwrap();
        assert_eq!(margin.to_display_string(2).unwrap(), "80.01");
    }

    #[test]
    fn leverage_of_zero_is_an_error_not_a_panic() {
        assert_eq!(
            margin_for(p("80010.00"), q("0.10"), 0),
            Err(ArithError::DivideByZero)
        );
    }

    #[test]
    fn opening_a_position_ties_up_margin_and_moves_equity() {
        let (mut account, market) = setup();
        let policy = MarginPolicy::default();

        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("0.10"),
                price: p("80010.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();

        // Margin is marked to the bid the position would close at.
        assert_eq!(
            account
                .used_margin(&market, policy)
                .unwrap()
                .to_display_string(2)
                .unwrap(),
            "80.00"
        );
        // Bought the ask, valued at the bid: down the spread.
        assert_eq!(
            account
                .floating(&market)
                .unwrap()
                .to_display_string(2)
                .unwrap(),
            "-1.00"
        );
        assert_eq!(
            account
                .equity(&market)
                .unwrap()
                .to_display_string(2)
                .unwrap(),
            "9999.00"
        );
    }

    #[test]
    fn a_long_is_valued_at_the_bid_and_a_short_at_the_ask() {
        let (mut account, market) = setup();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("1"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        account
            .fill(Fill {
                instrument: ETH,
                ticket: OrderId(2),
                side: Side::Sell,
                qty: q("1"),
                price: p("2601.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();

        // The long lost the spread; the short gained it back.
        assert_eq!(
            account
                .floating(&market)
                .unwrap()
                .to_display_string(2)
                .unwrap(),
            "0.00"
        );
    }

    #[test]
    fn margin_level_matches_equity_over_used_margin() {
        let (mut account, market) = setup();
        let policy = MarginPolicy::default();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("0.10"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();

        let level = account.margin_level_bp(&market, policy).unwrap().unwrap();
        // 10,000 equity over 80.00 margin is 12,500%.
        assert_eq!(level, 1_250_000);
    }

    #[test]
    fn an_affordable_order_is_admitted() {
        let (account, market) = setup();
        let order = ProposedOrder {
            instrument: BTC,
            side: Side::Buy,
            kind: OrderType::Market,
            qty: q("1"),
            price: Price::ZERO,
            commission: Money::ZERO,
        };
        assert_eq!(
            check_order(&account, &terms(), &market, order, MarginPolicy::default()),
            Ok(())
        );
    }

    #[test]
    fn an_unaffordable_order_is_refused() {
        let (account, market) = setup();
        // 20 BTC at 80,010 is 1.6M notional; at 1:100 that needs 16,002 margin
        // against 10,000 of equity.
        let order = ProposedOrder {
            instrument: BTC,
            side: Side::Buy,
            kind: OrderType::Market,
            qty: q("20"),
            price: Price::ZERO,
            commission: Money::ZERO,
        };
        assert_eq!(
            check_order(&account, &terms(), &market, order, MarginPolicy::default()),
            Err(RejectReason::InsufficientMargin)
        );
    }

    #[test]
    fn a_closing_order_needs_no_margin_even_past_stop_out() {
        // The invariant that matters most in this module: an account in trouble
        // must always be able to reduce risk. A naive notional/leverage check
        // would refuse this and trap the trader.
        let (mut account, mut market) = setup();
        let policy = MarginPolicy::default();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("1"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();

        // Price collapses; the account is deep under water.
        market.quote(BTC, p("70000.00"), p("70010.00"));
        assert!(account.free_margin(&market, policy).unwrap().is_negative());

        let closing = ProposedOrder {
            instrument: BTC,
            side: Side::Sell,
            kind: OrderType::Market,
            qty: q("1"),
            price: Price::ZERO,
            commission: Money::ZERO,
        };
        assert_eq!(
            margin_needed(&account, &market, closing, policy).unwrap(),
            Money::ZERO
        );
        assert_eq!(
            check_order(&account, &terms(), &market, closing, policy),
            Ok(())
        );

        // While doubling down is still refused.
        let doubling = ProposedOrder {
            side: Side::Buy,
            ..closing
        };
        assert_eq!(
            check_order(&account, &terms(), &market, doubling, policy),
            Err(RejectReason::InsufficientMargin)
        );
    }

    #[test]
    fn quantity_limits_are_enforced() {
        let (account, market) = setup();
        let policy = MarginPolicy::default();
        let base = ProposedOrder {
            instrument: BTC,
            side: Side::Buy,
            kind: OrderType::Market,
            qty: q("0.10"),
            price: Price::ZERO,
            commission: Money::ZERO,
        };

        let too_small = ProposedOrder {
            qty: q("0.001"),
            ..base
        };
        let too_big = ProposedOrder {
            qty: q("200"),
            ..base
        };
        let off_step = ProposedOrder {
            qty: q("0.015"),
            ..base
        };
        let zero = ProposedOrder {
            qty: Qty::ZERO,
            ..base
        };

        for bad in [too_small, too_big, off_step, zero] {
            assert_eq!(
                check_order(&account, &terms(), &market, bad, policy),
                Err(RejectReason::InvalidQuantity),
                "{bad:?} should be refused"
            );
        }
    }

    #[test]
    fn the_shape_check_ignores_money_and_the_full_check_does_not() {
        let (account, market) = setup();
        let policy = MarginPolicy::default();
        // Far beyond what 10,000 of equity can carry at 1:100.
        let huge = ProposedOrder {
            instrument: BTC,
            side: Side::Buy,
            kind: OrderType::Limit,
            qty: q("20"),
            price: p("70000.00"),
            commission: Money::ZERO,
        };

        // Shape is fine: the size is legal and the price is on the grid.
        assert_eq!(check_order_shape(&account, &terms(), &market, huge), Ok(()));
        // Money is not.
        assert_eq!(
            check_order(&account, &terms(), &market, huge, policy),
            Err(RejectReason::InsufficientMargin)
        );

        // And the shape check still refuses a bad shape.
        let off_step = ProposedOrder {
            qty: q("0.015"),
            ..huge
        };
        assert_eq!(
            check_order_shape(&account, &terms(), &market, off_step),
            Err(RejectReason::InvalidQuantity)
        );
    }

    #[test]
    fn a_limit_price_off_the_tick_grid_is_refused() {
        let (account, market) = setup();
        let policy = MarginPolicy::default();
        let order = ProposedOrder {
            instrument: BTC,
            side: Side::Buy,
            kind: OrderType::Limit,
            qty: q("0.10"),
            price: p("79000.005"),
            commission: Money::ZERO,
        };
        assert_eq!(
            check_order(&account, &terms(), &market, order, policy),
            Err(RejectReason::InvalidPrice)
        );

        let on_grid = ProposedOrder {
            price: p("79000.01"),
            ..order
        };
        assert_eq!(
            check_order(&account, &terms(), &market, on_grid, policy),
            Ok(())
        );
    }

    #[test]
    fn a_suspended_account_and_a_closed_market_are_both_refused() {
        let (mut account, market) = setup();
        let policy = MarginPolicy::default();
        let order = ProposedOrder {
            instrument: BTC,
            side: Side::Buy,
            kind: OrderType::Market,
            qty: q("0.10"),
            price: Price::ZERO,
            commission: Money::ZERO,
        };

        account.tradeable = false;
        assert_eq!(
            check_order(&account, &terms(), &market, order, policy),
            Err(RejectReason::AccountNotTradeable)
        );

        account.tradeable = true;
        let mut closed = terms();
        closed.tradeable = false;
        assert_eq!(
            check_order(&account, &closed, &market, order, policy),
            Err(RejectReason::MarketClosed)
        );
    }

    #[test]
    fn an_unquoted_instrument_cannot_be_traded() {
        let (account, _) = setup();
        let empty = Market::new();
        let order = ProposedOrder {
            instrument: BTC,
            side: Side::Buy,
            kind: OrderType::Market,
            qty: q("0.10"),
            price: Price::ZERO,
            commission: Money::ZERO,
        };
        assert_eq!(
            check_order(&account, &terms(), &empty, order, MarginPolicy::default()),
            Err(RejectReason::MarketClosed)
        );
    }

    #[test]
    fn closing_is_fifo_and_realises_profit_into_the_balance() {
        let (mut account, _) = setup();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("1"),
                price: p("70000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(2),
                side: Side::Buy,
                qty: q("1"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();

        // Selling one closes the OLDER parcel, at 70,000, for +15,000.
        let realised = account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(3),
                side: Side::Sell,
                qty: q("1"),
                price: p("85000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        assert_eq!(
            realised.total().unwrap().to_display_string(2).unwrap(),
            "15000.00"
        );
        assert_eq!(account.balance.to_display_string(2).unwrap(), "25000.00");

        let left = account.lots(BTC);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].open_price, p("80000.00"));
    }

    #[test]
    fn an_oversized_close_flips_the_position() {
        let (mut account, _) = setup();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("1"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(2),
                side: Side::Sell,
                qty: q("3"),
                price: p("81000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();

        let left = account.lots(BTC);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].side, Side::Sell);
        assert_eq!(left[0].qty, q("2"));
        // The closed 1 BTC made 1,000.
        assert_eq!(account.balance.to_display_string(2).unwrap(), "11000.00");
    }

    #[test]
    fn a_margin_call_is_issued_once_not_on_every_tick() {
        let (mut account, mut market) = setup();
        let policy = MarginPolicy::default();
        // 1.2 BTC at 1:100 needs ~960 margin. Equity has to fall below that.
        account.balance = m("1000.00");
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("1.2"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();

        market.quote(BTC, p("79800.00"), p("79810.00"));
        let first = evaluate(&mut account, &market, policy).unwrap();
        assert!(first.call_bp.is_some(), "a call must be raised");
        assert!(first.stopped_out.is_empty());

        // Another tick at the same level must not raise a second call.
        market.quote(BTC, p("79790.00"), p("79800.00"));
        let second = evaluate(&mut account, &market, policy).unwrap();
        assert_eq!(second.call_bp, None);
    }

    #[test]
    fn recovering_above_the_call_level_re_arms_the_call() {
        let (mut account, mut market) = setup();
        let policy = MarginPolicy::default();
        account.balance = m("1000.00");
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("1.2"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();

        market.quote(BTC, p("79800.00"), p("79810.00"));
        assert!(evaluate(&mut account, &market, policy)
            .unwrap()
            .call_bp
            .is_some());

        market.quote(BTC, p("81000.00"), p("81010.00"));
        assert!(evaluate(&mut account, &market, policy).unwrap().is_quiet());
        assert!(!account.called);

        market.quote(BTC, p("79800.00"), p("79810.00"));
        assert!(evaluate(&mut account, &market, policy)
            .unwrap()
            .call_bp
            .is_some());
    }

    #[test]
    fn stop_out_closes_the_largest_loser_first() {
        let (mut account, mut market) = setup();
        let policy = MarginPolicy::default();
        account.balance = m("2000.00");

        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("1"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        account
            .fill(Fill {
                instrument: ETH,
                ticket: OrderId(2),
                side: Side::Buy,
                qty: q("10"),
                price: p("2600.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();

        // BTC falls 2,000 (a 2,000 loss); ETH falls 20 (a 200 loss).
        market.quote(BTC, p("78000.00"), p("78010.00"));
        market.quote(ETH, p("2580.00"), p("2581.00"));

        let outcome = evaluate(&mut account, &market, policy).unwrap();
        assert!(
            !outcome.stopped_out.is_empty(),
            "the account must be stopped out"
        );
        assert_eq!(
            outcome.stopped_out[0].instrument, BTC,
            "the larger loser goes first"
        );
    }

    #[test]
    fn stop_out_stops_as_soon_as_the_account_is_safe() {
        let (mut account, mut market) = setup();
        let policy = MarginPolicy::default();
        account.balance = m("2000.00");

        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("1"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        account
            .fill(Fill {
                instrument: ETH,
                ticket: OrderId(2),
                side: Side::Buy,
                qty: q("1"),
                price: p("2600.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();

        market.quote(BTC, p("78400.00"), p("78410.00"));
        let outcome = evaluate(&mut account, &market, policy).unwrap();

        // One close was enough; the ETH position survives.
        assert_eq!(outcome.stopped_out.len(), 1);
        assert_eq!(outcome.stopped_out[0].instrument, BTC);
        assert!(!account.lots(ETH).is_empty());

        let level = account.margin_level_bp(&market, policy).unwrap();
        assert!(level.is_none_or(|l| l >= policy.stop_out_level_bp));
    }

    #[test]
    fn stop_out_terminates_even_when_closing_everything_is_not_enough() {
        // Negative equity: closing every position still leaves the account
        // below the threshold. The loop must end flat rather than spin.
        let (mut account, mut market) = setup();
        let policy = MarginPolicy::default();
        account.balance = m("100.00");
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("1"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();

        market.quote(BTC, p("40000.00"), p("40010.00"));
        let outcome = evaluate(&mut account, &market, policy).unwrap();

        assert_eq!(outcome.stopped_out.len(), 1);
        assert!(account.is_flat());
        assert!(
            account.balance.is_negative(),
            "the deficit is real and must be visible"
        );
    }

    #[test]
    fn margin_basis_changes_what_a_position_ties_up() {
        let (mut account, mut market) = setup();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("1"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        market.quote(BTC, p("90000.00"), p("90010.00"));

        let locked = MarginPolicy {
            basis: MarginBasis::OpenPrice,
            ..MarginPolicy::default()
        };
        let marked = MarginPolicy {
            basis: MarginBasis::MarketPrice,
            ..MarginPolicy::default()
        };

        assert_eq!(
            account
                .used_margin(&market, locked)
                .unwrap()
                .to_display_string(2)
                .unwrap(),
            "800.00"
        );
        assert_eq!(
            account
                .used_margin(&market, marked)
                .unwrap()
                .to_display_string(2)
                .unwrap(),
            "900.00"
        );
    }

    #[test]
    fn credit_supports_margin() {
        let (mut account, market) = setup();
        let policy = MarginPolicy::default();
        account.balance = m("500.00");
        account.credit = m("500.00");

        assert_eq!(
            account
                .equity(&market)
                .unwrap()
                .to_display_string(2)
                .unwrap(),
            "1000.00"
        );

        // An order the cash alone could not carry is admitted on cash + bonus.
        let order = ProposedOrder {
            instrument: BTC,
            side: Side::Buy,
            kind: OrderType::Market,
            qty: q("1"),
            price: Price::ZERO,
            commission: Money::ZERO,
        };
        assert_eq!(
            check_order(&account, &terms(), &market, order, policy),
            Ok(())
        );
    }

    // ── Portfolio fold ──────────────────────────────────────────────

    fn rec(seq: u64, event: Event) -> Record {
        Record {
            seq: Seq(seq),
            at: Timestamp(seq as i64),
            event,
        }
    }

    #[test]
    fn a_transfer_checks_both_accounts_before_either_balance_moves() {
        // The point of the single event is that there is no state where the
        // money has left and arrived nowhere. The fold has to honour that
        // too: a debit applied before discovering the other side is missing
        // would produce exactly that state, on every replay, with nobody to
        // tell.
        let mut pf = Portfolio::new();
        let other = AccountId(1002);
        for (i, e) in [
            Event::AccountOpened {
                account: TRADER,
                currency: Currency::USD,
                leverage: 100,
            },
            Event::AccountOpened {
                account: other,
                currency: Currency::USD,
                leverage: 100,
            },
            Event::FundsDeposited {
                account: TRADER,
                amount: m("10000.00"),
                by: UserId::SYSTEM,
            },
        ]
        .into_iter()
        .enumerate()
        {
            pf.apply(&rec(i as u64, e)).expect("applies");
        }

        let before = pf.account(TRADER).unwrap().balance;

        // A transfer to an account nobody opened.
        let missing = pf.apply(&rec(
            10,
            Event::FundsTransferred {
                from: TRADER,
                to: AccountId(9_999),
                amount: m("100.00"),
                by: UserId(1),
            },
        ));
        assert!(missing.is_err(), "it should not fold");
        assert_eq!(
            pf.account(TRADER).unwrap().balance,
            before,
            "the sending balance moved anyway"
        );

        // And one from an account nobody opened.
        let backwards = pf.apply(&rec(
            11,
            Event::FundsTransferred {
                from: AccountId(9_999),
                to: TRADER,
                amount: m("100.00"),
                by: UserId(1),
            },
        ));
        assert!(backwards.is_err());
        assert_eq!(pf.account(TRADER).unwrap().balance, before);

        // The real one moves both, and the pair sums to what it did.
        pf.apply(&rec(
            12,
            Event::FundsTransferred {
                from: TRADER,
                to: other,
                amount: m("2500.00"),
                by: UserId(1),
            },
        ))
        .expect("applies");
        let from = pf.account(TRADER).unwrap().balance;
        let into = pf.account(other).unwrap().balance;
        assert_eq!(before.sub(from).unwrap(), m("2500.00"));
        assert_eq!(into, m("2500.00"));
        assert_eq!(from.add(into).unwrap(), before);
    }

    #[test]
    fn closing_part_of_a_parcel_leaves_the_rest_at_the_price_it_opened_at() {
        // The whole reason for this feature. Closing all of it and
        // re-opening pays the spread twice and gives up the entry price,
        // which is what a trader taking half off is trying not to do.
        let (mut account, _market) = setup();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("1.00"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        let opening = account.balance;

        let realised = account
            .close_lot(
                BTC,
                OrderId(1),
                q("0.40"),
                p("81000.00"),
                Price::ONE,
                Money::ZERO,
            )
            .unwrap();
        // 0.40 at 1,000 up.
        assert_eq!(
            realised.total().unwrap().to_display_string(2).unwrap(),
            "400.00"
        );
        assert_eq!(
            account.balance.sub(opening).unwrap(),
            realised.total().unwrap()
        );

        let left = account.lots(BTC);
        assert_eq!(left.len(), 1, "it is still one position, not two");
        assert_eq!(left[0].ticket, OrderId(1), "and it keeps its ticket");
        assert_eq!(left[0].qty, q("0.60"));
        assert_eq!(
            left[0].open_price,
            p("80000.00"),
            "the remainder is still in at the price it got"
        );

        // And the rest closes for the rest of the profit.
        let rest = account
            .close_lot(
                BTC,
                OrderId(1),
                q("0.60"),
                p("81000.00"),
                Price::ONE,
                Money::ZERO,
            )
            .unwrap();
        assert_eq!(
            rest.total().unwrap().to_display_string(2).unwrap(),
            "600.00"
        );
        assert!(account.lots(BTC).is_empty());
        // The two parts add to what closing the whole lot would have made.
        assert_eq!(
            account
                .balance
                .sub(opening)
                .unwrap()
                .to_display_string(2)
                .unwrap(),
            "1000.00"
        );
    }

    #[test]
    fn a_partial_close_takes_its_share_of_the_financing_and_no_more() {
        // Swap accrues on the position, so closing part of it realises part
        // of the swap. Taking all of it would charge the remainder's
        // financing twice: once now and again when it closes.
        let (mut account, _market) = setup();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("1.00"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        account.charge_swap(BTC, OrderId(1), m("-100.00")).unwrap();

        let realised = account
            .close_lot(
                BTC,
                OrderId(1),
                q("0.25"),
                p("80000.00"),
                Price::ONE,
                Money::ZERO,
            )
            .unwrap();
        assert_eq!(realised.swap.to_display_string(2).unwrap(), "-25.00");
        assert_eq!(
            account.lots(BTC)[0].swap.to_display_string(2).unwrap(),
            "-75.00",
            "the remainder keeps the financing it has not realised"
        );

        // Closing the rest realises exactly the rest, so the night costs
        // what it cost and not a penny more.
        let rest = account
            .close_lot(
                BTC,
                OrderId(1),
                q("0.75"),
                p("80000.00"),
                Price::ONE,
                Money::ZERO,
            )
            .unwrap();
        assert_eq!(rest.swap.to_display_string(2).unwrap(), "-75.00");
    }

    #[test]
    fn a_close_of_zero_or_more_than_is_there_takes_the_whole_parcel() {
        // Zero is what every record written before partial closes existed
        // meant by the size it carried, so those replay unchanged. More
        // than the parcel is clamped here and refused at the gateway: a
        // replay has nobody to tell, and the only honest reading of
        // "close 2.0" of a 1.0 parcel is the parcel that is there.
        for asked in [q("0"), q("5.00")] {
            let (mut account, _market) = setup();
            account
                .fill(Fill {
                    instrument: BTC,
                    ticket: OrderId(1),
                    side: Side::Buy,
                    qty: q("1.00"),
                    price: p("80000.00"),
                    rate: Price::ONE,
                    commission: Money::ZERO,
                })
                .unwrap();
            account
                .close_lot(
                    BTC,
                    OrderId(1),
                    asked,
                    p("81000.00"),
                    Price::ONE,
                    Money::ZERO,
                )
                .unwrap();
            assert!(
                account.lots(BTC).is_empty(),
                "asking for {asked:?} left something behind"
            );
        }
    }

    #[test]
    fn a_partially_closed_position_still_ties_up_margin_for_what_is_left() {
        // The point of closing part of a position is usually to free
        // margin. If the used margin did not fall, the trader would have
        // given up the profit and got nothing for it.
        let (mut account, market) = setup();
        let policy = MarginPolicy::default();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("1.00"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        let before = account.used_margin(&market, policy).unwrap();

        account
            .close_lot(
                BTC,
                OrderId(1),
                q("0.50"),
                p("80000.00"),
                Price::ONE,
                Money::ZERO,
            )
            .unwrap();
        let after = account.used_margin(&market, policy).unwrap();

        assert!(after < before, "{after:?} should be below {before:?}");
        // Half the size, half the margin.
        assert_eq!(
            after.mul_ratio(2, 1).unwrap().to_display_string(2).unwrap(),
            before.to_display_string(2).unwrap()
        );
    }

    #[test]
    fn a_portfolio_rebuilds_itself_from_events() {
        let mut pf = Portfolio::new();
        let events = [
            Event::AccountOpened {
                account: TRADER,
                currency: Currency::USD,
                leverage: 100,
            },
            Event::InstrumentListed { instrument: btc() },
            Event::FundsDeposited {
                account: TRADER,
                amount: m("10000.00"),
                by: UserId::SYSTEM,
            },
            Event::QuoteTicked {
                instrument: BTC,
                bid: p("80000.00"),
                ask: p("80010.00"),
            },
            Event::OrderAccepted {
                order: OrderId(7),
                account: TRADER,
                instrument: BTC,
                side: Side::Buy,
                kind: OrderType::Market,
                qty: q("0.50"),
                price: Price::ZERO,
                tif: domain::TimeInForce::ImmediateOrCancel,
                by: UserId::SYSTEM,
            },
            Event::OrderFilled {
                order: OrderId(7),
                instrument: BTC,
                side: Side::Buy,
                qty: q("0.50"),
                price: p("80010.00"),
                rate: Price::ONE,
                markup: Price::ZERO,
                commission: Money::ZERO,
            },
        ];
        for (i, e) in events.into_iter().enumerate() {
            pf.apply(&rec(i as u64, e)).expect("applies");
        }

        let account = pf.account(TRADER).expect("account exists");
        assert_eq!(account.balance.to_display_string(2).unwrap(), "10000.00");
        assert_eq!(account.lots(BTC).len(), 1);
        assert_eq!(account.lots(BTC)[0].qty, q("0.50"));
        assert_eq!(
            pf.instrument(BTC).map(|i| i.symbol.as_str().to_owned()),
            Some("BTCUSD".into())
        );
        assert_eq!(pf.market.top(BTC), Some((p("80000.00"), p("80010.00"))));
    }

    #[test]
    fn replaying_the_same_events_twice_gives_the_same_state() {
        let build = || {
            let mut pf = Portfolio::new();
            let events = [
                Event::AccountOpened {
                    account: TRADER,
                    currency: Currency::USD,
                    leverage: 100,
                },
                Event::FundsDeposited {
                    account: TRADER,
                    amount: m("10000.00"),
                    by: UserId::SYSTEM,
                },
                Event::QuoteTicked {
                    instrument: BTC,
                    bid: p("80000.00"),
                    ask: p("80010.00"),
                },
                Event::OrderAccepted {
                    order: OrderId(1),
                    account: TRADER,
                    instrument: BTC,
                    side: Side::Buy,
                    kind: OrderType::Market,
                    qty: q("1"),
                    price: Price::ZERO,
                    tif: domain::TimeInForce::ImmediateOrCancel,
                    by: UserId::SYSTEM,
                },
                Event::OrderFilled {
                    order: OrderId(1),
                    instrument: BTC,
                    side: Side::Buy,
                    qty: q("1"),
                    price: p("80010.00"),
                    rate: Price::ONE,
                    markup: Price::ZERO,
                    commission: Money::ZERO,
                },
            ];
            for (i, e) in events.into_iter().enumerate() {
                pf.apply(&rec(i as u64, e)).unwrap();
            }
            let a = pf.account(TRADER).unwrap();
            (a.balance, a.lots(BTC).to_vec())
        };
        assert_eq!(build(), build());
    }

    #[test]
    fn a_pending_order_that_later_fills_is_not_orphaned() {
        // `PendingOrderPlaced` is the only record of who owns the ticket. If
        // the fold forgot it, the fill arriving minutes later would be
        // rejected as an unknown order and the position would never appear.
        let mut pf = Portfolio::new();
        for (i, e) in [
            Event::AccountOpened {
                account: TRADER,
                currency: Currency::USD,
                leverage: 100,
            },
            Event::FundsDeposited {
                account: TRADER,
                amount: m("10000.00"),
                by: UserId::SYSTEM,
            },
            Event::PendingOrderPlaced {
                order: OrderId(42),
                account: TRADER,
                instrument: BTC,
                side: Side::Buy,
                kind: OrderType::Limit,
                qty: q("1"),
                trigger_price: p("70000.00"),
                limit_price: Price::ZERO,
                tif: domain::TimeInForce::GoodTilCancelled,
                by: UserId::SYSTEM,
            },
            Event::PendingOrderTriggered {
                order: OrderId(42),
                price: p("70000.00"),
            },
            Event::OrderFilled {
                order: OrderId(42),
                instrument: BTC,
                side: Side::Buy,
                qty: q("1"),
                price: p("70000.00"),
                rate: Price::ONE,
                markup: Price::ZERO,
                commission: Money::ZERO,
            },
        ]
        .into_iter()
        .enumerate()
        {
            pf.apply(&rec(i as u64, e)).expect("applies");
        }

        let account = pf.account(TRADER).expect("account");
        assert_eq!(account.lots(BTC).len(), 1);
        assert_eq!(account.lots(BTC)[0].open_price, p("70000.00"));
    }

    #[test]
    fn a_fill_for_an_order_the_portfolio_never_accepted_is_an_error() {
        let mut pf = Portfolio::new();
        pf.apply(&rec(
            0,
            Event::AccountOpened {
                account: TRADER,
                currency: Currency::USD,
                leverage: 100,
            },
        ))
        .unwrap();
        let orphan = Event::OrderFilled {
            order: OrderId(99),
            instrument: BTC,
            side: Side::Buy,
            qty: q("1"),
            price: p("80000.00"),
            rate: Price::ONE,
            markup: Price::ZERO,
            commission: Money::ZERO,
        };
        assert_eq!(
            pf.apply(&rec(1, orphan)),
            Err(RiskError::UnknownOrder(OrderId(99)))
        );
    }

    #[test]
    fn a_stop_out_replays_to_the_same_state() {
        // The live path mutates through `Portfolio::evaluate` and journals the
        // decision. The replay path applies that journaled event instead. If
        // these two ever diverge, every audit and every CI replay is worthless.
        let policy = MarginPolicy::default();

        let opening = [
            Event::AccountOpened {
                account: TRADER,
                currency: Currency::USD,
                leverage: 100,
            },
            Event::InstrumentListed { instrument: btc() },
            Event::FundsDeposited {
                account: TRADER,
                amount: m("1000.00"),
                by: UserId::SYSTEM,
            },
            Event::QuoteTicked {
                instrument: BTC,
                bid: p("80000.00"),
                ask: p("80010.00"),
            },
            Event::OrderAccepted {
                order: OrderId(1),
                account: TRADER,
                instrument: BTC,
                side: Side::Buy,
                kind: OrderType::Market,
                qty: q("1"),
                price: Price::ZERO,
                tif: domain::TimeInForce::ImmediateOrCancel,
                by: UserId::SYSTEM,
            },
            Event::OrderFilled {
                order: OrderId(1),
                instrument: BTC,
                side: Side::Buy,
                qty: q("1"),
                price: p("80000.00"),
                rate: Price::ONE,
                markup: Price::ZERO,
                commission: Money::ZERO,
            },
            Event::QuoteTicked {
                instrument: BTC,
                bid: p("40000.00"),
                ask: p("40010.00"),
            },
        ];

        // Live: fold the openings, then evaluate.
        let mut live = Portfolio::new();
        for (i, e) in opening.into_iter().enumerate() {
            live.apply(&rec(i as u64, e)).unwrap();
        }
        let outcome = live.evaluate(TRADER, policy).unwrap();
        assert_eq!(outcome.stopped_out.len(), 1);
        let closed = outcome.stopped_out[0];

        // Replay: fold the same openings, then the journaled stop-out.
        let mut replayed = Portfolio::new();
        for (i, e) in opening.into_iter().enumerate() {
            replayed.apply(&rec(i as u64, e)).unwrap();
        }
        replayed
            .apply(&rec(
                99,
                Event::PositionStoppedOut {
                    account: TRADER,
                    instrument: closed.instrument,
                    order: closed.ticket,
                    qty: closed.qty,
                    price: closed.price,
                    rate: Price::ONE,
                },
            ))
            .unwrap();

        let a = live.account(TRADER).unwrap();
        let b = replayed.account(TRADER).unwrap();
        assert_eq!(a.balance, b.balance, "balances must agree");
        assert_eq!(a.lots(BTC), b.lots(BTC), "positions must agree");
        assert!(a.is_flat() && b.is_flat());
    }

    #[test]
    fn the_portfolio_admits_and_refuses_through_its_own_market() {
        let mut pf = Portfolio::new();
        let policy = MarginPolicy::default();
        for (i, e) in [
            Event::AccountOpened {
                account: TRADER,
                currency: Currency::USD,
                leverage: 100,
            },
            Event::InstrumentListed { instrument: btc() },
            Event::FundsDeposited {
                account: TRADER,
                amount: m("10000.00"),
                by: UserId::SYSTEM,
            },
            Event::QuoteTicked {
                instrument: BTC,
                bid: p("80000.00"),
                ask: p("80010.00"),
            },
        ]
        .into_iter()
        .enumerate()
        {
            pf.apply(&rec(i as u64, e)).unwrap();
        }

        let ok = ProposedOrder {
            instrument: BTC,
            side: Side::Buy,
            kind: OrderType::Market,
            qty: q("1"),
            price: Price::ZERO,
            commission: Money::ZERO,
        };
        assert_eq!(pf.check(TRADER, &terms(), ok, policy), Ok(()));

        let too_big = ProposedOrder { qty: q("50"), ..ok };
        assert_eq!(
            pf.check(TRADER, &terms(), too_big, policy),
            Err(RejectReason::InsufficientMargin)
        );

        // An account the portfolio has never seen cannot trade.
        assert_eq!(
            pf.check(AccountId(4242), &terms(), ok, policy),
            Err(RejectReason::AccountNotTradeable)
        );
    }

    // ── Swap ────────────────────────────────────────────────────────

    #[test]
    fn accrued_swap_moves_equity_but_not_the_balance() {
        let (mut account, market) = setup();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("1"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        let equity_before = account.equity(&market).unwrap();

        account.charge_swap(BTC, OrderId(1), m("-21.92")).unwrap();

        assert_eq!(account.balance.to_display_string(2).unwrap(), "10000.00");
        assert_eq!(
            account.equity(&market).unwrap(),
            equity_before.sub(m("21.92")).unwrap(),
            "financing is a real cost the moment it accrues"
        );
        assert_eq!(account.accrued_swap().unwrap(), m("-21.92"));
    }

    #[test]
    fn closing_realises_the_swap_into_the_balance() {
        let (mut account, _) = setup();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("1"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        account.charge_swap(BTC, OrderId(1), m("-21.92")).unwrap();
        account.charge_swap(BTC, OrderId(1), m("-21.92")).unwrap();

        // Close flat on price, so the only thing realised is the financing.
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(2),
                side: Side::Sell,
                qty: q("1"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();

        assert_eq!(account.balance.to_display_string(2).unwrap(), "9956.16");
        assert_eq!(account.accrued_swap().unwrap(), Money::ZERO);
        assert!(account.is_flat());
    }

    #[test]
    fn a_partial_close_takes_its_share_and_leaves_the_rest() {
        let (mut account, _) = setup();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("3"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        // An amount that does not divide by three.
        account.charge_swap(BTC, OrderId(1), m("-10.00")).unwrap();

        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(2),
                side: Side::Sell,
                qty: q("1"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();

        let realised = account.balance.sub(m("10000.00")).unwrap();
        let remaining = account.lots(BTC)[0].swap;

        assert_eq!(realised.raw(), -3_333_333_333, "a third, rounded");
        assert_eq!(
            remaining.raw(),
            -6_666_666_667,
            "two thirds, carrying the remainder"
        );
        assert_eq!(
            realised.add(remaining).unwrap(),
            m("-10.00"),
            "rounding moved a nano between the parts and lost nothing"
        );
    }

    #[test]
    fn many_small_partial_closes_leave_no_residue() {
        // The failure mode a proportional split invites: each close rounds, and
        // the rounding accumulates into a swap nobody is ever charged.
        let (mut account, _) = setup();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("7"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        account.charge_swap(BTC, OrderId(1), m("-1.00")).unwrap();

        for i in 0..7 {
            account
                .fill(Fill {
                    instrument: BTC,
                    ticket: OrderId(10 + i),
                    side: Side::Sell,
                    qty: q("1"),
                    price: p("80000.00"),
                    rate: Price::ONE,
                    commission: Money::ZERO,
                })
                .unwrap();
        }

        assert!(account.is_flat());
        assert_eq!(
            account.balance,
            m("9999.00"),
            "exactly the one unit charged, closed across seven partials"
        );
    }

    #[test]
    fn a_stop_out_realises_accrued_swap() {
        let (mut account, mut market) = setup();
        let policy = MarginPolicy::default();
        account.balance = m("100.00");
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("1"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        account.charge_swap(BTC, OrderId(1), m("-5.00")).unwrap();

        market.quote(BTC, p("40000.00"), p("40010.00"));
        evaluate(&mut account, &market, policy).unwrap();

        assert!(account.is_flat());
        // 100 − 40,000 loss − 5 swap.
        assert_eq!(account.balance.to_display_string(2).unwrap(), "-39905.00");
    }

    #[test]
    fn swap_on_an_unknown_ticket_is_an_error_not_a_balance_charge() {
        let (mut account, _) = setup();
        let err = account
            .charge_swap(BTC, OrderId(404), m("-1.00"))
            .unwrap_err();
        assert_eq!(err, RiskError::UnknownOrder(OrderId(404)));
        assert_eq!(account.balance.to_display_string(2).unwrap(), "10000.00");
    }

    #[test]
    fn the_portfolio_folds_swap_and_remembers_the_rollover() {
        let mut pf = Portfolio::new();
        let events = [
            Event::AccountOpened {
                account: TRADER,
                currency: Currency::USD,
                leverage: 100,
            },
            Event::FundsDeposited {
                account: TRADER,
                amount: m("10000.00"),
                by: UserId::SYSTEM,
            },
            Event::OrderAccepted {
                order: OrderId(1),
                account: TRADER,
                instrument: BTC,
                side: Side::Buy,
                kind: OrderType::Market,
                qty: q("1"),
                price: Price::ZERO,
                tif: domain::TimeInForce::ImmediateOrCancel,
                by: UserId::SYSTEM,
            },
            Event::OrderFilled {
                order: OrderId(1),
                instrument: BTC,
                side: Side::Buy,
                qty: q("1"),
                price: p("80000.00"),
                rate: Price::ONE,
                markup: Price::ZERO,
                commission: Money::ZERO,
            },
            Event::SwapCharged {
                account: TRADER,
                instrument: BTC,
                order: OrderId(1),
                amount: m("-21.92"),
            },
            Event::RolloverCompleted { day: 20_710 },
        ];
        assert_eq!(pf.last_rollover(), None);
        for (i, e) in events.into_iter().enumerate() {
            pf.apply(&rec(i as u64, e)).expect("applies");
        }

        let account = pf.account(TRADER).unwrap();
        assert_eq!(account.lots(BTC)[0].swap, m("-21.92"));
        assert_eq!(account.balance, m("10000.00"));
        assert_eq!(pf.last_rollover(), Some(20_710));
    }

    #[test]
    fn funds_for_an_unopened_account_are_an_error_not_a_silent_create() {
        let mut pf = Portfolio::new();
        let e = Event::FundsDeposited {
            account: TRADER,
            amount: m("100.00"),
            by: UserId::SYSTEM,
        };
        assert_eq!(pf.apply(&rec(0, e)), Err(RiskError::UnknownAccount(TRADER)));
    }

    // ── Hedged margin ───────────────────────────────────────────────────

    #[test]
    fn a_hedge_ties_up_the_difference_plus_a_charge_on_the_matched_part() {
        let (mut account, market) = setup();
        let policy = MarginPolicy::default();

        // One lot long: 1 BTC at the bid, 80,000, at 1:100 is 800.
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("1"),
                price: p("80010.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        assert_eq!(
            account
                .used_margin(&market, policy)
                .unwrap()
                .to_display_string(2)
                .unwrap(),
            "800.00"
        );

        // Hedge it exactly. Both sides are 800 of margin; matched is 800, and
        // nothing is left exposed — so the pair ties up 25% of 800.
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(2),
                side: Side::Sell,
                qty: q("1"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        // `fill` closes opposing lots first, so an opposing order of the same
        // size *closes* rather than hedges. Both legs have to be opened
        // without the other being there to close against — which is what a
        // separate hedging account does, and what this engine does not model.
        assert!(
            account.is_flat(),
            "an opposing order closes; hedging two legs at once is P5"
        );
    }

    #[test]
    fn two_sides_held_at_once_are_charged_once_over() {
        // Built by hand, because `fill` would net them off: this is the state
        // a hedging account reaches, and the question is what it ties up.
        let (mut account, market) = setup();
        let policy = MarginPolicy::default();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("3"),
                price: p("80010.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        // Add a short leg directly beside the long one.
        if let Some(lots) = account.lots.get_mut(&BTC) {
            lots.push(Lot {
                ticket: OrderId(2),
                side: Side::Sell,
                qty: q("2"),
                open_price: p("80000.00"),
                stop_loss: Price::ZERO,
                take_profit: Price::ZERO,
                swap: Money::ZERO,
            });
        }

        // Long 3 valued at the bid, 80,000, is 2,400.00. Short 2 valued at the
        // ask, 80,010, is 1,600.20. The matched pair is 2 × 1,600.20 =
        // 3,200.40, of which a quarter is 800.10; what is left exposed is
        // 2,400.00 + 1,600.20 − 3,200.40 = 799.80. Together, 1,599.90.
        let used = account.used_margin(&market, policy).unwrap();
        assert_eq!(used.to_display_string(2).unwrap(), "1599.90");
        // Unhedged, the same two positions would have tied up 4,000.20.
        assert!(
            used < m("4000.20"),
            "a hedge must be cheaper than two outright positions"
        );
        assert!(
            used > m("799.80"),
            "and not free: both legs still cross a spread to close"
        );
    }

    #[test]
    fn a_full_hedge_charged_at_nothing_is_a_policy_not_a_default() {
        let (mut account, market) = setup();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("1"),
                price: p("80010.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        if let Some(lots) = account.lots.get_mut(&BTC) {
            lots.push(Lot {
                ticket: OrderId(2),
                side: Side::Sell,
                qty: q("1"),
                open_price: p("80000.00"),
                stop_loss: Price::ZERO,
                take_profit: Price::ZERO,
                swap: Money::ZERO,
            });
        }

        let free_hedge = MarginPolicy {
            hedged_bp: 0,
            ..MarginPolicy::default()
        };
        let used = account.used_margin(&market, free_hedge).unwrap();
        // The two legs are 800.00 and 800.10 — the matched part is twice the
        // smaller, so a hair of the larger is still exposed.
        assert!(used < m("1.00"), "nearly nothing, at zero basis points");

        // And the default sits between the two: a quarter of the pair.
        let used = account
            .used_margin(&market, MarginPolicy::default())
            .unwrap();
        assert_eq!(used.to_display_string(2).unwrap(), "400.10");

        let full = MarginPolicy {
            hedged_bp: 10_000,
            ..MarginPolicy::default()
        };
        assert_eq!(
            account
                .used_margin(&market, full)
                .unwrap()
                .to_display_string(2)
                .unwrap(),
            "1600.10",
            "charged in full, a hedge is just two positions"
        );
    }

    #[test]
    fn hedging_is_per_instrument_and_never_across_them() {
        // Long BTC against short ETH is correlated, not hedged. Charging it
        // as hedged is how a broker finds out the difference on the one
        // morning the correlation breaks.
        let (mut account, market) = setup();
        let policy = MarginPolicy::default();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("1"),
                price: p("80010.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        account
            .fill(Fill {
                instrument: ETH,
                ticket: OrderId(2),
                side: Side::Sell,
                qty: q("10"),
                price: p("2600.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();

        // BTC long: 800.00. ETH short at the ask, 2,601, on 10: 260.10.
        let used = account.used_margin(&market, policy).unwrap();
        assert_eq!(used.to_display_string(2).unwrap(), "1060.10");
    }

    // ── Stop loss and take profit ───────────────────────────────────────

    /// A long 0.10 BTC at 80,010, with the market at 80,000 / 80,010.
    fn long_btc() -> (Account, Market) {
        let (mut account, market) = setup();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("0.10"),
                price: p("80010.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        (account, market)
    }

    #[test]
    fn a_long_s_stop_watches_the_bid_and_its_target_does_too() {
        let (mut account, mut market) = long_btc();
        account
            .set_protection(BTC, OrderId(1), p("79000.00"), p("82000.00"))
            .unwrap();

        // Nothing yet.
        assert!(protection_hits(&account, BTC, p("80000.00"), p("80010.00")).is_empty());

        // The bid reaches the stop. The ask is irrelevant: a long is closed
        // by selling into the bid, so the bid is what its stop watches.
        let hits = protection_hits(&account, BTC, p("79000.00"), p("79500.00"));
        assert_eq!(hits.len(), 1);
        assert!(hits[0].stop_loss);
        assert_eq!(hits[0].price, p("79000.00"), "it closes at the bid");
        assert_eq!(hits[0].qty, q("0.10"));

        // The other direction takes the profit.
        let hits = protection_hits(&account, BTC, p("82000.00"), p("82010.00"));
        assert_eq!(hits.len(), 1);
        assert!(!hits[0].stop_loss);
        assert_eq!(hits[0].price, p("82000.00"));

        // And a gap that slips past the stop still fires it, at the price the
        // market actually offered.
        market.quote(BTC, p("70000.00"), p("70010.00"));
        let hits = protection_hits(&account, BTC, p("70000.00"), p("70010.00"));
        assert_eq!(hits[0].price, p("70000.00"), "a stop is not a guarantee");
    }

    #[test]
    fn a_short_s_levels_are_the_mirror_image() {
        let (mut account, _market) = setup();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(2),
                side: Side::Sell,
                qty: q("0.10"),
                price: p("80000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        account
            .set_protection(BTC, OrderId(2), p("81000.00"), p("79000.00"))
            .unwrap();

        assert!(protection_hits(&account, BTC, p("80000.00"), p("80010.00")).is_empty());
        // A short closes by buying the ask, so the ask is what it watches.
        let stop = protection_hits(&account, BTC, p("80990.00"), p("81000.00"));
        assert!(stop[0].stop_loss && stop[0].price == p("81000.00"));
        let target = protection_hits(&account, BTC, p("78990.00"), p("79000.00"));
        assert!(!target[0].stop_loss && target[0].price == p("79000.00"));
    }

    #[test]
    fn a_gap_through_both_levels_fires_the_stop_loss() {
        // The alternative is paying out the better side of a move that also
        // blew through the worse one, on a price the account never traded.
        let (mut account, _market) = long_btc();
        account
            .set_protection(BTC, OrderId(1), p("79000.00"), p("79500.00"))
            .unwrap();
        let hits = protection_hits(&account, BTC, p("78000.00"), p("78010.00"));
        assert_eq!(hits.len(), 1);
        assert!(hits[0].stop_loss, "the stop wins a straddle");
    }

    #[test]
    fn protection_on_the_wrong_side_of_the_market_is_refused() {
        let tick = p("0.01");
        // A long: stop below, target above.
        assert_eq!(
            check_protection(Side::Buy, p("80000.00"), p("79000.00"), p("82000.00"), tick),
            Ok(())
        );
        // A stop above a long is an order to sell at a loss now, which nobody
        // means — and firing it on the next tick would be worse than refusing.
        assert_eq!(
            check_protection(Side::Buy, p("80000.00"), p("81000.00"), p("82000.00"), tick),
            Err(RejectReason::InvalidPrice)
        );
        assert_eq!(
            check_protection(Side::Buy, p("80000.00"), p("79000.00"), p("79500.00"), tick),
            Err(RejectReason::InvalidPrice)
        );
        // At the market exactly is not protection either.
        assert_eq!(
            check_protection(Side::Buy, p("80000.00"), p("80000.00"), Price::ZERO, tick),
            Err(RejectReason::InvalidPrice)
        );
        // A short is the mirror image.
        assert_eq!(
            check_protection(
                Side::Sell,
                p("80000.00"),
                p("81000.00"),
                p("79000.00"),
                tick
            ),
            Ok(())
        );
        assert_eq!(
            check_protection(
                Side::Sell,
                p("80000.00"),
                p("79000.00"),
                p("79500.00"),
                tick
            ),
            Err(RejectReason::InvalidPrice)
        );
        // Zero is "none", and off the tick grid is refused like any price.
        assert_eq!(
            check_protection(Side::Buy, p("80000.00"), Price::ZERO, Price::ZERO, tick),
            Ok(())
        );
        assert_eq!(
            check_protection(Side::Buy, p("80000.00"), p("79000.005"), Price::ZERO, tick),
            Err(RejectReason::InvalidPrice)
        );
    }

    #[test]
    fn protection_belongs_to_a_parcel_not_to_an_instrument() {
        // Two lots of the same symbol, opened at different prices, protected
        // at different prices. Collapsing them onto the instrument would move
        // one position's stop because another was opened.
        let (mut account, _market) = long_btc();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(2),
                side: Side::Buy,
                qty: q("0.10"),
                price: p("81000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        account
            .set_protection(BTC, OrderId(1), p("79000.00"), Price::ZERO)
            .unwrap();
        account
            .set_protection(BTC, OrderId(2), p("80500.00"), Price::ZERO)
            .unwrap();

        // A move to 80,400 takes out the second parcel only.
        let hits = protection_hits(&account, BTC, p("80400.00"), p("80410.00"));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].ticket, OrderId(2));

        assert_eq!(
            account.set_protection(BTC, OrderId(99), p("1.00"), Price::ZERO),
            Err(RiskError::UnknownOrder(OrderId(99))),
            "a stop nobody can see is worse than a refusal"
        );
    }

    #[test]
    fn closing_by_ticket_closes_that_parcel_and_not_the_oldest() {
        let (mut account, _market) = long_btc();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(2),
                side: Side::Buy,
                qty: q("0.10"),
                price: p("90000.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        let opening = account.balance;

        // Close the *second* parcel, which is the one carrying the loss.
        let realised = account
            .close_lot(
                BTC,
                OrderId(2),
                q("0.10"),
                p("80000.00"),
                Price::ONE,
                Money::ZERO,
            )
            .unwrap();
        assert_eq!(
            realised.total().unwrap().to_display_string(2).unwrap(),
            "-1000.00"
        );
        assert_eq!(
            account.balance.sub(opening).unwrap(),
            realised.total().unwrap()
        );

        // The first is untouched, with its own entry price.
        let left = account.lots(BTC);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].ticket, OrderId(1));
        assert_eq!(left[0].open_price, p("80010.00"));

        assert_eq!(
            account.close_lot(
                BTC,
                OrderId(2),
                q("0.10"),
                p("80000.00"),
                Price::ONE,
                Money::ZERO
            ),
            Err(RiskError::UnknownOrder(OrderId(2))),
            "closing it twice is an error, not a second realisation"
        );
    }

    #[test]
    fn a_stop_out_replays_the_parcel_it_actually_closed() {
        // The bug this guards: a stop-out picks the largest loser, which may
        // not be the oldest parcel, and replay used to close the front of the
        // queue regardless — reaching a different book from the live one.
        let policy = MarginPolicy::default();
        let opening = [
            Event::AccountOpened {
                account: TRADER,
                currency: Currency::USD,
                leverage: 100,
            },
            Event::InstrumentListed { instrument: btc() },
            // Enough that closing the loser alone brings the account back
            // above its stop-out level: the point of the test is one close,
            // on the right parcel.
            Event::FundsDeposited {
                account: TRADER,
                amount: m("600.00"),
                by: UserId::SYSTEM,
            },
            Event::QuoteTicked {
                instrument: BTC,
                bid: p("80000.00"),
                ask: p("80010.00"),
            },
            // An old parcel bought below the market: a small winner.
            Event::OrderAccepted {
                by: UserId(2),
                order: OrderId(1),
                account: TRADER,
                instrument: BTC,
                side: Side::Buy,
                kind: OrderType::Market,
                qty: q("1"),
                price: Price::ZERO,
                tif: domain::TimeInForce::ImmediateOrCancel,
            },
            Event::OrderFilled {
                order: OrderId(1),
                instrument: BTC,
                side: Side::Buy,
                qty: q("1"),
                price: p("79500.00"),
                rate: Price::ONE,
                markup: Price::ZERO,
                commission: Money::ZERO,
            },
            // A newer parcel bought above it: the bigger loser.
            Event::OrderAccepted {
                by: UserId(2),
                order: OrderId(2),
                account: TRADER,
                instrument: BTC,
                side: Side::Buy,
                kind: OrderType::Market,
                qty: q("1"),
                price: Price::ZERO,
                tif: domain::TimeInForce::ImmediateOrCancel,
            },
            Event::OrderFilled {
                order: OrderId(2),
                instrument: BTC,
                side: Side::Buy,
                qty: q("1"),
                price: p("80600.00"),
                rate: Price::ONE,
                markup: Price::ZERO,
                commission: Money::ZERO,
            },
        ];

        let mut live = Portfolio::new();
        for (i, e) in opening.into_iter().enumerate() {
            live.apply(&rec(i as u64, e)).unwrap();
        }
        let outcome = live.evaluate(TRADER, policy).unwrap();
        let closed = *outcome.stopped_out.first().expect("a stop-out");
        assert_eq!(
            closed.ticket,
            OrderId(2),
            "the largest loser is the newer parcel"
        );

        let mut replayed = Portfolio::new();
        for (i, e) in opening.into_iter().enumerate() {
            replayed.apply(&rec(i as u64, e)).unwrap();
        }
        replayed
            .apply(&rec(
                99,
                Event::PositionStoppedOut {
                    account: TRADER,
                    instrument: BTC,
                    order: closed.ticket,
                    qty: closed.qty,
                    price: closed.price,
                    rate: closed.rate,
                },
            ))
            .unwrap();

        let a = live.account(TRADER).unwrap();
        let b = replayed.account(TRADER).unwrap();
        assert_eq!(a.balance, b.balance, "balances must agree");
        assert_eq!(a.lots(BTC), b.lots(BTC), "and so must the parcels left");
        assert_eq!(
            b.lots(BTC).first().map(|l| l.ticket),
            Some(OrderId(1)),
            "the winner is what remains, on both paths"
        );
    }

    #[test]
    fn protection_replays_from_the_journal() {
        let mut pf = Portfolio::new();
        for (i, e) in [
            Event::AccountOpened {
                account: TRADER,
                currency: Currency::USD,
                leverage: 100,
            },
            Event::InstrumentListed { instrument: btc() },
            Event::FundsDeposited {
                account: TRADER,
                amount: m("10000.00"),
                by: UserId::SYSTEM,
            },
            Event::QuoteTicked {
                instrument: BTC,
                bid: p("80000.00"),
                ask: p("80010.00"),
            },
            Event::OrderAccepted {
                by: UserId(2),
                order: OrderId(1),
                account: TRADER,
                instrument: BTC,
                side: Side::Buy,
                kind: OrderType::Market,
                qty: q("0.10"),
                price: Price::ZERO,
                tif: domain::TimeInForce::ImmediateOrCancel,
            },
            Event::OrderFilled {
                order: OrderId(1),
                instrument: BTC,
                side: Side::Buy,
                qty: q("0.10"),
                price: p("80010.00"),
                rate: Price::ONE,
                markup: Price::ZERO,
                commission: Money::ZERO,
            },
            Event::ProtectionSet {
                account: TRADER,
                order: OrderId(1),
                stop_loss: p("79000.00"),
                take_profit: p("82000.00"),
                by: UserId(2),
            },
        ]
        .into_iter()
        .enumerate()
        {
            pf.apply(&rec(i as u64, e)).expect("applies");
        }
        let lot = pf.account(TRADER).unwrap().lots(BTC)[0];
        assert_eq!(lot.stop_loss, p("79000.00"));
        assert_eq!(lot.take_profit, p("82000.00"));

        // The stop fires, and the close lands on the balance.
        pf.apply(&rec(
            50,
            Event::ProtectionTriggered {
                account: TRADER,
                instrument: BTC,
                order: OrderId(1),
                qty: q("0.10"),
                price: p("79000.00"),
                rate: Price::ONE,
                stop_loss: true,
            },
        ))
        .unwrap();
        let account = pf.account(TRADER).unwrap();
        assert!(account.is_flat());
        // 0.10 BTC from 80,010 down to 79,000 is a 101.00 loss.
        assert_eq!(account.balance.to_display_string(2).unwrap(), "9899.00");

        // Protection for a ticket nobody holds is an error, not a shrug.
        assert_eq!(
            pf.apply(&rec(
                51,
                Event::ProtectionSet {
                    account: TRADER,
                    order: OrderId(404),
                    stop_loss: p("1.00"),
                    take_profit: Price::ZERO,
                    by: UserId(2),
                }
            )),
            Err(RiskError::UnknownOrder(OrderId(404)))
        );
    }

    // ── Credit and suspension are journaled state ───────────────────────

    #[test]
    fn credit_is_folded_from_the_journal_so_it_survives_a_restart() {
        // It used to be set straight onto the account at startup, which meant
        // a bonus that worked until the next restart and then silently did
        // not. Equity is balance + credit + floating, so losing it moves a
        // figure the trader is looking at.
        let mut pf = Portfolio::new();
        for (i, e) in [
            Event::AccountOpened {
                account: TRADER,
                currency: Currency::USD,
                leverage: 100,
            },
            Event::FundsDeposited {
                account: TRADER,
                amount: m("10000.00"),
                by: UserId(1),
            },
            Event::CreditSet {
                account: TRADER,
                amount: m("500.00"),
                by: UserId(1),
            },
        ]
        .into_iter()
        .enumerate()
        {
            pf.apply(&rec(i as u64, e)).expect("applies");
        }
        let market = Market::new();
        assert_eq!(pf.account(TRADER).unwrap().credit, m("500.00"));
        assert_eq!(
            pf.account(TRADER).unwrap().equity(&market).unwrap(),
            m("10500.00"),
            "credit supports margin, so it counts toward equity"
        );

        // Set again, absolutely: a bonus is what an operator says it is.
        pf.apply(&rec(
            9,
            Event::CreditSet {
                account: TRADER,
                amount: m("250.00"),
                by: UserId(1),
            },
        ))
        .unwrap();
        assert_eq!(pf.account(TRADER).unwrap().credit, m("250.00"));
        assert_eq!(
            pf.account(TRADER).unwrap().balance,
            m("10000.00"),
            "credit is not cash and must not touch the balance"
        );
    }

    #[test]
    fn a_suspended_account_refuses_new_orders_and_still_closes() {
        let (mut account, market) = setup();
        let policy = MarginPolicy::default();
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("0.10"),
                price: p("80010.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();

        account.tradeable = false;
        let opening = ProposedOrder {
            instrument: BTC,
            side: Side::Buy,
            kind: OrderType::Market,
            qty: q("0.10"),
            price: Price::ZERO,
            commission: Money::ZERO,
        };
        assert_eq!(
            check_order(&account, &terms(), &market, opening, policy),
            Err(RejectReason::AccountNotTradeable)
        );

        // Closing is refused too, today — which is the honest statement of
        // what suspension does in this engine: it stops the account trading.
        // Letting a suspended account reduce risk is the next question, and
        // the stop-out path does not ask it: `evaluate` closes positions
        // without consulting `tradeable` at all.
        let mut stopped = account.clone();
        stopped.balance = m("1.00");
        let market_down = {
            let mut mk = market.clone();
            mk.quote(BTC, p("70000.00"), p("70010.00"));
            mk
        };
        let outcome = evaluate(&mut stopped, &market_down, policy).unwrap();
        assert!(
            !outcome.stopped_out.is_empty(),
            "a suspension must not stop the engine protecting the account"
        );
    }

    #[test]
    fn suspension_is_folded_from_the_journal_too() {
        let mut pf = Portfolio::new();
        for (i, e) in [
            Event::AccountOpened {
                account: TRADER,
                currency: Currency::USD,
                leverage: 100,
            },
            Event::AccountSuspended {
                account: TRADER,
                by: UserId(1),
            },
        ]
        .into_iter()
        .enumerate()
        {
            pf.apply(&rec(i as u64, e)).expect("applies");
        }
        assert!(!pf.account(TRADER).unwrap().tradeable);

        pf.apply(&rec(
            5,
            Event::AccountReinstated {
                account: TRADER,
                by: UserId(1),
            },
        ))
        .unwrap();
        assert!(pf.account(TRADER).unwrap().tradeable);
    }

    #[test]
    fn money_events_for_an_account_that_does_not_exist_are_errors() {
        // Including the new ones: a credit or a suspension against an unopened
        // account means the journal and the book disagree, and that is worth
        // stopping for rather than ignoring.
        let mut pf = Portfolio::new();
        assert_eq!(
            pf.apply(&rec(
                0,
                Event::CreditSet {
                    account: TRADER,
                    amount: m("100.00"),
                    by: UserId(1),
                }
            )),
            Err(RiskError::UnknownAccount(TRADER))
        );
        assert_eq!(
            pf.apply(&rec(
                1,
                Event::AccountSuspended {
                    account: TRADER,
                    by: UserId(1),
                }
            )),
            Err(RiskError::UnknownAccount(TRADER))
        );
    }

    // ── Currency conversion (invariant P1-3) ────────────────────────────

    const USDJPY: InstrumentId = InstrumentId(30);
    const EURUSD: InstrumentId = InstrumentId(20);

    fn usdjpy() -> Instrument {
        Instrument {
            id: USDJPY,
            symbol: Symbol::new("USDJPY"),
            base: Currency::USD,
            quote: Currency::JPY,
            display_digits: 3,
            contract_size: q("100000"),
            tick_size: p("0.001"),
        }
    }

    fn jpy_terms() -> InstrumentTerms {
        InstrumentTerms::new(usdjpy(), q("1000"), q("5000000"), q("1000"))
    }

    /// A USD account holding yen risk: USDJPY quoted, and routed to itself.
    ///
    /// The pair that prices the instrument is the pair that converts it, which
    /// is the ordinary case for USDJPY, USDCAD and USDCHF.
    fn jpy_setup() -> (Account, Market) {
        let mut market = Market::new();
        market.list(&usdjpy());
        market.route(Currency::JPY, Conversion::PerSettlement(USDJPY));
        market.quote(USDJPY, p("147.500"), p("147.508"));
        (
            Account::new(TRADER, Currency::USD, 100, m("10000.00")),
            market,
        )
    }

    #[test]
    fn a_yen_profit_reaches_the_balance_in_dollars() {
        let (mut account, mut market) = jpy_setup();
        // Long 100,000 USD against yen at the ask, 147.508.
        account
            .fill(Fill {
                instrument: USDJPY,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("100000"),
                price: p("147.508"),
                rate: p("147.504"),
                commission: Money::ZERO,
            })
            .unwrap();

        // The yen falls to 148.500 / 148.508: the long is up.
        market.quote(USDJPY, p("148.500"), p("148.508"));

        // Closing at the bid, 148.500, makes 0.992 yen per dollar on 100,000:
        // 99,200 yen. At a mid of 148.504 that is 99,200 ÷ 148.504 = 667.9955
        // dollars — and nowhere near the 99,200 an unconverted book would show.
        let floating = account.floating(&market).unwrap();
        assert_eq!(floating.to_display_string(2).unwrap(), "668.00");

        let realised = account
            .fill(Fill {
                instrument: USDJPY,
                ticket: OrderId(2),
                side: Side::Sell,
                qty: q("100000"),
                price: p("148.500"),
                rate: p("148.504"),
                commission: Money::ZERO,
            })
            .unwrap();
        assert_eq!(
            realised.total().unwrap().to_display_string(2).unwrap(),
            "668.00"
        );
        assert_eq!(account.balance.to_display_string(2).unwrap(), "10668.00");
        assert!(account.is_flat());

        // The point of all of it: 99,200 of anything is not 99,200 dollars.
        assert!(realised.total().unwrap() < m("1000.00"));
    }

    #[test]
    fn yen_margin_is_a_dollar_figure() {
        let (mut account, market) = jpy_setup();
        let policy = MarginPolicy::default();
        account
            .fill(Fill {
                instrument: USDJPY,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("100000"),
                price: p("147.508"),
                rate: p("147.504"),
                commission: Money::ZERO,
            })
            .unwrap();

        // Notional is 100,000 × 147.500 = 14,750,000 yen; at 1:100 that is
        // 147,500 yen of margin, which at 147.504 is 999.97 USD. The familiar
        // answer — one lot at 1:100 is about 1,000 of the base currency —
        // which it should be, because the base currency here is the dollar.
        let used = account.used_margin(&market, policy).unwrap();
        assert_eq!(used.to_display_string(2).unwrap(), "999.97");
        assert!(
            used < m("1500.00"),
            "unconverted, this would read as 147,500"
        );
    }

    #[test]
    fn an_unquoted_conversion_pair_refuses_the_order_rather_than_guessing() {
        let mut market = Market::new();
        market.list(&usdjpy());
        market.route(Currency::JPY, Conversion::PerSettlement(USDJPY));
        // USDJPY is quoted, so the instrument is tradeable on its own terms —
        // but here the pair that converts it is the same pair, so quoting it
        // fixes both. Take the quote away and both go.
        let account = Account::new(TRADER, Currency::USD, 100, m("10000.00"));
        let order = ProposedOrder {
            instrument: USDJPY,
            side: Side::Buy,
            kind: OrderType::Market,
            qty: q("1000"),
            price: Price::ZERO,
            commission: Money::ZERO,
        };
        assert_eq!(
            check_order(
                &account,
                &jpy_terms(),
                &market,
                order,
                MarginPolicy::default()
            ),
            Err(RejectReason::MarketClosed),
            "no quote at all is a closed market"
        );

        // Now a case where the instrument is quoted and only the *rate* is
        // missing: a euro-quoted instrument with no route for the euro.
        let mut market = Market::new();
        let eur_quoted = Instrument {
            id: EURUSD,
            symbol: Symbol::new("XAUEUR"),
            base: Currency::new(b"XAU"),
            quote: Currency::EUR,
            display_digits: 2,
            contract_size: q("100"),
            tick_size: p("0.01"),
        };
        market.list(&eur_quoted);
        market.quote(EURUSD, p("2378.44"), p("2378.72"));
        let terms = InstrumentTerms::new(eur_quoted, q("1"), q("100"), q("1"));
        assert_eq!(
            check_order(
                &account,
                &terms,
                &market,
                ProposedOrder {
                    instrument: EURUSD,
                    qty: q("1"),
                    ..order
                },
                MarginPolicy::default()
            ),
            Err(RejectReason::NoConversionRate),
            "priced, tradeable, and still unvaluable: say so precisely"
        );
    }

    #[test]
    fn a_reciprocal_route_prices_a_currency_the_other_way_round() {
        // An instrument quoted in euros, in a dollar book, converts through
        // EURUSD — which is dollars per euro, the opposite direction to
        // USDJPY. The rate stored is one over the price.
        let mut market = Market::new();
        let eur_quoted = Instrument {
            id: InstrumentId(99),
            symbol: Symbol::new("DAXEUR"),
            base: Currency::new(b"DAX"),
            quote: Currency::EUR,
            display_digits: 2,
            contract_size: q("1"),
            tick_size: p("0.01"),
        };
        market.list(&eur_quoted);
        market.route(Currency::EUR, Conversion::InSettlement(EURUSD));
        market.quote(EURUSD, p("1.08530"), p("1.08534"));

        // EURUSD mid is 1.08532, so one euro is 1.08532 dollars and the rate
        // — euros per dollar — is 1 ÷ 1.08532 = 0.921387…
        let rate = market.rate_for(Currency::EUR).unwrap();
        assert_eq!(rate.to_display_string(6).unwrap(), "0.921387");

        // 1,000 euros of profit is therefore about 1,085.32 dollars.
        let dollars = m("1000.00").convert_per(rate).unwrap();
        assert_eq!(dollars.to_display_string(2).unwrap(), "1085.32");
    }

    #[test]
    fn an_account_in_another_currency_is_refused_not_mis_valued() {
        let (_, market) = jpy_setup();
        let euro_account = Account::new(TRADER, Currency::EUR, 100, m("10000.00"));
        assert_eq!(
            euro_account.floating(&market),
            Err(RiskError::CurrencyMismatch {
                account: Currency::EUR,
                settlement: Currency::USD,
            })
        );
    }

    #[test]
    fn a_stop_out_ranks_a_yen_loss_against_a_dollar_loss_in_dollars() {
        // 10,000 yen lost is about 68 dollars. A 100-dollar loss on BTC is
        // worse, and must be the one that closes first — comparing the bare
        // numbers would close the yen position instead.
        let mut market = Market::new();
        market.list(&btc());
        market.list(&usdjpy());
        market.route(Currency::JPY, Conversion::PerSettlement(USDJPY));
        market.quote(BTC, p("80000.00"), p("80010.00"));
        market.quote(USDJPY, p("147.500"), p("147.508"));

        let mut account = Account::new(TRADER, Currency::USD, 100, m("10000.00"));
        // Long BTC, then long yen. Both will be underwater.
        account
            .fill(Fill {
                instrument: BTC,
                ticket: OrderId(1),
                side: Side::Buy,
                qty: q("0.10"),
                price: p("81010.00"),
                rate: Price::ONE,
                commission: Money::ZERO,
            })
            .unwrap();
        account
            .fill(Fill {
                instrument: USDJPY,
                ticket: OrderId(2),
                side: Side::Buy,
                qty: q("100000"),
                price: p("147.608"),
                rate: p("147.504"),
                commission: Money::ZERO,
            })
            .unwrap();

        // BTC is down 1,010 per coin on 0.10 — about 101 dollars.
        // USDJPY is down 0.108 yen per dollar on 100,000 — 10,800 yen, which
        // is about 73 dollars.
        let worst = worst_lot(&account, &market).unwrap().expect("a worst lot");
        assert_eq!(worst.0, BTC, "the dollar loss is the larger one");

        let btc_loss = account
            .lot_value(BTC, &account.lots(BTC)[0], &market)
            .unwrap();
        let jpy_loss = account
            .lot_value(USDJPY, &account.lots(USDJPY)[0], &market)
            .unwrap();
        assert_eq!(btc_loss.to_display_string(2).unwrap(), "-101.00");
        assert_eq!(jpy_loss.to_display_string(2).unwrap(), "-73.22");
    }

    #[test]
    fn a_replayed_yen_fill_realises_the_rate_in_the_journal_not_todays() {
        // The rate moves between the close and the replay. The balance must
        // not: it is the rate in the event that counts.
        let listing = [
            Event::AccountOpened {
                account: TRADER,
                currency: Currency::USD,
                leverage: 100,
            },
            Event::InstrumentListed {
                instrument: usdjpy(),
            },
            Event::FundsDeposited {
                account: TRADER,
                amount: m("10000.00"),
                by: UserId::SYSTEM,
            },
            Event::QuoteTicked {
                instrument: USDJPY,
                bid: p("147.500"),
                ask: p("147.508"),
            },
            Event::OrderAccepted {
                order: OrderId(1),
                account: TRADER,
                instrument: USDJPY,
                side: Side::Buy,
                kind: OrderType::Market,
                qty: q("100000"),
                price: Price::ZERO,
                tif: domain::TimeInForce::ImmediateOrCancel,
                by: UserId::SYSTEM,
            },
            Event::OrderFilled {
                order: OrderId(1),
                instrument: USDJPY,
                side: Side::Buy,
                qty: q("100000"),
                price: p("147.508"),
                rate: p("147.504"),
                markup: Price::ZERO,
                commission: Money::ZERO,
            },
            Event::OrderAccepted {
                order: OrderId(2),
                account: TRADER,
                instrument: USDJPY,
                side: Side::Sell,
                kind: OrderType::Market,
                qty: q("100000"),
                price: Price::ZERO,
                tif: domain::TimeInForce::ImmediateOrCancel,
                by: UserId::SYSTEM,
            },
            // Closed at 148.500 with the rate that was live at the time.
            Event::OrderFilled {
                order: OrderId(2),
                instrument: USDJPY,
                side: Side::Sell,
                qty: q("100000"),
                price: p("148.500"),
                rate: p("148.504"),
                markup: Price::ZERO,
                commission: Money::ZERO,
            },
            // ... and then the yen moves a long way, as it does.
            Event::QuoteTicked {
                instrument: USDJPY,
                bid: p("160.000"),
                ask: p("160.008"),
            },
        ];

        let mut pf = Portfolio::new();
        pf.market
            .route(Currency::JPY, Conversion::PerSettlement(USDJPY));
        for (i, e) in listing.into_iter().enumerate() {
            pf.apply(&rec(i as u64, e)).expect("applies");
        }
        let replayed = pf.account(TRADER).unwrap().balance;
        assert_eq!(
            replayed.to_display_string(2).unwrap(),
            "10668.00",
            "the balance is what the rate at the close made it"
        );
    }

    // ── Ownership ───────────────────────────────────────────────────────

    #[test]
    fn ownership_is_folded_from_the_journal_not_assumed() {
        let mut pf = Portfolio::new();
        let alice = UserId(1);
        let bob = UserId(2);
        for (i, e) in [
            Event::AccountOpened {
                account: AccountId(1001),
                currency: Currency::USD,
                leverage: 100,
            },
            Event::AccountOwnerSet {
                account: AccountId(1001),
                owner: alice,
            },
            Event::AccountOpened {
                account: AccountId(1002),
                currency: Currency::USD,
                leverage: 100,
            },
            Event::AccountOwnerSet {
                account: AccountId(1002),
                owner: bob,
            },
            // A second account for Alice, opened later.
            Event::AccountOpened {
                account: AccountId(1003),
                currency: Currency::USD,
                leverage: 200,
            },
            Event::AccountOwnerSet {
                account: AccountId(1003),
                owner: alice,
            },
        ]
        .into_iter()
        .enumerate()
        {
            pf.apply(&rec(i as u64, e)).expect("applies");
        }

        assert_eq!(pf.owner_of(AccountId(1002)), Some(bob));
        assert_eq!(
            pf.accounts_of(alice),
            vec![AccountId(1001), AccountId(1003)]
        );
        assert_eq!(pf.accounts_of(UserId(99)), vec![]);
        // An account nobody has claimed is not everybody's.
        assert_eq!(pf.owner_of(AccountId(4004)), None);
    }

    #[test]
    fn the_next_account_id_skips_what_the_journal_already_used() {
        let mut pf = Portfolio::new();
        assert_eq!(pf.next_account_id(AccountId(1001)), AccountId(1001));
        for (i, id) in [1001u64, 1002, 1004].into_iter().enumerate() {
            pf.apply(&rec(
                i as u64,
                Event::AccountOpened {
                    account: AccountId(id),
                    currency: Currency::USD,
                    leverage: 100,
                },
            ))
            .unwrap();
        }
        // 1003 is free, and taking it is right: ids identify, they do not
        // order. What must never happen is handing out 1001 twice.
        assert_eq!(pf.next_account_id(AccountId(1001)), AccountId(1003));
    }

    #[test]
    fn a_zero_amount_needs_no_rate() {
        // A flat account must stay valuable while a conversion pair is dark —
        // nothing to convert is not the same as nothing to say.
        let mut market = Market::new();
        market.list(&usdjpy());
        market.route(Currency::JPY, Conversion::PerSettlement(USDJPY));
        let account = Account::new(TRADER, Currency::USD, 100, m("10000.00"));
        assert_eq!(account.floating(&market).unwrap(), Money::ZERO);
        assert_eq!(account.equity(&market).unwrap(), m("10000.00"));
    }
}
