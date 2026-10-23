//! Snapshots: engine state on disk, so startup stops reading all of history.
//!
//! # What a snapshot is for
//!
//! Segmentation bounds the *memory* a replay needs; it does nothing about the
//! *time*. A journal that has been running for a year is folded from genesis
//! every time the gateway starts. A snapshot says "this was the state at
//! sequence N", and startup folds only what came after it.
//!
//! # What it is not
//!
//! Not the record. The journal is the record and a snapshot is derived from
//! it: delete every snapshot and the gateway still comes up with exactly the
//! same state, only slower. That is the property worth keeping, and it is why
//! a snapshot that cannot be read is a log line rather than an error.
//!
//! # The format
//!
//! JSON, with every money value as a decimal string — the same discipline as
//! the browser protocol, for the same reason: a float in a balance is a bug
//! that only shows up at magnitudes nobody tests. It carries a version, and a
//! snapshot whose version this build does not know is ignored rather than
//! guessed at.
//!
//! The journal's own wire format is deliberately *not* used here. That format
//! must stay byte-stable for ever because a regulator may replay a 2029
//! trading day; a snapshot is a cache, and holding it to the same promise
//! would mean never being able to change it.
//!
//! # Writing is atomic
//!
//! A snapshot is written to a temporary file, flushed, and renamed over the
//! old one. A half-written snapshot that looked complete would be the one
//! thing worse than no snapshot: a gateway that starts from a state that
//! never existed.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use domain::{AccountId, Currency, Instrument, InstrumentId, OrderId, Seq, Symbol, UserId};
use ledger::{AccountKind, Ledger, LedgerAccount, TxId};
use money::{Money, Price, Qty};
use risk::{AccountState, Lot, Portfolio, PortfolioState};
use serde::{Deserialize, Serialize};

/// Bumped whenever the shape below changes. An older or newer snapshot is
/// ignored, and the journal is replayed in full instead.
pub const SNAPSHOT_VERSION: u32 = 2;

/// How many events may pass before the gateway writes a new snapshot.
///
/// Ten thousand is a few seconds of replay, which is the point: the cost of a
/// crash is bounded by how much of the tail has to be re-folded, and the cost
/// of writing is bounded by how often.
pub const SNAPSHOT_EVERY: u64 = 10_000;

