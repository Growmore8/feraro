//! What the desk has changed about an instrument.
//!
//! # Why this is a file and not a journal event
//!
//! The same reason `markups.json` and `payments.json` are files. The
//! journal holds *facts* — this order was accepted, that money moved —
//! and a fact is permanent. "BTCUSD is enabled" is not a fact, it is a
//! setting: it is switched back and forth, and the thing anybody wants to
//! know is what it is *now*, not the whole history of what it has been.
//!
//! Putting settings in an append-only log would mean replaying every
//! change ever made to answer a question the current value answers, and
//! would make a typo permanent.
//!
//! # Why only the differences are stored
//!
//! The compiled catalogue stays the source of truth and this holds what
//! the desk has said on top of it. An instrument nobody has touched has no
//! entry at all, so a catalogue correction — a wrong tick size, a new
//! listing — reaches every platform on the next deploy rather than being
//! shadowed forever by a value somebody accepted once.
//!
//! It also means "put it back" is deleting a line rather than knowing what
//! the number used to be.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use domain::InstrumentId;
use money::Qty;
use serde::{Deserialize, Serialize};

/// What the desk has said about one instrument, where it has said anything.
///
/// Every field is optional and absent means "whatever the catalogue says".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Override {
    /// False to stop the instrument accepting orders.
    ///
    /// The reason a desk reaches for this is a feed gone wrong: the price
    /// is visibly stale or plainly incorrect, and every second it stays
    /// open is a trade somebody will dispute. It must be one click.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Smallest order accepted, as a decimal string.
    ///
    /// A string rather than a number, like every size on this platform: a
    /// quantity that has been through an `f64` is a quantity that may not
    /// come back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_qty: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_qty: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qty_step: Option<String>,
}

impl Override {
    /// True when nothing is set, so the line can be dropped entirely.
    fn is_empty(&self) -> bool {
        self.enabled.is_none()
            && self.min_qty.is_none()
            && self.max_qty.is_none()
            && self.qty_step.is_none()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Stored {
    #[serde(default)]
    symbols: BTreeMap<u32, Override>,
}

/// The overrides, loaded into memory.
#[derive(Debug)]
pub struct Symbols {
    path: PathBuf,
    held: BTreeMap<InstrumentId, Override>,
}

impl Symbols {
    /// Loads the file, or starts with nothing overridden.
    ///
    /// Nothing is the right default and an unreadable file is not: a
    /// platform that silently forgets which instruments the desk disabled
    /// would re-enable a symbol somebody pulled for a reason.
    pub fn load(path: &Path) -> Result<Symbols> {
        let stored: Stored = match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("parsing the symbol settings at {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Stored::default(),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("reading the symbol settings at {}", path.display()))
            }
        };

        let mut held = BTreeMap::new();
        for (id, set) in stored.symbols {
            // A size that will not parse is dropped rather than guessed
            // at: refusing to start over one bad line would take the whole
            // platform down for a typo, and guessing would silently admit
            // orders nobody agreed to.
            let checked = Override {
                enabled: set.enabled,
                min_qty: set.min_qty.filter(|v| Qty::parse(v).is_ok()),
                max_qty: set.max_qty.filter(|v| Qty::parse(v).is_ok()),
                qty_step: set.qty_step.filter(|v| Qty::parse(v).is_ok()),
            };
            if !checked.is_empty() {
                held.insert(InstrumentId(id), checked);
            }
        }

        Ok(Symbols {
            path: path.to_path_buf(),
            held,
        })
    }

    /// What the desk has said about one instrument, if anything.
    pub fn of(&self, instrument: InstrumentId) -> Option<&Override> {
        self.held.get(&instrument)
    }

    /// Everything the desk has changed, for the screen to show.
    pub fn all(&self) -> impl Iterator<Item = (InstrumentId, &Override)> + '_ {
        self.held.iter().map(|(id, set)| (*id, set))
    }

