//! Resting orders and the price conditions that activate them.
//!
//! # Which side of the book activates what
//!
//! An order activates against the price it would *execute* at, never the mid
//! and never the price on its own side. Getting this wrong is subtle and
//! expensive, so it is stated once here and encoded in one function:
//!
//! | Order | Sits | Activates when | Fills at |
//! |-------|------|----------------|----------|
//! | Buy limit | below the market | `ask <= limit` | the ask |
//! | Sell limit | above the market | `bid >= limit` | the bid |
//! | Buy stop | above the market | `ask >= stop` | the ask |
//! | Sell stop | below the market | `bid <= stop` | the bid |
//!
//! A limit order fills at the ask rather than at its own limit price because
//! the ask is by then equal to or better than the limit, and a client is
//! entitled to the improvement. A stop fills at the market and may therefore
//! slip past its trigger — that is what a stop is, and pretending otherwise
//! would mean promising a price the engine cannot honour in a gap.
//!
//! # Margin is not reserved while an order rests
//!
//! It is checked when the order activates, which is what MetaTrader does and
//! what traders expect. Reserving margin for a stop parked forty per cent away
//! would lock up an account for an order that may never fire. The consequence
//! is that a triggered order can be refused for margin, and that refusal is a
//! journaled decision like any other.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic)]

use std::collections::BTreeMap;
use std::fmt;

use domain::{AccountId, InstrumentId, OrderId, OrderType, Price, Qty, Side, TimeInForce};

/// An order waiting for its price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingOrder {
    /// Client order identity.
    pub order: OrderId,
    /// Owning account.
    pub account: AccountId,
    /// Instrument traded.
    pub instrument: InstrumentId,
    /// Direction.
    pub side: Side,
    /// `Limit`, `Stop` or `StopLimit`.
    pub kind: OrderType,
    /// Size in base units.
    pub qty: Qty,
    /// The price that activates it.
    pub trigger_price: Price,
    /// Where a stop-limit rests once triggered. Zero otherwise.
    pub limit_price: Price,
    /// Lifetime.
    pub tif: TimeInForce,
    /// Arrival order, so two orders triggering on the same tick fire in the
    /// sequence they were placed.
    pub placed: u64,
}

/// Why an order could not be accepted as a resting order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingError {
    /// A market order does not rest.
    NotRestable(OrderType),
    /// The trigger price was zero or negative.
    InvalidTrigger,
    /// A stop-limit needs a limit price; the others must not carry one.
    InvalidLimit,
    /// That ticket is already resting.
    Duplicate(OrderId),
}

impl fmt::Display for PendingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PendingError::NotRestable(k) => write!(f, "{k:?} orders do not rest"),
            PendingError::InvalidTrigger => f.write_str("trigger price must be positive"),
            PendingError::InvalidLimit => f.write_str("limit price is wrong for this order type"),
            PendingError::Duplicate(o) => write!(f, "order {o} is already resting"),
        }
    }
}

impl std::error::Error for PendingError {}

/// What the engine should do with an order whose condition was met.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    /// Execute at `price`, subject to the margin check.
    Fill {
        /// The resting order.
        order: PendingOrder,
        /// The market price it activated at, and fills at.
        price: Price,
    },
    /// A stop-limit's stop was hit; it now rests as a limit at `limit_price`.
    BecomesLimit {
        /// The resting order, before conversion.
        order: PendingOrder,
        /// The price the stop was hit at.
        price: Price,
    },
}

impl Activation {
    /// The order that activated.
    pub fn order(&self) -> PendingOrder {
        match self {
            Activation::Fill { order, .. } | Activation::BecomesLimit { order, .. } => *order,
        }
    }

    /// The market price that activated it.
    pub fn price(&self) -> Price {
        match self {
            Activation::Fill { price, .. } | Activation::BecomesLimit { price, .. } => *price,
        }
    }
}

