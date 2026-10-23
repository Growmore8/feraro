//! Who is signing in, and what they are allowed to touch.
//!
//! # Credentials are not journaled
//!
//! The journal is append-only and permanent, which is exactly what a trade
//! history needs and exactly what a password hash must not have: hashes are
//! rotated when the algorithm ages, reset when someone forgets, and erased
//! when a person leaves. So they live here, in a file that can be rewritten,
//! while the journal records only the link between a user id and an account
//! (`Event::AccountOwnerSet`). Replaying the journal still reproduces who owns
//! what; it just cannot sign anybody in.
//!
//! # What is stored
//!
//! An Argon2id PHC string per user — salt, parameters and hash in one field,
//! so a future parameter change verifies old hashes and writes new ones
//! without a migration. Never the password, and nothing derived from it that
//! could be compared without the same work factor.
//!
//! # What this is not yet
//!
//! Single-factor, no password rotation policy, no audit of sign-ins, and
//! sessions live in memory so a gateway restart signs everybody out. The last
//! one is deliberate for now: a token that survives a restart has to be
//! revocable across restarts too, and that is a store of its own.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use domain::UserId;
use serde::{Deserialize, Serialize};

use crate::totp::Totp;

/// How long a session token stays valid without being used.
pub const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// Consecutive failures before a login is made to wait.
const MAX_FAILURES: u32 = 5;
/// How long it waits.
const LOCKOUT: Duration = Duration::from_secs(60);

/// One power an administrator can give a manager, or take away.
///
/// # Why a list and not a role
///
/// A role answers "what kind of person is this", and that was enough while
/// a manager could do nothing with money at all. It stops being enough the
/// moment one desk wants a manager who can approve deposits but not
/// withdrawals, and another wants the reverse. Those are not two roles,
/// they are one role with a switch, and inventing a role per combination
/// ends with eleven roles nobody can tell apart.
///
/// # Why default-on rather than default-off
///
/// A manager exists to work a book. Starting them with nothing means every
/// new manager is useless until somebody remembers to switch eleven things
/// on, and the thing people do when faced with that is switch all eleven
/// on without reading them. Starting from a working manager and taking
/// powers away is the decision an operator actually wants to make.
///
/// The exceptions are the three that are never a manager's: see
/// [`Power::manager_may`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Power {
    /// Make a new client login.
    CreateClients,
    /// Approve money coming in.
    ProcessDeposits,
    /// Approve money going out.
    ProcessWithdrawals,
    /// Grant or withdraw a bonus.
    CreditBonus,
    /// Move money between a client's own accounts.
    TransferFunds,
    /// Place an order on a client's behalf.
    ManualTrade,
    /// Close a client's position.
    CloseTrades,
    /// Put a trade right after the fact.
    EditTrades,
    /// Read the audit trail.
    ViewAudit,
    /// Set what the house charges.
    EditSpread,
    /// Suspend an account, or let it trade again.
    SuspendAccounts,
}

/// Every power, in the order an operator reads them.
pub const POWERS: [Power; 11] = [
    Power::CreateClients,
    Power::ProcessDeposits,
    Power::ProcessWithdrawals,
    Power::CreditBonus,
    Power::TransferFunds,
    Power::ManualTrade,
    Power::CloseTrades,
    Power::EditTrades,
    Power::SuspendAccounts,
    Power::ViewAudit,
    Power::EditSpread,
];

impl Power {
    /// The name this power crosses the wire and the user store under.
    ///
    /// Stable: it is a key in a file somebody's permissions are stored
    /// against, so renaming one silently restores a power that was taken
    /// away.
    pub const fn as_str(self) -> &'static str {
        match self {
            Power::CreateClients => "createClients",
            Power::ProcessDeposits => "processDeposits",
            Power::ProcessWithdrawals => "processWithdrawals",
            Power::CreditBonus => "creditBonus",
            Power::TransferFunds => "transferFunds",
            Power::ManualTrade => "manualTrade",
            Power::CloseTrades => "closeTrades",
            Power::EditTrades => "editTrades",
            Power::ViewAudit => "viewAudit",
            Power::EditSpread => "editSpread",
            Power::SuspendAccounts => "suspendAccounts",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        POWERS.into_iter().find(|p| p.as_str() == name)
    }

    /// What to call it on screen.
    pub const fn said(self) -> &'static str {
        match self {
            Power::CreateClients => "Create clients",
            Power::ProcessDeposits => "Approve deposits",
            Power::ProcessWithdrawals => "Approve withdrawals",
            Power::CreditBonus => "Grant and remove bonus",
            Power::TransferFunds => "Move money between accounts",
            Power::ManualTrade => "Trade for a client",
            Power::CloseTrades => "Close a client's positions",
            Power::EditTrades => "Correct a trade",
            Power::ViewAudit => "Read the audit trail",
            Power::EditSpread => "Set markup and commission",
            Power::SuspendAccounts => "Suspend and reinstate accounts",
        }
    }

    /// Whether a manager may hold this power at all.
    ///
    /// Three are the platform's rather than a book's, and no switch makes
    /// them a manager's:
    ///
    /// * **Reading the audit trail** would mean reading every other
    ///   manager's clients, which is the one thing the scoping exists to
    ///   prevent.
    /// * **Setting markup and commission** is what every client on the
    ///   platform is quoted, not one book's business.
    /// * **Correcting a trade** rewrites what the journal says happened.
    ///
    /// They are in the list so the screen can show them greyed out with a
    /// reason, rather than leaving an operator wondering where they went.
    pub const fn manager_may(self) -> bool {
        !matches!(
            self,
            Power::ViewAudit | Power::EditSpread | Power::EditTrades
        )
    }

    /// Why a manager may never hold it, for the screen to show.
    pub const fn why_not(self) -> Option<&'static str> {
        match self {
            Power::ViewAudit => Some("the audit trail covers every manager's clients"),
            Power::EditSpread => Some("pricing is the platform's, not one book's"),
            Power::EditTrades => Some("correcting a trade rewrites what happened"),
            _ => None,
        }
    }
}

/// What a signed-in user may do.
///
/// Ordered from least to most, so a check reads as "at least this role".
///
/// # Why a manager is not a small administrator
///
/// A manager runs *their own clients*: the same screen, the same figures, the
/// same deal ticket, over a book that holds only the clients an administrator
/// assigned to them. The limit is on **whose accounts**, and it is enforced
/// the same way a client's is — by the list of accounts the session may act
/// on — rather than by hiding buttons. A manager who guesses a number outside
/// their book is refused by [`Signed::may_act_on`], not by the browser.
///
/// What a manager may never do is move money in or out, decide identity, or
/// create a login. Those are the powers that make a book rather than work
/// one, and they stay with the person accountable for the platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Trades their own accounts and sees nothing else.
    Client,
    /// Works the accounts of the clients assigned to them, and no others.
    Manager,
    /// Runs the platform: every account, the books, and the people.
    Admin,
}

impl Role {
    /// The name the browser uses.
    pub const fn as_str(self) -> &'static str {
        match self {
            Role::Client => "client",
            Role::Manager => "manager",
            Role::Admin => "admin",
        }
    }

    /// The role that name stands for, if it is one.
    pub fn parse(name: &str) -> Option<Role> {
        match name {
            "client" => Some(Role::Client),
            "manager" => Some(Role::Manager),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How far a client has got with identity checks.
///
/// # Why this is here and not in the journal
///
/// It is identity, like the password, and identity changes: a document
/// expires, a name changes, a person asks to be forgotten. The journal cannot
/// honour any of that. What *is* journaled is the consequence — an account
/// becoming live, with the administrator who approved it on the event — so
/// the trading record still answers "who allowed this".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Kyc {
    /// Nothing asked for yet. A demo account is all they can have.
    None,
    /// They have asked to be verified and the desk has not decided.
    Submitted,
    /// Verified. A live account may be opened for them.
    Approved,
    /// Refused. They can still trade a demo account.
    Rejected,
}

impl Kyc {
    pub const fn as_str(self) -> &'static str {
        match self {
            Kyc::None => "none",
            Kyc::Submitted => "submitted",
            Kyc::Approved => "approved",
            Kyc::Rejected => "rejected",
        }
    }
}

