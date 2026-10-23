//! Bars built from the prices the gateway has seen.
//!
//! # Why the gateway keeps them at all
//!
//! The terminal's charts came from the exchanges, which works for crypto and
//! not at all for anything else: Binance has no EURUSD and no gold. The
//! gateway is the only thing that sees every instrument's price, so it is the
//! only thing that can draw them.
//!
//! # What this is not
//!
//! Not a time-series database. Bars live in memory, capped, and are rebuilt by
//! replaying the journal's quotes at startup — which means the history only
//! goes back as far as the journal, and a fresh install has none. That is an
//! honest limit and the terminal says so rather than drawing a flat line.
//! Production archives ticks in ClickHouse and serves bars from there; this is
//! what makes the chart work in the meantime.
//!
//! # One store, many intervals
//!
//! Only one-minute bars are kept. Everything longer is folded from them on
//! request, because five one-minute bars *are* a five-minute bar — storing
//! both would be two numbers that can disagree.

use std::collections::BTreeMap;

use domain::InstrumentId;
use money::Price;

/// One-minute bars kept per instrument.
///
/// Twenty-five hours, so a daily bar has a full day behind it and the
/// four-hourly ones have six. At 22 instruments this is about a megabyte.
pub const MINUTES_KEPT: usize = 1_500;

/// The longest history the gateway will send in one answer.
pub const MAX_BARS: usize = 500;

/// One bar. Prices are mids: a chart is about where the market was, and the
/// side a position would close on is the blotter's question, not the chart's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bar {
    /// Start of the bucket, in epoch seconds.
    pub start: i64,
    pub open: Price,
    pub high: Price,
    pub low: Price,
    pub close: Price,
}

impl Bar {
    fn new(start: i64, price: Price) -> Self {
        Bar {
            start,
            open: price,
            high: price,
            low: price,
            close: price,
        }
    }

    fn extend(&mut self, price: Price) {
        if price > self.high {
            self.high = price;
        }
        if price < self.low {
            self.low = price;
        }
        self.close = price;
    }

    /// Folds a later bar into this one, as aggregating an interval does.
    fn absorb(&mut self, other: &Bar) {
        if other.high > self.high {
            self.high = other.high;
        }
        if other.low < self.low {
            self.low = other.low;
        }
        self.close = other.close;
    }
}

/// Chart intervals the gateway serves, in seconds.
///
/// The same set the terminal offers. Anything not on this list is refused
/// rather than approximated: a chart labelled 7m that is really 5m is worse
/// than no chart.
pub const fn seconds_of(interval: &str) -> Option<i64> {
    match interval.as_bytes() {
        b"1m" => Some(60),
        b"5m" => Some(300),
        b"15m" => Some(900),
        b"1h" => Some(3_600),
        b"4h" => Some(14_400),
        b"1d" => Some(86_400),
        _ => None,
    }
}

/// Minute bars for every instrument, oldest first.
#[derive(Debug, Default)]
pub struct Candles {
    bars: BTreeMap<InstrumentId, Vec<Bar>>,
}

impl Candles {
    pub fn new() -> Self {
        Candles::default()
    }

    /// Folds one quote into the current minute.
    ///
    /// `at_nanos` is the engine's journaled time, not the clock: replaying the
    /// journal must rebuild the same bars, and a bar stamped with the time of
    /// the replay would be a different one.
    pub fn observe(&mut self, instrument: InstrumentId, bid: Price, ask: Price, at_nanos: i64) {
        let Ok(mid) = bid.midpoint(ask) else {
            return;
        };
        if mid <= Price::ZERO {
            return;
        }
        let start = (at_nanos / 1_000_000_000).div_euclid(60) * 60;
        let series = self.bars.entry(instrument).or_default();

        match series.last_mut() {
            // The same minute: widen it.
            Some(last) if last.start == start => last.extend(mid),
            // A minute that has already closed, from a quote arriving out of
            // order. Dropped rather than appended: a bar list that is not
            // ordered is not a chart.
            Some(last) if last.start > start => {}
            _ => {
                series.push(Bar::new(start, mid));
                if series.len() > MINUTES_KEPT {
                    series.remove(0);
                }
            }
        }
    }

