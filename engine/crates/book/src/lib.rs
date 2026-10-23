//! Price-time priority order book and matching.
//!
//! # Invariant P0-5 — one writer, no locks
//!
//! An [`OrderBook`] is deliberately not `Sync`-friendly by design intent: it is
//! owned by exactly one thread, which applies orders in a serialised loop. That
//! is the whole of our concurrency story for the trade path.
//!
//! Locking a shared book from many threads is the standard way these systems
//! develop the race that lets two orders both pass a margin check against the
//! same free balance and leave an account with negative equity. A single
//! writer makes that state unreachable rather than unlikely.
//!
//! # Known gap
//!
//! Self-match prevention is not implemented. On a pure B-book it cannot
//! trigger, because the house is always the counterparty; it becomes required
//! in P5 when client flow meets client flow on an internalised book.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic)]

use std::collections::{BTreeMap, VecDeque};

use domain::{AccountId, InstrumentId, OrderId, OrderType, TimeInForce};
use money::{ArithError, Price, Qty, Side};

/// An order resting on the book, waiting for a counterparty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestingOrder {
    /// Client order identity.
    pub order: OrderId,
    /// Owning account.
    pub account: AccountId,
    /// Price it rests at.
    pub price: Price,
    /// Size still unfilled.
    pub remaining: Qty,
    /// Arrival counter, giving time priority within a price level.
    pub arrival: u64,
}

/// A trade between an aggressing order and a resting one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fill {
    /// The aggressing order.
    pub taker: OrderId,
    /// The resting order that provided liquidity.
    pub maker: OrderId,
    /// Size traded.
    pub qty: Qty,
    /// Price traded at — always the maker's resting price, so the taker
    /// receives any price improvement.
    pub price: Price,
}

/// An order arriving at the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Incoming {
    /// Client order identity.
    pub order: OrderId,
    /// Owning account.
    pub account: AccountId,
    /// Direction.
    pub side: Side,
    /// Pricing behaviour. `Stop` and `StopLimit` are triggered upstream and
    /// arrive here as `Market` and `Limit` respectively.
    pub kind: OrderType,
    /// Size in base units.
    pub qty: Qty,
    /// Limit price. Ignored for a market order.
    pub price: Price,
    /// Lifetime.
    pub tif: TimeInForce,
}

/// What happened to an incoming order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchResult {
    /// Trades generated, in execution order.
    pub fills: Vec<Fill>,
    /// Size that neither filled nor rested.
    pub cancelled: Qty,
    /// Size now resting on the book.
    pub resting: Qty,
}

impl MatchResult {
    /// Total size that traded.
    pub fn filled(&self) -> Result<Qty, ArithError> {
        let mut total = Qty::ZERO;
        for f in &self.fills {
            total = total.add(f.qty)?;
        }
        Ok(total)
    }

    /// True when nothing traded.
    pub fn is_empty(&self) -> bool {
        self.fills.is_empty()
    }
}

/// A single instrument's book.
#[derive(Debug)]
pub struct OrderBook {
    instrument: InstrumentId,
    bids: BTreeMap<Price, VecDeque<RestingOrder>>,
    asks: BTreeMap<Price, VecDeque<RestingOrder>>,
    locate: BTreeMap<OrderId, (Side, Price)>,
    arrival: u64,
}

impl OrderBook {
    /// An empty book for `instrument`.
    pub fn new(instrument: InstrumentId) -> Self {
        OrderBook {
            instrument,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            locate: BTreeMap::new(),
            arrival: 0,
        }
    }

    /// The instrument this book trades.
    pub fn instrument(&self) -> InstrumentId {
        self.instrument
    }

    /// Highest price anyone is willing to pay.
    pub fn best_bid(&self) -> Option<Price> {
        self.bids.keys().next_back().copied()
    }

    /// Lowest price anyone is willing to sell at.
    pub fn best_ask(&self) -> Option<Price> {
        self.asks.keys().next().copied()
    }