/// One person who can sign in.
#[derive(Debug, Clone)]
pub struct User {
    pub id: UserId,
    /// Their email address, which is their login and their identity here.
    ///
    /// Compared case-insensitively — nobody remembers whether they
    /// capitalised it — and unique, because two people sharing a login is two
    /// people sharing an account.
    pub login: String,
    pub role: Role,
    /// How far identity checks have got. Clients only; a desk login is not a
    /// client.
    pub kyc: Kyc,
    /// The manager whose book this client sits in, when one has been
    /// assigned.
    ///
    /// A relationship between two people rather than a fact about an account,
    /// which is why it lives here with the logins and not in the journal: who
    /// looks after whom is reassigned, and an append-only log is the wrong
    /// shape for something that changes. What the journal keeps is the
    /// consequence — every action the manager took, with their name on it.
    pub manager: Option<UserId>,
    /// Argon2id PHC string. Never leaves this process.
    hash: String,
    /// Powers an administrator has taken away from this manager.
    ///
    /// Only the refusals are stored, so a power added to the platform
    /// later is held by every existing manager without anybody editing a
    /// file — which is the right default, because a new power nobody has
    /// heard of should not silently switch a working desk off.
    withheld: BTreeSet<String>,
    /// The authenticator secret, once one has been set up.
    ///
    /// Held here and not in the journal for the same reason the password
    /// hash is: a second factor is rotated, replaced and switched off, and
    /// an append-only log cannot honour any of that.
    second: Option<Second>,
}

/// One login's second factor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Second {
    /// Base32, as the authenticator app was given it.
    secret: String,
    /// False while enrolment is half done.
    ///
    /// A secret is written before it is proved to work, because the app has
    /// it as soon as the code is scanned and a crash in between must not
    /// leave the two disagreeing. Until a code from it has been typed back,
    /// it does not gate anything — otherwise a mistyped setup would lock
    /// somebody out of their own platform.
    confirmed: bool,
    /// The last time step a code was accepted from.
    ///
    /// # Why this is stored
    ///
    /// A code lasts thirty seconds, so one read over a shoulder or off a
    /// screen share can be used again inside its window. Refusing anything
    /// at or below the last accepted step is what makes a one-time
    /// password actually one-time.
    #[serde(default)]
    used_step: u64,
}

/// The on-disk shape.
///
/// Separate from [`User`] so that `domain` keeps its zero third-party
/// dependencies: ids cross as plain integers here rather than `domain`
/// deriving serde to suit the gateway's file format.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredUser {
    id: u64,
    login: String,
    role: Role,
    /// Absent in a store written before identity checks existed, which is
    /// `none` — the safe reading, because it allows only a demo account.
    #[serde(default = "no_kyc")]
    kyc: Kyc,
    /// Absent in a store written before managers existed: unassigned, which
    /// is the reading that gives nobody a book they were not given.
    #[serde(default)]
    manager: Option<u64>,
    hash: String,
    /// Absent in a store written before two-factor existed, which is a
    /// login with no second factor — the only safe reading, since the
    /// alternative would lock everyone out at once.
    #[serde(default)]
    second: Option<Second>,
    /// Absent in a store written before permissions existed: nothing
    /// withheld, which is what every manager had then.
    #[serde(default)]
    withheld: BTreeSet<String>,
}

fn no_kyc() -> Kyc {
    Kyc::None
}

impl From<&User> for StoredUser {
    fn from(u: &User) -> Self {
        StoredUser {
            id: u.id.0,
            login: u.login.clone(),
            role: u.role,
            kyc: u.kyc,
            manager: u.manager.map(|m| m.0),
            hash: u.hash.clone(),
            second: u.second.clone(),
            withheld: u.withheld.clone(),
        }
    }
}

impl From<StoredUser> for User {
    fn from(s: StoredUser) -> Self {
        User {
            id: UserId(s.id),
            login: s.login,
            role: s.role,
            kyc: s.kyc,
            manager: s.manager.map(UserId),
            hash: s.hash,
            second: s.second,
            withheld: s.withheld,
        }
    }
}

impl User {
    /// Whether this login holds one power.
    ///
    /// An administrator holds everything: the role is the point of it, and
    /// the audit trail is what makes that safe. A client holds nothing —
    /// none of these are a client's to hold, and the question is only ever
    /// asked about a desk login. A manager holds everything a manager may,
    /// less whatever has been taken away.
    pub fn may(&self, power: Power) -> bool {
        match self.role {
            Role::Admin => true,
            Role::Client => false,
            Role::Manager => power.manager_may() && !self.withheld.contains(power.as_str()),
        }
    }

    /// Every power, and whether this login holds it.
    pub fn powers(&self) -> Vec<(Power, bool)> {
        POWERS.into_iter().map(|p| (p, self.may(p))).collect()
    }

    /// True when a working second factor is set up on this login.
    pub fn has_second_factor(&self) -> bool {
        self.second.as_ref().is_some_and(|s| s.confirmed)
    }

    /// True when this login can move money, so it ought to have one.
    ///
    /// Said as a question about the role rather than about the person: a
    /// client cannot move money without the desk agreeing, and making them
    /// carry a phone to trade would cost the platform clients for no gain.
    pub fn should_have_second_factor(&self) -> bool {
        self.role >= Role::Manager
    }