    /// How many minute bars one instrument has.
    ///
    /// The tests' way of asking whether a tick was charted at all, and what
    /// an operations screen would show if one asked how much history there
    /// is — which is a question worth being able to answer.
    #[allow(dead_code, reason = "a chart-coverage screen is not built yet")]
    pub fn minutes(&self, instrument: InstrumentId) -> usize {
        self.bars.get(&instrument).map_or(0, Vec::len)
    }

    /// The most recent `limit` bars of `seconds`, oldest first.
    ///
    /// Buckets are aligned to the epoch, so a four-hour bar starts at 00:00,
    /// 04:00 and so on in UTC — the same boundaries every exchange uses, and
    /// the same ones a reload will produce.
    pub fn series(&self, instrument: InstrumentId, seconds: i64, limit: usize) -> Vec<Bar> {
        let Some(minutes) = self.bars.get(&instrument) else {
            return Vec::new();
        };
        if seconds <= 60 {
            let take = limit.min(minutes.len());
            return minutes[minutes.len() - take..].to_vec();
        }

        let mut out: Vec<Bar> = Vec::new();
        for bar in minutes {
            let start = bar.start.div_euclid(seconds) * seconds;
            match out.last_mut() {
                Some(current) if current.start == start => current.absorb(bar),
                _ => {
                    let mut fresh = *bar;
                    fresh.start = start;
                    out.push(fresh);
                }
            }
        }
        let take = limit.min(out.len());
        out.split_off(out.len() - take)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BTC: InstrumentId = InstrumentId(1);

    fn p(s: &str) -> Price {
        Price::parse(s).expect("price")
    }

    /// Nanoseconds for a whole number of seconds since the epoch.
    fn at(seconds: i64) -> i64 {
        seconds * 1_000_000_000
    }

    #[test]
    fn ticks_in_one_minute_become_one_bar() {
        let mut c = Candles::new();
        c.observe(BTC, p("100.00"), p("100.02"), at(0));
        c.observe(BTC, p("102.00"), p("102.02"), at(15));
        c.observe(BTC, p("98.00"), p("98.02"), at(30));
        c.observe(BTC, p("101.00"), p("101.02"), at(59));

        let bars = c.series(BTC, 60, 10);
        assert_eq!(bars.len(), 1);
        let bar = bars[0];
        assert_eq!(bar.start, 0);
        // Mids throughout: the chart is about where the market was.
        assert_eq!(bar.open, p("100.01"));
        assert_eq!(bar.high, p("102.01"));
        assert_eq!(bar.low, p("98.01"));
        assert_eq!(bar.close, p("101.01"));
    }

    #[test]
    fn a_new_minute_starts_a_new_bar() {
        let mut c = Candles::new();
        c.observe(BTC, p("100.00"), p("100.00"), at(30));
        c.observe(BTC, p("105.00"), p("105.00"), at(61));
        let bars = c.series(BTC, 60, 10);
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].start, 0);
        assert_eq!(bars[1].start, 60);
        assert_eq!(bars[1].open, p("105.00"));
    }

    #[test]
    fn longer_intervals_are_folded_from_the_minutes() {
        let mut c = Candles::new();
        // Five minutes, each a single price: 10, 20, 5, 30, 15.
        for (minute, price) in [(0, "10"), (1, "20"), (2, "5"), (3, "30"), (4, "15")] {
            c.observe(BTC, p(price), p(price), at(minute * 60 + 1));
        }
        let five = c.series(BTC, 300, 10);
        assert_eq!(
            five.len(),
            1,
            "five one-minute bars are one five-minute bar"
        );
        assert_eq!(five[0].start, 0);
        assert_eq!(five[0].open, p("10"));
        assert_eq!(five[0].high, p("30"));
        assert_eq!(five[0].low, p("5"));
        assert_eq!(five[0].close, p("15"));

        // A sixth minute opens the next bucket.
        c.observe(BTC, p("7"), p("7"), at(5 * 60 + 1));
        let five = c.series(BTC, 300, 10);
        assert_eq!(five.len(), 2);
        assert_eq!(five[1].start, 300);
        assert_eq!(five[1].open, p("7"));
    }

    #[test]
    fn buckets_are_aligned_to_the_epoch_not_to_the_first_tick() {
        let mut c = Candles::new();
        // 01:30 and 02:10 on an hour boundary of 01:00–02:00.
        c.observe(BTC, p("10"), p("10"), at(90 * 60));
        c.observe(BTC, p("20"), p("20"), at(130 * 60));
        let hours = c.series(BTC, 3_600, 10);
        assert_eq!(hours.len(), 2);
        assert_eq!(hours[0].start, 3_600, "01:00, not 01:30");
        assert_eq!(hours[1].start, 7_200);
    }

    #[test]
    fn only_the_last_bars_are_returned_and_in_order() {
        let mut c = Candles::new();
        for minute in 0..10 {
            c.observe(BTC, p("10"), p("10"), at(minute * 60));
        }
        let bars = c.series(BTC, 60, 3);
        assert_eq!(bars.len(), 3);
        assert_eq!(bars[0].start, 7 * 60);
        assert_eq!(bars[2].start, 9 * 60, "oldest first, newest last");
    }

    #[test]
    fn the_store_is_capped_and_keeps_the_newest() {
        let mut c = Candles::new();
        for minute in 0..(MINUTES_KEPT as i64 + 50) {
            c.observe(BTC, p("10"), p("10"), at(minute * 60));
        }
        assert_eq!(c.minutes(BTC), MINUTES_KEPT);
        let bars = c.series(BTC, 60, MAX_BARS);
        assert_eq!(
            bars.last().unwrap().start,
            (MINUTES_KEPT as i64 + 49) * 60,
            "the newest bar survives; the oldest is what goes"
        );
    }

    #[test]
    fn a_quote_from_a_minute_that_has_closed_is_dropped() {
        let mut c = Candles::new();
        c.observe(BTC, p("10"), p("10"), at(0));
        c.observe(BTC, p("20"), p("20"), at(120));
        // Out of order: the 00:01 minute is already behind the latest bar.
        c.observe(BTC, p("99"), p("99"), at(60));

        let bars = c.series(BTC, 60, 10);
        assert_eq!(bars.len(), 2, "no bar is inserted in the middle");
        assert!(
            bars.windows(2).all(|w| w[0].start < w[1].start),
            "a bar list that is not ordered is not a chart"
        );
        assert_eq!(bars[1].high, p("20"), "and nothing is back-dated into it");
    }

    #[test]
    fn an_unquoted_instrument_has_no_bars_rather_than_an_empty_one() {
        let c = Candles::new();
        assert_eq!(c.minutes(InstrumentId(99)), 0);
        assert!(c.series(InstrumentId(99), 60, 10).is_empty());
    }

    #[test]
    fn crossed_and_impossible_prices_are_not_charted() {
        let mut c = Candles::new();
        c.observe(BTC, Price::ZERO, Price::ZERO, at(0));
        assert_eq!(c.minutes(BTC), 0, "zero is not a price");
    }

    #[test]
    fn every_interval_the_terminal_offers_is_one_the_gateway_knows() {
        for interval in ["1m", "5m", "15m", "1h", "4h", "1d"] {
            assert!(seconds_of(interval).is_some(), "{interval} is not served");
        }
        // And nothing else: an approximated interval is a mislabelled chart.
        assert_eq!(seconds_of("7m"), None);
        assert_eq!(seconds_of(""), None);
    }
}
