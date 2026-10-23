//! The instrument catalogue: contract terms, financing, and where each price
//! comes from.
//!
//! The gateway is the authority. The browser renders what the snapshot tells
//! it, and a symbol not listed here cannot be traded however the terminal is
//! edited.
//!
//! # Instrument ids are permanent
//!
//! Journals on disk hold positions by id. An id is never reused for a
//! different instrument, even after the original is delisted — which is why the
//! EUR/USD proxy keeps id 12 and real EUR/USD takes a new one.
//!
//! # Instruments quoted in another currency
//!
//! USD/JPY, USD/CAD and USD/CHF are quoted in yen, Canadian dollars and Swiss
//! francs, so their P&L and margin arise in that currency while the account is
//! in US dollars. [`conversions`] routes each of those currencies through the
//! pair that prices it — which is the same instrument the trader is trading —
//! and the engine converts every amount on the way to the balance. An
//! instrument whose currency has no route, or whose route has no price, is
//! refused rather than valued as if the currencies matched.

use std::time::Duration;

use domain::{Currency, Instrument, InstrumentId, Symbol};
use money::{Price, Qty};
use risk::{Conversion, InstrumentTerms};
use rollover::SwapTerms;

use crate::feeds::aggregator::Route;
use crate::feeds::Source;

/// One upstream symbol on one provider.
#[derive(Debug, Clone, Copy)]
pub struct Feed {
    pub source: Source,
    pub symbol: &'static str,
}

const fn feed(source: Source, symbol: &'static str) -> Feed {
    Feed { source, symbol }
}

/// One listed instrument.
pub struct Listing {
    pub terms: InstrumentTerms,
    /// Overnight financing.
    pub swap: SwapTerms,
    /// Where the price comes from, highest priority first.
    pub feeds: Vec<Feed>,
    /// Applied either side of a price-only feed's quote.
    pub half_spread: Price,
    /// Older than this, a quote is not a price: the aggregator will not use it
    /// and the engine will not fill on it.
    pub max_quote_age: Duration,
    /// Why a trader might not be able to trade it, if they cannot.
    pub note: Option<&'static str>,
}

impl Listing {
    /// The aggregator's view of this instrument.
    pub fn route(&self) -> Route {
        Route {
            sources: self.feeds.iter().map(|f| f.source).collect(),
            half_spread: self.half_spread,
            max_age: self.max_quote_age,
            // Long enough that a primary dropping out every few seconds does
            // not drag the price back and forth.
            rejoin_after: Duration::from_secs(10),
        }
    }
}

fn p(s: &str) -> Price {
    Price::parse(s).expect("literal price")
}

fn q(s: &str) -> Qty {
    Qty::parse(s).expect("literal qty")
}

/// Contract and order limits for one listing.
struct Contract {
    id: u32,
    symbol: &'static str,
    base: &'static [u8; 3],
    quote: &'static [u8; 3],
    digits: u8,
    tick: &'static str,
    /// Base units per lot. `1` for crypto, `100000` for FX, `100` for gold.
    contract: &'static str,
    min: &'static str,
    max: &'static str,
    step: &'static str,
}

fn listing(
    c: Contract,
    swap: SwapTerms,
    feeds: Vec<Feed>,
    half_spread: &str,
    max_quote_age: Duration,
    note: Option<&'static str>,
) -> Listing {
    let mut terms = InstrumentTerms::new(
        Instrument {
            id: InstrumentId(c.id),
            symbol: Symbol::new(c.symbol),
            base: Currency::new(c.base),
            quote: Currency::new(c.quote),
            display_digits: c.digits,
            contract_size: q(c.contract),
            tick_size: p(c.tick),
        },
        q(c.min),
        q(c.max),
        q(c.step),
    );
    // Anything carrying a note is not tradeable yet.
    terms.tradeable = note.is_none();
    Listing {
        terms,
        swap,
        feeds,
        half_spread: p(half_spread),
        max_quote_age,
        note,
    }
}

/// Demo financing for crypto: longs pay ten per cent a year, shorts five.
const CRYPTO: SwapTerms = SwapTerms::every_day(-1000, -500);
/// Demo financing for FX, on FX convention.
const FX: SwapTerms = SwapTerms::fx(-150, 50);
/// Demo financing for metals, on FX convention.
const METAL: SwapTerms = SwapTerms::fx(-350, 100);

/// Crypto ticks many times a second; five seconds of silence is an outage.
const CRYPTO_AGE: Duration = Duration::from_secs(5);
/// FX and metals from price-only feeds can go a few seconds between prints.
const FX_AGE: Duration = Duration::from_secs(15);

