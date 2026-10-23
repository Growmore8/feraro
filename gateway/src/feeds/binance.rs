//! Binance: crypto book tickers. No key.
//!
//! One combined stream of `<symbol>@bookTicker` channels, each pushing best bid
//! and ask on every change, prices as decimal strings.

use std::time::Instant;

use serde::Deserialize;
use serde_json::value::RawValue;

use super::{number, FeedUpdate, Observation, Provider, Source, Subscriptions};

const HOST: &str = "wss://stream.binance.com:9443/stream";

pub struct Binance {
    pub subscriptions: Subscriptions,
}

#[derive(Deserialize)]
struct Envelope {
    data: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
struct BookTicker {
    s: String,
    b: Box<RawValue>,
    a: Box<RawValue>,
}

impl Provider for Binance {
    fn source(&self) -> Source {
        Source::BINANCE
    }

    fn label(&self) -> &'static str {
        "binance"
    }

    fn url(&self) -> String {
        let streams = self
            .subscriptions
            .iter()
            .map(|(symbol, _)| format!("{}@bookTicker", symbol.to_lowercase()))
            .collect::<Vec<_>>()
            .join("/");
        format!("{HOST}?streams={streams}")
    }

    fn on_open(&self) -> Vec<String> {
        // Subscriptions ride in the URL.
        Vec::new()
    }

    fn parse(&self, text: &str, now: Instant) -> Vec<FeedUpdate> {
        let payload = match serde_json::from_str::<Envelope>(text) {
            Ok(Envelope { data: Some(inner) }) => inner.get().to_owned(),
            _ => text.to_owned(),
        };
        let Ok(ticker) = serde_json::from_str::<BookTicker>(&payload) else {
            return Vec::new();
        };
        let Some(instrument) = self
            .subscriptions
            .iter()
            .find(|(s, _)| s.eq_ignore_ascii_case(&ticker.s))
            .map(|(_, id)| *id)
        else {
            return Vec::new();
        };
        match (number::price(&ticker.b), number::price(&ticker.a)) {
            (Some(bid), Some(ask)) => vec![FeedUpdate {
                instrument,
                source: Source::BINANCE,
                observation: Observation::quote(bid, ask, now),
            }],
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::InstrumentId;
    use money::Price;

    fn provider() -> Binance {
        Binance {
            subscriptions: vec![("BTCUSDT".into(), InstrumentId(1))],
        }
    }

    #[test]
    fn reads_a_combined_stream_book_ticker() {
        let frame = r#"{"stream":"btcusdt@bookTicker","data":{"u":1,"s":"BTCUSDT","b":"78389.03000000","B":"0.5","a":"78389.04000000","A":"9.4"}}"#;
        let out = provider().parse(frame, Instant::now());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].observation.bid, Price::parse("78389.03").ok());
        assert_eq!(out[0].observation.ask, Price::parse("78389.04").ok());
    }

    #[test]
    fn builds_the_stream_url_from_its_subscriptions() {
        assert!(provider().url().ends_with("?streams=btcusdt@bookTicker"));
    }

    #[test]
    fn ignores_what_it_did_not_subscribe_to() {
        let p = provider();
        assert!(p
            .parse(
                r#"{"data":{"s":"XLMUSDT","b":"1","a":"2"}}"#,
                Instant::now()
            )
            .is_empty());
        assert!(p
            .parse(r#"{"result":null,"id":1}"#, Instant::now())
            .is_empty());
        assert!(p.parse("not json", Instant::now()).is_empty());
    }
}
