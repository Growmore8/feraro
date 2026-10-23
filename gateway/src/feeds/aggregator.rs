//! Which feed prices each instrument, moment to moment.
//!
//! # The rules
//!
//! 1. **Priority, then freshness.** Each instrument has an ordered list of
//!    sources. The highest-priority source whose last quote is younger than
//!    `max_age` prices it.
//! 2. **Fail over at once, fail back slowly.** When the active source goes
//!    stale, the next fresh one takes over on its very next quote. When a
//!    higher-priority source comes back, it must stay fresh for `rejoin_after`
//!    before it reclaims the instrument. Without that, a primary that drops
//!    every few seconds flips the price source back and forth, and a trader
//!    watching the chart sees the spread jump for no reason they can see.
//! 3. **Never invent a bid or an ask from nothing.** A source that sends both
//!    is used as sent. A source that sends only a last price — Finnhub always,
//!    Twelve Data on most plans — gets the instrument's configured half-spread
//!    either side. That is a broker markup, and it is labelled as one.
//! 4. **Refuse what cannot be a market.** A crossed quote (bid above ask), a
//!    non-positive price, or an empty frame is rejected, not repaired.
//!
//! Pure: time is passed in, nothing is spawned, and every rule above has a
//! test.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use domain::InstrumentId;
use money::Price;

use super::Source;

/// What one provider said about one instrument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    pub bid: Option<Price>,
    pub ask: Option<Price>,
    /// A last trade or indicative price, for feeds with no book.
    pub last: Option<Price>,
    /// When the gateway received it.
    pub at: Instant,
}

impl Observation {
    /// A two-sided observation.
    pub fn quote(bid: Price, ask: Price, at: Instant) -> Self {
        Observation {
            bid: Some(bid),
            ask: Some(ask),
            last: None,
            at,
        }
    }

    /// A last-price-only observation.
    pub fn last(price: Price, at: Instant) -> Self {
        Observation {
            bid: None,
            ask: None,
            last: Some(price),
            at,
        }
    }
}

/// How one instrument is sourced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// Highest priority first.
    pub sources: Vec<Source>,
    /// Applied either side of a last price. Zero means a last price quotes at
    /// no spread at all, which is allowed and almost never what you want.
    pub half_spread: Price,
    /// Older than this, a source's quote does not count.
    pub max_age: Duration,
    /// How long a returning higher-priority source must stay fresh before it
    /// takes the instrument back.
    pub rejoin_after: Duration,
}

/// A quote the engine should apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Published {
    pub instrument: InstrumentId,
    pub source: Source,
    pub bid: Price,
    pub ask: Price,
    /// True when this quote is the first from a newly active source.
    pub switched: bool,
}

/// Why an observation was not used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    /// The instrument has no route, or this source is not on it.
    Unrouted,
    /// Neither a two-sided quote nor a last price.
    Empty,
    /// Bid above ask.
    Crossed,
    /// A price at or below zero, including a spread wider than the price.
    NonPositive,
}

/// Turns an observation into a bid and an ask, or refuses it.
fn two_sided(obs: &Observation, half_spread: Price) -> Result<(Price, Price), Rejected> {
    match (obs.bid, obs.ask, obs.last) {
        (Some(bid), Some(ask), _) => {
            if bid <= Price::ZERO || ask <= Price::ZERO {
                return Err(Rejected::NonPositive);
            }
            if bid > ask {
                return Err(Rejected::Crossed);
            }
            Ok((bid, ask))
        }
        (_, _, Some(last)) => {
            if last <= Price::ZERO {
                return Err(Rejected::NonPositive);
            }
            let bid = last.sub(half_spread).map_err(|_| Rejected::NonPositive)?;
            let ask = last.add(half_spread).map_err(|_| Rejected::NonPositive)?;
            if bid <= Price::ZERO {
                return Err(Rejected::NonPositive);
            }
            Ok((bid, ask))
        }
        _ => Err(Rejected::Empty),
    }
}

/// Holds the latest word from every source and decides who is believed.
#[derive(Debug, Default)]
pub struct Aggregator {
    routes: BTreeMap<InstrumentId, Route>,
    latest: BTreeMap<(InstrumentId, Source), (Price, Price, Instant)>,
    fresh_since: BTreeMap<(InstrumentId, Source), Instant>,
    active: BTreeMap<InstrumentId, Source>,
}

impl Aggregator {
    /// An aggregator over these routes.
    pub fn new(routes: BTreeMap<InstrumentId, Route>) -> Self {
        Aggregator {
            routes,
            ..Aggregator::default()
        }
    }

    /// The source currently pricing an instrument.
    pub fn active(&self, instrument: InstrumentId) -> Option<Source> {
        self.active.get(&instrument).copied()
    }

