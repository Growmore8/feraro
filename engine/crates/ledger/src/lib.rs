//! Double-entry ledger with an enforced zero-sum invariant.
//!
//! # Where it sits
//!
//! The gateway folds one of these from the same journal the portfolio is
//! folded from, so the books and the balances are two views of one history
//! rather than two systems to reconcile. Every money movement — a deposit, a
//! withdrawal, a bonus, a closed position and its financing — is a balanced
//! transaction here, keyed by the journal sequence that produced it.
//!
//! # Invariant P0-4 — every currency nets to zero
//!
//! Money is never created or destroyed, only moved between accounts. A
//! transaction that does not sum to exactly zero *per currency* is rejected
//! before it touches a balance.
//!
//! This is what makes daily reconciliation against the liquidity provider
//! tractable: if our trial balance is non-zero, the bug is ours and we know it
//! within seconds rather than discovering it in an audit. Cross-currency moves
//! balance through an [`AccountKind::FxConversion`] leg rather than being
//! allowed to net across currencies, because netting across currencies is how
//! a rounding error hides.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic)]

use std::collections::BTreeMap;
use std::fmt;

use domain::{AccountId, Currency, Timestamp};
use money::{ArithError, Money};

/// Which book an amount sits in.
///
/// Client-facing kinds carry a real [`AccountId`]; house kinds use
/// [`AccountId(0)`](AccountId) since they belong to the broker, not a trader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AccountKind {
    /// A trader's cash balance.
    ClientCash,
    /// Funds in flight from the payment provider, not yet cleared.
    PspClearing,
    /// Realised trading profit and loss against the house.
    RealisedPnl,
    /// Commission earned by the broker.
    Commission,
    /// Overnight financing charged or paid.
    Swap,
    /// Non-withdrawable bonus extended to a trader.
    ///
    /// Counts toward margin but never toward withdrawable funds, so it is a
    /// separate account rather than a credit to `ClientCash`. Folding the two
    /// together is how a broker ends up paying out its own promotion.
    Bonus,
    /// Margin posted to a liquidity provider.
    LpMargin,
    /// The other leg of a currency conversion.
    FxConversion,
    /// Introducing-broker commission owed or paid.
    IbCommission,
    /// Money moved to put a trade right: a bad price, a dealing error, a fee
    /// returned, goodwill.
    ///
    /// Its own book, never [`AccountKind::RealisedPnl`]. An adjustment is not
    /// trading profit, and a broker whose corrections are mixed into its
    /// trading revenue cannot answer the one question that matters about
    /// them: how much, how often, and to whom.
    Adjustment,
    /// A trader's own money, set aside so it cannot be traded away.
    ///
    /// Not [`AccountKind::Bonus`]: a bonus is the house's money lent out
    /// and counts toward margin, while this is the client's own and does
    /// not. Folding the two together would let a broker margin a client
    /// against money the client had deliberately put beyond reach.
    Insurance,
}

/// A ledger account: a party, a book and a currency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LedgerAccount {
    /// Whose money it is. [`AccountId(0)`](AccountId) means the house.
    pub owner: AccountId,
    /// Which book.
    pub kind: AccountKind,
    /// Denomination.
    pub currency: Currency,
}

impl LedgerAccount {
    /// A client-owned account.
    pub const fn client(owner: AccountId, kind: AccountKind, currency: Currency) -> Self {
        LedgerAccount {
            owner,
            kind,
            currency,
        }
    }

    /// A house-owned account.
    pub const fn house(kind: AccountKind, currency: Currency) -> Self {
        LedgerAccount {
            owner: AccountId(0),
            kind,
            currency,
        }
    }
}

impl fmt::Display for LedgerAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.owner.0 == 0 {
            write!(f, "house/{:?}/{}", self.kind, self.currency)
        } else {
            write!(f, "{}/{:?}/{}", self.owner.0, self.kind, self.currency)
        }
    }
}

/// One leg of a transaction. Positive credits the account, negative debits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Posting {
    /// Account affected.
    pub account: LedgerAccount,
    /// Signed amount in the account's currency.
    pub amount: Money,
}

impl Posting {
    /// Builds a posting.
    pub const fn new(account: LedgerAccount, amount: Money) -> Self {
        Posting { account, amount }
    }
}

/// Identifies a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct TxId(pub u64);

