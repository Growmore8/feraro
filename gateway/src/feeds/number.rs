//! Exact prices from JSON, whether a provider sends a string or a number.
//!
//! Binance and Twelve Data's REST API send `"78389.03"`. Kraken's WebSocket and
//! Finnhub send `78389.03`. Parse the second into a `serde_json::Value` and it
//! becomes an `f64` before any code here runs — the same float the engine
//! forbids everywhere else.
//!
//! The obvious fix, serde_json's `arbitrary_precision` feature, keeps number
//! text exact but breaks internally tagged enums (a long-standing serde
//! limitation), and the gateway's own client protocol is one. So instead,
//! provider frames declare their price fields as [`RawValue`]: the untouched
//! source token, `78389.03` or `"78389.03"`, handed to the engine's own
//! decimal parser. No float ever exists.

use money::Price;
use serde_json::value::RawValue;

/// The exact decimal text of a raw JSON token, string or number.
///
/// Refuses exponent notation rather than guessing: `1e-5` is legal JSON, but
/// no feed we use sends it, and silently mis-scaling a price by five orders of
/// magnitude is not a failure mode worth risking for a case that does not
/// occur.
pub fn decimal_text(raw: &RawValue) -> Option<&str> {
    let token = raw.get().trim();
    let inner = token
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(token);
    if inner.is_empty() || inner.contains(['e', 'E']) || inner == "null" {
        return None;
    }
    Some(inner)
}

/// A price from a raw JSON token, or `None` when it is not a positive decimal.
pub fn price(raw: &RawValue) -> Option<Price> {
    let parsed = Price::parse(decimal_text(raw)?).ok()?;
    (parsed > Price::ZERO).then_some(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Frame {
        p: Box<RawValue>,
    }

    fn price_of(json: &str) -> Option<Price> {
        let f: Frame = serde_json::from_str(json).expect("valid frame");
        price(&f.p)
    }

    #[test]
    fn a_string_price_is_read_exactly() {
        assert_eq!(
            price_of(r#"{"p":"78389.03000000"}"#),
            Price::parse("78389.03").ok()
        );
    }

    #[test]
    fn a_number_price_keeps_every_digit_it_was_sent_with() {
        // Through f64 this would still print as 1.0853; the trailing zero
        // survives only because no float was ever made.
        let f: Frame = serde_json::from_str(r#"{"p":1.08530}"#).unwrap();
        assert_eq!(decimal_text(&f.p), Some("1.08530"));
        assert_eq!(price(&f.p), Price::parse("1.0853").ok());
    }

    #[test]
    fn a_price_a_double_cannot_hold_is_still_exact() {
        // Seventeen significant digits: an f64 rounds this, the parser does not.
        let f: Frame = serde_json::from_str(r#"{"p":12345678.123456789}"#).unwrap();
        assert_eq!(
            price(&f.p).unwrap().to_decimal_string(),
            "12345678.123456789"
        );
    }

    #[test]
    fn exponents_nulls_zeroes_and_garbage_are_refused() {
        assert_eq!(price_of(r#"{"p":1e-5}"#), None);
        assert_eq!(price_of(r#"{"p":null}"#), None);
        assert_eq!(price_of(r#"{"p":0}"#), None);
        assert_eq!(price_of(r#"{"p":"-1.5"}"#), None);
        assert_eq!(price_of(r#"{"p":"abc"}"#), None);
        assert_eq!(price_of(r#"{"p":""}"#), None);
    }
}