    /// Takes one observation and says what, if anything, to publish.
    pub fn observe(
        &mut self,
        instrument: InstrumentId,
        source: Source,
        obs: Observation,
        now: Instant,
    ) -> Result<Option<Published>, Rejected> {
        let route = self.routes.get(&instrument).ok_or(Rejected::Unrouted)?;
        if !route.sources.contains(&source) {
            return Err(Rejected::Unrouted);
        }
        let (bid, ask) = two_sided(&obs, route.half_spread)?;
        let max_age = route.max_age;

        let key = (instrument, source);
        // A source is continuously fresh while its quotes arrive no further
        // apart than max_age. A gap restarts the clock rejoin_after is
        // measured against.
        let continuous = self
            .latest
            .get(&key)
            .is_some_and(|(_, _, at)| obs.at.saturating_duration_since(*at) <= max_age);
        if !continuous {
            self.fresh_since.insert(key, obs.at);
        }
        self.latest.insert(key, (bid, ask, obs.at));

        let previous = self.active(instrument);
        let Some(chosen) = self.choose(instrument, previous, now) else {
            return Ok(None);
        };

        let switched = previous != Some(chosen);
        if switched {
            self.active.insert(instrument, chosen);
        }
        if chosen != source && !switched {
            return Ok(None);
        }
        let Some(&(bid, ask, _)) = self.latest.get(&(instrument, chosen)) else {
            return Ok(None);
        };
        Ok(Some(Published {
            instrument,
            source: chosen,
            bid,
            ask,
            switched,
        }))
    }

    fn is_fresh(&self, instrument: InstrumentId, source: Source, now: Instant) -> bool {
        let Some(route) = self.routes.get(&instrument) else {
            return false;
        };
        self.latest
            .get(&(instrument, source))
            .is_some_and(|(_, _, at)| now.saturating_duration_since(*at) <= route.max_age)
    }