/// The file, next to the journal it describes.
pub fn path_for(dir: &Path, base: &str) -> PathBuf {
    dir.join(format!("{base}.snapshot.json"))
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredLot {
    ticket: u64,
    side: String,
    qty: String,
    open_price: String,
    stop_loss: String,
    take_profit: String,
    swap: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredAccount {
    id: u64,
    currency: String,
    leverage: u32,
    balance: String,
    credit: String,
    tradeable: bool,
    hedging: bool,
    live: bool,
    /// Nanoseconds since the epoch: when the demo clock started.
    clock_started: i64,
    called: bool,
    lots: Vec<(u32, Vec<StoredLot>)>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredInstrument {
    id: u32,
    symbol: String,
    base: String,
    quote: String,
    display_digits: u8,
    contract_size: String,
    tick_size: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredBook {
    owner: u64,
    kind: String,
    currency: String,
    amount: String,
}

/// Everything a snapshot holds.
#[derive(Debug, Serialize, Deserialize)]
pub struct Stored {
    version: u32,
    /// The state is as of *after* this sequence. Replay resumes at the next.
    pub through: u64,
    /// The currency the portfolio settles in, checked on load: a snapshot
    /// from a dollar book must not be loaded into a euro one.
    settlement: String,
    accounts: Vec<StoredAccount>,
    instruments: Vec<StoredInstrument>,
    order_owner: Vec<(u64, u64, u32)>,
    owners: Vec<(u64, u64)>,
    last_rollover: Option<u32>,
    quotes: Vec<(u32, String, String)>,
    books: Vec<StoredBook>,
}

/// The books' pot names. Stable on disk, so a rename in the ledger crate does
/// not silently move money between pots.
fn kind_name(kind: AccountKind) -> &'static str {
    match kind {
        AccountKind::ClientCash => "clientCash",
        AccountKind::PspClearing => "pspClearing",
        AccountKind::RealisedPnl => "realisedPnl",
        AccountKind::Commission => "commission",
        AccountKind::Swap => "swap",
        AccountKind::Bonus => "bonus",
        AccountKind::LpMargin => "lpMargin",
        AccountKind::FxConversion => "fxConversion",
        AccountKind::IbCommission => "ibCommission",
        AccountKind::Adjustment => "adjustment",
    }
}

fn kind_from(name: &str) -> Option<AccountKind> {
    Some(match name {
        "clientCash" => AccountKind::ClientCash,
        "pspClearing" => AccountKind::PspClearing,
        "realisedPnl" => AccountKind::RealisedPnl,
        "commission" => AccountKind::Commission,
        "swap" => AccountKind::Swap,
        "bonus" => AccountKind::Bonus,
        "lpMargin" => AccountKind::LpMargin,
        "fxConversion" => AccountKind::FxConversion,
        "ibCommission" => AccountKind::IbCommission,
        "adjustment" => AccountKind::Adjustment,
        _ => return None,
    })
}

/// Builds a snapshot of the state as of `through`.
pub fn capture(portfolio: &Portfolio, ledger: &Ledger, through: Seq) -> Stored {
    let state = portfolio.export();
    Stored {
        version: SNAPSHOT_VERSION,
        through: through.0,
        settlement: portfolio.market.settlement().to_string(),
        accounts: state
            .accounts
            .iter()
            .map(|a| StoredAccount {
                id: a.id.0,
                currency: a.currency.to_string(),
                leverage: a.leverage,
                balance: a.balance.to_decimal_string(),
                credit: a.credit.to_decimal_string(),
                tradeable: a.tradeable,
                hedging: a.hedging,
                live: a.live,
                clock_started: a.clock_started.0,
                called: a.called,
                lots: a
                    .lots
                    .iter()
                    .map(|(instrument, lots)| {
                        (
                            instrument.0,
                            lots.iter()
                                .map(|l| StoredLot {
                                    ticket: l.ticket.0,
                                    side: match l.side {
                                        money::Side::Buy => "buy".to_owned(),
                                        money::Side::Sell => "sell".to_owned(),
                                    },
                                    qty: l.qty.to_decimal_string(),
                                    open_price: l.open_price.to_decimal_string(),
                                    stop_loss: l.stop_loss.to_decimal_string(),
                                    take_profit: l.take_profit.to_decimal_string(),
                                    swap: l.swap.to_decimal_string(),
                                })
                                .collect(),
                        )
                    })
                    .collect(),
            })
            .collect(),
        instruments: state
            .instruments
            .iter()
            .map(|i| StoredInstrument {
                id: i.id.0,
                symbol: i.symbol.as_str().to_owned(),
                base: i.base.to_string(),
                quote: i.quote.to_string(),
                display_digits: i.display_digits,
                contract_size: i.contract_size.to_decimal_string(),
                tick_size: i.tick_size.to_decimal_string(),
            })
            .collect(),
        order_owner: state
            .order_owner
            .iter()
            .map(|(o, a, i)| (o.0, a.0, i.0))
            .collect(),
        owners: state.owners.iter().map(|(a, u)| (a.0, u.0)).collect(),
        last_rollover: state.last_rollover,
        quotes: state
            .quotes
            .iter()
            .map(|(i, bid, ask)| (i.0, bid.to_decimal_string(), ask.to_decimal_string()))
            .collect(),
        books: ledger
            .export()
            .iter()
            .map(|(account, amount)| StoredBook {
                owner: account.owner.0,
                kind: kind_name(account.kind).to_owned(),
                currency: account.currency.to_string(),
                amount: amount.to_decimal_string(),
            })
            .collect(),
    }
}

/// Writes a snapshot, atomically.
pub fn write(dir: &Path, base: &str, stored: &Stored) -> Result<()> {
    let final_path = path_for(dir, base);
    let temp = final_path.with_extension("json.tmp");
    let text = serde_json::to_string(stored).context("serialising a snapshot")?;
    std::fs::write(&temp, text).with_context(|| format!("writing {}", temp.display()))?;
    // Rename over the old one: a reader sees either the previous snapshot or
    // this one, never half of either.
    std::fs::rename(&temp, &final_path)
        .with_context(|| format!("replacing {}", final_path.display()))?;
    Ok(())
}

/// Reads a snapshot, or `None` when there is nothing usable to read.
///
/// Every failure is `None` rather than an error: a snapshot is a cache, and a
/// gateway that refuses to start because its cache is unreadable is worse than
/// one that takes longer to start.
pub fn read(dir: &Path, base: &str, settlement: Currency) -> Option<Stored> {
    let path = path_for(dir, base);
    let text = std::fs::read_to_string(&path).ok()?;
    let stored: Stored = match serde_json::from_str(&text) {
        Ok(stored) => stored,
        Err(err) => {
            eprintln!(
                "snapshot: {} could not be read ({err}); replaying the journal in full",
                path.display()
            );
            return None;
        }
    };
    if stored.version != SNAPSHOT_VERSION {
        eprintln!(
            "snapshot: version {} is not this build's {SNAPSHOT_VERSION}; replaying in full",
            stored.version
        );
        return None;
    }
    if stored.settlement != settlement.to_string() {
        eprintln!(
            "snapshot: settles in {} but this gateway settles in {settlement}; replaying in full",
            stored.settlement
        );
        return None;
    }
    Some(stored)
}

/// Rebuilds a portfolio and the books from a snapshot.
///
/// `None` when any number in it cannot be parsed — a snapshot is only useful
/// if it is exactly right, and "mostly right" state is the worst outcome
/// available.
pub fn restore(stored: &Stored, settlement: Currency) -> Option<(Portfolio, Ledger)> {
    let money = |s: &str| Money::parse(s).ok();
    let price = |s: &str| Price::parse(s).ok();
    let qty = |s: &str| Qty::parse(s).ok();
    let currency = |s: &str| {
        let bytes = s.as_bytes();
        (bytes.len() == 3).then(|| Currency::new(&[bytes[0], bytes[1], bytes[2]]))
    };

    let mut accounts = Vec::new();
    for a in &stored.accounts {
        let mut lots = Vec::new();
        for (instrument, stored_lots) in &a.lots {
            let mut parcels = Vec::new();
            for l in stored_lots {
                parcels.push(Lot {
                    ticket: OrderId(l.ticket),
                    side: match l.side.as_str() {
                        "buy" => money::Side::Buy,
                        "sell" => money::Side::Sell,
                        _ => return None,
                    },
                    qty: qty(&l.qty)?,
                    open_price: price(&l.open_price)?,
                    stop_loss: price(&l.stop_loss)?,
                    take_profit: price(&l.take_profit)?,
                    swap: money(&l.swap)?,
                });
            }
            lots.push((InstrumentId(*instrument), parcels));
        }
        accounts.push(AccountState {
            id: AccountId(a.id),
            currency: currency(&a.currency)?,
            leverage: a.leverage,
            balance: money(&a.balance)?,
            credit: money(&a.credit)?,
            tradeable: a.tradeable,
            hedging: a.hedging,
            live: a.live,
            clock_started: domain::Timestamp(a.clock_started),
            called: a.called,
            lots,
        });
    }

    let mut instruments = Vec::new();
    for i in &stored.instruments {
        instruments.push(Instrument {
            id: InstrumentId(i.id),
            symbol: Symbol::new(&i.symbol),
            base: currency(&i.base)?,
            quote: currency(&i.quote)?,
            display_digits: i.display_digits,
            contract_size: qty(&i.contract_size)?,
            tick_size: price(&i.tick_size)?,
        });
    }

    let mut quotes = Vec::new();
    for (instrument, bid, ask) in &stored.quotes {
        quotes.push((InstrumentId(*instrument), price(bid)?, price(ask)?));
    }

    let state = PortfolioState {
        accounts,
        instruments,
        order_owner: stored
            .order_owner
            .iter()
            .map(|(o, a, i)| (OrderId(*o), AccountId(*a), InstrumentId(*i)))
            .collect(),
        owners: stored
            .owners
            .iter()
            .map(|(a, u)| (AccountId(*a), UserId(*u)))
            .collect(),
        last_rollover: stored.last_rollover,
        quotes,
    };

    let mut books = Vec::new();
    for b in &stored.books {
        books.push((
            LedgerAccount {
                owner: AccountId(b.owner),
                kind: kind_from(&b.kind)?,
                currency: currency(&b.currency)?,
            },
            money(&b.amount)?,
        ));
    }

    Some((
        Portfolio::import(state, settlement),
        Ledger::import(books, TxId(stored.through)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("feraro-snap-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&p).expect("scratch");
            Scratch(p)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_snapshot_round_trips_through_a_file() {
        let s = Scratch::new("round-trip");
        let portfolio = Portfolio::new();
        let ledger = Ledger::new();
        let stored = capture(&portfolio, &ledger, Seq(42));
        write(&s.0, "test", &stored).expect("writes");

        let back = read(&s.0, "test", Currency::USD).expect("reads");
        assert_eq!(back.through, 42);
        assert!(restore(&back, Currency::USD).is_some());
    }

    #[test]
    fn a_snapshot_from_another_version_or_currency_is_ignored() {
        let s = Scratch::new("mismatch");
        let stored = capture(&Portfolio::new(), &Ledger::new(), Seq(1));
        write(&s.0, "test", &stored).expect("writes");

        // A different settlement currency is not a snapshot of this book.
        assert!(read(&s.0, "test", Currency::EUR).is_none());

        // And neither is a version this build does not know.
        let text = std::fs::read_to_string(path_for(&s.0, "test")).unwrap();
        let bumped = text.replace(
            &format!("\"version\":{SNAPSHOT_VERSION}"),
            "\"version\":9999",
        );
        std::fs::write(path_for(&s.0, "test"), bumped).unwrap();
        assert!(read(&s.0, "test", Currency::USD).is_none());
    }

    #[test]
    fn a_torn_or_missing_snapshot_is_not_an_error() {
        let s = Scratch::new("torn");
        assert!(
            read(&s.0, "test", Currency::USD).is_none(),
            "nothing there yet"
        );

        std::fs::write(path_for(&s.0, "test"), "{ this is not json").unwrap();
        assert!(
            read(&s.0, "test", Currency::USD).is_none(),
            "a snapshot is a cache; an unreadable one costs time, not correctness"
        );
    }

    #[test]
    fn writing_leaves_no_temporary_file_behind() {
        let s = Scratch::new("atomic");
        let stored = capture(&Portfolio::new(), &Ledger::new(), Seq(7));
        write(&s.0, "test", &stored).expect("writes");

        let names: Vec<String> = std::fs::read_dir(&s.0)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["test.snapshot.json".to_owned()]);
    }

    #[test]
    fn every_ledger_pot_has_a_stable_name_on_disk() {
        // A rename in the ledger crate must not silently move money between
        // pots, so the names are written out here and checked both ways.
        for kind in [
            AccountKind::ClientCash,
            AccountKind::PspClearing,
            AccountKind::RealisedPnl,
            AccountKind::Commission,
            AccountKind::Swap,
            AccountKind::Bonus,
            AccountKind::LpMargin,
            AccountKind::FxConversion,
            AccountKind::IbCommission,
            AccountKind::Adjustment,
        ] {
            assert_eq!(kind_from(kind_name(kind)), Some(kind));
        }
        assert_eq!(kind_from("somethingElse"), None);
    }
}
