//! Fixed-point money primitives for the Feraro trading core.
//!
//! # Invariant P0-1 — no floating point
//!
//! No `f32`/`f64` value ever represents a price, quantity or balance anywhere
//! in the engine. Every monetary value here is a scaled integer, and every
//! operation that loses precision rounds explicitly and deterministically.
//!
//! Two rounding drifts of 0.00001 on a million trades is a real reconciliation
//! break against the liquidity provider's statement. Floats produce exactly
//! that, silently, weeks after the trade. Integers do not.
//!
//! # Scales
//!
//! | Type    | Decimal places | Raw storage |
//! |---------|----------------|-------------|
//! | `Price` | 9              | `i64`       |
//! | `Qty`   | 8              | `i64`       |
//! | `Money` | 9              | `i128`      |
//!
//! Nine decimal places on an `i64` gives a price range of ±9.22e9, which
//! covers every instrument we will ever list. `Money` is `i128` because
//! `price × qty` intermediates overflow 64 bits at institutional size.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic)]
// `add`, `sub` and `neg` deliberately take the names of the std operator
// traits' methods without implementing those traits. Implementing
// `std::ops::Add` would make `a + b` compile and wrap on overflow with no
// diagnostic. Forcing every caller through a `Result` is the entire point of
// this crate, so the shadowing is the feature, not an oversight.
#![allow(clippy::should_implement_trait)]

use core::fmt;
use core::str::FromStr;

/// Decimal places carried internally by every [`Price`].
pub const PRICE_DIGITS: u32 = 9;
/// Decimal places carried internally by every [`Qty`].
pub const QTY_DIGITS: u32 = 8;
/// Decimal places carried internally by every [`Money`].
pub const MONEY_DIGITS: u32 = 9;

/// Raw value of one whole unit of price.
pub const PRICE_ONE: i64 = 1_000_000_000;
/// Raw value of one whole unit of quantity.
pub const QTY_ONE: i64 = 100_000_000;
/// Raw value of one whole unit of money.
pub const MONEY_ONE: i128 = 1_000_000_000;

/// Arithmetic that could not be completed exactly or in range.
///
/// Every fallible operation returns this rather than wrapping or saturating.
/// A silent wrap in a margin calculation is a liquidation at the wrong price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithError {
    /// The result does not fit the target integer width.
    Overflow,
    /// Division by a zero divisor.
    DivideByZero,
}

impl fmt::Display for ArithError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArithError::Overflow => f.write_str("arithmetic overflow"),
            ArithError::DivideByZero => f.write_str("divide by zero"),
        }
    }
}

impl std::error::Error for ArithError {}

/// A decimal string that could not be read as a fixed-point value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// The input was empty or held no digits.
    Empty,
    /// A character other than a digit, sign or single decimal point.
    InvalidChar,
    /// More fractional digits than the target type can represent exactly.
    TooPrecise,
    /// The value does not fit the target integer width.
    Overflow,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Empty => f.write_str("empty decimal"),
            ParseError::InvalidChar => f.write_str("invalid character in decimal"),
            ParseError::TooPrecise => f.write_str("more fractional digits than the scale allows"),
            ParseError::Overflow => f.write_str("decimal out of range"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Integer division rounding half away from zero.
///
/// Deterministic across platforms and architectures, which matters because the
/// journal replay in CI must reproduce byte-identical balances. Rust's `/`
/// truncates toward zero, which biases every rounded P&L toward the house.
fn div_round(num: i128, den: i128) -> Result<i128, ArithError> {
    if den == 0 {
        return Err(ArithError::DivideByZero);
    }
    let q = num / den;
    let r = num % den;
    if r == 0 {
        return Ok(q);
    }
    let twice_rem = r
        .unsigned_abs()
        .checked_mul(2)
        .ok_or(ArithError::Overflow)?;
    if twice_rem < den.unsigned_abs() {
        return Ok(q);
    }
    let away_from_zero = (num < 0) != (den < 0);
    if away_from_zero {
        q.checked_sub(1).ok_or(ArithError::Overflow)
    } else {
        q.checked_add(1).ok_or(ArithError::Overflow)
    }
}

/// Renders a scaled integer as a decimal string with exactly `digits` places.
fn fmt_fixed(raw: i128, digits: u32) -> String {
    let scale = 10i128.pow(digits);
    let neg = raw < 0;
    let abs = raw.unsigned_abs();
    let scale_u = scale.unsigned_abs();
    let whole = abs / scale_u;
    let frac = abs % scale_u;
    let sign = if neg { "-" } else { "" };
    if digits == 0 {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole}.{frac:0width$}", width = digits as usize)
    }
}