    fn choose(
        &self,
        instrument: InstrumentId,
        previous: Option<Source>,
        now: Instant,
    ) -> Option<Source> {
        let route = self.routes.get(&instrument)?;
        let previous_fresh = previous.is_some_and(|p| self.is_fresh(instrument, p, now));

        for &candidate in &route.sources {
            if !self.is_fresh(instrument, candidate, now) {
                continue;
            }
            if Some(candidate) == previous {
                return Some(candidate);
            }
            if previous_fresh {
                // Sources are walked in priority order, so reaching a fresh
                // candidate before the fresh incumbent means it outranks it.
                // It only takes over once it has proved it is back.
                let settled = self
                    .fresh_since
                    .get(&(instrument, candidate))
                    .is_some_and(|since| {
                        now.saturating_duration_since(*since) >= route.rejoin_after
                    });
                if settled {
                    return Some(candidate);
                }
                continue;
            }
            return Some(candidate);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EURUSD: InstrumentId = InstrumentId(20);
    const A: Source = Source::TWELVE_DATA;
    const B: Source = Source::FINNHUB;

    fn p(s: &str) -> Price {
        Price::parse(s).expect("price")
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn agg() -> Aggregator {
        let mut routes = BTreeMap::new();
        routes.insert(
            EURUSD,
            Route {
                sources: vec![A, B],
                half_spread: p("0.00005"),
                max_age: ms(5_000),
                rejoin_after: ms(10_000),
            },
        );
        Aggregator::new(routes)
    }

    #[test]
    fn a_two_sided_quote_is_used_exactly_as_sent() {
        let mut g = agg();
        let t = Instant::now();
        let out = g
            .observe(
                EURUSD,
                A,
                Observation::quote(p("1.15380"), p("1.15390"), t),
                t,
            )
            .unwrap()
            .unwrap();
        assert_eq!((out.bid, out.ask), (p("1.15380"), p("1.15390")));
        assert!(out.switched, "the first source to speak becomes active");
    }

    #[test]
    fn a_last_price_gets_the_configured_spread_either_side() {
        let mut g = agg();
        let t = Instant::now();
        let out = g
            .observe(EURUSD, A, Observation::last(p("1.15385"), t), t)
            .unwrap()
            .unwrap();
        assert_eq!(out.bid, p("1.15380"));
        assert_eq!(out.ask, p("1.15390"));
    }

    #[test]
    fn what_cannot_be_a_market_is_refused_not_repaired() {
        let mut g = agg();
        let t = Instant::now();
        assert_eq!(
            g.observe(EURUSD, A, Observation::quote(p("1.16"), p("1.15"), t), t),
            Err(Rejected::Crossed)
        );
        let empty = Observation {
            bid: None,
            ask: None,
            last: None,
            at: t,
        };
        assert_eq!(g.observe(EURUSD, A, empty, t), Err(Rejected::Empty));
        assert_eq!(
            g.observe(EURUSD, A, Observation::last(p("0.00001"), t), t),
            Err(Rejected::NonPositive),
            "a spread wider than the price leaves no bid"
        );
        assert_eq!(
            g.observe(InstrumentId(999), A, Observation::last(p("1"), t), t),
            Err(Rejected::Unrouted)
        );
        assert_eq!(
            g.observe(EURUSD, Source::BINANCE, Observation::last(p("1.15"), t), t),
            Err(Rejected::Unrouted),
            "a source not on the route cannot price the instrument"
        );
    }

    #[test]
    fn a_lower_priority_source_is_ignored_while_the_primary_is_fresh() {
        let mut g = agg();
        let t = Instant::now();
        g.observe(EURUSD, A, Observation::last(p("1.15385"), t), t)
            .unwrap();

        let out = g
            .observe(
                EURUSD,
                B,
                Observation::last(p("1.15999"), t + ms(100)),
                t + ms(100),
            )
            .unwrap();
        assert_eq!(out, None, "the backup's price does not leak through");
        assert_eq!(g.active(EURUSD), Some(A));
    }

    #[test]
    fn a_stale_primary_fails_over_on_the_backups_next_quote() {
        let mut g = agg();
        let t = Instant::now();
        g.observe(EURUSD, A, Observation::last(p("1.15385"), t), t)
            .unwrap();

        // Six seconds of silence from A, then B speaks.
        let later = t + ms(6_000);
        let out = g
            .observe(EURUSD, B, Observation::last(p("1.15400"), later), later)
            .unwrap()
            .expect("the backup takes over");
        assert_eq!(out.source, B);
        assert!(out.switched);
        assert_eq!(g.active(EURUSD), Some(B));
    }

    #[test]
    fn a_returning_primary_waits_before_taking_the_price_back() {
        // Fail over at once, fail back slowly: no flapping.
        let mut g = agg();
        let t = Instant::now();
        g.observe(EURUSD, A, Observation::last(p("1.15385"), t), t)
            .unwrap();
        let t1 = t + ms(6_000);
        g.observe(EURUSD, B, Observation::last(p("1.15400"), t1), t1)
            .unwrap();
        assert_eq!(g.active(EURUSD), Some(B));

        // A comes back and ticks every second. B keeps ticking too.
        let mut now = t1;
        for _ in 0..9 {
            now += ms(1_000);
            g.observe(EURUSD, B, Observation::last(p("1.15400"), now), now)
                .unwrap();
            let back = g
                .observe(EURUSD, A, Observation::last(p("1.15385"), now), now)
                .unwrap();
            assert_eq!(back, None, "not yet — A has not proved itself");
            assert_eq!(g.active(EURUSD), Some(B));
        }

        // A's probation began at its first return, one second after t1. Past
        // rejoin_after of continuous freshness from then, A reclaims it.
        now += ms(2_500);
        let reclaimed = g
            .observe(EURUSD, A, Observation::last(p("1.15385"), now), now)
            .unwrap()
            .expect("A is back");
        assert_eq!(reclaimed.source, A);
        assert!(reclaimed.switched);

        // B's next tick is now the backup's, and goes nowhere.
        let ignored = g
            .observe(EURUSD, B, Observation::last(p("1.15400"), now), now)
            .unwrap();
        assert_eq!(ignored, None);
    }

    #[test]
    fn a_gap_in_the_returning_source_restarts_its_probation() {
        let mut g = agg();
        let t = Instant::now();
        g.observe(EURUSD, A, Observation::last(p("1.15385"), t), t)
            .unwrap();
        let t1 = t + ms(6_000);
        g.observe(EURUSD, B, Observation::last(p("1.15400"), t1), t1)
            .unwrap();

        // A returns for a while, drops out for longer than max_age, returns.
        let back = t1 + ms(1_000);
        g.observe(EURUSD, A, Observation::last(p("1.15385"), back), back)
            .unwrap();
        let again = back + ms(8_000);
        g.observe(EURUSD, B, Observation::last(p("1.15400"), again), again)
            .unwrap();
        g.observe(EURUSD, A, Observation::last(p("1.15385"), again), again)
            .unwrap();

        // Eleven seconds after its first return, but only three into its
        // current unbroken run: still not trusted.
        let check = again + ms(3_000);
        g.observe(EURUSD, B, Observation::last(p("1.15400"), check), check)
            .unwrap();
        g.observe(EURUSD, A, Observation::last(p("1.15385"), check), check)
            .unwrap();
        assert_eq!(g.active(EURUSD), Some(B));
    }

    #[test]
    fn when_every_source_is_stale_nothing_is_published() {
        let mut g = agg();
        let t = Instant::now();
        // An observation that arrived long ago, processed now.
        let old = Observation::last(p("1.15385"), t);
        let out = g.observe(EURUSD, A, old, t + ms(60_000)).unwrap();
        assert_eq!(out, None, "an old price is not a price");
    }

    #[test]
    fn only_the_active_sources_updates_are_published() {
        let mut g = agg();
        let t = Instant::now();
        let first = g
            .observe(EURUSD, A, Observation::last(p("1.15385"), t), t)
            .unwrap();
        assert!(first.is_some());

        let t2 = t + ms(200);
        let second = g
            .observe(EURUSD, A, Observation::last(p("1.15390"), t2), t2)
            .unwrap()
            .unwrap();
        assert!(
            !second.switched,
            "an update from the incumbent is not a switch"
        );
        assert_eq!(second.bid, p("1.15385"));
    }
}
