//! How a client pays money in, and how the desk confirms it arrived.
//!
//! # No money moves through here
//!
//! There is no payment provider. What this holds is the *instructions* — a
//! wallet address, a UPI id, a bank account — and the **claims** clients
//! make that they have paid. Nothing is credited by this module. A request
//! sits here until somebody at the desk looks at their bank or their wallet,
//! sees the money, and deposits it; that deposit is journaled with the
//! operator's name on it like every other.
//!
//! That ordering is the point. A platform that credits an account because a
//! client typed a reference number is a platform that pays out money it
//! never received.
//!
//! # Why this is a file and not the journal
//!
//! Payment methods are configuration: a wallet is rotated, a bank changes,
//! a method is switched off for a week. An append-only log is the wrong
//! shape for a thing whose current value is the only one that matters.
//!
//! Requests are desk workflow — a queue of claims waiting to be checked.
//! They are kept here so a restart does not lose a client's claim, and
//! they are *not* money. The money is the journaled deposit that may
//! follow, and the request records which one it became.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use domain::{AccountId, UserId};

/// How the money travels.
///
/// Three kinds because three is what the desk takes, and each needs
/// different words on screen: a wallet address is copied, a UPI id is typed
/// into another app, bank details are read in a form. They are not
/// interchangeable to the person paying, so they are not one kind with a
/// label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rail {
    Crypto,
    Upi,
    Bank,
}

impl Rail {
    pub const fn as_str(self) -> &'static str {
        match self {
            Rail::Crypto => "crypto",
            Rail::Upi => "upi",
            Rail::Bank => "bank",
        }
    }

    pub fn parse(name: &str) -> Option<Rail> {
        match name {
            "crypto" => Some(Rail::Crypto),
            "upi" => Some(Rail::Upi),
            "bank" => Some(Rail::Bank),
            _ => None,
        }
    }
}

/// One way to pay, as the desk has set it up.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Method {
    pub id: u64,
    pub rail: Rail,
    /// What the client sees it called: "USDT (TRC-20)", "HDFC Bank".
    pub label: String,
    /// The address, id or account details. Shown verbatim and copyable.
    pub address: String,
    /// Anything the client must do to be credited — a note in the transfer,
    /// a minimum, a network to use. Empty when there is nothing to say.
    #[serde(default)]
    pub note: String,
    /// Off means clients do not see it. Rotating a wallet means switching
    /// the old one off, not deleting it: a client may already be paying to
    /// it, and the desk needs to recognise the address when they do.
    pub enabled: bool,
}

/// What is being asked for.
///
/// Every way money moves on this platform except a trade, in one queue,
/// because they share a shape: somebody asks, somebody checks, somebody
/// decides. One queue means one answer to "what is waiting", and one place
/// where the rule "no money moves until an administrator says so" lives.
///
/// The in-and-out pairs are deltas on a pot rather than absolute settings.
/// A client asking for a bonus is asking for *more*, and an operator
/// clearing one is taking *some* away; neither knows what the figure was
/// before, and neither should have to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    /// "I have sent you money." Credited once the desk has seen it.
    Deposit,
    /// "Please send me money." Paid once the desk has sent it.
    Withdrawal,
    /// Bonus granted by the house. Counts toward margin, never withdrawable.
    BonusIn,
    /// Bonus taken back, expired, or cleared.
    BonusOut,
    /// Money set aside against losses.
    InsuranceIn,
    /// Insurance released back, or used.
    InsuranceOut,
}

impl Kind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Kind::Deposit => "deposit",
            Kind::Withdrawal => "withdrawal",
            Kind::BonusIn => "bonusIn",
            Kind::BonusOut => "bonusOut",
            Kind::InsuranceIn => "insuranceIn",
            Kind::InsuranceOut => "insuranceOut",
        }
    }

    pub fn parse(name: &str) -> Option<Kind> {
        match name {
            "deposit" => Some(Kind::Deposit),
            "withdrawal" => Some(Kind::Withdrawal),
            "bonusIn" => Some(Kind::BonusIn),
            "bonusOut" => Some(Kind::BonusOut),
            "insuranceIn" => Some(Kind::InsuranceIn),
            "insuranceOut" => Some(Kind::InsuranceOut),
            _ => None,
        }
    }

    /// Whether a client may raise this themselves.
    ///
    /// Bonus and insurance are the house's to grant: a client wanting one
    /// is asking a person, not pressing a button.
    pub const fn client_may_ask(self) -> bool {
        matches!(self, Kind::Deposit | Kind::Withdrawal)
    }

    /// Which pot it moves, for the words on screen.
    pub const fn pot(self) -> &'static str {
        match self {
            Kind::Deposit | Kind::Withdrawal => "balance",
            Kind::BonusIn | Kind::BonusOut => "bonus",
            Kind::InsuranceIn | Kind::InsuranceOut => "insurance",
        }
    }
}