/// Decides whether one order activates against a top of book.
///
/// Pure, and the only place the table in the module header is encoded. The
/// engine calls it through [`PendingBook::activated`]; it is public so the
/// rules can be tested in isolation, one row at a time.
pub fn activates(order: &PendingOrder, bid: Price, ask: Price) -> Option<Activation> {
    let hit = match (order.kind, order.side) {
        (OrderType::Limit, Side::Buy) => (ask <= order.trigger_price).then_some(ask),
        (OrderType::Limit, Side::Sell) => (bid >= order.trigger_price).then_some(bid),
        (OrderType::Stop | OrderType::StopLimit, Side::Buy) => {
            (ask >= order.trigger_price).then_some(ask)
        }
        (OrderType::Stop | OrderType::StopLimit, Side::Sell) => {
            (bid <= order.trigger_price).then_some(bid)
        }
        // A market order never reaches this book.
        (OrderType::Market, _) => None,
    }?;

    Some(if order.kind == OrderType::StopLimit {
        Activation::BecomesLimit {
            order: *order,
            price: hit,
        }
    } else {
        Activation::Fill {
            order: *order,
            price: hit,
        }
    })
}

/// Every resting order, indexed so a tick only inspects one instrument.
#[derive(Debug, Clone, Default)]
pub struct PendingBook {
    orders: BTreeMap<OrderId, PendingOrder>,
    by_instrument: BTreeMap<InstrumentId, Vec<OrderId>>,
    next_arrival: u64,
}

impl PendingBook {
    /// An empty book.
    pub fn new() -> Self {
        PendingBook::default()
    }

    /// How many orders are resting.
    pub fn len(&self) -> usize {
        self.orders.len()
    }