/// Currencies other than the account's, and the pair that prices each one.
///
/// A yen amount in a dollar book converts through USDJPY, which is already
/// listed as instrument 30: the pair a trader trades is the pair that values
/// their position, so there is nothing extra to subscribe to. The direction is
/// [`Conversion::PerSettlement`] because these pairs are quoted *per dollar* —
/// 147 yen to the dollar. A pair quoted the other way round, such as EURUSD
/// for a euro-denominated instrument, would route `InSettlement` instead.
pub fn conversions() -> Vec<(Currency, Conversion)> {
    vec![
        (
            Currency::new(b"JPY"),
            Conversion::PerSettlement(InstrumentId(30)),
        ),
        (
            Currency::new(b"CAD"),
            Conversion::PerSettlement(InstrumentId(31)),
        ),
        (
            Currency::new(b"CHF"),
            Conversion::PerSettlement(InstrumentId(32)),
        ),
    ]
}

const B: Source = Source::BINANCE;
const K: Source = Source::KRAKEN;
const T: Source = Source::TWELVE_DATA;
const F: Source = Source::FINNHUB;

/// A crypto pair, one coin to the lot.
struct Crypto {
    id: u32,
    symbol: &'static str,
    base: &'static [u8; 3],
    digits: u8,
    tick: &'static str,
    min: &'static str,
    max: &'static str,
    half_spread: &'static str,
}

fn crypto(c: Crypto, feeds: Vec<Feed>) -> Listing {
    listing(
        Contract {
            id: c.id,
            symbol: c.symbol,
            base: c.base,
            quote: b"USD",
            digits: c.digits,
            tick: c.tick,
            contract: "1",
            min: c.min,
            max: c.max,
            step: c.min,
        },
        CRYPTO,
        feeds,
        c.half_spread,
        CRYPTO_AGE,
        None,
    )
}

/// An FX pair, 100,000 base units to the lot, Twelve Data first then Finnhub.
struct Fx {
    id: u32,
    symbol: &'static str,
    base: &'static [u8; 3],
    quote: &'static [u8; 3],
    digits: u8,
    tick: &'static str,
    twelve: &'static str,
    finnhub: &'static str,
    half_spread: &'static str,
}

fn fx(c: Fx, note: Option<&'static str>) -> Listing {
    listing(
        Contract {
            id: c.id,
            symbol: c.symbol,
            base: c.base,
            quote: c.quote,
            digits: c.digits,
            tick: c.tick,
            contract: "100000",
            min: "1000",
            max: "5000000",
            step: "1000",
        },
        FX,
        vec![feed(T, c.twelve), feed(F, c.finnhub)],
        c.half_spread,
        FX_AGE,
        note,
    )
}

