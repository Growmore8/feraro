//! The trading-day calendar and overnight financing arithmetic.
//!
//! # How a swap is expressed
//!
//! As an **annual rate in basis points of notional**, separately for longs and
//! shorts: `-1000` means a long position pays ten per cent a year. One night's
//! charge is `notional × rate ÷ 10,000 ÷ 365`.
//!
//! MetaTrader expresses swap in points per lot instead. That works for FX,
//! where a point has a stable meaning, and breaks for crypto, where a point of
//! BTC and a point of DOGE differ by six orders of magnitude. An annual rate
//! means the same thing on every instrument, which is also how a broker's
//! treasury desk actually thinks about financing.
//!
//! # Triple swap, and weekends
//!
//! FX settles two days after the trade. A position held from Wednesday to
//! Thursday settles across the weekend, so Wednesday's rollover charges three
//! nights and Saturday's and Sunday's charge none. Crypto trades every day and
//! settles every day, so it charges one night, every night. Both are just
//! configuration here — [`SwapTerms::triple_on`] and
//! [`SwapTerms::skip_weekends`].
//!
//! # No date library
//!
//! The engine workspace takes no dependencies, and the calendar needed here is
//! small: days since the Unix epoch, a weekday, and an hour of the day. All of
//! it is UTC, because a rollover schedule that moves with a server's timezone
//! is a rollover that runs twice on the night the clocks change.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic)]

use std::fmt;

use domain::{Money, Price, Qty, Side};
use money::ArithError;

/// Nanoseconds in one day.
pub const NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;

/// Nanoseconds in one hour.
pub const NANOS_PER_HOUR: i64 = 3_600 * 1_000_000_000;

/// Basis points in one whole.
pub const BP_PER_UNIT: i128 = 10_000;

/// Days in the financing year.
///
/// 365 rather than 360: crypto and most CFD financing use actual/365. FX money
/// markets use actual/360 for some currencies, and if that is ever needed it is
/// a per-instrument field, not a change to this constant.
pub const DAYS_PER_YEAR: i128 = 365;

/// The most rollovers the engine will catch up on by itself.
///
/// A gateway down for a weekend should charge the nights it missed. A gateway
/// down for a month should not silently post thirty nights of financing at
/// today's prices; that is an operator's decision, so it stops and says so.
pub const MAX_CATCH_UP_DAYS: u32 = 14;

/// A day of the week.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Weekday {
    /// Sunday.
    Sunday,
    /// Monday.
    Monday,
    /// Tuesday.
    Tuesday,
    /// Wednesday.
    Wednesday,
    /// Thursday.
    Thursday,
    /// Friday.
    Friday,
    /// Saturday.
    Saturday,
}

impl Weekday {
    /// True on Saturday and Sunday.
    pub const fn is_weekend(self) -> bool {
        matches!(self, Weekday::Saturday | Weekday::Sunday)
    }
}

/// Why a rollover could not be scheduled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloverError {
    /// More rollovers are due than the engine will post unattended.
    TooManyMissed {
        /// How many were due.
        missed: u32,
    },
    /// A time before the Unix epoch.
    BeforeEpoch,
    /// The configured hour is not an hour.
    InvalidHour(u8),
}

impl fmt::Display for RolloverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RolloverError::TooManyMissed { missed } => write!(
                f,
                "{missed} rollovers are due, more than the {MAX_CATCH_UP_DAYS} the engine will post \
                 unattended"
            ),
            RolloverError::BeforeEpoch => f.write_str("time is before the Unix epoch"),
            RolloverError::InvalidHour(h) => write!(f, "{h} is not an hour of the day"),
        }
    }
}

impl std::error::Error for RolloverError {}

/// Days since the Unix epoch of a UTC timestamp.
pub fn day_of(nanos: i64) -> Result<u32, RolloverError> {
    if nanos < 0 {
        return Err(RolloverError::BeforeEpoch);
    }
    u32::try_from(nanos / NANOS_PER_DAY).map_err(|_| RolloverError::BeforeEpoch)
}

/// The weekday of a day number.
///
/// Day zero, 1 January 1970, was a Thursday.
pub const fn weekday(day: u32) -> Weekday {
    match (day + 4) % 7 {
        0 => Weekday::Sunday,
        1 => Weekday::Monday,
        2 => Weekday::Tuesday,
        3 => Weekday::Wednesday,
        4 => Weekday::Thursday,
        5 => Weekday::Friday,
        _ => Weekday::Saturday,
    }
}

/// The most recent day whose rollover moment has already passed.
///
/// Day `d`'s rollover happens at `d` midnight UTC plus `hour`. At 21:59 on the
/// fourth, with a 22:00 rollover, the latest due rollover is the third's.
pub fn latest_due(now_nanos: i64, hour: u8) -> Result<u32, RolloverError> {
    if hour > 23 {
        return Err(RolloverError::InvalidHour(hour));
    }
    let day = day_of(now_nanos)?;
    let into_day = now_nanos - i64::from(day) * NANOS_PER_DAY;
    if into_day >= i64::from(hour) * NANOS_PER_HOUR {
        Ok(day)
    } else {
        day.checked_sub(1).ok_or(RolloverError::BeforeEpoch)
    }
}