/// Reads a decimal string into a value scaled by `digits` places.
fn parse_fixed(s: &str, digits: u32) -> Result<i128, ParseError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ParseError::Empty);
    }
    let (neg, body) = match s.as_bytes()[0] {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    if body.is_empty() {
        return Err(ParseError::Empty);
    }

    let mut parts = body.splitn(2, '.');
    let whole_str = parts.next().unwrap_or("");
    let frac_str = parts.next().unwrap_or("");
    if body.matches('.').count() > 1 {
        return Err(ParseError::InvalidChar);
    }
    if whole_str.is_empty() && frac_str.is_empty() {
        return Err(ParseError::Empty);
    }
    if frac_str.len() > digits as usize {
        return Err(ParseError::TooPrecise);
    }

    let mut acc: i128 = 0;
    for b in whole_str.bytes().chain(frac_str.bytes()) {
        if !b.is_ascii_digit() {
            return Err(ParseError::InvalidChar);
        }
        acc = acc
            .checked_mul(10)
            .and_then(|v| v.checked_add(i128::from(b - b'0')))
            .ok_or(ParseError::Overflow)?;
    }
    // Pad the fraction out to the full scale.
    let pad = digits as usize - frac_str.len();
    for _ in 0..pad {
        acc = acc.checked_mul(10).ok_or(ParseError::Overflow)?;
    }
    Ok(if neg { -acc } else { acc })
}

macro_rules! fixed_type {
    ($name:ident, $inner:ty, $digits:expr, $doc:literal) => {
        #[doc = $doc]
        #[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        pub struct $name($inner);

        impl $name {
            /// Zero.
            pub const ZERO: Self = Self(0);

            /// Wraps an already-scaled raw integer.
            #[inline]
            pub const fn from_raw(raw: $inner) -> Self {
                Self(raw)
            }

            /// The underlying scaled integer.
            #[inline]
            pub const fn raw(self) -> $inner {
                self.0
            }

            /// Number of decimal places this type carries.
            #[inline]
            pub const fn digits() -> u32 {
                $digits
            }

            /// True when the value is exactly zero.
            #[inline]
            pub const fn is_zero(self) -> bool {
                self.0 == 0
            }

            /// True when the value is below zero.
            #[inline]
            pub const fn is_negative(self) -> bool {
                self.0 < 0
            }

            /// Absolute value.
            #[inline]
            pub fn abs(self) -> Result<Self, ArithError> {
                self.0.checked_abs().map(Self).ok_or(ArithError::Overflow)
            }

            /// Addition, erroring rather than wrapping.
            #[inline]
            pub fn add(self, rhs: Self) -> Result<Self, ArithError> {
                self.0
                    .checked_add(rhs.0)
                    .map(Self)
                    .ok_or(ArithError::Overflow)
            }

            /// Subtraction, erroring rather than wrapping.
            #[inline]
            pub fn sub(self, rhs: Self) -> Result<Self, ArithError> {
                self.0
                    .checked_sub(rhs.0)
                    .map(Self)
                    .ok_or(ArithError::Overflow)
            }

            /// Negation.
            #[inline]
            pub fn neg(self) -> Result<Self, ArithError> {
                self.0.checked_neg().map(Self).ok_or(ArithError::Overflow)
            }

            /// Multiplication by a plain integer count.
            #[inline]
            pub fn scale_by(self, n: i64) -> Result<Self, ArithError> {
                let v = <$inner>::try_from(n).map_err(|_| ArithError::Overflow)?;
                self.0.checked_mul(v).map(Self).ok_or(ArithError::Overflow)
            }

            /// Renders with the type's full internal precision.
            pub fn to_decimal_string(self) -> String {
                fmt_fixed(self.0 as i128, $digits)
            }

            /// Renders rounded to `digits` decimal places, as a venue would
            /// display it — five for FX majors, three for JPY pairs, two for
            /// index CFDs.
            pub fn to_display_string(self, digits: u32) -> Result<String, ArithError> {
                if digits >= $digits {
                    return Ok(fmt_fixed(self.0 as i128, $digits));
                }
                let factor = 10i128.pow($digits - digits);
                let rounded = div_round(self.0 as i128, factor)?;
                Ok(fmt_fixed(rounded, digits))
            }

            /// Reads a decimal string such as `"1.08532"`.
            pub fn parse(s: &str) -> Result<Self, ParseError> {
                let raw = parse_fixed(s, $digits)?;
                <$inner>::try_from(raw)
                    .map(Self)
                    .map_err(|_| ParseError::Overflow)
            }
        }

        impl FromStr for $name {
            type Err = ParseError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.to_decimal_string())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    f,
                    concat!(stringify!($name), "({})"),
                    self.to_decimal_string()
                )
            }
        }
    };
}