fn deposit_kind() -> Kind {
    Kind::Deposit
}

/// One request, as somebody asks for it.
///
/// A struct rather than eight arguments, because six of them are strings
/// and numbers in a row — which is how the wrong two get swapped, silently,
/// on a line that moves money.
#[derive(Debug, Clone, Copy)]
pub struct Asked<'a> {
    pub kind: Kind,
    pub account: AccountId,
    pub user: UserId,
    /// The house wallet paid to, for a deposit; the client's own payout
    /// method, for a withdrawal. Zero for the kinds that need neither.
    pub method: u64,
    pub amount: &'a str,
    pub reference: &'a str,
    /// Epoch milliseconds.
    pub at: i64,
}

/// Somewhere a client wants to be paid, as they save it. See [`Asked`].
#[derive(Debug, Clone, Copy)]
pub struct Saving<'a> {
    /// None adds a new one.
    pub id: Option<u64>,
    pub user: UserId,
    pub rail: Rail,
    pub label: &'a str,
    pub address: &'a str,
    pub note: &'a str,
    pub preferred: bool,
}

/// Where a client wants their money sent.
///
/// Theirs, not the platform's: a payment method is the house's wallet and
/// every client sees it, while this is one client's own bank or wallet.
/// Nobody else may see it and nothing else may be paid to it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Payout {
    pub id: u64,
    /// Whose it is.
    pub user: u64,
    pub rail: Rail,
    /// What they call it: "My Binance USDT", "HDFC current".
    pub label: String,
    /// The address, UPI id or account details money would be sent to.
    pub address: String,
    #[serde(default)]
    pub note: String,
    /// The one used unless another is chosen.
    #[serde(default)]
    pub preferred: bool,
}

/// Where a claim has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// The client says they have paid. Nobody has checked.
    Pending,
    /// The desk saw the money and credited the account.
    Credited,
    /// The desk did not see the money, or it was not what was claimed.
    Refused,
}

impl Stage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Stage::Pending => "pending",
            Stage::Credited => "credited",
            Stage::Refused => "refused",
        }
    }
}

/// A client's claim that they have paid.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    /// What is being asked for. Absent in a file written when a deposit
    /// was the only thing anybody could ask for — which it was.
    #[serde(default = "deposit_kind")]
    pub kind: Kind,
    pub account: u64,
    pub user: u64,
    pub method: u64,
    /// What the client says they sent, as a decimal string. A claim, not a
    /// figure the platform has agreed to — which is why it never reaches
    /// `Money` until an operator types it into the deposit.
    pub amount: String,
    /// Their transaction hash, UPI reference or transfer note.
    #[serde(default)]
    pub reference: String,
    /// Epoch milliseconds.
    pub at: i64,
    pub stage: Stage,
    /// Who settled it, once somebody has.
    #[serde(default)]
    pub settled_by: Option<u64>,
    /// Why it was refused, for the client to read.
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Stored {
    #[serde(default)]
    methods: Vec<Method>,
    #[serde(default)]
    requests: Vec<Request>,
    #[serde(default)]
    payouts: Vec<Payout>,
    #[serde(default)]
    last_payout: u64,
    /// The highest method id ever handed out.
    ///
    /// Kept rather than recomputed from the list, because the list shrinks:
    /// taking `max + 1` over what is left hands a removed method's id to a
    /// new one, and a request points at the method it was paid to. Absent
    /// in a file written before this was tracked, where the highest id
    /// present is the best that can be known.
    #[serde(default)]
    last_method: u64,
}