/// A balanced set of postings applied atomically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    /// Identity, unique and never reused.
    pub id: TxId,
    /// When it occurred.
    pub at: Timestamp,
    /// The legs. Must net to zero per currency.
    pub postings: Vec<Posting>,
}

/// A transaction the ledger refused, or arithmetic that could not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedgerError {
    /// The postings did not net to zero in some currency.
    Unbalanced {
        /// Currency that failed to net.
        currency: Currency,
        /// What was left over.
        residual: Money,
    },
    /// A transaction with no postings.
    NoPostings,
    /// A transaction id already applied.
    DuplicateTransaction(TxId),
    /// A balance would move outside the representable range.
    Arithmetic(ArithError),
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LedgerError::Unbalanced { currency, residual } => write!(
                f,
                "transaction does not balance in {currency}: residual {}",
                residual.to_decimal_string()
            ),
            LedgerError::NoPostings => f.write_str("transaction has no postings"),
            LedgerError::DuplicateTransaction(id) => {
                write!(f, "transaction {} already applied", id.0)
            }
            LedgerError::Arithmetic(e) => write!(f, "ledger arithmetic: {e}"),
        }
    }
}

impl std::error::Error for LedgerError {}

impl From<ArithError> for LedgerError {
    fn from(e: ArithError) -> Self {
        LedgerError::Arithmetic(e)
    }
}

/// Checks a transaction nets to zero in every currency it touches.
///
/// Exposed separately so callers can validate before journaling, rather than
/// discovering the problem after the event is already durable.
pub fn check_balanced(tx: &Transaction) -> Result<(), LedgerError> {
    if tx.postings.is_empty() {
        return Err(LedgerError::NoPostings);
    }
    let mut per_currency: BTreeMap<Currency, Money> = BTreeMap::new();
    for p in &tx.postings {
        let slot = per_currency
            .entry(p.account.currency)
            .or_insert(Money::ZERO);
        *slot = slot.add(p.amount)?;
    }
    for (currency, residual) in per_currency {
        if !residual.is_zero() {
            return Err(LedgerError::Unbalanced { currency, residual });
        }
    }
    Ok(())
}

/// Balances for every account that has ever been posted to.
#[derive(Debug, Default, Clone)]
pub struct Ledger {
    balances: BTreeMap<LedgerAccount, Money>,
    applied: BTreeMap<TxId, usize>,
    /// Transactions up to and including this are taken as already posted,
    /// because a snapshot said so. Without it, a snapshot would lose the
    /// duplicate protection for everything it covers.
    imported_up_to: Option<TxId>,
}

impl Ledger {
    /// An empty ledger.
    pub fn new() -> Self {
        Ledger::default()
    }

    /// Every pot and its balance, for a snapshot.
    pub fn export(&self) -> Vec<(LedgerAccount, Money)> {
        self.balances.iter().map(|(a, m)| (*a, *m)).collect()
    }

    /// Rebuilds a ledger from exported balances.
    ///
    /// `applied_up_to` is the last transaction those balances already
    /// include. Anything at or below it is refused as a duplicate, exactly as
    /// it would have been had this ledger seen it the first time — which is
    /// what makes replaying a journal's tail onto a snapshot safe.
    pub fn import(balances: Vec<(LedgerAccount, Money)>, applied_up_to: TxId) -> Self {
        Ledger {
            balances: balances.into_iter().collect(),
            applied: BTreeMap::new(),
            imported_up_to: Some(applied_up_to),
        }
    }

    /// Applies a transaction, or rejects it leaving every balance untouched.
    pub fn post(&mut self, tx: &Transaction) -> Result<(), LedgerError> {
        check_balanced(tx)?;
        if self.applied.contains_key(&tx.id) {
            return Err(LedgerError::DuplicateTransaction(tx.id));
        }
        // Everything a snapshot covered is already in these balances, even
        // though this ledger never saw the transactions themselves.
        if self.imported_up_to.is_some_and(|last| tx.id <= last) {
            return Err(LedgerError::DuplicateTransaction(tx.id));
        }

        // Compute the whole result before mutating, so a mid-transaction
        // overflow cannot leave the ledger half-applied.
        let mut staged: BTreeMap<LedgerAccount, Money> = BTreeMap::new();
        for p in &tx.postings {
            let current = staged
                .get(&p.account)
                .copied()
                .or_else(|| self.balances.get(&p.account).copied())
                .unwrap_or(Money::ZERO);
            staged.insert(p.account, current.add(p.amount)?);
        }

        self.balances.extend(staged);
        self.applied.insert(tx.id, tx.postings.len());
        Ok(())
    }