    /// Difference between best ask and best bid, when both sides are populated.
    pub fn spread(&self) -> Option<Price> {
        match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) => a.sub(b).ok(),
            _ => None,
        }
    }

    /// Total resting size at a given price on a given side.
    pub fn depth_at(&self, side: Side, price: Price) -> Qty {
        let levels = match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        };
        levels
            .get(&price)
            .map(|q| {
                q.iter()
                    .try_fold(Qty::ZERO, |acc, o| acc.add(o.remaining))
                    .unwrap_or(Qty::ZERO)
            })
            .unwrap_or(Qty::ZERO)
    }

    /// Number of orders resting on a side.
    pub fn order_count(&self, side: Side) -> usize {
        let levels = match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        };
        levels.values().map(VecDeque::len).sum()
    }

    /// True when neither side holds an order.
    pub fn is_empty(&self) -> bool {
        self.bids.is_empty() && self.asks.is_empty()
    }

    /// Whether `resting` is a price an aggressor at `limit` will accept.
    fn crosses(taker: Side, limit: Option<Price>, resting: Price) -> bool {
        match limit {
            None => true, // market order takes any price
            Some(l) => match taker {
                Side::Buy => resting <= l,
                Side::Sell => resting >= l,
            },
        }
    }

    /// Best price on the side opposite `taker`.
    fn best_opposite(&self, taker: Side) -> Option<Price> {
        match taker {
            Side::Buy => self.best_ask(),
            Side::Sell => self.best_bid(),
        }
    }

    /// Size available to an aggressor at `limit` or better.
    pub fn available_to(&self, taker: Side, limit: Option<Price>) -> Result<Qty, ArithError> {
        let levels = match taker {
            Side::Buy => &self.asks,
            Side::Sell => &self.bids,
        };
        let mut total = Qty::ZERO;
        let ordered: Vec<(&Price, &VecDeque<RestingOrder>)> = match taker {
            Side::Buy => levels.iter().collect(),
            Side::Sell => levels.iter().rev().collect(),
        };
        for (price, queue) in ordered {
            if !Self::crosses(taker, limit, *price) {
                break;
            }
            for order in queue {
                total = total.add(order.remaining)?;
            }
        }
        Ok(total)
    }

    /// Applies an incoming order, matching then resting whatever is left.
    pub fn submit(&mut self, incoming: Incoming) -> Result<MatchResult, ArithError> {
        let limit = match incoming.kind {
            OrderType::Market | OrderType::Stop => None,
            OrderType::Limit | OrderType::StopLimit => Some(incoming.price),
        };

        // Fill-or-kill is decided before a single fill is generated, so a
        // rejected FOK leaves the book exactly as it found it.
        if incoming.tif == TimeInForce::FillOrKill {
            let available = self.available_to(incoming.side, limit)?;
            if available < incoming.qty {
                return Ok(MatchResult {
                    fills: Vec::new(),
                    cancelled: incoming.qty,
                    resting: Qty::ZERO,
                });
            }
        }

        let mut fills = Vec::new();
        let mut remaining = incoming.qty;

        while !remaining.is_zero() {
            let Some(price) = self.best_opposite(incoming.side) else {
                break;
            };
            if !Self::crosses(incoming.side, limit, price) {
                break;
            }

            let levels = match incoming.side {
                Side::Buy => &mut self.asks,
                Side::Sell => &mut self.bids,
            };
            let Some(queue) = levels.get_mut(&price) else {
                break;
            };

            // Time priority: always take from the front of the queue.
            while !remaining.is_zero() {
                let Some(maker) = queue.front_mut() else {
                    break;
                };
                let traded = if maker.remaining <= remaining {
                    maker.remaining
                } else {
                    remaining
                };

                fills.push(Fill {
                    taker: incoming.order,
                    maker: maker.order,
                    qty: traded,
                    price,
                });

                maker.remaining = maker.remaining.sub(traded)?;
                remaining = remaining.sub(traded)?;

                if maker.remaining.is_zero() {
                    let done = queue.pop_front().expect("front exists");
                    self.locate.remove(&done.order);
                }
            }

            if queue.is_empty() {
                levels.remove(&price);
            }
        }

        // Whatever did not fill either rests or is cancelled.
        let rests = matches!(incoming.kind, OrderType::Limit | OrderType::StopLimit)
            && matches!(
                incoming.tif,
                TimeInForce::GoodTilCancelled | TimeInForce::Day
            );

        let (cancelled, resting) = if remaining.is_zero() {
            (Qty::ZERO, Qty::ZERO)
        } else if rests {
            self.rest(incoming, remaining);
            (Qty::ZERO, remaining)
        } else {
            (remaining, Qty::ZERO)
        };

        Ok(MatchResult {
            fills,
            cancelled,
            resting,
        })
    }

    /// Places the unfilled remainder on the book.
    fn rest(&mut self, incoming: Incoming, remaining: Qty) {
        let order = RestingOrder {
            order: incoming.order,
            account: incoming.account,
            price: incoming.price,
            remaining,
            arrival: self.arrival,
        };
        self.arrival += 1;

        let levels = match incoming.side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        levels.entry(incoming.price).or_default().push_back(order);
        self.locate
            .insert(incoming.order, (incoming.side, incoming.price));
    }

    /// Withdraws a resting order, returning the size that never traded.
    pub fn cancel(&mut self, order: OrderId) -> Option<Qty> {
        let (side, price) = self.locate.remove(&order)?;
        let levels = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        let queue = levels.get_mut(&price)?;
        let idx = queue.iter().position(|o| o.order == order)?;
        let removed = queue.remove(idx)?;
        if queue.is_empty() {
            levels.remove(&price);
        }
        Some(removed.remaining)
    }

    /// Resting orders on a side, best price first, time ordered within a price.
    pub fn resting(&self, side: Side) -> Vec<RestingOrder> {
        let levels = match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        };
        let ordered: Vec<&VecDeque<RestingOrder>> = match side {
            Side::Buy => levels.values().rev().collect(),
            Side::Sell => levels.values().collect(),
        };
        ordered
            .into_iter()
            .flat_map(|q| q.iter().copied())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INSTRUMENT: InstrumentId = InstrumentId(1);

    fn px(s: &str) -> Price {
        Price::parse(s).expect("price")
    }

    fn qt(s: &str) -> Qty {
        Qty::parse(s).expect("qty")
    }

    fn limit(order: u64, account: u64, side: Side, qty: &str, price: &str) -> Incoming {
        Incoming {
            order: OrderId(order),
            account: AccountId(account),
            side,
            kind: OrderType::Limit,
            qty: qt(qty),
            price: px(price),
            tif: TimeInForce::GoodTilCancelled,
        }
    }

    fn market(order: u64, account: u64, side: Side, qty: &str) -> Incoming {
        Incoming {
            order: OrderId(order),
            account: AccountId(account),
            side,
            kind: OrderType::Market,
            qty: qt(qty),
            price: Price::ZERO,
            tif: TimeInForce::ImmediateOrCancel,
        }
    }

    /// A book with three asks and two bids around 1.0853.
    fn seeded() -> OrderBook {
        let mut b = OrderBook::new(INSTRUMENT);
        b.submit(limit(1, 10, Side::Sell, "100000", "1.08540"))
            .unwrap();
        b.submit(limit(2, 11, Side::Sell, "150000", "1.08545"))
            .unwrap();
        b.submit(limit(3, 12, Side::Sell, "200000", "1.08550"))
            .unwrap();
        b.submit(limit(4, 13, Side::Buy, "100000", "1.08530"))
            .unwrap();
        b.submit(limit(5, 14, Side::Buy, "250000", "1.08525"))
            .unwrap();
        b
    }

    #[test]
    fn a_non_crossing_limit_rests() {
        let b = seeded();
        assert_eq!(b.best_bid(), Some(px("1.08530")));
        assert_eq!(b.best_ask(), Some(px("1.08540")));
        assert_eq!(b.spread(), Some(px("0.00010")));
        assert_eq!(b.order_count(Side::Buy), 2);
        assert_eq!(b.order_count(Side::Sell), 3);
    }

    #[test]
    fn a_market_buy_takes_the_best_ask_first() {
        let mut b = seeded();
        let r = b.submit(market(6, 20, Side::Buy, "100000")).unwrap();

        assert_eq!(r.fills.len(), 1);
        assert_eq!(r.fills[0].maker, OrderId(1));
        assert_eq!(r.fills[0].price, px("1.08540"));
        assert_eq!(r.fills[0].qty, qt("100000"));
        assert_eq!(r.resting, Qty::ZERO);
        assert_eq!(b.best_ask(), Some(px("1.08545")));
    }

    #[test]
    fn a_large_market_order_walks_the_book_in_price_order() {
        let mut b = seeded();
        let r = b.submit(market(6, 20, Side::Buy, "300000")).unwrap();

        assert_eq!(r.fills.len(), 3);
        assert_eq!(r.fills[0].price, px("1.08540"));
        assert_eq!(r.fills[1].price, px("1.08545"));
        assert_eq!(r.fills[2].price, px("1.08550"));
        assert_eq!(r.fills[2].qty, qt("50000"));
        assert_eq!(r.filled().unwrap(), qt("300000"));
        assert_eq!(b.depth_at(Side::Sell, px("1.08550")), qt("150000"));
    }

    #[test]
    fn price_time_priority_fills_the_earlier_order_first() {
        let mut b = OrderBook::new(INSTRUMENT);
        b.submit(limit(1, 10, Side::Sell, "100000", "1.08540"))
            .unwrap();
        b.submit(limit(2, 11, Side::Sell, "100000", "1.08540"))
            .unwrap();

        let r = b.submit(market(3, 20, Side::Buy, "150000")).unwrap();
        assert_eq!(r.fills.len(), 2);
        assert_eq!(
            r.fills[0].maker,
            OrderId(1),
            "earlier order must fill first"
        );
        assert_eq!(r.fills[0].qty, qt("100000"));
        assert_eq!(r.fills[1].maker, OrderId(2));
        assert_eq!(r.fills[1].qty, qt("50000"));
    }

    #[test]
    fn the_taker_receives_the_price_improvement() {
        // A buyer willing to pay 1.08560 meets an ask resting at 1.08540 and
        // trades at 1.08540, not at their own limit.
        let mut b = seeded();
        let r = b
            .submit(limit(6, 20, Side::Buy, "100000", "1.08560"))
            .unwrap();
        assert_eq!(r.fills[0].price, px("1.08540"));
    }

    #[test]
    fn a_crossing_limit_fills_what_it_can_and_rests_the_rest() {
        let mut b = seeded();
        let r = b
            .submit(limit(6, 20, Side::Buy, "400000", "1.08545"))
            .unwrap();

        assert_eq!(r.filled().unwrap(), qt("250000"));
        assert_eq!(r.resting, qt("150000"));
        assert_eq!(r.cancelled, Qty::ZERO);
        assert_eq!(b.best_bid(), Some(px("1.08545")));
        assert_eq!(b.depth_at(Side::Buy, px("1.08545")), qt("150000"));
    }

    #[test]
    fn immediate_or_cancel_never_rests() {
        let mut b = seeded();
        let mut order = limit(6, 20, Side::Buy, "400000", "1.08545");
        order.tif = TimeInForce::ImmediateOrCancel;

        let r = b.submit(order).unwrap();
        assert_eq!(r.filled().unwrap(), qt("250000"));
        assert_eq!(r.cancelled, qt("150000"));
        assert_eq!(r.resting, Qty::ZERO);
        assert_eq!(
            b.best_bid(),
            Some(px("1.08530")),
            "book must be unchanged on the bid"
        );
    }

    #[test]
    fn fill_or_kill_that_cannot_complete_leaves_the_book_untouched() {
        let mut b = seeded();
        let before = b.resting(Side::Sell);

        let mut order = limit(6, 20, Side::Buy, "500000", "1.08545");
        order.tif = TimeInForce::FillOrKill;
        let r = b.submit(order).unwrap();

        assert!(r.is_empty(), "no fills may be generated");
        assert_eq!(r.cancelled, qt("500000"));
        assert_eq!(b.resting(Side::Sell), before, "book must be byte-identical");
    }

    #[test]
    fn fill_or_kill_that_can_complete_does() {
        let mut b = seeded();
        let mut order = limit(6, 20, Side::Buy, "250000", "1.08545");
        order.tif = TimeInForce::FillOrKill;

        let r = b.submit(order).unwrap();
        assert_eq!(r.filled().unwrap(), qt("250000"));
        assert_eq!(r.cancelled, Qty::ZERO);
        assert_eq!(b.best_ask(), Some(px("1.08550")));
    }

    #[test]
    fn a_market_order_into_an_empty_book_is_cancelled_not_rested() {
        let mut b = OrderBook::new(INSTRUMENT);
        let r = b.submit(market(1, 10, Side::Buy, "100000")).unwrap();
        assert!(r.is_empty());
        assert_eq!(r.cancelled, qt("100000"));
        assert!(b.is_empty());
    }

    #[test]
    fn cancelling_removes_the_order_and_returns_its_unfilled_size() {
        let mut b = seeded();
        assert_eq!(b.cancel(OrderId(2)), Some(qt("150000")));
        assert_eq!(b.order_count(Side::Sell), 2);
        assert_eq!(b.depth_at(Side::Sell, px("1.08545")), Qty::ZERO);
        assert_eq!(
            b.cancel(OrderId(2)),
            None,
            "cancelling twice must be a no-op"
        );
    }

    #[test]
    fn cancelling_a_partially_filled_order_returns_only_the_remainder() {
        let mut b = seeded();
        b.submit(market(6, 20, Side::Buy, "150000")).unwrap();
        // Order 1 fully filled, order 2 has 100_000 of 150_000 left.
        assert_eq!(b.cancel(OrderId(1)), None);
        assert_eq!(b.cancel(OrderId(2)), Some(qt("100000")));
    }

    #[test]
    fn emptied_price_levels_are_removed() {
        let mut b = seeded();
        b.submit(market(6, 20, Side::Buy, "250000")).unwrap();
        assert_eq!(b.depth_at(Side::Sell, px("1.08540")), Qty::ZERO);
        assert_eq!(b.depth_at(Side::Sell, px("1.08545")), Qty::ZERO);
        assert_eq!(b.best_ask(), Some(px("1.08550")));
        assert_eq!(b.order_count(Side::Sell), 1);
    }

    #[test]
    fn available_size_respects_the_limit_price() {
        let b = seeded();
        assert_eq!(b.available_to(Side::Buy, None).unwrap(), qt("450000"));
        assert_eq!(
            b.available_to(Side::Buy, Some(px("1.08545"))).unwrap(),
            qt("250000")
        );
        assert_eq!(
            b.available_to(Side::Buy, Some(px("1.08530"))).unwrap(),
            Qty::ZERO
        );
        assert_eq!(b.available_to(Side::Sell, None).unwrap(), qt("350000"));
        assert_eq!(
            b.available_to(Side::Sell, Some(px("1.08530"))).unwrap(),
            qt("100000")
        );
    }

    #[test]
    fn a_sell_walks_the_bids_downward() {
        let mut b = seeded();
        let r = b.submit(market(6, 20, Side::Sell, "300000")).unwrap();
        assert_eq!(r.fills.len(), 2);
        assert_eq!(r.fills[0].price, px("1.08530"));
        assert_eq!(r.fills[1].price, px("1.08525"));
        assert_eq!(r.fills[1].qty, qt("200000"));
        assert_eq!(b.depth_at(Side::Buy, px("1.08525")), qt("50000"));
    }

    #[test]
    fn resting_orders_come_back_best_price_first() {
        let b = seeded();
        let asks = b.resting(Side::Sell);
        assert_eq!(asks[0].price, px("1.08540"));
        assert_eq!(asks[2].price, px("1.08550"));

        let bids = b.resting(Side::Buy);
        assert_eq!(bids[0].price, px("1.08530"));
        assert_eq!(bids[1].price, px("1.08525"));
    }

    #[test]
    fn the_same_sequence_of_orders_always_produces_the_same_fills() {
        // Determinism is what makes journal replay meaningful. Two books fed
        // identical input must agree on every fill, in order.
        let run = || {
            let mut b = seeded();
            let mut all = Vec::new();
            for (i, size) in ["120000", "80000", "310000", "50000"].iter().enumerate() {
                let side = if i % 2 == 0 { Side::Buy } else { Side::Sell };
                all.extend(
                    b.submit(market(100 + i as u64, 20, side, size))
                        .unwrap()
                        .fills,
                );
            }
            all
        };
        assert_eq!(run(), run());
    }
}