fixed_type!(
    Price,
    i64,
    PRICE_DIGITS,
    "An instrument price, or a price delta, at nine decimal places.\n\nAlso used for FX conversion rates, which are prices of one currency in another."
);
fixed_type!(
    Qty,
    i64,
    QTY_DIGITS,
    "A quantity in an instrument's base units at eight decimal places.\n\nThis is *units*, never lots. One standard EURUSD lot is 100_000 units; the lot-to-unit conversion belongs to the instrument definition, not here."
);
fixed_type!(
    Money,
    i128,
    MONEY_DIGITS,
    "An amount in some currency at nine decimal places.\n\nThe currency is tracked by the ledger account, not by this type — mixing currencies is prevented at the ledger boundary where the account is known."
);

impl Price {
    /// One whole unit. The identity conversion rate.
    pub const ONE: Price = Price::from_raw(PRICE_ONE);

    /// The point halfway between two prices.
    ///
    /// Used for conversion rates, never for valuing a position: a position is
    /// valued at the side it would close on (see `risk::Market::close_price`).
    /// A conversion is the opposite case — the house is not taking the other
    /// side of it, so charging half a spread for turning yen into dollars
    /// would be a fee, and a fee belongs in the ledger where it can be seen.
    pub fn midpoint(self, other: Price) -> Result<Price, ArithError> {
        let sum = i128::from(self.raw())
            .checked_add(i128::from(other.raw()))
            .ok_or(ArithError::Overflow)?;
        let half = div_round(sum, 2)?;
        i64::try_from(half)
            .map(Price::from_raw)
            .map_err(|_| ArithError::Overflow)
    }

    /// Notional value of `qty` units at this price, in the quote currency.
    ///
    /// `100_000` units of EURUSD at `1.08532` is `108_532` USD.
    pub fn notional(self, qty: Qty) -> Result<Money, ArithError> {
        let product = i128::from(self.raw())
            .checked_mul(i128::from(qty.raw()))
            .ok_or(ArithError::Overflow)?;
        div_round(product, i128::from(QTY_ONE)).map(Money::from_raw)
    }

    /// Distance from this price to `other`, signed.
    pub fn delta_to(self, other: Price) -> Result<Price, ArithError> {
        other.sub(self)
    }
}

impl Money {
    /// Converts into another currency at `rate`, where `rate` is the price of
    /// this money's currency expressed in the target currency.
    pub fn convert(self, rate: Price) -> Result<Money, ArithError> {
        let product = self
            .raw()
            .checked_mul(i128::from(rate.raw()))
            .ok_or(ArithError::Overflow)?;
        div_round(product, i128::from(PRICE_ONE)).map(Money::from_raw)
    }