    /// Balance of one account, zero if never posted to.
    pub fn balance(&self, account: &LedgerAccount) -> Money {
        self.balances.get(account).copied().unwrap_or(Money::ZERO)
    }

    /// Every non-empty account balance.
    pub fn balances(&self) -> impl Iterator<Item = (&LedgerAccount, &Money)> {
        self.balances.iter()
    }

    /// Number of transactions applied.
    pub fn transaction_count(&self) -> usize {
        self.applied.len()
    }

    /// Net position per currency across every account.
    ///
    /// Every entry must be zero. A non-zero entry means money was created or
    /// destroyed, which is a bug in the engine, not in the data.
    pub fn trial_balance(&self) -> Result<BTreeMap<Currency, Money>, LedgerError> {
        let mut totals: BTreeMap<Currency, Money> = BTreeMap::new();
        for (account, amount) in &self.balances {
            let slot = totals.entry(account.currency).or_insert(Money::ZERO);
            *slot = slot.add(*amount)?;
        }
        Ok(totals)
    }

    /// Fails if any currency has drifted from zero.
    pub fn assert_balanced(&self) -> Result<(), LedgerError> {
        for (currency, residual) in self.trial_balance()? {
            if !residual.is_zero() {
                return Err(LedgerError::Unbalanced { currency, residual });
            }
        }
        Ok(())
    }
}

/// A trader's deposit: money moves from the payment provider into their cash.
pub fn deposit(
    id: TxId,
    at: Timestamp,
    who: AccountId,
    currency: Currency,
    amount: Money,
) -> Transaction {
    Transaction {
        id,
        at,
        postings: vec![
            Posting::new(
                LedgerAccount::client(who, AccountKind::ClientCash, currency),
                amount,
            ),
            Posting::new(
                LedgerAccount::house(AccountKind::PspClearing, currency),
                amount.neg().unwrap_or(Money::ZERO),
            ),
        ],
    }
}

/// A closed trade: profit moves from the house to the trader, or the reverse.
pub fn realise_pnl(
    id: TxId,
    at: Timestamp,
    who: AccountId,
    currency: Currency,
    profit: Money,
) -> Transaction {
    Transaction {
        id,
        at,
        postings: vec![
            Posting::new(
                LedgerAccount::client(who, AccountKind::ClientCash, currency),
                profit,
            ),
            Posting::new(
                LedgerAccount::house(AccountKind::RealisedPnl, currency),
                profit.neg().unwrap_or(Money::ZERO),
            ),
        ],
    }
}

/// A closed position: trading profit, its financing, and what closing cost.
///
/// Three legs on the house side rather than one, because a broker's trading
/// revenue, its financing revenue and its commission are different numbers
/// that different people are asked about. One transaction rather than three,
/// because they are one event — and because a transaction id is the journal
/// sequence that produced it, which there is only one of. Posting them
/// separately is refused by the ledger, and rightly: that guard is what
/// makes a replay safe.
///
/// `swap` is signed as the trader sees it: negative is financing they paid.
/// `commission` is unsigned — it always leaves the client.
pub fn close_position(
    id: TxId,
    at: Timestamp,
    who: AccountId,
    currency: Currency,
    profit: Money,
    swap: Money,
    commission: Money,
) -> Result<Transaction, LedgerError> {
    let to_client = profit.add(swap)?.sub(commission)?;
    Ok(Transaction {
        id,
        at,
        postings: vec![
            Posting::new(
                LedgerAccount::client(who, AccountKind::ClientCash, currency),
                to_client,
            ),
            Posting::new(
                LedgerAccount::house(AccountKind::RealisedPnl, currency),
                profit.neg()?,
            ),
            Posting::new(
                LedgerAccount::house(AccountKind::Swap, currency),
                swap.neg()?,
            ),
            Posting::new(
                LedgerAccount::house(AccountKind::Commission, currency),
                commission,
            ),
        ],
    })
}

