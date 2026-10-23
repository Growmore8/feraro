//! What the house adds to the feed's price.
//!
//! # Why a broker needs this
//!
//! On a B-book the house is the counterparty to every trade, so its income
//! is the difference between the price it pays upstream and the price the
//! client deals at. Without a markup the platform quotes the feed exactly
//! and earns nothing on a trade. This is where that difference is set.
//!
//! # Why it is configuration and not a journaled fact
//!
//! A markup is a decision that holds until it is changed, and only its
//! current value matters: nobody replays "what was the markup last
//! Tuesday" to rebuild a balance. What *is* journaled is the consequence —
//! every fill carries the markup it actually charged — so the money is
//! reconstructible from the log even though the setting is not.
//!
//! That split is the same one the logins and the payment methods make, and
//! for the same reason: an append-only log is the wrong shape for a value
//! that is edited.
//!
//! # Units
//!
//! Points of the instrument's own price, added to **each side**, so the
//! spread a client sees widens by twice the figure. Expressed as a price
//! rather than in pips because a pip means different things on a
//! five-decimal pair and on bitcoin, and the engine deals in prices.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use domain::InstrumentId;
use money::{Money, Price};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stored {
    /// Instrument id to markup, as a decimal string.
    ///
    /// A string, not a float: this is a price, and a price that has been
    /// through an `f64` is a price that may not come back. The whole engine
    /// holds that rule and a settings file is no exception.
    #[serde(default)]
    markups: BTreeMap<u32, String>,
    /// Instrument id to commission per unit of quantity, as a decimal
    /// string. Absent in a file written before the house charged one.
    #[serde(default)]
    commissions: BTreeMap<u32, String>,
}

/// The markup file, loaded into memory.
#[derive(Debug, Default)]
pub struct Markups {
    path: PathBuf,
    held: BTreeMap<InstrumentId, Price>,
    charges: BTreeMap<InstrumentId, Money>,
}

impl Markups {
    /// Loads the file, or starts with nothing charged.
    ///
    /// Nothing is the right default. A platform that invents a markup is a
    /// platform quietly taking money from clients that its operator never
    /// agreed to take.
    pub fn load(path: &Path) -> Result<Markups> {
        let stored: Stored = match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("reading {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Stored {
                markups: BTreeMap::new(),
                commissions: BTreeMap::new(),
            },
            Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
        };

        let mut held = BTreeMap::new();
        for (id, text) in stored.markups {
            match Price::parse(text.trim()) {
                Ok(price) if price > Price::ZERO => {
                    held.insert(InstrumentId(id), price);
                }
                // A figure that will not parse is a typo in a file nobody
                // tested. Charging nothing is the safe reading of it, and
                // it is said out loud rather than swallowed.
                Ok(_) => {}
                Err(_) => eprintln!(
                    "markups: instrument {id} has an unreadable markup {text:?}; charging nothing"
                ),
            }
        }
        let mut charges = BTreeMap::new();
        for (id, text) in stored.commissions {
            match Money::parse(text.trim()) {
                Ok(amount) if amount > Money::ZERO => {
                    charges.insert(InstrumentId(id), amount);
                }
                Ok(_) => {}
                Err(_) => eprintln!(
                    "markups: instrument {id} has an unreadable commission {text:?}; charging nothing"
                ),
            }
        }

        Ok(Markups {
            path: path.to_path_buf(),
            held,
            charges,
        })
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let stored = Stored {
            markups: self
                .held
                .iter()
                .map(|(id, price)| (id.0, price.to_string()))
                .collect(),
            commissions: self
                .charges
                .iter()
                .map(|(id, amount)| (id.0, amount.to_string()))
                .collect(),
        };
        let text = serde_json::to_string_pretty(&stored)?;
        // Written beside and renamed, so a crash mid-write cannot leave
        // half a file where the pricing used to be.
        let temp = self.path.with_extension("tmp");
        fs::write(&temp, text).with_context(|| format!("writing {}", temp.display()))?;
        fs::rename(&temp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }

    /// What this instrument is marked up by, per side. Zero when none.
    pub fn of(&self, instrument: InstrumentId) -> Price {
        self.held.get(&instrument).copied().unwrap_or(Price::ZERO)
    }