    /// True when `password` is the one this user set.
    ///
    /// Always does the full Argon2 work, even against a hash that cannot
    /// parse, so a corrupted record is not detectable by how fast it fails.
    fn verify(&self, password: &str) -> bool {
        let Ok(parsed) = PasswordHash::new(&self.hash) else {
            return false;
        };
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Stored {
    users: Vec<StoredUser>,
}

/// The user file, loaded into memory.
#[derive(Debug)]
pub struct Users {
    path: PathBuf,
    users: Vec<User>,
    /// Consecutive failures per login, and when the count may be forgiven.
    failures: HashMap<String, (u32, Instant)>,
}

impl Users {
    /// Loads the store, creating it with a starter account if it is not there.
    ///
    /// Returns any passwords it had to invent, so the caller can print them
    /// once. They are not stored anywhere in plain form, here or on disk: if
    /// that line is lost, the password is reset, not recovered.
    pub fn load_or_seed(path: &Path) -> Result<(Self, Vec<(String, String)>)> {
        if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading the user store at {}", path.display()))?;
            let stored: Stored = serde_json::from_str(&text)
                .with_context(|| format!("parsing the user store at {}", path.display()))?;
            return Ok((
                Users {
                    path: path.to_path_buf(),
                    users: stored.users.into_iter().map(User::from).collect(),
                    failures: HashMap::new(),
                },
                Vec::new(),
            ));
        }

        let mut users = Users {
            path: path.to_path_buf(),
            users: Vec::new(),
            failures: HashMap::new(),
        };
        // Enough to sign in with on a fresh install, and no more: one desk
        // administrator and one trading client.
        let mut seeded = Vec::new();
        for (login, role) in [
            ("admin@feraro.local", Role::Admin),
            ("demo@feraro.local", Role::Client),
        ] {
            let password = invented_password();
            users.add(login, &password, role)?;
            seeded.push((login.to_owned(), password));
        }
        Ok((users, seeded))
    }

    /// Adds a user and writes the store.
    pub fn add(&mut self, login: &str, password: &str, role: Role) -> Result<UserId> {
        let login = login.trim();
        if !is_email(login) {
            bail!("a login is an email address");
        }
        if self.by_login(login).is_some() {
            // Said plainly: a client who already has an account needs to be
            // told that, not told that something went wrong.
            bail!("{login} already has an account");
        }
        if password.chars().count() < 8 {
            bail!("a password must be at least eight characters");
        }

        let hash = hash_password(password)?;

        // Ids are never reused, so a journal that recorded an account as
        // owned by user 3 cannot be re-pointed at a new person by deleting
        // and re-adding.
        let id = UserId(self.users.iter().map(|u| u.id.0).max().unwrap_or(0) + 1);
        self.users.push(User {
            id,
            login: login.to_owned(),
            role,
            kyc: Kyc::None,
            manager: None,
            hash,
            second: None,
            withheld: BTreeSet::new(),
        });
        self.save()?;
        Ok(id)
    }

    /// Replaces a user's password.
    ///
    /// Reachable only from tests until the admin terminal can call it; the
    /// rotation it exists for is the reason hashes are in this file and not
    /// in the journal, so it is written and tested now rather than later.
    #[allow(dead_code, reason = "the admin terminal's password reset lands in P4")]
    pub fn set_password(&mut self, id: UserId, password: &str) -> Result<()> {
        if password.chars().count() < 8 {
            bail!("a password must be at least eight characters");
        }
        let hash = hash_password(password)?;
        let Some(user) = self.users.iter_mut().find(|u| u.id == id) else {
            bail!("no user {id}");
        };
        user.hash = hash;
        self.save()
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let text = serde_json::to_string_pretty(&Stored {
            users: self.users.iter().map(StoredUser::from).collect(),
        })?;
        std::fs::write(&self.path, text)
            .with_context(|| format!("writing the user store at {}", self.path.display()))
    }

    /// Moves a client's identity checks along.
    ///
    /// Who may call this is the session's decision: a client may ask to be
    /// verified, and only the desk may approve or refuse.
    pub fn set_kyc(&mut self, id: UserId, kyc: Kyc) -> Result<()> {
        let Some(user) = self.users.iter_mut().find(|u| u.id == id) else {
            bail!("no user {id}");
        };
        user.kyc = kyc;
        self.save()
    }

    /// Puts a client in a manager's book, or takes them out of one.
    ///
    /// Refuses anything that would make the relationship meaningless: a
    /// client cannot be handed to somebody who is not a manager, a desk login
    /// is not somebody's client, and nobody manages themselves.
    pub fn set_manager(&mut self, client: UserId, manager: Option<UserId>) -> Result<()> {
        if let Some(manager) = manager {
            if manager == client {
                bail!("a user cannot be their own manager");
            }
            match self.by_id(manager) {
                None => bail!("no user {manager}"),
                Some(m) if m.role != Role::Manager => {
                    bail!("{} is not a manager", m.login)
                }
                Some(_) => {}
            }
        }
        let Some(user) = self.users.iter_mut().find(|u| u.id == client) else {
            bail!("no user {client}");
        };
        if user.role != Role::Client {
            bail!("{} is not a client", user.login);
        }
        user.manager = manager;
        self.save()
    }

    /// Promotes or demotes a login.
    ///
    /// Demoting a manager leaves their clients unassigned rather than in the
    /// book of somebody who can no longer work it: an assignment that points
    /// at a client login is worse than none, because it reads as supervision
    /// that is not happening.
    pub fn set_role(&mut self, id: UserId, role: Role) -> Result<()> {
        let Some(user) = self.users.iter_mut().find(|u| u.id == id) else {
            bail!("no user {id}");
        };
        let was = user.role;
        user.role = role;
        if role != Role::Client {
            user.manager = None;
        }
        if was == Role::Manager && role != Role::Manager {
            for other in &mut self.users {
                if other.manager == Some(id) {
                    other.manager = None;
                }
            }
        }
        self.save()
    }

    /// The clients in this manager's book.
    ///
    /// Their own user id is included, because a manager holds an account of
    /// their own like anybody else and a session that could not see it would
    /// have nowhere to start.
    pub fn book_of(&self, manager: UserId) -> Vec<UserId> {
        let mut ids = vec![manager];
        ids.extend(
            self.users
                .iter()
                .filter(|u| u.manager == Some(manager))
                .map(|u| u.id),
        );
        ids
    }

    /// Everyone, in the order they were added.
    pub fn all(&self) -> &[User] {
        &self.users
    }

    pub fn by_id(&self, id: UserId) -> Option<&User> {
        self.users.iter().find(|u| u.id == id)
    }

    fn by_login(&self, login: &str) -> Option<&User> {
        self.users
            .iter()
            .find(|u| u.login.eq_ignore_ascii_case(login.trim()))
    }

    /// Starts setting up a second factor, returning the secret once.
    ///
    /// The secret is written immediately but unconfirmed. The app has it as
    /// soon as the code is scanned, so a crash between showing it and
    /// proving it must not leave the two disagreeing — and an unconfirmed
    /// secret gates nothing, so writing it early costs nothing either.
    ///
    /// Starting again replaces whatever was half done. It will not replace
    /// a *confirmed* one: turning off a working second factor is its own
    /// decision and needs a current code, which is what
    /// [`Users::stop_second_factor`] is for.
    pub fn begin_second_factor(&mut self, id: UserId) -> Result<Totp> {
        // getrandom's error is not a std::error::Error, so it cannot be
        // carried; and there is nothing useful to say about it anyway. A
        // system that will not give random bytes is a system to refuse on:
        // a predictable second factor is worse than none, because it is
        // trusted.
        let Ok(fresh) = Totp::fresh() else {
            bail!("the system would not give random bytes, so no secret was made");
        };
        let user = self
            .users
            .iter_mut()
            .find(|u| u.id == id)
            .with_context(|| format!("no user {}", id.0))?;
        if user.has_second_factor() {
            bail!("this login already has an authenticator; turn it off first");
        }
        user.second = Some(Second {
            secret: fresh.to_base32(),
            confirmed: false,
            used_step: 0,
        });
        self.save()?;
        Ok(fresh)
    }

    /// Finishes setting one up, by proving a code from it works.
    pub fn confirm_second_factor(&mut self, id: UserId, code: &str, now_unix: u64) -> Result<()> {
        let user = self
            .users
            .iter_mut()
            .find(|u| u.id == id)
            .with_context(|| format!("no user {}", id.0))?;
        let Some(second) = user.second.as_mut() else {
            bail!("there is nothing to confirm — start again");
        };
        if second.confirmed {
            bail!("this login already has an authenticator");
        }
        let Some(totp) = Totp::parse(&second.secret) else {
            bail!("the stored secret is unreadable — start again");
        };
        let Some(step) = totp.check(code, now_unix) else {
            bail!("that code is not from this authenticator, or its clock is out");
        };
        second.confirmed = true;
        second.used_step = step;
        self.save()
    }

    /// Turns one off, which needs a current code from it.
    ///
    /// Not only the password: a session is already signed in, so asking for
    /// the password again would prove nothing a stolen session could not
    /// also prove. A code proves the phone is present, which is the thing
    /// being removed.
    pub fn stop_second_factor(&mut self, id: UserId, code: &str, now_unix: u64) -> Result<()> {
        let user = self
            .users
            .iter_mut()
            .find(|u| u.id == id)
            .with_context(|| format!("no user {}", id.0))?;
        let Some(second) = user.second.as_ref() else {
            bail!("this login has no authenticator");
        };
        if !second.confirmed {
            // Half-set-up is not a factor anybody is relying on, so it goes
            // without a code — there may be no working app to get one from.
            user.second = None;
            return self.save();
        }
        let Some(totp) = Totp::parse(&second.secret) else {
            bail!("the stored secret is unreadable");
        };
        if totp.check(code, now_unix).is_none() {
            bail!("that code is wrong");
        }
        user.second = None;
        self.save()
    }

    /// Gives a manager a power, or takes it away.
    ///
    /// Only a manager's permissions are stored. An administrator's are not
    /// adjustable — a role that can be whittled down to nothing is a role
    /// that can be used to lock everybody out of the platform, including
    /// whoever is doing the whittling.
    pub fn set_power(&mut self, id: UserId, power: Power, allowed: bool) -> Result<()> {
        let user = self
            .users
            .iter_mut()
            .find(|u| u.id == id)
            .with_context(|| format!("no user {}", id.0))?;
        if user.role != Role::Manager {
            bail!("only a manager's permissions can be set");
        }
        if allowed && !power.manager_may() {
            bail!(
                "{} is not a manager's to hold: {}",
                power.said(),
                power.why_not().unwrap_or("it belongs to the platform")
            );
        }
        if allowed {
            user.withheld.remove(power.as_str());
        } else {
            user.withheld.insert(power.as_str().to_owned());
        }
        self.save()
    }

    /// Checks a password, counting failures.
    ///
    /// After [`MAX_FAILURES`] wrong attempts a login waits [`LOCKOUT`] before
    /// it is tried again, so a password is not reachable by typing quickly.
    /// The lockout is per login, not per connection: moving to a new socket
    /// is free, and rate-limiting the socket would protect nothing.
    pub fn sign_in(
        &mut self,
        login: &str,
        password: &str,
        code: &str,
        now_unix: u64,
        now: Instant,
    ) -> SignIn {
        let key = login.trim().to_ascii_lowercase();
        if let Some((count, until)) = self.failures.get(&key) {
            if *count >= MAX_FAILURES && now < *until {
                return SignIn::TooManyAttempts {
                    retry_in: until.saturating_duration_since(now),
                };
            }
        }

        // An unknown login still pays for a verification, so "no such user"
        // and "wrong password" take the same time and tell an attacker the
        // same thing: nothing.
        let found = self.by_login(login).cloned();
        let ok = match &found {
            Some(user) => user.verify(password),
            None => {
                let _ = hash_password(password);
                false
            }
        };

        match (ok, found) {
            (true, Some(user)) => {
                // The password was right. A login with a second factor is
                // not in yet, and the failure counter keeps counting —
                // otherwise the code could be guessed at any rate.
                if user.has_second_factor() {
                    match self.check_second(user.id, code, now_unix) {
                        SecondFactor::Missing => return SignIn::CodeRequired,
                        SecondFactor::Wrong(why) => {
                            let entry = self.failures.entry(key).or_insert((0, now));
                            entry.0 = entry.0.saturating_add(1);
                            entry.1 = now + LOCKOUT;
                            return SignIn::CodeRefused(why);
                        }
                        SecondFactor::Ok => {}
                    }
                }
                self.failures.remove(&key);
                SignIn::Ok(user)
            }
            _ => {
                let entry = self.failures.entry(key).or_insert((0, now));
                entry.0 = entry.0.saturating_add(1);
                entry.1 = now + LOCKOUT;
                SignIn::Refused
            }
        }
    }
}

/// Why a second factor did not let somebody in.
enum SecondFactor {
    Ok,
    /// Nothing was typed, which is the normal first round trip.
    Missing,
    Wrong(&'static str),
}

impl Users {
    /// Checks a code and burns the step it came from.
    ///
    /// The step is recorded *before* the caller is told yes, so a code is
    /// spent whether or not anything else about the sign-in works out.
    fn check_second(&mut self, id: UserId, code: &str, now_unix: u64) -> SecondFactor {
        if code.trim().is_empty() {
            return SecondFactor::Missing;
        }
        let Some(user) = self.users.iter_mut().find(|u| u.id == id) else {
            return SecondFactor::Wrong("that code is wrong");
        };
        let Some(second) = user.second.as_mut() else {
            return SecondFactor::Wrong("that code is wrong");
        };
        let Some(totp) = Totp::parse(&second.secret) else {
            return SecondFactor::Wrong("the stored secret is unreadable");
        };
        let Some(step) = totp.check(code, now_unix) else {
            return SecondFactor::Wrong("that code is wrong");
        };
        // Already used. A code lasts thirty seconds and one read over a
        // shoulder would otherwise work twice.
        if step <= second.used_step {
            return SecondFactor::Wrong("that code has already been used — wait for the next one");
        }
        second.used_step = step;
        // A write per sign-in, which is the price of this being honest
        // about replay across a restart.
        if self.save().is_err() {
            return SecondFactor::Wrong("could not record that code");
        }
        SecondFactor::Ok
    }
}

/// The outcome of one sign-in attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignIn {
    Ok(User),
    /// Wrong login, wrong password — deliberately not distinguished.
    Refused,
    /// The password was right and this login needs a code as well.
    ///
    /// Distinguished from [`SignIn::Refused`], and deliberately so: it does
    /// tell somebody with the right password that the account exists, which
    /// they already knew. The alternative is a terminal that cannot ask for
    /// a code without asking everybody for one.
    CodeRequired,
    /// A code was typed and was not accepted.
    CodeRefused(&'static str),
    TooManyAttempts {
        retry_in: Duration,
    },
}

impl PartialEq for User {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for User {}

/// A signed-in connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Session {
    pub user: UserId,
    pub role: Role,
    /// When the token stops being accepted.
    expires: Instant,
}

/// Live tokens.
///
/// In memory on purpose: see the module note. A restart signs everybody out,
/// which is the safe direction to fail.
#[derive(Debug, Default)]
pub struct Sessions {
    live: HashMap<String, Session>,
}

impl Sessions {
    pub fn new() -> Self {
        Sessions::default()
    }