/// Money moved from one trader's cash to another's.
///
/// Two postings, both client books, no house book: nothing is created and
/// the platform is neither richer nor poorer for it. The amount is given
/// as it leaves, so `from` is debited and `to` credited by the same figure
/// and the transaction nets to zero like every other.
///
/// Both accounts must hold the same currency. A transfer across currencies
/// is a conversion, and this would silently book one at a rate of one.
pub fn transfer(
    id: TxId,
    at: Timestamp,
    from: AccountId,
    to: AccountId,
    currency: Currency,
    amount: Money,
) -> Result<Transaction, LedgerError> {
    Ok(Transaction {
        id,
        at,
        postings: vec![
            Posting::new(
                LedgerAccount::client(from, AccountKind::ClientCash, currency),
                amount.neg()?,
            ),
            Posting::new(
                LedgerAccount::client(to, AccountKind::ClientCash, currency),
                amount,
            ),
        ],
    })
}

/// Money moved between a trader's cash and their insurance.
///
/// Signed as the trader's cash sees it: negative is money leaving the
/// balance to be set aside, positive is insurance released back. Both
/// pots are the client's, so this never touches a house book — which is
/// exactly why it is its own transaction rather than a deposit.
pub fn insure(
    id: TxId,
    at: Timestamp,
    who: AccountId,
    currency: Currency,
    to_cash: Money,
) -> Result<Transaction, LedgerError> {
    Ok(Transaction {
        id,
        at,
        postings: vec![
            Posting::new(
                LedgerAccount::client(who, AccountKind::ClientCash, currency),
                to_cash,
            ),
            Posting::new(
                LedgerAccount::client(who, AccountKind::Insurance, currency),
                to_cash.neg()?,
            ),
        ],
    })
}

/// Commission charged when a position opens.
///
/// Two legs: the trader's cash goes down, the broker's commission book goes
/// up. It is its own transaction rather than part of the fill because a
/// fill moves no cash at all — opening a position posts margin, which is
/// not a payment — whereas commission leaves the account the moment the
/// trade is done.
///
/// Kept apart from `RealisedPnl` for the reason the swap book is: a broker
/// is asked what it earned in commission and what it earned in spread, and
/// they are different answers. Spread needs no book of its own — it is
/// already inside the price the client dealt at, so it reaches
/// `RealisedPnl` on its own when the position closes.
pub fn commission(
    id: TxId,
    at: Timestamp,
    who: AccountId,
    currency: Currency,
    amount: Money,
) -> Result<Transaction, LedgerError> {
    Ok(Transaction {
        id,
        at,
        postings: vec![
            Posting::new(
                LedgerAccount::client(who, AccountKind::ClientCash, currency),
                amount.neg()?,
            ),
            Posting::new(
                LedgerAccount::house(AccountKind::Commission, currency),
                amount,
            ),
        ],
    })
}

/// An amendment: money moved to put something right.
///
/// `amount` is signed as the client sees it — positive pays them. The house
/// leg lands in [`AccountKind::Adjustment`], so what corrections cost is a
/// number somebody can be shown.
pub fn adjust(
    id: TxId,
    at: Timestamp,
    who: AccountId,
    currency: Currency,
    amount: Money,
) -> Result<Transaction, LedgerError> {
    Ok(Transaction {
        id,
        at,
        postings: vec![
            Posting::new(
                LedgerAccount::client(who, AccountKind::ClientCash, currency),
                amount,
            ),
            Posting::new(
                LedgerAccount::house(AccountKind::Adjustment, currency),
                amount.neg()?,
            ),
        ],
    })
}

/// Commission charged on a fill.
pub fn charge_commission(
    id: TxId,
    at: Timestamp,
    who: AccountId,
    currency: Currency,
    fee: Money,
) -> Transaction {
    Transaction {
        id,
        at,
        postings: vec![
            Posting::new(
                LedgerAccount::client(who, AccountKind::ClientCash, currency),
                fee.neg().unwrap_or(Money::ZERO),
            ),
            Posting::new(LedgerAccount::house(AccountKind::Commission, currency), fee),
        ],
    }
}

/// A bonus extended to a trader: margin-eligible, never withdrawable.
pub fn grant_credit(
    id: TxId,
    at: Timestamp,
    who: AccountId,
    currency: Currency,
    amount: Money,
) -> Transaction {
    Transaction {
        id,
        at,
        postings: vec![
            Posting::new(
                LedgerAccount::client(who, AccountKind::Bonus, currency),
                amount,
            ),
            Posting::new(
                LedgerAccount::house(AccountKind::Bonus, currency),
                amount.neg().unwrap_or(Money::ZERO),
            ),
        ],
    }
}