/// Everything the gateway lists.
pub fn catalogue() -> Vec<Listing> {
    let c = |id, symbol, base, digits, tick, min, max, half_spread| Crypto {
        id,
        symbol,
        base,
        digits,
        tick,
        min,
        max,
        half_spread,
    };
    #[allow(clippy::too_many_arguments)]
    let f = |id, symbol, base, quote, digits, tick, twelve, finnhub, half_spread| Fx {
        id,
        symbol,
        base,
        quote,
        digits,
        tick,
        twelve,
        finnhub,
        half_spread,
    };

    vec![
        // ── Crypto: Binance and Kraken first, because they send a real book ──
        crypto(
            c(1, "BTCUSD", b"BTC", 2, "0.01", "0.001", "10", "5.00"),
            vec![
                feed(B, "BTCUSDT"),
                feed(K, "BTC/USD"),
                feed(F, "BINANCE:BTCUSDT"),
            ],
        ),
        crypto(
            c(2, "ETHUSD", b"ETH", 2, "0.01", "0.01", "200", "0.50"),
            vec![
                feed(B, "ETHUSDT"),
                feed(K, "ETH/USD"),
                feed(F, "BINANCE:ETHUSDT"),
            ],
        ),
        crypto(
            c(3, "SOLUSD", b"SOL", 2, "0.01", "0.1", "5000", "0.02"),
            vec![feed(B, "SOLUSDT"), feed(K, "SOL/USD")],
        ),
        crypto(
            c(4, "XRPUSD", b"XRP", 4, "0.0001", "1", "200000", "0.0005"),
            vec![feed(B, "XRPUSDT"), feed(K, "XRP/USD")],
        ),
        crypto(
            c(5, "ADAUSD", b"ADA", 4, "0.0001", "1", "200000", "0.0003"),
            vec![feed(B, "ADAUSDT"), feed(K, "ADA/USD")],
        ),
        crypto(
            c(
                6, "DOGEUSD", b"DOG", 5, "0.00001", "10", "2000000", "0.00005",
            ),
            vec![feed(B, "DOGEUSDT"), feed(K, "DOGE/USD")],
        ),
        crypto(
            c(7, "LTCUSD", b"LTC", 2, "0.01", "0.1", "5000", "0.02"),
            vec![feed(B, "LTCUSDT"), feed(K, "LTC/USD")],
        ),
        crypto(
            c(8, "LINKUSD", b"LNK", 3, "0.001", "0.1", "20000", "0.005"),
            vec![feed(B, "LINKUSDT"), feed(K, "LINK/USD")],
        ),
        crypto(
            c(9, "DOTUSD", b"DOT", 3, "0.001", "0.1", "20000", "0.005"),
            vec![feed(B, "DOTUSDT"), feed(K, "DOT/USD")],
        ),
        crypto(
            c(10, "AVAXUSD", b"AVX", 2, "0.01", "0.1", "10000", "0.02"),
            vec![feed(B, "AVAXUSDT"), feed(K, "AVAX/USD")],
        ),
        crypto(
            c(11, "BNBUSD", b"BNB", 2, "0.01", "0.01", "2000", "0.10"),
            vec![feed(B, "BNBUSDT")],
        ),
        // Kept at id 12 for journals that already hold it. Real EUR/USD is 20.
        crypto(
            c(12, "EURUSDT", b"EUR", 4, "0.0001", "1", "500000", "0.0001"),
            vec![feed(B, "EURUSDT")],
        ),
        // ── FX majors, quoted in US dollars ──
        fx(
            f(
                20,
                "EURUSD",
                b"EUR",
                b"USD",
                5,
                "0.00001",
                "EUR/USD",
                "OANDA:EUR_USD",
                "0.00005",
            ),
            None,
        ),
        fx(
            f(
                21,
                "GBPUSD",
                b"GBP",
                b"USD",
                5,
                "0.00001",
                "GBP/USD",
                "OANDA:GBP_USD",
                "0.00008",
            ),
            None,
        ),
        fx(
            f(
                22,
                "AUDUSD",
                b"AUD",
                b"USD",
                5,
                "0.00001",
                "AUD/USD",
                "OANDA:AUD_USD",
                "0.00006",
            ),
            None,
        ),
        fx(
            f(
                23,
                "NZDUSD",
                b"NZD",
                b"USD",
                5,
                "0.00001",
                "NZD/USD",
                "OANDA:NZD_USD",
                "0.00008",
            ),
            None,
        ),
        // ── Metals, quoted in US dollars ──
        listing(
            Contract {
                id: 24,
                symbol: "XAUUSD",
                base: b"XAU",
                quote: b"USD",
                digits: 2,
                tick: "0.01",
                contract: "100",
                min: "1",
                max: "5000",
                step: "1",
            },
            METAL,
            vec![feed(T, "XAU/USD"), feed(F, "OANDA:XAU_USD")],
            "0.15",
            FX_AGE,
            None,
        ),
        listing(
            Contract {
                id: 25,
                symbol: "XAGUSD",
                base: b"XAG",
                quote: b"USD",
                digits: 3,
                tick: "0.001",
                contract: "5000",
                min: "50",
                max: "250000",
                step: "50",
            },
            METAL,
            vec![feed(T, "XAG/USD"), feed(F, "OANDA:XAG_USD")],
            "0.012",
            FX_AGE,
            None,
        ),
        // ── FX quoted in another currency ──
        //
        // Tradeable now: profit and margin arise in yen, Canadian dollars and
        // francs, and the engine converts each through the pair itself. See
        // `conversions` above, and invariant P1-3 in the `risk` crate. If the
        // pair loses its price the instrument stops being tradeable on its
        // own — the rate and the quote are the same number.
        fx(
            f(
                30,
                "USDJPY",
                b"USD",
                b"JPY",
                3,
                "0.001",
                "USD/JPY",
                "OANDA:USD_JPY",
                "0.008",
            ),
            None,
        ),
        fx(
            f(
                31,
                "USDCAD",
                b"USD",
                b"CAD",
                5,
                "0.00001",
                "USD/CAD",
                "OANDA:USD_CAD",
                "0.00008",
            ),
            None,
        ),
        fx(
            f(
                32,
                "USDCHF",
                b"USD",
                b"CHF",
                5,
                "0.00001",
                "USD/CHF",
                "OANDA:USD_CHF",
                "0.00008",
            ),
            None,
        ),
    ]
}