/// Every rollover due after `last_done`, oldest first.
///
/// Empty when nothing is due. Refuses rather than returning a long list, for
/// the reason on [`MAX_CATCH_UP_DAYS`].
pub fn due_days(now_nanos: i64, last_done: u32, hour: u8) -> Result<Vec<u32>, RolloverError> {
    let latest = latest_due(now_nanos, hour)?;
    if latest <= last_done {
        return Ok(Vec::new());
    }
    let missed = latest - last_done;
    if missed > MAX_CATCH_UP_DAYS {
        return Err(RolloverError::TooManyMissed { missed });
    }
    Ok((last_done + 1..=latest).collect())
}

/// Overnight financing terms for one instrument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapTerms {
    /// Annual rate paid to (positive) or by (negative) a long, in basis points.
    pub long_bp: i64,
    /// The same for a short.
    pub short_bp: i64,
    /// The weekday whose rollover charges three nights, if any.
    pub triple_on: Option<Weekday>,
    /// True when Saturday's and Sunday's rollovers charge nothing.
    pub skip_weekends: bool,
}

impl SwapTerms {
    /// No financing at all.
    pub const NONE: SwapTerms = SwapTerms {
        long_bp: 0,
        short_bp: 0,
        triple_on: None,
        skip_weekends: false,
    };

    /// FX convention: triple on Wednesday, nothing at the weekend.
    pub const fn fx(long_bp: i64, short_bp: i64) -> SwapTerms {
        SwapTerms {
            long_bp,
            short_bp,
            triple_on: Some(Weekday::Wednesday),
            skip_weekends: true,
        }
    }

    /// Continuous-market convention: one night, every night.
    pub const fn every_day(long_bp: i64, short_bp: i64) -> SwapTerms {
        SwapTerms {
            long_bp,
            short_bp,
            triple_on: None,
            skip_weekends: false,
        }
    }

    /// How many nights one day's rollover charges.
    pub fn nights(&self, day: u32) -> i64 {
        let wd = weekday(day);
        if self.skip_weekends && wd.is_weekend() {
            0
        } else if self.triple_on == Some(wd) {
            3
        } else {
            1
        }
    }

    /// The annual rate that applies to a side.
    pub const fn rate_bp(&self, side: Side) -> i64 {
        match side {
            Side::Buy => self.long_bp,
            Side::Sell => self.short_bp,
        }
    }
}