    /// Converts into another currency at `rate`, where `rate` is the price of
    /// one unit of the *target* currency expressed in this money's currency.
    ///
    /// This is the division to [`convert`](Self::convert)'s multiplication,
    /// and it exists for precision rather than convenience. A yen amount in a
    /// dollar book converts at USDJPY — say `147.5` yen per dollar — and that
    /// rate is exact in nine places. Its reciprocal, `0.006779661…`, is not:
    /// multiplying by a rounded reciprocal loses about one part in ten
    /// million, which on a hundred-lot position is cents per close that
    /// reconcile against nothing. Dividing by the rate the market actually
    /// quoted keeps the arithmetic exact to the last place.
    pub fn convert_per(self, rate: Price) -> Result<Money, ArithError> {
        if rate.is_zero() {
            return Err(ArithError::DivideByZero);
        }
        let scaled = self
            .raw()
            .checked_mul(i128::from(PRICE_ONE))
            .ok_or(ArithError::Overflow)?;
        div_round(scaled, i128::from(rate.raw())).map(Money::from_raw)
    }

    /// Rounds to a settlement precision — two places for most account
    /// currencies, zero for JPY and similar.
    pub fn round_to(self, digits: u32) -> Result<Money, ArithError> {
        if digits >= MONEY_DIGITS {
            return Ok(self);
        }
        let factor = 10i128.pow(MONEY_DIGITS - digits);
        let units = div_round(self.raw(), factor)?;
        units
            .checked_mul(factor)
            .map(Money::from_raw)
            .ok_or(ArithError::Overflow)
    }

    /// `self × num ÷ den`, rounded half away from zero.
    ///
    /// Used wherever an amount is apportioned — a year's financing rate over
    /// one night, accrued swap over the part of a position being closed. The
    /// multiplication happens before the division, in `i128`, so no precision
    /// is lost to an intermediate truncation.
    pub fn mul_ratio(self, num: i128, den: i128) -> Result<Money, ArithError> {
        let product = self.raw().checked_mul(num).ok_or(ArithError::Overflow)?;
        div_round(product, den).map(Money::from_raw)
    }

    /// Sums an iterator, erroring on overflow rather than wrapping.
    pub fn sum<I: IntoIterator<Item = Money>>(items: I) -> Result<Money, ArithError> {
        let mut acc = Money::ZERO;
        for m in items {
            acc = acc.add(m)?;
        }
        Ok(acc)
    }
}

/// Which way a position or order faces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// Buying the base currency.
    Buy,
    /// Selling the base currency.
    Sell,
}

impl Side {
    /// The opposite side.
    #[inline]
    pub const fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }

    /// `+1` for a buy, `-1` for a sell.
    #[inline]
    pub const fn sign(self) -> i64 {
        match self {
            Side::Buy => 1,
            Side::Sell => -1,
        }
    }
}