    /// Issues a token for a user.
    pub fn open(&mut self, user: UserId, role: Role, now: Instant) -> String {
        self.sweep(now);
        let token = token();
        self.live.insert(
            token.clone(),
            Session {
                user,
                role,
                expires: now + SESSION_TTL,
            },
        );
        token
    }

    /// The session a token names, if it is live. Using it extends it.
    pub fn resume(&mut self, token: &str, now: Instant) -> Option<Session> {
        let session = self.live.get_mut(token)?;
        if now >= session.expires {
            self.live.remove(token);
            return None;
        }
        session.expires = now + SESSION_TTL;
        Some(*session)
    }

    /// Ends one session — a sign-out.
    #[allow(dead_code, reason = "the browser's sign-out lands in P3")]
    pub fn close(&mut self, token: &str) {
        self.live.remove(token);
    }

    /// Ends every session belonging to a user — a password change, or a
    /// suspension. Without this, revoking access would not revoke access.
    pub fn close_user(&mut self, user: UserId) {
        self.live.retain(|_, s| s.user != user);
    }

    /// How many sessions are live — for the admin terminal, and for tests
    /// that need to see a revocation actually remove something.
    #[allow(dead_code, reason = "shown on the admin terminal in P4")]
    pub fn count(&self) -> usize {
        self.live.len()
    }