    /// True when nothing is resting.
    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }

    /// One resting order.
    pub fn get(&self, order: OrderId) -> Option<&PendingOrder> {
        self.orders.get(&order)
    }

    /// Every resting order, in ticket order.
    pub fn all(&self) -> impl Iterator<Item = &PendingOrder> {
        self.orders.values()
    }

    /// Every resting order for one account.
    pub fn for_account(&self, account: AccountId) -> impl Iterator<Item = &PendingOrder> {
        self.orders.values().filter(move |o| o.account == account)
    }

    /// Accepts an order onto the book.
    ///
    /// Validates only what is intrinsic to resting — size, tick grid and
    /// affordability are the risk layer's business and were checked before
    /// this is reached.
    #[allow(clippy::too_many_arguments)]
    pub fn place(
        &mut self,
        order: OrderId,
        account: AccountId,
        instrument: InstrumentId,
        side: Side,
        kind: OrderType,
        qty: Qty,
        trigger_price: Price,
        limit_price: Price,
        tif: TimeInForce,
    ) -> Result<PendingOrder, PendingError> {
        if kind == OrderType::Market {
            return Err(PendingError::NotRestable(kind));
        }
        if trigger_price <= Price::ZERO {
            return Err(PendingError::InvalidTrigger);
        }
        if kind == OrderType::StopLimit {
            if limit_price <= Price::ZERO {
                return Err(PendingError::InvalidLimit);
            }
        } else if !limit_price.is_zero() {
            return Err(PendingError::InvalidLimit);
        }
        if self.orders.contains_key(&order) {
            return Err(PendingError::Duplicate(order));
        }

        let resting = PendingOrder {
            order,
            account,
            instrument,
            side,
            kind,
            qty,
            trigger_price,
            limit_price,
            tif,
            placed: self.next_arrival,
        };
        self.next_arrival += 1;

        self.orders.insert(order, resting);
        self.by_instrument
            .entry(instrument)
            .or_default()
            .push(order);
        Ok(resting)
    }

    /// Removes an order, returning it if it was there.
    pub fn cancel(&mut self, order: OrderId) -> Option<PendingOrder> {
        let removed = self.orders.remove(&order)?;
        if let Some(list) = self.by_instrument.get_mut(&removed.instrument) {
            list.retain(|o| *o != order);
            if list.is_empty() {
                self.by_instrument.remove(&removed.instrument);
            }
        }
        Some(removed)
    }

    /// Removes every order on one account, as a stop-out or suspension does.
    pub fn cancel_account(&mut self, account: AccountId) -> Vec<PendingOrder> {
        let doomed: Vec<OrderId> = self
            .orders
            .values()
            .filter(|o| o.account == account)
            .map(|o| o.order)
            .collect();
        doomed.into_iter().filter_map(|o| self.cancel(o)).collect()
    }

    /// Which orders on `instrument` activate against this top of book.
    ///
    /// Ordered by placement, so two orders that trigger on the same tick fire
    /// in the sequence the trader placed them. Does not mutate: the engine
    /// decides what to do with each activation and removes them itself, because
    /// an order refused on margin must not vanish silently.
    pub fn activated(&self, instrument: InstrumentId, bid: Price, ask: Price) -> Vec<Activation> {
        let Some(ids) = self.by_instrument.get(&instrument) else {
            return Vec::new();
        };
        let mut hits: Vec<Activation> = ids
            .iter()
            .filter_map(|id| self.orders.get(id))
            .filter_map(|o| activates(o, bid, ask))
            .collect();
        hits.sort_by_key(|a| a.order().placed);
        hits
    }

    /// Converts a triggered stop-limit into a resting limit order.
    ///
    /// Keeps the ticket, so the trader sees one order through its whole life
    /// rather than a cancellation and a mysterious new one.
    pub fn convert_to_limit(&mut self, order: OrderId) -> Option<PendingOrder> {
        let resting = self.orders.get_mut(&order)?;
        if resting.kind != OrderType::StopLimit {
            return None;
        }
        resting.kind = OrderType::Limit;
        resting.trigger_price = resting.limit_price;
        resting.limit_price = Price::ZERO;
        Some(*resting)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BTC: InstrumentId = InstrumentId(1);
    const TRADER: AccountId = AccountId(1001);

    fn p(s: &str) -> Price {
        Price::parse(s).expect("price")
    }
    fn q(s: &str) -> Qty {
        Qty::parse(s).expect("qty")
    }

    fn resting(kind: OrderType, side: Side, trigger: &str) -> PendingOrder {
        PendingOrder {
            order: OrderId(1),
            account: TRADER,
            instrument: BTC,
            side,
            kind,
            qty: q("1"),
            trigger_price: p(trigger),
            limit_price: Price::ZERO,
            tif: TimeInForce::GoodTilCancelled,
            placed: 0,
        }
    }

    // ── The table in the module header, one row per test ──

    #[test]
    fn a_buy_limit_waits_for_the_ask_to_fall_to_it() {
        let o = resting(OrderType::Limit, Side::Buy, "70000.00");
        assert!(
            activates(&o, p("70500.00"), p("70510.00")).is_none(),
            "still above"
        );
        assert!(
            activates(&o, p("69990.00"), p("70000.00")).is_some(),
            "ask touched it"
        );

        // And it fills at the ask, not at its own limit, so improvement is kept.
        let hit = activates(&o, p("69800.00"), p("69810.00")).unwrap();
        assert_eq!(hit.price(), p("69810.00"));
        assert!(
            hit.price() < o.trigger_price,
            "the client got a better price"
        );
    }

    #[test]
    fn a_sell_limit_waits_for_the_bid_to_rise_to_it() {
        let o = resting(OrderType::Limit, Side::Sell, "90000.00");
        assert!(
            activates(&o, p("89990.00"), p("90000.00")).is_none(),
            "the bid is what counts"
        );
        let hit = activates(&o, p("90100.00"), p("90110.00")).unwrap();
        assert_eq!(hit.price(), p("90100.00"));
        assert!(hit.price() > o.trigger_price);
    }

    #[test]
    fn a_buy_stop_waits_for_the_ask_to_rise_through_it() {
        let o = resting(OrderType::Stop, Side::Buy, "90000.00");
        assert!(activates(&o, p("89980.00"), p("89990.00")).is_none());
        assert_eq!(
            activates(&o, p("89995.00"), p("90000.00")).unwrap().price(),
            p("90000.00")
        );
    }

    #[test]
    fn a_sell_stop_waits_for_the_bid_to_fall_through_it() {
        let o = resting(OrderType::Stop, Side::Sell, "70000.00");
        assert!(activates(&o, p("70010.00"), p("70020.00")).is_none());
        assert_eq!(
            activates(&o, p("70000.00"), p("70010.00")).unwrap().price(),
            p("70000.00")
        );
    }

    #[test]
    fn a_stop_fills_where_the_market_gapped_to_not_at_its_trigger() {
        // The honest, unpopular behaviour. A stop is not a guaranteed price,
        // and a book that pretended otherwise would owe the difference.
        let o = resting(OrderType::Stop, Side::Sell, "70000.00");
        let hit = activates(&o, p("64000.00"), p("64010.00")).unwrap();
        assert_eq!(hit.price(), p("64000.00"));
        assert!(
            hit.price() < o.trigger_price,
            "slippage is real and visible"
        );
    }

    #[test]
    fn a_market_order_never_rests() {
        let o = resting(OrderType::Market, Side::Buy, "70000.00");
        assert!(activates(&o, p("1.00"), p("1.00")).is_none());

        let mut book = PendingBook::new();
        let err = book
            .place(
                OrderId(1),
                TRADER,
                BTC,
                Side::Buy,
                OrderType::Market,
                q("1"),
                p("70000.00"),
                Price::ZERO,
                TimeInForce::GoodTilCancelled,
            )
            .unwrap_err();
        assert_eq!(err, PendingError::NotRestable(OrderType::Market));
    }

    #[test]
    fn touching_the_trigger_exactly_activates() {
        // Off-by-one here means an order that never fires at its own price.
        let buy = resting(OrderType::Limit, Side::Buy, "70000.00");
        assert!(activates(&buy, p("69990.00"), p("70000.00")).is_some());

        let sell = resting(OrderType::Limit, Side::Sell, "90000.00");
        assert!(activates(&sell, p("90000.00"), p("90010.00")).is_some());
    }

    // ── The book ──

    fn book_with(kind: OrderType, side: Side, trigger: &str, id: u64) -> (PendingBook, OrderId) {
        let mut b = PendingBook::new();
        let ticket = OrderId(id);
        b.place(
            ticket,
            TRADER,
            BTC,
            side,
            kind,
            q("1"),
            p(trigger),
            Price::ZERO,
            TimeInForce::GoodTilCancelled,
        )
        .expect("places");
        (b, ticket)
    }

    #[test]
    fn a_placed_order_can_be_found_and_cancelled() {
        let (mut b, ticket) = book_with(OrderType::Limit, Side::Buy, "70000.00", 7);
        assert_eq!(b.len(), 1);
        assert_eq!(b.get(ticket).map(|o| o.trigger_price), Some(p("70000.00")));

        assert!(b.cancel(ticket).is_some());
        assert!(b.is_empty());
        assert!(b.cancel(ticket).is_none(), "cancelling twice is a no-op");
    }

    #[test]
    fn a_duplicate_ticket_is_refused() {
        let (mut b, ticket) = book_with(OrderType::Limit, Side::Buy, "70000.00", 7);
        let err = b
            .place(
                ticket,
                TRADER,
                BTC,
                Side::Buy,
                OrderType::Limit,
                q("1"),
                p("71000.00"),
                Price::ZERO,
                TimeInForce::GoodTilCancelled,
            )
            .unwrap_err();
        assert_eq!(err, PendingError::Duplicate(ticket));
    }

    #[test]
    fn a_stop_limit_needs_a_limit_price_and_the_others_must_not_have_one() {
        let mut b = PendingBook::new();

        let missing = b.place(
            OrderId(1),
            TRADER,
            BTC,
            Side::Buy,
            OrderType::StopLimit,
            q("1"),
            p("90000.00"),
            Price::ZERO,
            TimeInForce::GoodTilCancelled,
        );
        assert_eq!(missing.unwrap_err(), PendingError::InvalidLimit);

        let spurious = b.place(
            OrderId(2),
            TRADER,
            BTC,
            Side::Buy,
            OrderType::Limit,
            q("1"),
            p("70000.00"),
            p("70100.00"),
            TimeInForce::GoodTilCancelled,
        );
        assert_eq!(spurious.unwrap_err(), PendingError::InvalidLimit);

        assert!(b
            .place(
                OrderId(3),
                TRADER,
                BTC,
                Side::Buy,
                OrderType::StopLimit,
                q("1"),
                p("90000.00"),
                p("90100.00"),
                TimeInForce::GoodTilCancelled,
            )
            .is_ok());
    }

    #[test]
    fn a_zero_trigger_is_refused() {
        let mut b = PendingBook::new();
        let err = b
            .place(
                OrderId(1),
                TRADER,
                BTC,
                Side::Buy,
                OrderType::Limit,
                q("1"),
                Price::ZERO,
                Price::ZERO,
                TimeInForce::GoodTilCancelled,
            )
            .unwrap_err();
        assert_eq!(err, PendingError::InvalidTrigger);
    }

    #[test]
    fn only_the_ticked_instrument_is_inspected() {
        let mut b = PendingBook::new();
        b.place(
            OrderId(1),
            TRADER,
            BTC,
            Side::Buy,
            OrderType::Limit,
            q("1"),
            p("70000.00"),
            Price::ZERO,
            TimeInForce::GoodTilCancelled,
        )
        .unwrap();
        b.place(
            OrderId(2),
            TRADER,
            InstrumentId(2),
            Side::Buy,
            OrderType::Limit,
            q("1"),
            p("70000.00"),
            Price::ZERO,
            TimeInForce::GoodTilCancelled,
        )
        .unwrap();

        let hits = b.activated(BTC, p("69000.00"), p("69010.00"));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].order().order, OrderId(1));
    }

    #[test]
    fn orders_triggering_on_one_tick_fire_in_the_order_they_were_placed() {
        let mut b = PendingBook::new();
        for id in [5u64, 3, 9] {
            b.place(
                OrderId(id),
                TRADER,
                BTC,
                Side::Buy,
                OrderType::Limit,
                q("1"),
                p("70000.00"),
                Price::ZERO,
                TimeInForce::GoodTilCancelled,
            )
            .unwrap();
        }
        let hits = b.activated(BTC, p("69000.00"), p("69010.00"));
        let tickets: Vec<u64> = hits.iter().map(|h| h.order().order.0).collect();
        assert_eq!(tickets, vec![5, 3, 9], "placement order, not ticket order");
    }

    #[test]
    fn activation_does_not_remove_the_order() {
        // The engine removes it only once it knows what happened. An order
        // refused on margin must not disappear without a journaled decision.
        let (b, _) = book_with(OrderType::Limit, Side::Buy, "70000.00", 1);
        assert_eq!(b.activated(BTC, p("69000.00"), p("69010.00")).len(), 1);
        assert_eq!(b.len(), 1, "still resting");
    }

    #[test]
    fn a_triggered_stop_limit_becomes_a_limit_and_keeps_its_ticket() {
        let mut b = PendingBook::new();
        let ticket = OrderId(4);
        b.place(
            ticket,
            TRADER,
            BTC,
            Side::Buy,
            OrderType::StopLimit,
            q("1"),
            p("90000.00"),
            p("90100.00"),
            TimeInForce::GoodTilCancelled,
        )
        .unwrap();

        let hits = b.activated(BTC, p("89990.00"), p("90000.00"));
        assert!(matches!(hits[0], Activation::BecomesLimit { .. }));

        let converted = b.convert_to_limit(ticket).expect("converts");
        assert_eq!(
            converted.order, ticket,
            "same ticket through its whole life"
        );
        assert_eq!(converted.kind, OrderType::Limit);
        assert_eq!(converted.trigger_price, p("90100.00"));
        assert_eq!(converted.limit_price, Price::ZERO);

        // Converting a plain limit is meaningless and refused.
        assert!(b.convert_to_limit(ticket).is_none());
    }

    #[test]
    fn cancelling_an_account_clears_only_its_own_orders() {
        let mut b = PendingBook::new();
        b.place(
            OrderId(1),
            TRADER,
            BTC,
            Side::Buy,
            OrderType::Limit,
            q("1"),
            p("70000.00"),
            Price::ZERO,
            TimeInForce::GoodTilCancelled,
        )
        .unwrap();
        b.place(
            OrderId(2),
            AccountId(2002),
            BTC,
            Side::Buy,
            OrderType::Limit,
            q("1"),
            p("70000.00"),
            Price::ZERO,
            TimeInForce::GoodTilCancelled,
        )
        .unwrap();

        let cleared = b.cancel_account(TRADER);
        assert_eq!(cleared.len(), 1);
        assert_eq!(b.len(), 1);
        assert_eq!(b.for_account(AccountId(2002)).count(), 1);
    }

    #[test]
    fn the_instrument_index_is_cleaned_up_when_the_last_order_goes() {
        let (mut b, ticket) = book_with(OrderType::Limit, Side::Buy, "70000.00", 1);
        b.cancel(ticket);
        assert!(b.activated(BTC, p("1.00"), p("1.00")).is_empty());
        assert!(b.is_empty());
    }
}
