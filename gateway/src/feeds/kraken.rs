//! Kraken: crypto tickers. No key.
//!
//! The v2 `ticker` channel sends best bid and ask as JSON *numbers*. They are
//! read as raw tokens (see [`number`](super::number)), so the digits Kraken
//! sent are the digits the engine gets.

use std::time::Instant;

use serde::Deserialize;
use serde_json::value::RawValue;

use super::{number, FeedUpdate, Observation, Provider, Source, Subscriptions};

const HOST: &str = "wss://ws.kraken.com/v2";

pub struct Kraken {
    pub subscriptions: Subscriptions,
}

#[derive(Deserialize)]
struct Frame {
    channel: Option<String>,
    #[serde(default)]
    data: Vec<Tick>,
}

#[derive(Deserialize)]
struct Tick {
    symbol: String,
    bid: Option<Box<RawValue>>,
    ask: Option<Box<RawValue>>,
}

impl Provider for Kraken {
    fn source(&self) -> Source {
        Source::KRAKEN
    }

    fn label(&self) -> &'static str {
        "kraken"
    }

    fn url(&self) -> String {
        HOST.to_owned()
    }

    fn on_open(&self) -> Vec<String> {
        let symbols: Vec<&str> = self.subscriptions.iter().map(|(s, _)| s.as_str()).collect();
        vec![serde_json::json!({
            "method": "subscribe",
            "params": { "channel": "ticker", "symbol": symbols },
        })
        .to_string()]
    }

    fn parse(&self, text: &str, now: Instant) -> Vec<FeedUpdate> {
        let Ok(frame) = serde_json::from_str::<Frame>(text) else {
            return Vec::new();
        };
        if frame.channel.as_deref() != Some("ticker") {
            return Vec::new();
        }
        frame
            .data
            .iter()
            .filter_map(|tick| {
                let instrument = self
                    .subscriptions
                    .iter()
                    .find(|(s, _)| *s == tick.symbol)
                    .map(|(_, id)| *id)?;
                let bid = number::price(tick.bid.as_deref()?)?;
                let ask = number::price(tick.ask.as_deref()?)?;
                Some(FeedUpdate {
                    instrument,
                    source: Source::KRAKEN,
                    observation: Observation::quote(bid, ask, now),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::InstrumentId;
    use money::Price;

    fn provider() -> Kraken {
        Kraken {
            subscriptions: vec![("BTC/USD".into(), InstrumentId(1))],
        }
    }

    #[test]
    fn reads_a_ticker_update_with_numeric_prices_exactly() {
        let frame = r#"{"channel":"ticker","type":"update","data":[{"symbol":"BTC/USD","bid":78345.0,"bid_qty":1,"ask":78345.1,"ask_qty":1,"last":78345.1}]}"#;
        let out = provider().parse(frame, Instant::now());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].observation.bid, Price::parse("78345.0").ok());
        assert_eq!(out[0].observation.ask, Price::parse("78345.1").ok());
    }

    #[test]
    fn subscribes_to_the_ticker_channel() {
        let open = provider().on_open();
        assert!(open[0].contains(r#""channel":"ticker""#));
        assert!(open[0].contains("BTC/USD"));
    }

    #[test]
    fn ignores_heartbeats_and_status() {
        let p = provider();
        assert!(p
            .parse(r#"{"channel":"heartbeat"}"#, Instant::now())
            .is_empty());
        assert!(p
            .parse(
                r#"{"channel":"status","data":[{"system":"online"}]}"#,
                Instant::now()
            )
            .is_empty());
        assert!(p
            .parse(r#"{"method":"subscribe","success":true}"#, Instant::now())
            .is_empty());
    }
}