    fn sweep(&mut self, now: Instant) {
        self.live.retain(|_, s| now < s.expires);
    }
}

/// Argon2id over a fresh random salt, as a PHC string.
fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::encode_b64(&random_bytes::<16>())
        .map_err(|e| anyhow::anyhow!("encoding a salt: {e}"))?;
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hashing the password: {e}"))?
        .to_string())
}

/// Bytes from the operating system's entropy.
///
/// Not a seeded PRNG: two gateways started in the same second must not agree
/// about what the next token or salt is. A failure here is fatal rather than
/// fallen back on — there is no safe weaker source for this.
fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::getrandom(&mut bytes).expect("the operating system's entropy source");
    bytes
}

/// 256 bits from the OS, hex encoded. Long enough that guessing is not a
/// strategy.
fn token() -> String {
    random_bytes::<32>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// True when this looks like an email address.
///
/// Deliberately shallow: one `@`, something either side, a dot in the domain,
/// no spaces. Anything stricter rejects addresses that work, and the only
/// real test of an address is sending to it — which is what a verification
/// email is for, and is not built yet.
pub fn is_email(text: &str) -> bool {
    let text = text.trim();
    if text.len() < 6 || text.len() > 254 || text.chars().any(char::is_whitespace) {
        return false;
    }
    let mut parts = text.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty()
        && domain.len() >= 3
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
}

/// A readable starter password: no characters that are read as each other.
fn invented_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    random_bytes::<16>()
        .iter()
        .map(|b| ALPHABET[usize::from(*b) % ALPHABET.len()] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One code window, in seconds.
    const STEP_SECS: u64 = crate::totp::STEP;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("feraro-auth-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&p).expect("scratch");
            Scratch(p)
        }
        fn file(&self) -> PathBuf {
            self.0.join("users.json")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // ── Two-factor ────────────────────────────────────────────────────

    /// A store with one desk login, and the login's id.
    fn with_desk(s: &Scratch) -> (Users, UserId, String) {
        let (mut users, _) = Users::load_or_seed(&s.file()).unwrap();
        let id = users
            .add("desk@feraro.local", "a-long-enough-password", Role::Admin)
            .unwrap();
        (users, id, "desk@feraro.local".to_owned())
    }

    /// The code an enrolled login's app would be showing.
    fn code_now(users: &Users, id: UserId, at: u64) -> String {
        let secret = users
            .by_id(id)
            .unwrap()
            .second
            .as_ref()
            .expect("enrolled")
            .secret
            .clone();
        Totp::parse(&secret).unwrap().code_at(Totp::step_of(at))
    }

    // ── Permissions ───────────────────────────────────────────────────

    #[test]
    fn a_new_manager_can_work_a_book_without_anybody_switching_things_on() {
        // Default-on, deliberately. Starting a manager with nothing means
        // every new one is useless until eleven switches are found, and
        // what people do when faced with that is turn all eleven on
        // without reading them.
        let s = Scratch::new("perms-default");
        let (mut users, _) = Users::load_or_seed(&s.file()).unwrap();
        let id = users
            .add("desk@feraro.local", "a-long-enough-password", Role::Manager)
            .unwrap();
        let manager = users.by_id(id).unwrap();

        assert!(manager.may(Power::ProcessDeposits));
        assert!(manager.may(Power::ProcessWithdrawals));
        assert!(manager.may(Power::CreditBonus));
        assert!(manager.may(Power::ManualTrade));
    }

    #[test]
    fn a_power_taken_away_stays_away_across_a_restart() {
        let s = Scratch::new("perms-persist");
        let id = {
            let (mut users, _) = Users::load_or_seed(&s.file()).unwrap();
            let id = users
                .add("desk@feraro.local", "a-long-enough-password", Role::Manager)
                .unwrap();
            users
                .set_power(id, Power::ProcessWithdrawals, false)
                .unwrap();
            assert!(!users.by_id(id).unwrap().may(Power::ProcessWithdrawals));
            id
        };
        let (users, _) = Users::load_or_seed(&s.file()).unwrap();
        let manager = users.by_id(id).unwrap();
        assert!(!manager.may(Power::ProcessWithdrawals));
        // And only that one.
        assert!(manager.may(Power::ProcessDeposits));
    }

    #[test]
    fn giving_a_power_back_gives_back_only_that_one() {
        let s = Scratch::new("perms-restore");
        let (mut users, _) = Users::load_or_seed(&s.file()).unwrap();
        let id = users
            .add("desk@feraro.local", "a-long-enough-password", Role::Manager)
            .unwrap();
        users.set_power(id, Power::CreditBonus, false).unwrap();
        users.set_power(id, Power::ManualTrade, false).unwrap();
        users.set_power(id, Power::CreditBonus, true).unwrap();

        let manager = users.by_id(id).unwrap();
        assert!(manager.may(Power::CreditBonus));
        assert!(!manager.may(Power::ManualTrade));
    }

    #[test]
    fn the_platforms_own_powers_cannot_be_given_to_a_manager() {
        let s = Scratch::new("perms-impossible");
        let (mut users, _) = Users::load_or_seed(&s.file()).unwrap();
        let id = users
            .add("desk@feraro.local", "a-long-enough-password", Role::Manager)
            .unwrap();
        for power in [Power::ViewAudit, Power::EditSpread, Power::EditTrades] {
            assert!(
                users.set_power(id, power, true).is_err(),
                "{} was handed out",
                power.as_str()
            );
            assert!(!users.by_id(id).unwrap().may(power));
        }
    }

    #[test]
    fn an_administrators_permissions_are_not_adjustable() {
        // A role that can be whittled down to nothing is a way to lock
        // everybody out of the platform, including whoever is whittling.
        let s = Scratch::new("perms-admin");
        let (mut users, _) = Users::load_or_seed(&s.file()).unwrap();
        let id = users
            .add("boss@feraro.local", "a-long-enough-password", Role::Admin)
            .unwrap();
        assert!(users.set_power(id, Power::ProcessDeposits, false).is_err());
        assert!(users.by_id(id).unwrap().may(Power::ProcessDeposits));
        // Including the ones no manager may hold.
        assert!(users.by_id(id).unwrap().may(Power::ViewAudit));
    }

    #[test]
    fn a_client_holds_none_of_these() {
        let s = Scratch::new("perms-client");
        let (mut users, _) = Users::load_or_seed(&s.file()).unwrap();
        let id = users
            .add("trader@example.com", "a-long-enough-password", Role::Client)
            .unwrap();
        let client = users.by_id(id).unwrap();
        for power in POWERS {
            assert!(!client.may(power), "a client held {}", power.as_str());
        }
        assert!(users.set_power(id, Power::ManualTrade, false).is_err());
    }

    #[test]
    fn a_store_written_before_permissions_gives_a_manager_everything() {
        // The only safe reading. The alternative silently switches off
        // every working desk the moment the platform is upgraded.
        let s = Scratch::new("perms-old-store");
        std::fs::write(
            s.file(),
            r#"{"users":[{"id":1,"login":"desk@feraro.local","role":"manager","hash":"x"}]}"#,
        )
        .unwrap();
        let (users, _) = Users::load_or_seed(&s.file()).unwrap();
        let manager = users.by_id(UserId(1)).unwrap();
        for power in POWERS {
            assert_eq!(
                manager.may(power),
                power.manager_may(),
                "{} read wrong from an old store",
                power.as_str()
            );
        }
    }

    #[test]
    fn promoting_a_client_to_manager_starts_them_with_a_working_desk() {
        let s = Scratch::new("perms-promote");
        let (mut users, _) = Users::load_or_seed(&s.file()).unwrap();
        let id = users
            .add("trader@example.com", "a-long-enough-password", Role::Client)
            .unwrap();
        users.set_role(id, Role::Manager).unwrap();
        assert!(users.by_id(id).unwrap().may(Power::ProcessDeposits));
    }

    #[test]
    fn a_desk_login_is_told_it_should_have_a_second_factor_and_a_client_is_not() {
        let s = Scratch::new("2fa-wanted");
        let (mut users, desk, _) = with_desk(&s);
        let client = users
            .add("trader@example.com", "a-long-enough-password", Role::Client)
            .unwrap();

        // Said as a question about the role: a desk login can take money
        // out of a client's account, and a client cannot move money without
        // the desk agreeing.
        assert!(users.by_id(desk).unwrap().should_have_second_factor());
        assert!(!users.by_id(client).unwrap().should_have_second_factor());
        assert!(!users.by_id(desk).unwrap().has_second_factor());
    }

    #[test]
    fn enrolment_is_not_finished_until_a_code_from_it_is_typed_back() {
        let s = Scratch::new("2fa-enrol");
        let (mut users, id, login) = with_desk(&s);
        let now = 1_700_000_000u64;

        let totp = users.begin_second_factor(id).unwrap();
        // The secret is written at once, because the app has it as soon as
        // the code is scanned. But it gates nothing yet: a mistyped setup
        // must not lock somebody out of their own platform.
        assert!(!users.by_id(id).unwrap().has_second_factor());
        assert!(matches!(
            users.sign_in(&login, "a-long-enough-password", "", now, Instant::now()),
            SignIn::Ok(_)
        ));

        // A code from a different secret does not finish it.
        let other = Totp::fresh().unwrap();
        assert!(users
            .confirm_second_factor(id, &other.code_at(Totp::step_of(now)), now)
            .is_err());
        assert!(!users.by_id(id).unwrap().has_second_factor());

        users
            .confirm_second_factor(id, &totp.code_at(Totp::step_of(now)), now)
            .unwrap();
        assert!(users.by_id(id).unwrap().has_second_factor());
    }

    #[test]
    fn once_it_is_on_the_password_alone_is_not_enough() {
        let s = Scratch::new("2fa-signin");
        let (mut users, id, login) = with_desk(&s);
        let now = 1_700_000_000u64;
        let totp = users.begin_second_factor(id).unwrap();
        users
            .confirm_second_factor(id, &totp.code_at(Totp::step_of(now)), now)
            .unwrap();

        // The right password, and still not in. Told apart from a refusal
        // on purpose: a screen that could not ask for a code without
        // asking everybody for one would ask the clients too.
        assert_eq!(
            users.sign_in(
                &login,
                "a-long-enough-password",
                "",
                now + 60,
                Instant::now()
            ),
            SignIn::CodeRequired
        );
        // A wrong password is still a plain refusal, whatever the code.
        assert_eq!(
            users.sign_in(
                &login,
                "not the password",
                &totp.code_at(Totp::step_of(now + 60)),
                now + 60,
                Instant::now()
            ),
            SignIn::Refused
        );
        assert!(matches!(
            users.sign_in(
                &login,
                "a-long-enough-password",
                &totp.code_at(Totp::step_of(now + 60)),
                now + 60,
                Instant::now()
            ),
            SignIn::Ok(_)
        ));
    }

    #[test]
    fn a_code_works_once_even_inside_its_thirty_seconds() {
        // The reason the step is stored. A code read over a shoulder, or
        // off a screen share, would otherwise be good for the rest of its
        // window — which is not what "one-time password" means.
        let s = Scratch::new("2fa-replay");
        let (mut users, id, login) = with_desk(&s);
        let now = 1_700_000_000u64;
        let totp = users.begin_second_factor(id).unwrap();
        users
            .confirm_second_factor(id, &totp.code_at(Totp::step_of(now)), now)
            .unwrap();

        let next = now + STEP_SECS;
        let code = totp.code_at(Totp::step_of(next));
        assert!(matches!(
            users.sign_in(
                &login,
                "a-long-enough-password",
                &code,
                next,
                Instant::now()
            ),
            SignIn::Ok(_)
        ));
        // The same code, one second later, inside the same window.
        assert!(matches!(
            users.sign_in(
                &login,
                "a-long-enough-password",
                &code,
                next + 1,
                Instant::now()
            ),
            SignIn::CodeRefused(_)
        ));
        // And the next one works, so refusing a used code has not broken
        // the login.
        let after = next + STEP_SECS;
        assert!(matches!(
            users.sign_in(
                &login,
                "a-long-enough-password",
                &totp.code_at(Totp::step_of(after)),
                after,
                Instant::now()
            ),
            SignIn::Ok(_)
        ));
    }

    #[test]
    fn a_used_code_stays_used_across_a_restart() {
        // The step is on disk rather than in memory, so a gateway restart
        // is not a way to replay a code.
        let s = Scratch::new("2fa-restart");
        let (mut users, id, login) = with_desk(&s);
        let now = 1_700_000_000u64;
        let totp = users.begin_second_factor(id).unwrap();
        users
            .confirm_second_factor(id, &totp.code_at(Totp::step_of(now)), now)
            .unwrap();

        let next = now + STEP_SECS;
        let code = totp.code_at(Totp::step_of(next));
        assert!(matches!(
            users.sign_in(
                &login,
                "a-long-enough-password",
                &code,
                next,
                Instant::now()
            ),
            SignIn::Ok(_)
        ));
        drop(users);

        let (mut again, _) = Users::load_or_seed(&s.file()).unwrap();
        assert!(again.by_id(id).unwrap().has_second_factor());
        assert!(matches!(
            again.sign_in(
                &login,
                "a-long-enough-password",
                &code,
                next,
                Instant::now()
            ),
            SignIn::CodeRefused(_)
        ));
    }

    #[test]
    fn guessing_codes_runs_into_the_same_lockout_as_guessing_passwords() {
        // Otherwise the second factor would be six digits guessable at any
        // rate, which is a million tries against a thirty-second window.
        let s = Scratch::new("2fa-lockout");
        let (mut users, id, login) = with_desk(&s);
        let now = 1_700_000_000u64;
        let totp = users.begin_second_factor(id).unwrap();
        users
            .confirm_second_factor(id, &totp.code_at(Totp::step_of(now)), now)
            .unwrap();

        let start = Instant::now();
        for _ in 0..MAX_FAILURES {
            assert!(matches!(
                users.sign_in(&login, "a-long-enough-password", "000000", now, start),
                SignIn::CodeRefused(_)
            ));
        }
        assert!(matches!(
            users.sign_in(&login, "a-long-enough-password", "000000", now, start),
            SignIn::TooManyAttempts { .. }
        ));
    }

    #[test]
    fn turning_it_off_needs_a_current_code() {
        // A session is already signed in, so asking for the password again
        // would prove nothing a stolen session could not also prove. A code
        // proves the phone is present, which is the thing being removed.
        let s = Scratch::new("2fa-off");
        let (mut users, id, _) = with_desk(&s);
        let now = 1_700_000_000u64;
        let totp = users.begin_second_factor(id).unwrap();
        users
            .confirm_second_factor(id, &totp.code_at(Totp::step_of(now)), now)
            .unwrap();

        assert!(users.stop_second_factor(id, "000000", now).is_err());
        assert!(users.by_id(id).unwrap().has_second_factor());

        let later = now + STEP_SECS;
        users
            .stop_second_factor(id, &code_now(&users, id, later), later)
            .unwrap();
        assert!(!users.by_id(id).unwrap().has_second_factor());
    }

    #[test]
    fn a_half_finished_enrolment_can_be_abandoned_without_a_code() {
        // There may be no working app to get one from — that is what
        // half-finished means — and an unconfirmed secret gates nothing,
        // so there is nothing being weakened by letting it go.
        let s = Scratch::new("2fa-abandon");
        let (mut users, id, _) = with_desk(&s);
        users.begin_second_factor(id).unwrap();
        users.stop_second_factor(id, "", 0).unwrap();
        assert!(users.by_id(id).unwrap().second.is_none());
    }

    #[test]
    fn a_working_second_factor_is_not_replaced_by_starting_again() {
        // Otherwise anybody with the session could enrol their own phone
        // over the top, which is a way to take the account rather than a
        // way to secure it.
        let s = Scratch::new("2fa-replace");
        let (mut users, id, _) = with_desk(&s);
        let now = 1_700_000_000u64;
        let totp = users.begin_second_factor(id).unwrap();
        users
            .confirm_second_factor(id, &totp.code_at(Totp::step_of(now)), now)
            .unwrap();

        assert!(users.begin_second_factor(id).is_err());
        // And the original still works.
        let later = now + STEP_SECS;
        assert!(users
            .by_id(id)
            .unwrap()
            .second
            .as_ref()
            .map(|sec| sec.secret.clone())
            .and_then(|sec| Totp::parse(&sec))
            .map(|t| t.code_at(Totp::step_of(later)) == totp.code_at(Totp::step_of(later)))
            .unwrap_or(false));
    }

    #[test]
    fn a_store_written_before_two_factor_reads_as_a_login_without_one() {
        // The only safe reading: the alternative locks everybody out at
        // once, on the upgrade, with nobody able to sign in and fix it.
        let s = Scratch::new("2fa-old-store");
        std::fs::write(
            s.file(),
            r#"{"users":[{"id":1,"login":"old@feraro.local","role":"admin","hash":"x"}]}"#,
        )
        .unwrap();
        let (users, _) = Users::load_or_seed(&s.file()).unwrap();
        let user = users.by_id(UserId(1)).unwrap();
        assert!(!user.has_second_factor());
        assert!(user.should_have_second_factor());
    }

    #[test]
    fn a_fresh_install_seeds_two_logins_and_says_their_passwords_once() {
        let s = Scratch::new("seed");
        let (users, seeded) = Users::load_or_seed(&s.file()).unwrap();
        assert_eq!(seeded.len(), 2);
        assert_eq!(users.all().len(), 2);
        assert_eq!(users.by_id(UserId(1)).unwrap().role, Role::Admin);
        assert_eq!(users.by_id(UserId(2)).unwrap().role, Role::Client);

        // The file holds hashes, never the passwords it just invented.
        let on_disk = std::fs::read_to_string(s.file()).unwrap();
        for (_, password) in &seeded {
            assert!(!on_disk.contains(password), "a password reached the disk");
        }
        assert!(on_disk.contains("$argon2id$"));
    }

    #[test]
    fn a_client_goes_into_a_managers_book_and_comes_out_again() {
        let s = Scratch::new("assign");
        let (mut users, _) = Users::load_or_seed(&s.file()).unwrap();
        let boss = users
            .add("boss@feraro.local", "manager-pass-1", Role::Manager)
            .unwrap();
        let client = users
            .add("trader@example.com", "client-pass-1", Role::Client)
            .unwrap();

        users.set_manager(client, Some(boss)).unwrap();
        assert_eq!(users.by_id(client).unwrap().manager, Some(boss));
        assert_eq!(users.book_of(boss), vec![boss, client]);

        users.set_manager(client, None).unwrap();
        assert_eq!(users.by_id(client).unwrap().manager, None);
        assert_eq!(
            users.book_of(boss),
            vec![boss],
            "an empty book is just themselves"
        );
    }

    #[test]
    fn an_assignment_that_would_mean_nothing_is_refused() {
        let s = Scratch::new("assign-refuse");
        let (mut users, _) = Users::load_or_seed(&s.file()).unwrap();
        let boss = users
            .add("boss@feraro.local", "manager-pass-1", Role::Manager)
            .unwrap();
        let client = users
            .add("trader@example.com", "client-pass-1", Role::Client)
            .unwrap();
        let other = users
            .add("other@example.com", "client-pass-2", Role::Client)
            .unwrap();

        // Handed to somebody who is not a manager.
        assert!(users.set_manager(client, Some(other)).is_err());
        // Managing yourself.
        assert!(users.set_manager(boss, Some(boss)).is_err());
        // A desk login is not somebody's client.
        assert!(users.set_manager(boss, Some(boss)).is_err());
        // Nobody at all.
        assert!(users.set_manager(client, Some(UserId(999))).is_err());

        assert_eq!(users.by_id(client).unwrap().manager, None, "nothing stuck");
    }

    #[test]
    fn demoting_a_manager_empties_their_book_rather_than_orphaning_it() {
        let s = Scratch::new("demote");
        let (mut users, _) = Users::load_or_seed(&s.file()).unwrap();
        let boss = users
            .add("boss@feraro.local", "manager-pass-1", Role::Manager)
            .unwrap();
        let client = users
            .add("trader@example.com", "client-pass-1", Role::Client)
            .unwrap();
        users.set_manager(client, Some(boss)).unwrap();

        users.set_role(boss, Role::Client).unwrap();

        // An assignment pointing at somebody who can no longer work the book
        // reads as supervision that is not happening, which is worse than
        // none.
        assert_eq!(users.by_id(client).unwrap().manager, None);
        assert_eq!(users.by_id(boss).unwrap().role, Role::Client);
    }

    #[test]
    fn promoting_a_client_takes_them_out_of_the_book_they_were_in() {
        let s = Scratch::new("promote");
        let (mut users, _) = Users::load_or_seed(&s.file()).unwrap();
        let boss = users
            .add("boss@feraro.local", "manager-pass-1", Role::Manager)
            .unwrap();
        let client = users
            .add("trader@example.com", "client-pass-1", Role::Client)
            .unwrap();
        users.set_manager(client, Some(boss)).unwrap();

        users.set_role(client, Role::Manager).unwrap();

        assert_eq!(users.by_id(client).unwrap().manager, None);
        assert_eq!(users.book_of(boss), vec![boss]);
    }

    #[test]
    fn an_assignment_survives_a_reload() {
        let s = Scratch::new("assign-reload");
        let boss;
        let client;
        {
            let (mut users, _) = Users::load_or_seed(&s.file()).unwrap();
            boss = users
                .add("boss@feraro.local", "manager-pass-1", Role::Manager)
                .unwrap();
            client = users
                .add("trader@example.com", "client-pass-1", Role::Client)
                .unwrap();
            users.set_manager(client, Some(boss)).unwrap();
        }
        let (users, _) = Users::load_or_seed(&s.file()).unwrap();
        assert_eq!(users.by_id(client).unwrap().manager, Some(boss));
        assert_eq!(users.by_id(boss).unwrap().role, Role::Manager);
    }

    #[test]
    fn a_store_written_before_managers_existed_still_loads() {
        let s = Scratch::new("old-store");
        // No `manager` field, and no `kyc` either: the shape this file had
        // two features ago. Unassigned and unverified are the readings that
        // give nobody something they were not given.
        std::fs::write(
            s.file(),
            r#"{"users":[{"id":1,"login":"admin@feraro.local","role":"admin","hash":"$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0$aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]}"#,
        )
        .unwrap();
        let (users, seeded) = Users::load_or_seed(&s.file()).unwrap();
        assert!(seeded.is_empty(), "an existing store is not re-seeded");
        let admin = users.by_id(UserId(1)).unwrap();
        assert_eq!(admin.role, Role::Admin);
        assert_eq!(admin.manager, None);
        assert_eq!(admin.kyc, Kyc::None);
    }

    #[test]
    fn the_roles_are_ordered_from_least_to_most() {
        assert!(Role::Client < Role::Manager);
        assert!(Role::Manager < Role::Admin);
        assert_eq!(Role::parse("manager"), Some(Role::Manager));
        assert_eq!(Role::parse("dealer"), None, "a role we do not have");
        assert_eq!(Role::Manager.as_str(), "manager");
    }

    #[test]
    fn a_reload_keeps_the_same_users_and_passwords() {
        let s = Scratch::new("reload");
        let (_, seeded) = Users::load_or_seed(&s.file()).unwrap();
        let (mut users, again) = Users::load_or_seed(&s.file()).unwrap();
        assert!(again.is_empty(), "an existing store is not re-seeded");

        let (login, password) = &seeded[1];
        let SignIn::Ok(user) = users.sign_in(login, password, "", 0, Instant::now()) else {
            panic!("the seeded password should work after a reload");
        };
        assert_eq!(user.role, Role::Client);
    }

    #[test]
    fn a_wrong_password_and_an_unknown_login_are_the_same_answer() {
        let s = Scratch::new("refuse");
        let (mut users, seeded) = Users::load_or_seed(&s.file()).unwrap();
        let now = Instant::now();
        assert_eq!(
            users.sign_in(&seeded[0].0, "not it", "", 0, now),
            SignIn::Refused
        );
        assert_eq!(
            users.sign_in("nobody", "not it", "", 0, now),
            SignIn::Refused
        );
    }

    #[test]
    fn repeated_failures_make_a_login_wait() {
        let s = Scratch::new("lockout");
        let (mut users, seeded) = Users::load_or_seed(&s.file()).unwrap();
        let (login, password) = &seeded[0];
        let now = Instant::now();
        for _ in 0..MAX_FAILURES {
            assert_eq!(users.sign_in(login, "wrong", "", 0, now), SignIn::Refused);
        }
        // Even the right password waits: the attempt is what is limited.
        assert!(matches!(
            users.sign_in(login, password, "", 0, now),
            SignIn::TooManyAttempts { .. }
        ));
        // ... until the lockout passes.
        let later = now + LOCKOUT + Duration::from_secs(1);
        assert!(matches!(
            users.sign_in(login, password, "", 0, later),
            SignIn::Ok(_)
        ));
        // And a success forgives the count.
        assert!(matches!(
            users.sign_in(login, password, "", 0, later),
            SignIn::Ok(_)
        ));
    }

    #[test]
    fn a_new_password_replaces_the_old_one() {
        let s = Scratch::new("rotate");
        let (mut users, seeded) = Users::load_or_seed(&s.file()).unwrap();
        let (login, old) = seeded[1].clone();
        let id = users.by_id(UserId(2)).unwrap().id;
        users.set_password(id, "a-much-longer-one").unwrap();

        let now = Instant::now();
        assert!(matches!(
            users.sign_in(&login, "a-much-longer-one", "", 0, now),
            SignIn::Ok(_)
        ));
        assert_eq!(users.sign_in(&login, &old, "", 0, now), SignIn::Refused);
        assert!(users.set_password(id, "short").is_err());
    }

    #[test]
    fn a_login_is_an_email_and_only_one_person_has_it() {
        let s = Scratch::new("unique");
        let (mut users, _) = Users::load_or_seed(&s.file()).unwrap();

        // The same address again, in different case: one person, one account.
        assert!(users
            .add("DEMO@feraro.local", "longenough", Role::Client)
            .is_err());
        assert!(matches!(
            users.sign_in("DEMO@FERARO.LOCAL", "wrong", "", 0, Instant::now()),
            SignIn::Refused
        ));

        // And it has to be an address at all.
        for not_an_email in ["demo", "demo@", "@feraro.local", "a b@c.com", "demo@local"] {
            assert!(
                users.add(not_an_email, "longenough", Role::Client).is_err(),
                "{not_an_email} is not an email"
            );
        }
        assert!(users
            .add("new.client@example.com", "longenough", Role::Client)
            .is_ok());
    }

    #[test]
    fn a_client_starts_with_no_identity_checks_and_the_desk_moves_them_along() {
        let s = Scratch::new("kyc");
        let (mut users, _) = Users::load_or_seed(&s.file()).unwrap();
        let id = users
            .add("kyc@example.com", "longenough", Role::Client)
            .unwrap();
        assert_eq!(users.by_id(id).unwrap().kyc, Kyc::None);

        users.set_kyc(id, Kyc::Submitted).unwrap();
        users.set_kyc(id, Kyc::Approved).unwrap();
        assert_eq!(users.by_id(id).unwrap().kyc, Kyc::Approved);

        // And it survives a reload, because it is on disk with the login.
        let (reloaded, _) = Users::load_or_seed(&s.file()).unwrap();
        assert_eq!(reloaded.by_id(id).unwrap().kyc, Kyc::Approved);
        assert!(users.set_kyc(UserId(999), Kyc::Approved).is_err());
    }

    #[test]
    fn a_store_written_before_identity_checks_reads_as_unchecked() {
        // The safe reading: unchecked allows a demo account and nothing more.
        let s = Scratch::new("kyc-legacy");
        let (users, _) = Users::load_or_seed(&s.file()).unwrap();
        let text = std::fs::read_to_string(s.file()).unwrap();
        let stripped = text.replace("\"kyc\": \"none\",", "");
        std::fs::write(s.file(), stripped).unwrap();
        drop(users);

        let (reloaded, seeded) = Users::load_or_seed(&s.file()).unwrap();
        assert!(seeded.is_empty());
        assert!(reloaded.all().iter().all(|u| u.kyc == Kyc::None));
    }

    #[test]
    fn a_token_is_unguessable_and_expires() {
        let mut sessions = Sessions::new();
        let now = Instant::now();
        let a = sessions.open(UserId(1), Role::Client, now);
        let b = sessions.open(UserId(1), Role::Client, now);
        assert_ne!(a, b, "two sessions must not share a token");
        assert_eq!(a.len(), 64);

        assert_eq!(sessions.resume(&a, now).map(|s| s.user), Some(UserId(1)));
        assert_eq!(sessions.resume("not a token", now), None);

        // Used just before it expires, a token keeps going: the use moves the
        // expiry out by another full TTL.
        let nearly = now + SESSION_TTL - Duration::from_secs(1);
        assert!(sessions.resume(&a, nearly).is_some());
        assert!(sessions
            .resume(&a, nearly + SESSION_TTL - Duration::from_secs(1))
            .is_some());

        // Left alone, it does not. `b` was never used after it was issued, so
        // it dies at its original expiry — a session expires *at* that
        // moment, not after it.
        assert!(sessions.resume(&b, now + SESSION_TTL).is_none());
    }

    #[test]
    fn closing_a_user_ends_every_session_they_hold() {
        let mut sessions = Sessions::new();
        let now = Instant::now();
        let one = sessions.open(UserId(7), Role::Admin, now);
        let two = sessions.open(UserId(7), Role::Admin, now);
        let other = sessions.open(UserId(8), Role::Client, now);

        sessions.close_user(UserId(7));
        assert!(sessions.resume(&one, now).is_none());
        assert!(sessions.resume(&two, now).is_none());
        assert!(
            sessions.resume(&other, now).is_some(),
            "revoking one user must not sign out another"
        );
        assert_eq!(sessions.count(), 1);
    }

    #[test]
    fn roles_are_ordered_by_what_they_can_do() {
        assert!(Role::Admin > Role::Client);
    }
}