/// One rollover's financing on a position.
///
/// `mark` is the price the position is valued at for financing — the price it
/// would close at, as everywhere else in the engine. Rounded half away from
/// zero, in one step, so the rounding is never compounded across the three
/// divisions the formula implies.
pub fn charge(
    side: Side,
    qty: Qty,
    mark: Price,
    terms: &SwapTerms,
    nights: i64,
) -> Result<Money, ArithError> {
    if nights == 0 {
        return Ok(Money::ZERO);
    }
    let rate = terms.rate_bp(side);
    if rate == 0 {
        return Ok(Money::ZERO);
    }
    let notional = mark.notional(qty)?;
    let numerator = i128::from(rate)
        .checked_mul(i128::from(nights))
        .ok_or(ArithError::Overflow)?;
    notional.mul_ratio(numerator, BP_PER_UNIT * DAYS_PER_YEAR)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Price {
        Price::parse(s).expect("price")
    }
    fn q(s: &str) -> Qty {
        Qty::parse(s).expect("qty")
    }

    /// Nanoseconds for a UTC day number and hour.
    fn at(day: u32, hour: i64, minute: i64) -> i64 {
        i64::from(day) * NANOS_PER_DAY + hour * NANOS_PER_HOUR + minute * 60 * 1_000_000_000
    }

    // 2026-09-14 is day 20,710, a Monday.
    const MONDAY: u32 = 20_710;

    #[test]
    fn the_epoch_was_a_thursday() {
        assert_eq!(weekday(0), Weekday::Thursday);
        assert_eq!(weekday(1), Weekday::Friday);
        assert_eq!(weekday(3), Weekday::Sunday);
    }

    #[test]
    fn a_known_date_lands_on_the_right_weekday() {
        assert_eq!(weekday(MONDAY), Weekday::Monday);
        assert_eq!(weekday(MONDAY + 2), Weekday::Wednesday);
        assert_eq!(weekday(MONDAY + 5), Weekday::Saturday);
        assert_eq!(weekday(MONDAY + 6), Weekday::Sunday);
    }

    #[test]
    fn a_rollover_is_not_due_until_its_hour() {
        // At 21:59 on Monday, Sunday's rollover is the latest due.
        assert_eq!(latest_due(at(MONDAY, 21, 59), 22).unwrap(), MONDAY - 1);
        // At 22:00 exactly, Monday's is.
        assert_eq!(latest_due(at(MONDAY, 22, 0), 22).unwrap(), MONDAY);
        assert_eq!(latest_due(at(MONDAY, 23, 30), 22).unwrap(), MONDAY);
    }

    #[test]
    fn nothing_is_due_when_the_last_rollover_is_current() {
        let now = at(MONDAY, 23, 0);
        assert!(due_days(now, MONDAY, 22).unwrap().is_empty());
        // Nor just before the next one.
        assert!(due_days(at(MONDAY + 1, 21, 59), MONDAY, 22)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_gateway_down_over_a_weekend_catches_up_every_night_in_order() {
        let now = at(MONDAY, 22, 5);
        let due = due_days(now, MONDAY - 3, 22).unwrap();
        assert_eq!(due, vec![MONDAY - 2, MONDAY - 1, MONDAY]);
    }

    #[test]
    fn a_month_of_missed_rollovers_is_an_operator_decision() {
        let now = at(MONDAY, 22, 5);
        let err = due_days(now, MONDAY - 30, 22).unwrap_err();
        assert_eq!(err, RolloverError::TooManyMissed { missed: 30 });
    }

    #[test]
    fn an_impossible_hour_is_refused() {
        assert_eq!(latest_due(0, 24), Err(RolloverError::InvalidHour(24)));
        assert_eq!(day_of(-1), Err(RolloverError::BeforeEpoch));
    }

    #[test]
    fn fx_charges_three_nights_on_wednesday_and_none_at_the_weekend() {
        let fx = SwapTerms::fx(-150, 50);
        assert_eq!(fx.nights(MONDAY), 1);
        assert_eq!(fx.nights(MONDAY + 1), 1); // Tuesday
        assert_eq!(fx.nights(MONDAY + 2), 3); // Wednesday
        assert_eq!(fx.nights(MONDAY + 3), 1); // Thursday
        assert_eq!(fx.nights(MONDAY + 4), 1); // Friday
        assert_eq!(fx.nights(MONDAY + 5), 0); // Saturday
        assert_eq!(fx.nights(MONDAY + 6), 0); // Sunday

        // Across a full week, seven nights of financing — the weekend is not
        // free, it is moved to Wednesday.
        let week: i64 = (0..7).map(|d| fx.nights(MONDAY + d)).sum();
        assert_eq!(week, 7);
    }

    #[test]
    fn crypto_charges_one_night_every_night() {
        let crypto = SwapTerms::every_day(-1000, -500);
        for d in 0..7 {
            assert_eq!(crypto.nights(MONDAY + d), 1);
        }
    }

    #[test]
    fn one_nights_financing_on_one_btc_at_ten_percent() {
        // 80,000 notional × −10% ÷ 365 = −21.917808219…
        let terms = SwapTerms::every_day(-1000, -500);
        let c = charge(Side::Buy, q("1"), p("80000.00"), &terms, 1).unwrap();
        assert_eq!(c.to_display_string(2).unwrap(), "-21.92");
        assert_eq!(c.raw(), -21_917_808_219);
    }

    #[test]
    fn a_short_uses_its_own_rate() {
        let terms = SwapTerms::every_day(-1000, -500);
        let long = charge(Side::Buy, q("1"), p("80000.00"), &terms, 1).unwrap();
        let short = charge(Side::Sell, q("1"), p("80000.00"), &terms, 1).unwrap();
        assert_eq!(short.to_display_string(2).unwrap(), "-10.96");
        assert!(short > long, "the short pays less");
    }

    #[test]
    fn a_positive_rate_is_a_credit() {
        let terms = SwapTerms::fx(-150, 50);
        let c = charge(Side::Sell, q("100000"), p("1.08500"), &terms, 1).unwrap();
        assert!(c > Money::ZERO, "a short in a positive-carry pair is paid");
    }

    #[test]
    fn a_triple_night_is_exactly_three_single_nights_after_rounding() {
        // Computed in one step, so the triple is not three rounded singles
        // added up — which could differ by a nano on an awkward notional.
        let terms = SwapTerms::fx(-150, 50);
        let triple = charge(Side::Buy, q("123456"), p("1.08517"), &terms, 3).unwrap();
        let unrounded_single = p("1.08517")
            .notional(q("123456"))
            .unwrap()
            .mul_ratio(-150 * 3, BP_PER_UNIT * DAYS_PER_YEAR)
            .unwrap();
        assert_eq!(triple, unrounded_single);
    }

    #[test]
    fn no_nights_and_no_rate_both_charge_nothing() {
        let fx = SwapTerms::fx(-150, 50);
        assert_eq!(
            charge(Side::Buy, q("1"), p("80000.00"), &fx, 0).unwrap(),
            Money::ZERO
        );
        assert_eq!(
            charge(Side::Buy, q("1"), p("80000.00"), &SwapTerms::NONE, 1).unwrap(),
            Money::ZERO
        );
    }
}