/// Unrealised or realised profit on `qty` units moved from `entry` to `exit`.
///
/// Expressed in the quote currency. A long is profitable when the exit price
/// is above entry; a short is the exact negation, which
/// [`long_and_short_pnl_are_symmetric`](self) asserts.
pub fn pnl(side: Side, entry: Price, exit: Price, qty: Qty) -> Result<Money, ArithError> {
    let delta = entry.delta_to(exit)?;
    let signed = delta.scale_by(side.sign())?;
    signed.notional(qty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_renders_an_fx_major() {
        let p = Price::parse("1.08532").expect("valid price");
        assert_eq!(p.raw(), 1_085_320_000);
        assert_eq!(p.to_display_string(5).unwrap(), "1.08532");
        assert_eq!(p.to_decimal_string(), "1.085320000");
    }

    #[test]
    fn parses_a_jpy_pair_and_a_negative() {
        assert_eq!(Price::parse("157.482").unwrap().raw(), 157_482_000_000);
        assert_eq!(Price::parse("-0.00001").unwrap().raw(), -10_000);
        assert_eq!(
            Price::parse("-0.00001")
                .unwrap()
                .to_display_string(5)
                .unwrap(),
            "-0.00001"
        );
    }

    #[test]
    fn rejects_precision_it_cannot_hold_exactly() {
        assert_eq!(Price::parse("1.0123456789"), Err(ParseError::TooPrecise));
        assert_eq!(Price::parse("1.2.3"), Err(ParseError::InvalidChar));
        assert_eq!(Price::parse("abc"), Err(ParseError::InvalidChar));
        assert_eq!(Price::parse(""), Err(ParseError::Empty));
    }

    #[test]
    fn one_lot_of_eurusd_has_the_right_notional() {
        let price = Price::parse("1.08532").unwrap();
        let qty = Qty::parse("100000").unwrap();
        let notional = price.notional(qty).unwrap();
        assert_eq!(notional.to_display_string(2).unwrap(), "108532.00");
    }

    #[test]
    fn a_fractional_crypto_size_has_the_right_notional() {
        let price = Price::parse("64250.75").unwrap();
        let qty = Qty::parse("0.37500000").unwrap();
        let notional = price.notional(qty).unwrap();
        assert_eq!(notional.to_display_string(2).unwrap(), "24094.03");
    }

    #[test]
    fn long_and_short_pnl_are_symmetric() {
        let entry = Price::parse("1.08532").unwrap();
        let exit = Price::parse("1.08691").unwrap();
        let qty = Qty::parse("100000").unwrap();

        let long = pnl(Side::Buy, entry, exit, qty).unwrap();
        let short = pnl(Side::Sell, entry, exit, qty).unwrap();

        assert_eq!(long.to_display_string(2).unwrap(), "159.00");
        assert_eq!(long.add(short).unwrap(), Money::ZERO);
    }

    #[test]
    fn a_flat_move_earns_nothing() {
        let p = Price::parse("1.08532").unwrap();
        let qty = Qty::parse("250000").unwrap();
        assert_eq!(pnl(Side::Buy, p, p, qty).unwrap(), Money::ZERO);
    }

    #[test]
    fn rounding_is_half_away_from_zero_in_both_directions() {
        assert_eq!(div_round(5, 10).unwrap(), 1);
        assert_eq!(div_round(-5, 10).unwrap(), -1);
        assert_eq!(div_round(4, 10).unwrap(), 0);
        assert_eq!(div_round(-4, 10).unwrap(), 0);
        assert_eq!(div_round(15, 10).unwrap(), 2);
        assert_eq!(div_round(-15, 10).unwrap(), -2);
    }

    #[test]
    fn rounding_does_not_favour_the_house() {
        // Truncation toward zero would make every one of these zero for the
        // client and non-zero for us. Over a million fills that is real money.
        let half_a_cent = Money::from_raw(5_000_000);
        assert_eq!(half_a_cent.round_to(2).unwrap().raw(), 10_000_000);
        assert_eq!(
            half_a_cent.neg().unwrap().round_to(2).unwrap().raw(),
            -10_000_000
        );
    }

    #[test]
    fn a_currency_round_trip_loses_only_the_rate_s_own_precision() {
        // 108532.00 USD to EUR at 0.92135, then back at 1.08536.
        //
        // The two rates are not exact reciprocals — they cannot be, at five
        // decimal places — so the round trip does not return the original
        // amount. That residual is the quoted rate's precision, and it must
        // stay bounded by it. Anything larger would mean our arithmetic is
        // losing money on top of the rate, which is the bug this guards.
        let usd = Money::parse("108532.00").unwrap();

        let eur = usd.convert(Price::parse("0.92135").unwrap()).unwrap();
        assert_eq!(eur.to_display_string(2).unwrap(), "99995.96");

        let back = eur.convert(Price::parse("1.08536").unwrap()).unwrap();
        assert_eq!(back.to_display_string(2).unwrap(), "108531.61");

        // One tick of a five-decimal rate on this notional is about 1.09.
        let residual = usd.sub(back).unwrap().abs().unwrap();
        let one_tick_of_rate = Money::parse("1.09").unwrap();
        assert!(
            residual < one_tick_of_rate,
            "round-trip residual {} exceeds the rate's own precision",
            residual.to_display_string(5).unwrap()
        );
    }

    #[test]
    fn dividing_by_the_quoted_rate_beats_multiplying_by_its_reciprocal() {
        // 10,000 JPY profit in a USD book, with USDJPY at 147.5.
        let jpy = Money::parse("10000.00").unwrap();
        let usdjpy = Price::parse("147.5").unwrap();

        let exact = jpy.convert_per(usdjpy).unwrap();
        assert_eq!(exact.to_display_string(2).unwrap(), "67.80");
        // 10000 ÷ 147.5 = 67.796610169491525…, and nine places is what we keep.
        assert_eq!(exact.to_decimal_string(), "67.796610169");

        // The same conversion through a nine-place reciprocal, which is what
        // storing "the rate" the other way round would force.
        let reciprocal = Price::ONE.notional(Qty::parse("1").unwrap()).unwrap();
        assert_eq!(reciprocal.to_decimal_string(), "1.000000000");
        let rounded_reciprocal = Price::parse("0.006779661").unwrap();
        let approximate = jpy.convert(rounded_reciprocal).unwrap();
        assert!(
            approximate != exact,
            "the reciprocal route is lossy, which is why `convert_per` exists"
        );
    }

    #[test]
    fn converting_at_one_changes_nothing_and_at_zero_is_an_error() {
        let m = Money::parse("-1234.567891234").unwrap();
        assert_eq!(m.convert_per(Price::ONE).unwrap(), m);
        assert_eq!(m.convert(Price::ONE).unwrap(), m);
        assert_eq!(m.convert_per(Price::ZERO), Err(ArithError::DivideByZero));
    }

    #[test]
    fn a_conversion_rounds_half_away_from_zero_in_both_directions() {
        // A rate of 3 leaves a third, which has to land somewhere, and it must
        // land the same distance from zero whichever sign the amount has.
        let rate = Price::parse("3").unwrap();
        let up = Money::from_raw(5).convert_per(rate).unwrap();
        let down = Money::from_raw(-5).convert_per(rate).unwrap();
        assert_eq!(up.raw(), 2, "5 ÷ 3 at the last place rounds away from zero");
        assert_eq!(down.raw(), -2);
    }

    #[test]
    fn a_midpoint_sits_halfway_and_rounds_away_from_zero() {
        let bid = Price::parse("147.500").unwrap();
        let ask = Price::parse("147.508").unwrap();
        assert_eq!(bid.midpoint(ask).unwrap(), Price::parse("147.504").unwrap());
        // An odd raw sum cannot be halved exactly; it rounds, it does not drift.
        let a = Price::from_raw(1);
        let b = Price::from_raw(2);
        assert_eq!(a.midpoint(b).unwrap().raw(), 2);
        assert_eq!(
            Price::from_raw(-1)
                .midpoint(Price::from_raw(-2))
                .unwrap()
                .raw(),
            -2
        );
        assert_eq!(bid.midpoint(bid).unwrap(), bid);
    }

    #[test]
    fn arithmetic_errors_instead_of_wrapping() {
        let big = Price::from_raw(i64::MAX);
        assert_eq!(big.add(Price::from_raw(1)), Err(ArithError::Overflow));
        assert_eq!(div_round(1, 0), Err(ArithError::DivideByZero));
    }

    #[test]
    fn summing_many_small_amounts_stays_exact() {
        // A float accumulator drifts here. An integer one cannot.
        let one_pip = Money::parse("0.00001").unwrap();
        let total = Money::sum(std::iter::repeat_n(one_pip, 1_000_000)).unwrap();
        assert_eq!(total.to_display_string(5).unwrap(), "10.00000");
    }

    #[test]
    fn a_ratio_multiplies_before_it_divides() {
        // 1,000.00 × 1 ÷ 3 = 333.333333333…, which must round, not truncate.
        let m = Money::parse("1000.00").unwrap();
        assert_eq!(m.mul_ratio(1, 3).unwrap().raw(), 333_333_333_333);
        assert_eq!(m.mul_ratio(2, 3).unwrap().raw(), 666_666_666_667);
        // Negative amounts round away from zero too.
        assert_eq!(
            m.neg().unwrap().mul_ratio(2, 3).unwrap().raw(),
            -666_666_666_667
        );
        // A ratio of one is exact.
        assert_eq!(m.mul_ratio(7, 7).unwrap(), m);
        assert_eq!(m.mul_ratio(1, 0), Err(ArithError::DivideByZero));
    }

    #[test]
    fn side_sign_and_opposite_agree() {
        assert_eq!(Side::Buy.opposite(), Side::Sell);
        assert_eq!(Side::Sell.opposite(), Side::Buy);
        assert_eq!(Side::Buy.sign(), 1);
        assert_eq!(Side::Sell.sign(), -1);
    }
}