/// The payment file, loaded into memory.
#[derive(Debug)]
pub struct Payments {
    path: PathBuf,
    methods: Vec<Method>,
    requests: Vec<Request>,
    payouts: Vec<Payout>,
    last_method: u64,
    last_payout: u64,
}

/// How many settled requests are kept. The desk needs recent history; a
/// claim from two years ago is in the journal as a deposit, which is the
/// record that matters.
const KEEP_SETTLED: usize = 500;

impl Payments {
    /// Loads the file, or starts empty.
    ///
    /// Empty is the right default: a platform that invents a wallet address
    /// is a platform that tells clients to send money somewhere nobody
    /// watches.
    pub fn load(path: &Path) -> Result<Payments> {
        let held: Stored = match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("reading {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Stored::default(),
            Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
        };
        let highest = held.methods.iter().map(|m| m.id).max().unwrap_or(0);
        let tallest = held.payouts.iter().map(|p| p.id).max().unwrap_or(0);
        Ok(Payments {
            path: path.to_path_buf(),
            last_method: held.last_method.max(highest),
            last_payout: held.last_payout.max(tallest),
            methods: held.methods,
            requests: held.requests,
            payouts: held.payouts,
        })
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let text = serde_json::to_string_pretty(&Stored {
            methods: self.methods.clone(),
            requests: self.requests.clone(),
            payouts: self.payouts.clone(),
            last_method: self.last_method,
            last_payout: self.last_payout,
        })?;
        // Written beside and renamed, so a crash mid-write cannot leave a
        // half a file where the payment details used to be.
        let temp = self.path.with_extension("tmp");
        fs::write(&temp, text).with_context(|| format!("writing {}", temp.display()))?;
        fs::rename(&temp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }

    pub fn methods(&self) -> &[Method] {
        &self.methods
    }

    /// What a client is shown: the ones that are switched on.
    pub fn offered(&self) -> impl Iterator<Item = &Method> {
        self.methods.iter().filter(|m| m.enabled)
    }

    pub fn method(&self, id: u64) -> Option<&Method> {
        self.methods.iter().find(|m| m.id == id)
    }

    /// Adds a way to pay, or replaces one by id.
    pub fn put(
        &mut self,
        id: Option<u64>,
        rail: Rail,
        label: &str,
        address: &str,
        note: &str,
        enabled: bool,
    ) -> Result<u64> {
        let label = label.trim();
        let address = address.trim();
        if label.is_empty() {
            bail!("a payment method needs a name the client will recognise");
        }
        if address.is_empty() {
            bail!("a payment method needs an address to pay to");
        }

        match id {
            Some(id) => {
                let Some(found) = self.methods.iter_mut().find(|m| m.id == id) else {
                    bail!("no payment method {id}");
                };
                found.rail = rail;
                found.label = label.to_owned();
                found.address = address.to_owned();
                found.note = note.trim().to_owned();
                found.enabled = enabled;
                self.save()?;
                Ok(id)
            }
            None => {
                // Ids are never reused: a request points at the method it
                // was paid to, and that must keep meaning the same thing.
                // Counted forward rather than taken from the list, which
                // shrinks when a method is removed.
                self.last_method += 1;
                let next = self.last_method;
                self.methods.push(Method {
                    id: next,
                    rail,
                    label: label.to_owned(),
                    address: address.to_owned(),
                    note: note.trim().to_owned(),
                    enabled,
                });
                self.save()?;
                Ok(next)
            }
        }
    }

    /// Removes a method nothing has been paid to.
    ///
    /// One with requests against it is switched off instead, so the record
    /// of what somebody paid to still resolves to an address.
    pub fn remove(&mut self, id: u64) -> Result<()> {
        if self.requests.iter().any(|r| r.method == id) {
            bail!("clients have paid to this one; switch it off rather than removing it");
        }
        let before = self.methods.len();
        self.methods.retain(|m| m.id != id);
        if self.methods.len() == before {
            bail!("no payment method {id}");
        }
        self.save()
    }

    /// Puts a request in the queue.
    ///
    /// `method` names the house wallet money was sent to, for a deposit, or
    /// the client's own payout method, for a withdrawal. The kinds that are
    /// the house's own doing — bonus, insurance — need neither and pass
    /// zero.
    pub fn raise(&mut self, asked: Asked<'_>) -> Result<u64> {
        let Asked {
            kind,
            account,
            user,
            method,
            amount,
            reference,
            at,
        } = asked;
        let amount = amount.trim();
        if amount.is_empty() {
            bail!("say how much");
        }

        match kind {
            Kind::Deposit => {
                let Some(found) = self.method(method) else {
                    bail!("no payment method {method}");
                };
                if !found.enabled {
                    bail!("that payment method is not currently offered");
                }
            }
            Kind::Withdrawal => {
                // Paying out to an address nobody saved is how money goes
                // to a typo. It has to be one of theirs.
                let Some(payout) = self.payouts.iter().find(|p| p.id == method) else {
                    bail!("choose where the money should be sent");
                };
                if payout.user != user.0 {
                    bail!("that is not one of your payout methods");
                }
            }
            _ => {}
        }

        // One open request per account per kind. A client who presses the
        // button twice has not paid twice, and two identical claims in the
        // queue is how one gets paid twice.
        if self
            .requests
            .iter()
            .any(|r| r.stage == Stage::Pending && r.account == account.0 && r.kind == kind)
        {
            bail!(
                "there is already a {} waiting on this account",
                kind.as_str()
            );
        }

        let next = self.requests.iter().map(|r| r.id).max().unwrap_or(0) + 1;
        self.requests.push(Request {
            id: next,
            kind,
            account: account.0,
            user: user.0,
            method,
            amount: amount.to_owned(),
            reference: reference.trim().to_owned(),
            at,
            stage: Stage::Pending,
            settled_by: None,
            detail: String::new(),
        });
        self.save()?;
        Ok(next)
    }

    /// Where one client's money may be sent.
    pub fn payouts_of(&self, user: UserId) -> Vec<&Payout> {
        self.payouts.iter().filter(|p| p.user == user.0).collect()
    }

    pub fn payout(&self, id: u64) -> Option<&Payout> {
        self.payouts.iter().find(|p| p.id == id)
    }

    /// Saves somewhere a client wants money sent, or replaces one by id.
    pub fn put_payout(&mut self, saving: Saving<'_>) -> Result<u64> {
        let Saving {
            id,
            user,
            rail,
            label,
            address,
            note,
            preferred,
        } = saving;
        let label = label.trim();
        let address = address.trim();
        if label.is_empty() {
            bail!("give it a name you will recognise");
        }
        if address.is_empty() {
            bail!("say where the money should be sent");
        }

        // Only one preferred at a time, or "the usual one" means nothing.
        if preferred {
            for other in self.payouts.iter_mut().filter(|p| p.user == user.0) {
                other.preferred = false;
            }
        }

        match id {
            Some(id) => {
                let Some(found) = self
                    .payouts
                    .iter_mut()
                    .find(|p| p.id == id && p.user == user.0)
                else {
                    bail!("no payout method {id}");
                };
                found.rail = rail;
                found.label = label.to_owned();
                found.address = address.to_owned();
                found.note = note.trim().to_owned();
                found.preferred = preferred;
                self.save()?;
                Ok(id)
            }
            None => {
                // Counted forward and never reused: a settled withdrawal
                // points at where the money went.
                self.last_payout += 1;
                let next = self.last_payout;
                self.payouts.push(Payout {
                    id: next,
                    user: user.0,
                    rail,
                    label: label.to_owned(),
                    address: address.to_owned(),
                    note: note.trim().to_owned(),
                    preferred,
                });
                self.save()?;
                Ok(next)
            }
        }
    }

    /// Forgets a payout method nothing has been sent to.
    pub fn forget_payout(&mut self, id: u64, user: UserId) -> Result<()> {
        if self
            .requests
            .iter()
            .any(|r| r.kind == Kind::Withdrawal && r.method == id && r.stage != Stage::Refused)
        {
            bail!("money has been sent here; it stays on the record");
        }
        let before = self.payouts.len();
        self.payouts.retain(|p| !(p.id == id && p.user == user.0));
        if self.payouts.len() == before {
            bail!("no payout method {id}");
        }
        self.save()
    }

    /// Marks a claim settled. Crediting the account is the caller's job and
    /// happens through the journal, not here.
    pub fn settle(&mut self, id: u64, by: UserId, credited: bool, detail: &str) -> Result<Request> {
        let Some(found) = self.requests.iter_mut().find(|r| r.id == id) else {
            bail!("no deposit request {id}");
        };
        if found.stage != Stage::Pending {
            bail!("that request was already {}", found.stage.as_str());
        }
        found.stage = if credited {
            Stage::Credited
        } else {
            Stage::Refused
        };
        found.settled_by = Some(by.0);
        found.detail = detail.trim().to_owned();
        let settled = found.clone();

        // Keep the queue short without losing anything recent.
        let open = self
            .requests
            .iter()
            .filter(|r| r.stage == Stage::Pending)
            .count();
        if self.requests.len() > open + KEEP_SETTLED {
            let mut done: Vec<u64> = self
                .requests
                .iter()
                .filter(|r| r.stage != Stage::Pending)
                .map(|r| r.id)
                .collect();
            done.sort_unstable();
            let drop: std::collections::HashSet<u64> = done
                .into_iter()
                .take(self.requests.len() - open - KEEP_SETTLED)
                .collect();
            self.requests.retain(|r| !drop.contains(&r.id));
        }

        self.save()?;
        Ok(settled)
    }

    pub fn requests(&self) -> &[Request] {
        &self.requests
    }

    /// What one client may see: their own claims, newest first.
    pub fn mine(&self, user: UserId) -> Vec<&Request> {
        let mut mine: Vec<&Request> = self.requests.iter().filter(|r| r.user == user.0).collect();
        mine.sort_by_key(|r| std::cmp::Reverse(r.id));
        mine
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("feraro-pay-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&p).expect("scratch");
            Scratch(p)
        }
        fn file(&self) -> PathBuf {
            self.0.join("payments.json")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_fresh_platform_offers_nothing() {
        let s = Scratch::new("fresh");
        let p = Payments::load(&s.file()).unwrap();
        assert!(p.methods().is_empty());
        assert_eq!(
            p.offered().count(),
            0,
            "inventing an address is worse than none"
        );
    }

    #[test]
    fn a_method_survives_a_reload() {
        let s = Scratch::new("reload");
        let id = {
            let mut p = Payments::load(&s.file()).unwrap();
            p.put(
                None,
                Rail::Crypto,
                "USDT (TRC-20)",
                "TXyz...",
                "send only TRC-20",
                true,
            )
            .unwrap()
        };
        let p = Payments::load(&s.file()).unwrap();
        let held = p.method(id).unwrap();
        assert_eq!(held.rail, Rail::Crypto);
        assert_eq!(held.label, "USDT (TRC-20)");
        assert_eq!(held.note, "send only TRC-20");
        assert!(held.enabled);
    }

    #[test]
    fn a_method_without_an_address_is_refused() {
        let s = Scratch::new("blank");
        let mut p = Payments::load(&s.file()).unwrap();
        assert!(p.put(None, Rail::Upi, "UPI", "   ", "", true).is_err());
        assert!(p
            .put(None, Rail::Upi, "  ", "feraro@upi", "", true)
            .is_err());
        assert!(p.methods().is_empty());
    }

    #[test]
    fn only_enabled_methods_are_offered() {
        let s = Scratch::new("offered");
        let mut p = Payments::load(&s.file()).unwrap();
        p.put(None, Rail::Upi, "UPI", "feraro@upi", "", true)
            .unwrap();
        let off = p
            .put(None, Rail::Bank, "Old bank", "123456", "", false)
            .unwrap();
        let offered: Vec<u64> = p.offered().map(|m| m.id).collect();
        assert_eq!(offered.len(), 1);
        assert!(!offered.contains(&off));
    }

    #[test]
    fn a_client_cannot_pay_to_a_method_that_is_switched_off() {
        let s = Scratch::new("off");
        let mut p = Payments::load(&s.file()).unwrap();
        let id = p
            .put(None, Rail::Bank, "Bank", "123456", "", false)
            .unwrap();
        assert!(p
            .raise(Asked {
                kind: Kind::Deposit,
                account: AccountId(1001),
                user: UserId(2),
                method: id,
                amount: "500.00",
                reference: "ref",
                at: 0,
            })
            .is_err());
    }

    #[test]
    fn one_open_claim_per_method() {
        let s = Scratch::new("twice");
        let mut p = Payments::load(&s.file()).unwrap();
        let id = p
            .put(None, Rail::Upi, "UPI", "feraro@upi", "", true)
            .unwrap();
        p.raise(Asked {
            kind: Kind::Deposit,
            account: AccountId(1001),
            user: UserId(2),
            method: id,
            amount: "500.00",
            reference: "one",
            at: 0,
        })
        .unwrap();
        // Pressing the button twice is not paying twice, and two identical
        // claims is how one account gets credited twice.
        assert!(p
            .raise(Asked {
                kind: Kind::Deposit,
                account: AccountId(1001),
                user: UserId(2),
                method: id,
                amount: "500.00",
                reference: "one",
                at: 0,
            })
            .is_err());
        // Settling the first frees it.
        p.settle(1, UserId(1), true, "seen").unwrap();
        assert!(p
            .raise(Asked {
                kind: Kind::Deposit,
                account: AccountId(1001),
                user: UserId(2),
                method: id,
                amount: "500.00",
                reference: "two",
                at: 0,
            })
            .is_ok());
    }

    #[test]
    fn a_settled_request_cannot_be_settled_again() {
        let s = Scratch::new("settle");
        let mut p = Payments::load(&s.file()).unwrap();
        let id = p
            .put(None, Rail::Upi, "UPI", "feraro@upi", "", true)
            .unwrap();
        let req = p
            .raise(Asked {
                kind: Kind::Deposit,
                account: AccountId(1001),
                user: UserId(2),
                method: id,
                amount: "500.00",
                reference: "",
                at: 0,
            })
            .unwrap();
        let settled = p.settle(req, UserId(1), true, "seen in the bank").unwrap();
        assert_eq!(settled.stage, Stage::Credited);
        assert_eq!(settled.settled_by, Some(1));
        // Crediting twice is paying twice.
        assert!(p.settle(req, UserId(1), true, "again").is_err());
    }

    #[test]
    fn a_method_somebody_paid_to_is_kept() {
        let s = Scratch::new("keep");
        let mut p = Payments::load(&s.file()).unwrap();
        let id = p.put(None, Rail::Crypto, "USDT", "TXyz", "", true).unwrap();
        let spare = p.put(None, Rail::Bank, "Spare", "99", "", true).unwrap();
        p.raise(Asked {
            kind: Kind::Deposit,
            account: AccountId(1001),
            user: UserId(2),
            method: id,
            amount: "100",
            reference: "",
            at: 0,
        })
        .unwrap();

        // The record of what somebody paid to must still resolve.
        assert!(p.remove(id).is_err());
        // One nobody used goes.
        assert!(p.remove(spare).is_ok());
    }

    #[test]
    fn a_client_sees_only_their_own_claims() {
        let s = Scratch::new("mine");
        let mut p = Payments::load(&s.file()).unwrap();
        let id = p
            .put(None, Rail::Upi, "UPI", "feraro@upi", "", true)
            .unwrap();
        p.raise(Asked {
            kind: Kind::Deposit,
            account: AccountId(1001),
            user: UserId(2),
            method: id,
            amount: "100",
            reference: "",
            at: 0,
        })
        .unwrap();
        p.raise(Asked {
            kind: Kind::Deposit,
            account: AccountId(1009),
            user: UserId(9),
            method: id,
            amount: "200",
            reference: "",
            at: 0,
        })
        .unwrap();

        let mine = p.mine(UserId(2));
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].account, 1001);
    }

    fn payout(p: &mut Payments, user: u64, label: &str) -> u64 {
        p.put_payout(Saving {
            id: None,
            user: UserId(user),
            rail: Rail::Crypto,
            label,
            address: "TWallet",
            note: "",
            preferred: false,
        })
        .unwrap()
    }

    fn want(p: &mut Payments, user: u64, method: u64, amount: &str) -> Result<u64> {
        p.raise(Asked {
            kind: Kind::Withdrawal,
            account: AccountId(1001),
            user: UserId(user),
            method,
            amount,
            reference: "",
            at: 0,
        })
    }

    #[test]
    fn money_is_only_sent_somewhere_the_client_saved() {
        let s = Scratch::new("payout-theirs");
        let mut p = Payments::load(&s.file()).unwrap();
        let mine = payout(&mut p, 2, "My wallet");

        assert!(want(&mut p, 2, mine, "100").is_ok());
        // Paying to an address nobody saved is how money goes to a typo.
        assert!(want(&mut p, 2, 999, "100").is_err());
    }

    #[test]
    fn one_client_cannot_be_paid_to_anothers_wallet() {
        let s = Scratch::new("payout-mine");
        let mut p = Payments::load(&s.file()).unwrap();
        let theirs = payout(&mut p, 9, "Somebody else");
        assert!(want(&mut p, 2, theirs, "100").is_err());
    }

    #[test]
    fn only_one_withdrawal_waits_at_a_time() {
        let s = Scratch::new("payout-once");
        let mut p = Payments::load(&s.file()).unwrap();
        let mine = payout(&mut p, 2, "My wallet");
        let first = want(&mut p, 2, mine, "100").unwrap();
        // Pressing it twice is not asking twice, and two identical
        // requests in the queue is how one gets paid twice.
        assert!(want(&mut p, 2, mine, "100").is_err());

        p.settle(first, UserId(1), true, "sent").unwrap();
        assert!(want(&mut p, 2, mine, "100").is_ok());
    }

    #[test]
    fn a_deposit_and_a_withdrawal_can_wait_together() {
        // Different kinds are different queues: somebody paying in while
        // waiting to be paid out is ordinary.
        let s = Scratch::new("payout-both");
        let mut p = Payments::load(&s.file()).unwrap();
        let method = p
            .put(None, Rail::Upi, "UPI", "feraro@upi", "", true)
            .unwrap();
        let mine = payout(&mut p, 2, "My wallet");

        p.raise(Asked {
            kind: Kind::Deposit,
            account: AccountId(1001),
            user: UserId(2),
            method,
            amount: "500",
            reference: "",
            at: 0,
        })
        .unwrap();
        assert!(want(&mut p, 2, mine, "100").is_ok());
    }

    #[test]
    fn only_one_payout_is_the_preferred_one() {
        let s = Scratch::new("payout-preferred");
        let mut p = Payments::load(&s.file()).unwrap();
        for label in ["First", "Second"] {
            p.put_payout(Saving {
                id: None,
                user: UserId(2),
                rail: Rail::Bank,
                label,
                address: "123456",
                note: "",
                preferred: true,
            })
            .unwrap();
        }
        let preferred = p
            .payouts_of(UserId(2))
            .iter()
            .filter(|x| x.preferred)
            .count();
        assert_eq!(
            preferred, 1,
            "\"the usual one\" means nothing if there are two"
        );
    }

    #[test]
    fn a_payout_money_went_to_is_kept() {
        let s = Scratch::new("payout-keep");
        let mut p = Payments::load(&s.file()).unwrap();
        let used = payout(&mut p, 2, "Used");
        let spare = payout(&mut p, 2, "Spare");
        let req = want(&mut p, 2, used, "100").unwrap();
        p.settle(req, UserId(1), true, "sent").unwrap();

        // The record of where money went must still resolve.
        assert!(p.forget_payout(used, UserId(2)).is_err());
        assert!(p.forget_payout(spare, UserId(2)).is_ok());
    }

    #[test]
    fn a_client_sees_only_their_own_payouts() {
        let s = Scratch::new("payout-privacy");
        let mut p = Payments::load(&s.file()).unwrap();
        payout(&mut p, 2, "Mine");
        payout(&mut p, 9, "Theirs");
        assert_eq!(p.payouts_of(UserId(2)).len(), 1);
        assert_eq!(p.payouts_of(UserId(2))[0].label, "Mine");
    }

    #[test]
    fn ids_are_never_reused() {
        let s = Scratch::new("ids");
        let mut p = Payments::load(&s.file()).unwrap();
        let first = p.put(None, Rail::Upi, "One", "a", "", true).unwrap();
        p.remove(first).unwrap();
        let second = p.put(None, Rail::Upi, "Two", "b", "", true).unwrap();
        assert!(
            second > first,
            "a request points at the method it was paid to"
        );
    }
}