    /// Opens an instrument for trading, or closes it.
    pub fn set_enabled(&mut self, instrument: InstrumentId, enabled: bool) -> Result<()> {
        self.held.entry(instrument).or_default().enabled = Some(enabled);
        self.save()
    }

    /// Sets one of the size rules, or clears it back to the catalogue.
    ///
    /// The three are set together rather than one at a time because they
    /// only make sense together: a minimum above the maximum, or a step
    /// that does not divide the minimum, is a symbol nobody can trade, and
    /// checking one against two stale neighbours would let that through.
    pub fn set_sizes(
        &mut self,
        instrument: InstrumentId,
        min: &str,
        max: &str,
        step: &str,
    ) -> Result<()> {
        let min = min.trim();
        let max = max.trim();
        let step = step.trim();

        // All three empty is "put it back to the catalogue".
        if min.is_empty() && max.is_empty() && step.is_empty() {
            if let Some(set) = self.held.get_mut(&instrument) {
                set.min_qty = None;
                set.max_qty = None;
                set.qty_step = None;
                if set.is_empty() {
                    self.held.remove(&instrument);
                }
            }
            return self.save();
        }

        let parse = |what: &str, text: &str| -> Result<Qty> {
            Qty::parse(text).map_err(|_| anyhow::anyhow!("{what} {text:?} is not a size"))
        };
        let min_qty = parse("minimum", min)?;
        let max_qty = parse("maximum", max)?;
        let qty_step = parse("step", step)?;

        if min_qty <= Qty::ZERO || qty_step <= Qty::ZERO {
            bail!("a minimum and a step have to be more than nothing");
        }
        if max_qty < min_qty {
            bail!("the maximum is below the minimum, so nothing could be traded");
        }
        // Otherwise the smallest order anybody may place is not on the
        // grid every order has to sit on, and the instrument is closed to
        // everyone while looking open.
        if min_qty.raw() % qty_step.raw() != 0 {
            bail!("the minimum is not a whole number of steps, so no order would be accepted");
        }

        let set = self.held.entry(instrument).or_default();
        set.min_qty = Some(min_qty.to_string());
        set.max_qty = Some(max_qty.to_string());
        set.qty_step = Some(qty_step.to_string());
        self.save()
    }