/// One provider's subscriptions, in the order they should be requested.
///
/// Tradeable instruments come first, so a plan's symbol cap drops view-only
/// ones before anything a trader can actually trade.
pub fn subscriptions(listings: &[Listing], source: Source) -> Vec<(String, InstrumentId)> {
    let mut picked: Vec<(bool, String, InstrumentId)> = listings
        .iter()
        .flat_map(|l| {
            l.feeds
                .iter()
                .filter(move |f| f.source == source)
                .map(move |f| {
                    (
                        l.terms.tradeable,
                        f.symbol.to_owned(),
                        l.terms.instrument.id,
                    )
                })
        })
        .collect();
    // A stable sort keeps catalogue order within each group.
    picked.sort_by_key(|(tradeable, _, _)| !*tradeable);
    picked.into_iter().map(|(_, s, id)| (s, id)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn instrument_ids_are_unique() {
        let listings = catalogue();
        let ids: BTreeSet<u32> = listings.iter().map(|l| l.terms.instrument.id.0).collect();
        assert_eq!(ids.len(), listings.len());
    }

    #[test]
    fn every_instrument_has_at_least_one_feed() {
        for l in catalogue() {
            assert!(
                !l.feeds.is_empty(),
                "{} has no price source",
                l.terms.instrument.symbol
            );
        }
    }

    #[test]
    fn every_quote_currency_the_book_lists_can_be_converted() {
        // A listing in a currency with no route is an instrument whose profit
        // cannot be put into an account. The engine would refuse every order
        // on it, which is safe but useless — catch it here instead.
        let routed: BTreeSet<Currency> = conversions().into_iter().map(|(c, _)| c).collect();
        for l in catalogue() {
            let quote = l.terms.instrument.quote;
            assert!(
                quote == Currency::USD || routed.contains(&quote),
                "{} is quoted in {quote}, which has no conversion route",
                l.terms.instrument.symbol
            );
        }
    }

    #[test]
    fn every_conversion_route_points_at_a_listed_pair_that_prices_it() {
        // The route for yen must be USDJPY itself: a route to an instrument
        // that is not quoted in that currency would convert at a rate for
        // something else entirely.
        let listings = catalogue();
        for (currency, how) in conversions() {
            let (Conversion::PerSettlement(id) | Conversion::InSettlement(id)) = how;
            let pair = listings
                .iter()
                .find(|l| l.terms.instrument.id == id)
                .unwrap_or_else(|| panic!("{currency} routes to unlisted instrument {}", id.0));
            match how {
                Conversion::PerSettlement(_) => {
                    assert_eq!(
                        pair.terms.instrument.quote, currency,
                        "{} is not quoted in {currency}",
                        pair.terms.instrument.symbol
                    );
                    assert_eq!(pair.terms.instrument.base, Currency::USD);
                }
                Conversion::InSettlement(_) => {
                    assert_eq!(pair.terms.instrument.base, currency);
                    assert_eq!(pair.terms.instrument.quote, Currency::USD);
                }
            }
        }
    }

    #[test]
    fn everything_listed_is_tradeable_or_says_why_not() {
        for l in catalogue() {
            assert_eq!(
                l.terms.tradeable,
                l.note.is_none(),
                "{} must carry a note when it cannot be traded",
                l.terms.instrument.symbol
            );
        }
    }

    #[test]
    fn tradeable_instruments_are_subscribed_before_view_only_ones() {
        let listings = catalogue();
        let twelve = subscriptions(&listings, Source::TWELVE_DATA);
        let first_view_only = twelve
            .iter()
            .position(|(s, _)| s.starts_with("USD/"))
            .expect("view-only pairs are listed");
        assert!(
            twelve[first_view_only..]
                .iter()
                .all(|(s, _)| s.starts_with("USD/")),
            "a plan's symbol cap must cut view-only pairs first"
        );
    }

    #[test]
    fn fx_lots_are_a_hundred_thousand_units() {
        let eur = catalogue()
            .into_iter()
            .find(|l| l.terms.instrument.symbol.as_str() == "EURUSD")
            .unwrap();
        assert_eq!(eur.terms.instrument.contract_size, q("100000"));
        assert_eq!(eur.terms.min_qty, q("1000"), "a micro lot");
    }
}
