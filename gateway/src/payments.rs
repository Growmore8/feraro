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
}

/// The payment file, loaded into memory.
#[derive(Debug)]
pub struct Payments {
    path: PathBuf,
    methods: Vec<Method>,
    requests: Vec<Request>,
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
        Ok(Payments {
            path: path.to_path_buf(),
            methods: held.methods,
            requests: held.requests,
        })
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let text = serde_json::to_string_pretty(&Stored {
            methods: self.methods.clone(),
            requests: self.requests.clone(),
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
                let next = self.methods.iter().map(|m| m.id).max().unwrap_or(0) + 1;
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

    /// Records a client's claim that they have paid.
    pub fn raise(
        &mut self,
        account: AccountId,
        user: UserId,
        method: u64,
        amount: &str,
        reference: &str,
        at: i64,
    ) -> Result<u64> {
        let Some(found) = self.method(method) else {
            bail!("no payment method {method}");
        };
        if !found.enabled {
            bail!("that payment method is not currently offered");
        }
        let amount = amount.trim();
        if amount.is_empty() {
            bail!("say how much you sent");
        }

        // One open claim per account per method. A client who presses the
        // button twice has not paid twice, and two identical claims in the
        // desk's queue is how one gets credited twice.
        if self
            .requests
            .iter()
            .any(|r| r.stage == Stage::Pending && r.account == account.0 && r.method == method)
        {
            bail!("you already have a deposit waiting on this method");
        }

        let next = self.requests.iter().map(|r| r.id).max().unwrap_or(0) + 1;
        self.requests.push(Request {
            id: next,
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
            .raise(AccountId(1001), UserId(2), id, "500.00", "ref", 0)
            .is_err());
    }

    #[test]
    fn one_open_claim_per_method() {
        let s = Scratch::new("twice");
        let mut p = Payments::load(&s.file()).unwrap();
        let id = p
            .put(None, Rail::Upi, "UPI", "feraro@upi", "", true)
            .unwrap();
        p.raise(AccountId(1001), UserId(2), id, "500.00", "one", 0)
            .unwrap();
        // Pressing the button twice is not paying twice, and two identical
        // claims is how one account gets credited twice.
        assert!(p
            .raise(AccountId(1001), UserId(2), id, "500.00", "one", 0)
            .is_err());
        // Settling the first frees it.
        p.settle(1, UserId(1), true, "seen").unwrap();
        assert!(p
            .raise(AccountId(1001), UserId(2), id, "500.00", "two", 0)
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
            .raise(AccountId(1001), UserId(2), id, "500.00", "", 0)
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
        p.raise(AccountId(1001), UserId(2), id, "100", "", 0)
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
        p.raise(AccountId(1001), UserId(2), id, "100", "", 0)
            .unwrap();
        p.raise(AccountId(1009), UserId(9), id, "200", "", 0)
            .unwrap();

        let mine = p.mine(UserId(2));
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].account, 1001);
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
