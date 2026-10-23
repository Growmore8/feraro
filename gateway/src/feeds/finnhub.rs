//! Finnhub: FX via OANDA, stocks and crypto. Needs `FINNHUB_API_KEY`.
//!
//! The WebSocket streams *trades*, not quotes: each message is a batch of
//! last prices with no bid or ask. They go to the aggregator as last prices,
//! and the instrument's configured spread makes them two-sided. That makes
//! Finnhub a fine backup and a questionable primary for anything a trader
//! will hit on the spread — which is why it sits second on the FX routes.
//!
//! One subscribe message per symbol, and the server sends `{"type":"ping"}`
//! frames that need no reply.

use std::time::Instant;

use serde::Deserialize;
use serde_json::value::RawValue;

use super::{number, FeedUpdate, Observation, Provider, Source, Subscriptions};

const HOST: &str = "wss://ws.finnhub.io";

pub struct Finnhub {
    pub api_key: String,
    pub subscriptions: Subscriptions,
}

#[derive(Deserialize)]
struct Frame {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    data: Vec<Trade>,
    msg: Option<String>,
}

#[derive(Deserialize)]
struct Trade {
    s: String,
    p: Box<RawValue>,
}

impl Provider for Finnhub {
    fn source(&self) -> Source {
        Source::FINNHUB
    }

    fn label(&self) -> &'static str {
        "finnhub"
    }

    fn url(&self) -> String {
        format!("{HOST}?token={}", self.api_key)
    }

    fn on_open(&self) -> Vec<String> {
        self.subscriptions
            .iter()
            .map(|(symbol, _)| {
                serde_json::json!({ "type": "subscribe", "symbol": symbol }).to_string()
            })
            .collect()
    }

    fn parse(&self, text: &str, now: Instant) -> Vec<FeedUpdate> {
        let Ok(frame) = serde_json::from_str::<Frame>(text) else {
            return Vec::new();
        };
        match frame.kind.as_deref() {
            Some("trade") => {}
            Some("error") => {
                eprintln!(
                    "feed finnhub: provider error: {}",
                    frame.msg.as_deref().unwrap_or("no message")
                );
                return Vec::new();
            }
            _ => return Vec::new(),
        }

        // A batch can hold several trades for one symbol. Only the last one
        // matters; the aggregator would discard the rest anyway.
        let mut latest: Vec<FeedUpdate> = Vec::new();
        for trade in &frame.data {
            let Some(instrument) = self
                .subscriptions
                .iter()
                .find(|(s, _)| *s == trade.s)
                .map(|(_, id)| *id)
            else {
                continue;
            };
            let Some(price) = number::price(&trade.p) else {
                continue;
            };
            let update = FeedUpdate {
                instrument,
                source: Source::FINNHUB,
                observation: Observation::last(price, now),
            };
            match latest.iter_mut().find(|u| u.instrument == instrument) {
                Some(existing) => *existing = update,
                None => latest.push(update),
            }
        }
        latest
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::InstrumentId;
    use money::Price;

    fn provider() -> Finnhub {
        Finnhub {
            api_key: "k".into(),
            subscriptions: vec![
                ("OANDA:EUR_USD".into(), InstrumentId(20)),
                ("BINANCE:BTCUSDT".into(), InstrumentId(1)),
            ],
        }
    }

    #[test]
    fn reads_the_documented_trade_message() {
        // Verbatim from Finnhub's documentation.
        let frame = r#"{"data":[{"p":7296.89,"s":"BINANCE:BTCUSDT","t":1575526691134,"v":0.011467}],"type":"trade"}"#;
        let out = provider().parse(frame, Instant::now());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].instrument, InstrumentId(1));
        assert_eq!(out[0].observation.last, Price::parse("7296.89").ok());
        assert_eq!(out[0].observation.bid, None, "Finnhub trades carry no book");
    }

    #[test]
    fn keeps_only_the_last_trade_per_symbol_in_a_batch() {
        let frame = r#"{"type":"trade","data":[
            {"p":1.15380,"s":"OANDA:EUR_USD","t":1,"v":0},
            {"p":1.15390,"s":"OANDA:EUR_USD","t":2,"v":0},
            {"p":78000.5,"s":"BINANCE:BTCUSDT","t":3,"v":1}
        ]}"#;
        let out = provider().parse(frame, Instant::now());
        assert_eq!(out.len(), 2);
        let eur = out
            .iter()
            .find(|u| u.instrument == InstrumentId(20))
            .unwrap();
        assert_eq!(eur.observation.last, Price::parse("1.1539").ok());
    }

    #[test]
    fn subscribes_one_symbol_per_message() {
        let open = provider().on_open();
        assert_eq!(open.len(), 2);
        assert!(open[0].contains("OANDA:EUR_USD"));
    }

    #[test]
    fn ignores_pings_and_reports_errors_without_emitting_prices() {
        let p = provider();
        assert!(p.parse(r#"{"type":"ping"}"#, Instant::now()).is_empty());
        assert!(p
            .parse(r#"{"type":"error","msg":"Invalid symbol"}"#, Instant::now())
            .is_empty());
    }
}