/// Removes a bonus, or corrects an over-credit.
///
/// `amount` is the positive size of the removal; the postings negate it.
pub fn revoke_credit(
    id: TxId,
    at: Timestamp,
    who: AccountId,
    currency: Currency,
    amount: Money,
) -> Transaction {
    Transaction {
        id,
        at,
        postings: vec![
            Posting::new(
                LedgerAccount::client(who, AccountKind::Bonus, currency),
                amount.neg().unwrap_or(Money::ZERO),
            ),
            Posting::new(LedgerAccount::house(AccountKind::Bonus, currency), amount),
        ],
    }
}

/// Funds withdrawn by a trader, moving out through the payment provider.
pub fn withdraw(
    id: TxId,
    at: Timestamp,
    who: AccountId,
    currency: Currency,
    amount: Money,
) -> Transaction {
    Transaction {
        id,
        at,
        postings: vec![
            Posting::new(
                LedgerAccount::client(who, AccountKind::ClientCash, currency),
                amount.neg().unwrap_or(Money::ZERO),
            ),
            Posting::new(
                LedgerAccount::house(AccountKind::PspClearing, currency),
                amount,
            ),
        ],
    }
}

/// Every book one party holds, in a stable order.
///
/// For a statement, and for the administrator's view of one client: it is the
/// same question an operator asks of a bank — what is in each pot.
pub fn books_of(ledger: &Ledger, who: AccountId) -> Vec<(LedgerAccount, Money)> {
    ledger
        .balances()
        .filter(|(account, _)| account.owner == who)
        .map(|(account, amount)| (*account, *amount))
        .collect()
}

/// Funds a trader can actually take out: cash only, never bonus.
///
/// This is the single most important line in the module. `equity` includes
/// bonus because bonus supports margin; `withdrawable` must not, or the
/// promotion pays itself out.
pub fn withdrawable(ledger: &Ledger, who: AccountId, currency: Currency) -> Money {
    ledger.balance(&LedgerAccount::client(
        who,
        AccountKind::ClientCash,
        currency,
    ))
}