    /// What this instrument charges to open, per unit. Zero when none.
    pub fn commission_of(&self, instrument: InstrumentId) -> Money {
        self.charges
            .get(&instrument)
            .copied()
            .unwrap_or(Money::ZERO)
    }

    /// Sets the commission, or removes it when zero.
    pub fn set_commission(&mut self, instrument: InstrumentId, per_unit: Money) -> Result<()> {
        if per_unit < Money::ZERO {
            bail!("a commission cannot be negative — the house does not pay the client to trade");
        }
        self.put_commission(instrument, per_unit);
        self.save()
    }

    /// Everything set, for the desk to read.
    pub fn all(&self) -> impl Iterator<Item = (InstrumentId, Price)> + '_ {
        self.held.iter().map(|(id, price)| (*id, *price))
    }

    /// Sets a markup, or removes it when zero.
    pub fn set(&mut self, instrument: InstrumentId, markup: Price) -> Result<()> {
        self.put_markup(instrument, markup)?;
        self.save()
    }

    /// Sets many at once, writing the file a single time.
    ///
    /// # Why this is not a loop over `set`
    ///
    /// Two reasons, and the second is the one that matters.
    ///
    /// A loop writes the file once per instrument, so changing twenty-one
    /// symbols means twenty-one temp-and-renames. That is merely wasteful.
    ///
    /// The real point is that a loop can stop halfway. A bad value in the
    /// middle would leave some symbols repriced and some not, with nothing
    /// saying which — and the operator's next move would be to apply it
    /// again, over the half that already took. Here every value is checked
    /// before any of them is applied, so the answer is all or nothing.
    ///
    /// `markup` and `commission` are each optional: a desk setting a
    /// spread across a list should not have to restate the commission it
    /// was not changing.
    pub fn set_many(
        &mut self,
        instruments: &[InstrumentId],
        markup: Option<Price>,
        commission: Option<Money>,
    ) -> Result<usize> {
        if instruments.is_empty() {
            bail!("no instruments were named");
        }
        if markup.is_none() && commission.is_none() {
            bail!("nothing to set");
        }
        if markup.is_some_and(|m| m < Price::ZERO) {
            bail!("a markup cannot be negative — the house does not pay the client the spread");
        }
        if commission.is_some_and(|c| c < Money::ZERO) {
            bail!("a commission cannot be negative — the house does not pay the client to trade");
        }

        for id in instruments {
            if let Some(markup) = markup {
                self.put_markup(*id, markup)?;
            }
            if let Some(commission) = commission {
                self.put_commission(*id, commission);
            }
        }
        self.save()?;
        Ok(instruments.len())
    }

    /// Puts one markup in memory, without writing.
    fn put_markup(&mut self, instrument: InstrumentId, markup: Price) -> Result<()> {
        if markup < Price::ZERO {
            bail!("a markup cannot be negative — the house does not pay the client the spread");
        }
        // Zero is removal rather than a stored zero: the file says what
        // the desk has decided, and "nothing" is not a decision.
        if markup == Price::ZERO {
            self.held.remove(&instrument);
        } else {
            self.held.insert(instrument, markup);
        }
        Ok(())
    }

    /// The same for a commission. Non-negative is checked by the caller.
    fn put_commission(&mut self, instrument: InstrumentId, per_unit: Money) {
        if per_unit == Money::ZERO {
            self.charges.remove(&instrument);
        } else {
            self.charges.insert(instrument, per_unit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("feraro-mk-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&p).expect("scratch");
            Scratch(p)
        }
        fn file(&self) -> PathBuf {
            self.0.join("markups.json")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn setting_many_is_all_or_nothing() {
        // A loop over the single setter could stop halfway, leaving some
        // symbols repriced and some not with nothing saying which — and
        // the operator's next move would be to apply it again, over the
        // half that already took.
        let s = Scratch::new("bulk-atomic");
        let mut m = Markups::load(&s.file()).unwrap();
        let ids = [InstrumentId(1), InstrumentId(2), InstrumentId(3)];

        let bad = m.set_many(&ids, Some(Price::parse("-0.5").unwrap()), None);
        assert!(bad.is_err());
        for id in ids {
            assert_eq!(m.of(id), Price::ZERO, "a refusal priced something");
        }

        assert_eq!(
            m.set_many(&ids, Some(Price::parse("0.5").unwrap()), None)
                .unwrap(),
            3
        );
        for id in ids {
            assert_eq!(m.of(id), Price::parse("0.5").unwrap());
        }
    }

    #[test]
    fn an_empty_field_leaves_that_one_alone() {
        // A desk setting a spread across a list should not have to
        // restate the commission it is not changing.
        let s = Scratch::new("bulk-partial");
        let mut m = Markups::load(&s.file()).unwrap();
        let ids = [InstrumentId(1)];
        m.set_many(&ids, None, Some(Money::parse("2.50").unwrap()))
            .unwrap();
        m.set_many(&ids, Some(Price::parse("0.5").unwrap()), None)
            .unwrap();

        assert_eq!(m.of(InstrumentId(1)), Price::parse("0.5").unwrap());
        assert_eq!(
            m.commission_of(InstrumentId(1)),
            Money::parse("2.50").unwrap(),
            "the commission was wiped by a markup change"
        );
    }

    #[test]
    fn setting_many_to_zero_removes_them() {
        // The file says what the desk has decided, and "nothing" is not
        // a decision — otherwise every instrument ever touched stays in
        // the file forever at zero.
        let s = Scratch::new("bulk-zero");
        let mut m = Markups::load(&s.file()).unwrap();
        let ids = [InstrumentId(1), InstrumentId(2)];
        m.set_many(
            &ids,
            Some(Price::parse("0.5").unwrap()),
            Some(Money::parse("1.00").unwrap()),
        )
        .unwrap();
        assert_eq!(m.all().count(), 2);

        m.set_many(&ids, Some(Price::ZERO), Some(Money::ZERO))
            .unwrap();
        assert_eq!(m.all().count(), 0);
    }

    #[test]
    fn setting_many_refuses_an_empty_list_and_an_empty_change() {
        let s = Scratch::new("bulk-empty");
        let mut m = Markups::load(&s.file()).unwrap();
        assert!(m.set_many(&[], Some(Price::ZERO), None).is_err());
        assert!(m.set_many(&[InstrumentId(1)], None, None).is_err());
    }

    #[test]
    fn a_fresh_platform_charges_nothing() {
        let s = Scratch::new("fresh");
        let m = Markups::load(&s.file()).unwrap();
        assert_eq!(m.of(InstrumentId(1)), Price::ZERO);
        assert_eq!(
            m.all().count(),
            0,
            "inventing a markup is taking money nobody agreed to"
        );
    }

    #[test]
    fn a_markup_survives_a_reload_to_the_last_place() {
        let s = Scratch::new("reload");
        {
            let mut m = Markups::load(&s.file()).unwrap();
            m.set(InstrumentId(1), Price::parse("0.000000007").unwrap())
                .unwrap();
        }
        let m = Markups::load(&s.file()).unwrap();
        assert_eq!(m.of(InstrumentId(1)), Price::parse("0.000000007").unwrap());
    }

    #[test]
    fn setting_it_to_zero_removes_it() {
        let s = Scratch::new("zero");
        let mut m = Markups::load(&s.file()).unwrap();
        m.set(InstrumentId(1), Price::parse("0.02").unwrap())
            .unwrap();
        m.set(InstrumentId(1), Price::ZERO).unwrap();
        assert_eq!(m.of(InstrumentId(1)), Price::ZERO);
        assert_eq!(m.all().count(), 0, "zero is absence, not a stored zero");
    }

    #[test]
    fn a_negative_markup_is_refused() {
        let s = Scratch::new("negative");
        let mut m = Markups::load(&s.file()).unwrap();
        assert!(m
            .set(InstrumentId(1), Price::parse("-0.01").unwrap())
            .is_err());
        assert_eq!(m.of(InstrumentId(1)), Price::ZERO);
    }

    #[test]
    fn an_unreadable_figure_charges_nothing_rather_than_guessing() {
        let s = Scratch::new("junk");
        std::fs::write(s.file(), r#"{"markups":{"1":"two pips","2":"0.02"}}"#).unwrap();
        let m = Markups::load(&s.file()).unwrap();
        assert_eq!(m.of(InstrumentId(1)), Price::ZERO);
        // The one beside it is unaffected: a typo costs its own line only.
        assert_eq!(m.of(InstrumentId(2)), Price::parse("0.02").unwrap());
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let s = Scratch::new("absent");
        let m = Markups::load(&s.0.join("nothing-here.json")).unwrap();
        assert_eq!(m.all().count(), 0);
    }
}