    /// Writes it out, replacing the file in one step.
    ///
    /// Temp-and-rename, so a crash mid-write leaves the old settings
    /// rather than half of the new ones: a truncated file here would read
    /// as "nothing is overridden", which silently re-opens every symbol
    /// the desk had closed.
    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let stored = Stored {
            symbols: self
                .held
                .iter()
                .map(|(id, set)| (id.0, set.clone()))
                .collect(),
        };
        let text = serde_json::to_string_pretty(&stored)?;
        let temp = self.path.with_extension("tmp");
        fs::write(&temp, text)
            .with_context(|| format!("writing the symbol settings to {}", temp.display()))?;
        fs::rename(&temp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("feraro-sym-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&p).expect("scratch");
            Scratch(p)
        }
        fn file(&self) -> PathBuf {
            self.0.join("symbols.json")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const BTC: InstrumentId = InstrumentId(1);

    #[test]
    fn a_fresh_platform_overrides_nothing() {
        let s = Scratch::new("fresh");
        let held = Symbols::load(&s.file()).unwrap();
        assert!(held.of(BTC).is_none());
        assert_eq!(held.all().count(), 0);
    }

    #[test]
    fn closing_an_instrument_survives_a_restart() {
        // The reason a desk reaches for this is a feed gone wrong, and a
        // symbol that re-opened itself on the next deploy would be the
        // worst possible answer to that.
        let s = Scratch::new("closed");
        {
            let mut held = Symbols::load(&s.file()).unwrap();
            held.set_enabled(BTC, false).unwrap();
        }
        let again = Symbols::load(&s.file()).unwrap();
        assert_eq!(again.of(BTC).and_then(|o| o.enabled), Some(false));
    }

    #[test]
    fn sizes_round_trip_as_the_strings_they_were_given() {
        let s = Scratch::new("sizes");
        {
            let mut held = Symbols::load(&s.file()).unwrap();
            held.set_sizes(BTC, "0.002", "20", "0.002").unwrap();
        }
        let again = Symbols::load(&s.file()).unwrap();
        let set = again.of(BTC).expect("set");
        // Compared as sizes, not as text: they are stored in the canonical
        // form, so "20" comes back as "20.00000000". What has to survive
        // is the value, and it is kept as a string so that it can — a
        // size through an `f64` is a size that may not come back.
        let same = |held: &Option<String>, asked: &str| {
            Qty::parse(held.as_deref().expect("set")).unwrap() == Qty::parse(asked).unwrap()
        };
        assert!(same(&set.min_qty, "0.002"));
        assert!(same(&set.max_qty, "20"));
        assert!(same(&set.qty_step, "0.002"));
    }

    #[test]
    fn sizes_that_would_close_an_instrument_while_it_looks_open_are_refused() {
        let s = Scratch::new("sizes-bad");
        let mut held = Symbols::load(&s.file()).unwrap();

        // A maximum below the minimum: nothing could ever be traded.
        assert!(held.set_sizes(BTC, "1", "0.5", "0.1").is_err());
        // A minimum that is not a whole number of steps: every order is
        // either below the minimum or off the grid.
        assert!(held.set_sizes(BTC, "0.15", "10", "0.1").is_err());
        // Nothing at all.
        assert!(held.set_sizes(BTC, "0", "10", "0.1").is_err());
        assert!(held.set_sizes(BTC, "0.1", "10", "0").is_err());
        // Not a size.
        assert!(held.set_sizes(BTC, "lots", "10", "0.1").is_err());

        assert!(held.of(BTC).is_none(), "a refusal left something behind");
    }

    #[test]
    fn clearing_the_sizes_puts_the_catalogue_back() {
        // Putting it back is deleting a line, not remembering what the
        // number used to be.
        let s = Scratch::new("sizes-clear");
        let mut held = Symbols::load(&s.file()).unwrap();
        held.set_sizes(BTC, "0.002", "20", "0.002").unwrap();
        held.set_sizes(BTC, "", "", "").unwrap();
        assert!(held.of(BTC).is_none());
    }

    #[test]
    fn closing_an_instrument_and_clearing_its_sizes_keeps_it_closed() {
        // They are separate decisions, and clearing one must not quietly
        // undo the other.
        let s = Scratch::new("sizes-keep-closed");
        let mut held = Symbols::load(&s.file()).unwrap();
        held.set_enabled(BTC, false).unwrap();
        held.set_sizes(BTC, "0.002", "20", "0.002").unwrap();
        held.set_sizes(BTC, "", "", "").unwrap();
        assert_eq!(held.of(BTC).and_then(|o| o.enabled), Some(false));
        assert!(held.of(BTC).unwrap().min_qty.is_none());
    }

    #[test]
    fn a_setting_that_will_not_parse_is_dropped_rather_than_guessed_at() {
        // Refusing to start over one bad line would take the platform down
        // for a typo; guessing would admit orders nobody agreed to.
        let s = Scratch::new("bad-line");
        std::fs::write(
            s.file(),
            r#"{"symbols":{"1":{"enabled":false,"minQty":"not a size","maxQty":"10","qtyStep":"0.1"}}}"#,
        )
        .unwrap();
        let held = Symbols::load(&s.file()).unwrap();
        let set = held.of(BTC).expect("the readable parts survive");
        assert_eq!(set.enabled, Some(false));
        assert!(set.min_qty.is_none(), "an unreadable size was kept");
        assert_eq!(set.max_qty.as_deref(), Some("10"));
    }

    #[test]
    fn a_file_from_before_this_existed_reads_as_nothing_overridden() {
        let s = Scratch::new("old-file");
        std::fs::write(s.file(), "{}").unwrap();
        assert_eq!(Symbols::load(&s.file()).unwrap().all().count(), 0);
    }
}
