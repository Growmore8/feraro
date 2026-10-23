//! Twelve Data: FX, metals, stocks and crypto. Needs `TWELVEDATA_API_KEY`.
//!
//! Connects to the real-time price WebSocket, subscribes to every symbol in one
//! message, and must send a heartbeat every ten seconds or be disconnected.
//!
//! Price events carry `price`, and on some plans `bid` and `ask` as well. Both
//! are accepted, as strings or numbers. A two-sided event is used as sent; a
//! price-only event is passed to the aggregator as a last price, and the
//! instrument's configured spread is applied there.
//!
//! # Plan limits
//!
//! The free plan caps how many symbols one connection may stream. The gateway
//! subscribes to at most `TWELVEDATA_MAX_SYMBOLS` (eight by default), taking
//! tradeable instruments first, and logs any the provider reports as failed in
//! its `subscribe-status` reply.

use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::value::RawValue;

use super::{number, FeedUpdate, Observation, Provider, Source, Subscriptions};

const HOST: &str = "wss://ws.twelvedata.com/v1/quotes/price";

pub struct TwelveData {
    pub api_key: String,
    pub subscriptions: Subscriptions,
}

#[derive(Deserialize)]
struct Event {
    event: Option<String>,
    symbol: Option<String>,
    price: Option<Box<RawValue>>,
    bid: Option<Box<RawValue>>,
    ask: Option<Box<RawValue>>,
    #[serde(default)]
    fails: Vec<serde_json::Value>,
}

impl Provider for TwelveData {
    fn source(&self) -> Source {
        Source::TWELVE_DATA
    }

    fn label(&self) -> &'static str {
        "twelvedata"
    }

    fn url(&self) -> String {
        format!("{HOST}?apikey={}", self.api_key)
    }

    fn on_open(&self) -> Vec<String> {
        let symbols = self
            .subscriptions
            .iter()
            .map(|(s, _)| s.as_str())
            .collect::<Vec<_>>()
            .join(",");
        vec![serde_json::json!({
            "action": "subscribe",
            "params": { "symbols": symbols },
        })
        .to_string()]
    }

    fn heartbeat(&self) -> Option<(Duration, String)> {
        Some((
            Duration::from_secs(10),
            r#"{"action":"heartbeat"}"#.to_owned(),
        ))
    }

    fn parse(&self, text: &str, now: Instant) -> Vec<FeedUpdate> {
        let Ok(event) = serde_json::from_str::<Event>(text) else {
            return Vec::new();
        };

        match event.event.as_deref() {
            Some("subscribe-status") => {
                if !event.fails.is_empty() {
                    eprintln!(
                        "feed twelvedata: {} symbol(s) refused by the provider — check the plan's symbol limit: {}",
                        event.fails.len(),
                        serde_json::Value::Array(event.fails)
                    );
                }
                return Vec::new();
            }
            Some("price") => {}
            _ => return Vec::new(),
        }

        let Some(symbol) = event.symbol else {
            return Vec::new();
        };
        let Some(instrument) = self
            .subscriptions
            .iter()
            .find(|(s, _)| *s == symbol)
            .map(|(_, id)| *id)
        else {
            return Vec::new();
        };

        let bid = event.bid.as_deref().and_then(number::price);
        let ask = event.ask.as_deref().and_then(number::price);
        let last = event.price.as_deref().and_then(number::price);

        let observation = match (bid, ask, last) {
            (Some(b), Some(a), _) => Observation::quote(b, a, now),
            (_, _, Some(l)) => Observation::last(l, now),
            _ => return Vec::new(),
        };
        vec![FeedUpdate {
            instrument,
            source: Source::TWELVE_DATA,
            observation,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::InstrumentId;
    use money::Price;

    fn provider() -> TwelveData {
        TwelveData {
            api_key: "k".into(),
            subscriptions: vec![
                ("EUR/USD".into(), InstrumentId(20)),
                ("XAU/USD".into(), InstrumentId(24)),
            ],
        }
    }

    #[test]
    fn reads_a_price_only_event_as_a_last_price() {
        let frame = r#"{"event":"price","symbol":"EUR/USD","currency_base":"Euro","currency_quote":"US Dollar","exchange":"PHYSICAL CURRENCY","type":"Physical Currency","timestamp":1789377180,"price":1.15387}"#;
        let out = provider().parse(frame, Instant::now());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].instrument, InstrumentId(20));
        assert_eq!(out[0].observation.last, Price::parse("1.15387").ok());
        assert_eq!(out[0].observation.bid, None);
    }

    #[test]
    fn uses_bid_and_ask_when_the_plan_sends_them_as_strings() {
        let frame = r#"{"event":"price","symbol":"XAU/USD","price":"2378.58","bid":"2378.44","ask":"2378.72","last_quote_at":1631772000}"#;
        let out = provider().parse(frame, Instant::now());
        assert_eq!(out[0].observation.bid, Price::parse("2378.44").ok());
        assert_eq!(out[0].observation.ask, Price::parse("2378.72").ok());
    }

    #[test]
    fn subscribes_to_every_symbol_in_one_message_and_heartbeats() {
        let p = provider();
        let open = p.on_open();
        assert_eq!(open.len(), 1);
        assert!(open[0].contains("EUR/USD,XAU/USD"));
        let (every, frame) = p.heartbeat().unwrap();
        assert_eq!(every, Duration::from_secs(10));
        assert!(frame.contains("heartbeat"));
    }

    #[test]
    fn ignores_status_heartbeats_and_unknown_symbols() {
        let p = provider();
        let now = Instant::now();
        assert!(p
            .parse(r#"{"event":"heartbeat","status":"ok"}"#, now)
            .is_empty());
        assert!(p
            .parse(r#"{"event":"subscribe-status","status":"ok","success":[{"symbol":"EUR/USD"}],"fails":[]}"#, now)
            .is_empty());
        assert!(p
            .parse(r#"{"event":"price","symbol":"GBP/USD","price":1.27}"#, now)
            .is_empty());
    }
}