/// Cash plus bonus — what the margin engine sees.
pub fn margin_eligible(
    ledger: &Ledger,
    who: AccountId,
    currency: Currency,
) -> Result<Money, LedgerError> {
    let cash = ledger.balance(&LedgerAccount::client(
        who,
        AccountKind::ClientCash,
        currency,
    ));
    let bonus = ledger.balance(&LedgerAccount::client(who, AccountKind::Bonus, currency));
    Ok(cash.add(bonus)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic PRNG. Hand-rolled to keep the workspace dependency-free
    /// and, more importantly, so the property test below is reproducible from
    /// its seed on any machine.
    struct Lcg(u64);

    impl Lcg {
        fn new(seed: u64) -> Self {
            Lcg(seed)
        }
        fn next_u64(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next_u64() % n
        }
        /// A signed amount in ±10,000.00, to the cent.
        fn amount(&mut self) -> Money {
            // One cent is 10_000_000 raw units at MONEY_DIGITS = 9.
            let cents = i128::from(self.below(1_000_000));
            let magnitude = cents * 10_000_000;
            let negative = self.next_u64() & 1 == 1;
            Money::from_raw(if negative { -magnitude } else { magnitude })
        }
    }

    const TRADER: AccountId = AccountId(1001);

    fn cash(currency: Currency) -> LedgerAccount {
        LedgerAccount::client(TRADER, AccountKind::ClientCash, currency)
    }

    #[test]
    fn a_close_books_profit_financing_and_the_charge_in_one_transaction() {
        // One journal record gets one transaction id, and the ledger
        // refuses a second with the same one — so a close that charged
        // commission separately could not be booked at all. Which is the
        // right guard: it is what makes a replay safe.
        let mut l = Ledger::new();
        l.post(&deposit(
            TxId(1),
            Timestamp(0),
            TRADER,
            Currency::USD,
            Money::parse("10000.00").unwrap(),
        ))
        .unwrap();

        let tx = close_position(
            TxId(2),
            Timestamp(0),
            TRADER,
            Currency::USD,
            Money::parse("400.00").unwrap(),
            Money::parse("-20.00").unwrap(),
            Money::parse("2.50").unwrap(),
        )
        .expect("balances");
        l.post(&tx).unwrap();

        // The trader is up the profit, less the financing they paid and
        // less what closing cost: 400 − 20 − 2.50.
        assert_eq!(
            l.balance(&cash(Currency::USD)),
            Money::parse("10377.50").unwrap()
        );
        // Three house books, because a broker's trading revenue, its
        // financing revenue and its commission are three numbers three
        // different people ask about.
        assert_eq!(
            l.balance(&LedgerAccount::house(
                AccountKind::RealisedPnl,
                Currency::USD
            )),
            Money::parse("-400.00").unwrap()
        );
        assert_eq!(
            l.balance(&LedgerAccount::house(AccountKind::Swap, Currency::USD)),
            Money::parse("20.00").unwrap()
        );
        assert_eq!(
            l.balance(&LedgerAccount::house(
                AccountKind::Commission,
                Currency::USD
            )),
            Money::parse("2.50").unwrap()
        );
        l.assert_balanced().unwrap();
    }

    #[test]
    fn a_close_with_no_charge_leaves_the_commission_book_alone() {
        // A stop-out and a stop loss are free: the engine closed those, not
        // the trader. A zero posting would be noise in a book somebody
        // reads to answer what commission was earned.
        let mut l = Ledger::new();
        l.post(&deposit(
            TxId(1),
            Timestamp(0),
            TRADER,
            Currency::USD,
            Money::parse("10000.00").unwrap(),
        ))
        .unwrap();
        l.post(
            &close_position(
                TxId(2),
                Timestamp(0),
                TRADER,
                Currency::USD,
                Money::parse("100.00").unwrap(),
                Money::ZERO,
                Money::ZERO,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            l.balance(&LedgerAccount::house(
                AccountKind::Commission,
                Currency::USD
            )),
            Money::ZERO
        );
        l.assert_balanced().unwrap();
    }

    #[test]
    fn a_transfer_moves_money_between_clients_and_touches_no_house_book() {
        let tx = transfer(
            TxId(1),
            Timestamp(0),
            AccountId(1001),
            AccountId(1002),
            Currency::USD,
            Money::parse("250.00").unwrap(),
        )
        .expect("balances");

        assert_eq!(tx.postings.len(), 2);
        // Nothing is created, so the transaction nets to zero like every
        // other one. If it did not, the trial balance would stop reading
        // balanced and the platform would have invented money.
        let sum = tx
            .postings
            .iter()
            .fold(Money::ZERO, |acc, p| acc.add(p.amount).unwrap());
        assert_eq!(sum, Money::ZERO);

        // Both sides are client cash. A house book on either side would mean
        // the platform had taken a position in somebody else's money.
        assert!(tx.postings.iter().all(|p| p.account.owner != AccountId(0)));
        assert!(tx
            .postings
            .iter()
            .all(|p| p.account.kind == AccountKind::ClientCash));
    }

    #[test]
    fn a_deposit_moves_money_without_creating_it() {
        let mut l = Ledger::new();
        let amount = Money::parse("25000.00").unwrap();
        l.post(&deposit(
            TxId(1),
            Timestamp(0),
            TRADER,
            Currency::USD,
            amount,
        ))
        .unwrap();

        assert_eq!(l.balance(&cash(Currency::USD)), amount);
        assert_eq!(
            l.balance(&LedgerAccount::house(
                AccountKind::PspClearing,
                Currency::USD
            )),
            amount.neg().unwrap()
        );
        l.assert_balanced().unwrap();
    }

    #[test]
    fn a_winning_trade_debits_the_house() {
        let mut l = Ledger::new();
        l.post(&deposit(
            TxId(1),
            Timestamp(0),
            TRADER,
            Currency::USD,
            Money::parse("25000.00").unwrap(),
        ))
        .unwrap();
        l.post(&realise_pnl(
            TxId(2),
            Timestamp(1),
            TRADER,
            Currency::USD,
            Money::parse("159.00").unwrap(),
        ))
        .unwrap();

        assert_eq!(
            l.balance(&cash(Currency::USD))
                .to_display_string(2)
                .unwrap(),
            "25159.00"
        );
        assert_eq!(
            l.balance(&LedgerAccount::house(
                AccountKind::RealisedPnl,
                Currency::USD
            ))
            .to_display_string(2)
            .unwrap(),
            "-159.00"
        );
        l.assert_balanced().unwrap();
    }

    #[test]
    fn commission_leaves_the_client_and_reaches_the_house() {
        let mut l = Ledger::new();
        l.post(&deposit(
            TxId(1),
            Timestamp(0),
            TRADER,
            Currency::USD,
            Money::parse("1000.00").unwrap(),
        ))
        .unwrap();
        l.post(&charge_commission(
            TxId(2),
            Timestamp(1),
            TRADER,
            Currency::USD,
            Money::parse("7.00").unwrap(),
        ))
        .unwrap();

        assert_eq!(
            l.balance(&cash(Currency::USD))
                .to_display_string(2)
                .unwrap(),
            "993.00"
        );
        assert_eq!(
            l.balance(&LedgerAccount::house(
                AccountKind::Commission,
                Currency::USD
            ))
            .to_display_string(2)
            .unwrap(),
            "7.00"
        );
        l.assert_balanced().unwrap();
    }

    #[test]
    fn an_unbalanced_transaction_is_refused_and_changes_nothing() {
        let mut l = Ledger::new();
        let bad = Transaction {
            id: TxId(1),
            at: Timestamp(0),
            postings: vec![
                Posting::new(cash(Currency::USD), Money::parse("100.00").unwrap()),
                Posting::new(
                    LedgerAccount::house(AccountKind::PspClearing, Currency::USD),
                    Money::parse("-99.99").unwrap(),
                ),
            ],
        };

        let err = l.post(&bad).expect_err("must be refused");
        assert!(
            matches!(err, LedgerError::Unbalanced { currency, .. } if currency == Currency::USD)
        );
        assert_eq!(l.balance(&cash(Currency::USD)), Money::ZERO);
        assert_eq!(l.transaction_count(), 0);
    }

    #[test]
    fn currencies_must_balance_individually_not_in_aggregate() {
        // Naively summing across currencies would let these two legs cancel.
        // They must not: 100 USD is not minus 100 EUR.
        let tx = Transaction {
            id: TxId(1),
            at: Timestamp(0),
            postings: vec![
                Posting::new(cash(Currency::USD), Money::parse("100.00").unwrap()),
                Posting::new(cash(Currency::EUR), Money::parse("-100.00").unwrap()),
            ],
        };
        assert!(matches!(
            check_balanced(&tx),
            Err(LedgerError::Unbalanced { .. })
        ));
    }

    #[test]
    fn a_currency_conversion_balances_through_its_own_leg() {
        let mut l = Ledger::new();
        let usd_out = Money::parse("-108532.00").unwrap();
        let eur_in = Money::parse("99995.96").unwrap();

        let tx = Transaction {
            id: TxId(1),
            at: Timestamp(0),
            postings: vec![
                Posting::new(cash(Currency::USD), usd_out),
                Posting::new(
                    LedgerAccount::house(AccountKind::FxConversion, Currency::USD),
                    usd_out.neg().unwrap(),
                ),
                Posting::new(cash(Currency::EUR), eur_in),
                Posting::new(
                    LedgerAccount::house(AccountKind::FxConversion, Currency::EUR),
                    eur_in.neg().unwrap(),
                ),
            ],
        };

        l.post(&tx).unwrap();
        l.assert_balanced().unwrap();
        assert_eq!(
            l.balance(&cash(Currency::EUR))
                .to_display_string(2)
                .unwrap(),
            "99995.96"
        );
    }

    #[test]
    fn an_empty_transaction_is_refused() {
        let mut l = Ledger::new();
        let tx = Transaction {
            id: TxId(1),
            at: Timestamp(0),
            postings: vec![],
        };
        assert_eq!(l.post(&tx), Err(LedgerError::NoPostings));
    }

    #[test]
    fn replaying_the_same_transaction_twice_is_refused() {
        let mut l = Ledger::new();
        let tx = deposit(
            TxId(1),
            Timestamp(0),
            TRADER,
            Currency::USD,
            Money::parse("10.00").unwrap(),
        );
        l.post(&tx).unwrap();
        assert_eq!(l.post(&tx), Err(LedgerError::DuplicateTransaction(TxId(1))));
        assert_eq!(
            l.balance(&cash(Currency::USD))
                .to_display_string(2)
                .unwrap(),
            "10.00"
        );
    }

    #[test]
    fn ten_thousand_random_transactions_leave_the_books_exactly_flat() {
        // The P0 exit criterion: a double-entry property test. Every currency
        // must net to exactly zero after arbitrary balanced activity — not
        // "close to zero", which is the answer a float ledger gives.
        let mut rng = Lcg::new(0x5EED_1234_ABCD_0001);
        let mut l = Ledger::new();
        let currencies = [Currency::USD, Currency::EUR, Currency::GBP, Currency::JPY];
        let kinds = [
            AccountKind::RealisedPnl,
            AccountKind::Commission,
            AccountKind::Swap,
            AccountKind::PspClearing,
            AccountKind::IbCommission,
            AccountKind::LpMargin,
        ];

        for i in 0..10_000u64 {
            let currency = currencies[rng.below(currencies.len() as u64) as usize];
            let counterparty = kinds[rng.below(kinds.len() as u64) as usize];
            let trader = AccountId(1 + rng.below(250));
            let amount = rng.amount();

            let tx = Transaction {
                id: TxId(i),
                at: Timestamp(i as i64),
                postings: vec![
                    Posting::new(
                        LedgerAccount::client(trader, AccountKind::ClientCash, currency),
                        amount,
                    ),
                    Posting::new(
                        LedgerAccount::house(counterparty, currency),
                        amount.neg().unwrap(),
                    ),
                ],
            };
            l.post(&tx)
                .expect("balanced transactions are always accepted");
        }

        assert_eq!(l.transaction_count(), 10_000);
        l.assert_balanced().expect("books must be exactly flat");

        for (currency, residual) in l.trial_balance().unwrap() {
            assert_eq!(residual, Money::ZERO, "{currency} drifted");
        }
    }

    #[test]
    fn a_bonus_supports_margin_but_is_not_withdrawable() {
        let mut l = Ledger::new();
        l.post(&deposit(
            TxId(1),
            Timestamp(0),
            TRADER,
            Currency::USD,
            Money::parse("25000.00").unwrap(),
        ))
        .unwrap();
        l.post(&grant_credit(
            TxId(2),
            Timestamp(1),
            TRADER,
            Currency::USD,
            Money::parse("500.00").unwrap(),
        ))
        .unwrap();

        // The margin engine sees 25,500. The withdrawal desk sees 25,000.
        assert_eq!(
            margin_eligible(&l, TRADER, Currency::USD)
                .unwrap()
                .to_display_string(2)
                .unwrap(),
            "25500.00"
        );
        assert_eq!(
            withdrawable(&l, TRADER, Currency::USD)
                .to_display_string(2)
                .unwrap(),
            "25000.00"
        );
        l.assert_balanced().unwrap();
    }

    #[test]
    fn revoking_a_bonus_returns_the_books_to_where_they_were() {
        let mut l = Ledger::new();
        let amount = Money::parse("500.00").unwrap();
        l.post(&grant_credit(
            TxId(1),
            Timestamp(0),
            TRADER,
            Currency::USD,
            amount,
        ))
        .unwrap();
        l.post(&revoke_credit(
            TxId(2),
            Timestamp(1),
            TRADER,
            Currency::USD,
            amount,
        ))
        .unwrap();

        assert_eq!(
            l.balance(&LedgerAccount::client(
                TRADER,
                AccountKind::Bonus,
                Currency::USD
            )),
            Money::ZERO
        );
        l.assert_balanced().unwrap();
    }

    #[test]
    fn a_withdrawal_cannot_reach_bonus_money() {
        let mut l = Ledger::new();
        l.post(&deposit(
            TxId(1),
            Timestamp(0),
            TRADER,
            Currency::USD,
            Money::parse("1000.00").unwrap(),
        ))
        .unwrap();
        l.post(&grant_credit(
            TxId(2),
            Timestamp(1),
            TRADER,
            Currency::USD,
            Money::parse("500.00").unwrap(),
        ))
        .unwrap();
        l.post(&withdraw(
            TxId(3),
            Timestamp(2),
            TRADER,
            Currency::USD,
            Money::parse("1000.00").unwrap(),
        ))
        .unwrap();

        // Cash is drained; the bonus is untouched and still supports margin.
        assert_eq!(withdrawable(&l, TRADER, Currency::USD), Money::ZERO);
        assert_eq!(
            margin_eligible(&l, TRADER, Currency::USD)
                .unwrap()
                .to_display_string(2)
                .unwrap(),
            "500.00"
        );
        l.assert_balanced().unwrap();
    }

    #[test]
    fn the_property_test_is_reproducible_from_its_seed() {
        let mut a = Lcg::new(42);
        let mut b = Lcg::new(42);
        for _ in 0..1_000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }
}
