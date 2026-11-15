use std::collections::{HashMap, HashSet};

use crate::{
    AccountAddress, AccountPattern, AssetId, Balance, BalanceError, Hold, Metadata, Transaction,
    plan,
};

/// Monotonic transaction id assigned by the journal, starting at 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TxId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplyError {
    #[error("insufficient funds on {account} in {asset}")]
    InsufficientFunds {
        account: AccountAddress,
        asset: AssetId,
    },
    #[error("account {0} is frozen and cannot be a posting source")]
    AccountFrozen(AccountAddress),
    #[error("balance overflow")]
    Overflow,
}

impl From<BalanceError> for ApplyError {
    fn from(BalanceError::Overflow: BalanceError) -> Self {
        Self::Overflow
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RevertError {
    #[error("transaction {0:?} does not exist")]
    NotFound(TxId),
    #[error("transaction {0:?} has already been reverted")]
    AlreadyReverted(TxId),
    #[error(transparent)]
    Apply(#[from] ApplyError),
}

type BalanceKey = (AccountAddress, AssetId);

#[derive(Debug)]
struct Recorded {
    tx: Transaction,
    reverts: Option<TxId>,
    reverted_by: Option<TxId>,
}

/// In-memory double-entry journal: applies transactions atomically and keeps balances.
#[derive(Debug, Default)]
pub struct Journal {
    balances: HashMap<BalanceKey, Balance>,
    frozen: HashSet<AccountAddress>,
    transactions: Vec<Recorded>,
    pub(crate) holds: Vec<Hold>,
}

impl Journal {
    /// Applies all postings of `tx` or none of them, and returns the id assigned to it.
    pub fn apply(&mut self, tx: Transaction) -> Result<TxId, ApplyError> {
        self.record(tx, None)
    }

    /// Reverses transaction `id` with a new transaction of mirrored postings. A transaction can be
    /// reverted once; the reversal follows the usual rules (funds, frozen accounts, overflow).
    pub fn revert(&mut self, id: TxId, metadata: Metadata) -> Result<TxId, RevertError> {
        let original = self.recorded(id).ok_or(RevertError::NotFound(id))?;
        if original.reverted_by.is_some() {
            return Err(RevertError::AlreadyReverted(id));
        }
        let reversal = original.tx.reversed(metadata);
        let reversal_id = self.record(reversal, Some(id))?;
        if let Some(original) = self.recorded_mut(id) {
            original.reverted_by = Some(reversal_id);
        }
        Ok(reversal_id)
    }

    fn record(&mut self, tx: Transaction, reverts: Option<TxId>) -> Result<TxId, ApplyError> {
        let changes = plan(
            &tx,
            |account, asset| self.balance(account, asset),
            |account| self.frozen.contains(account),
        )?;
        for change in changes {
            self.balances
                .insert((change.account, change.asset), change.balance);
        }
        self.transactions.push(Recorded {
            tx,
            reverts,
            reverted_by: None,
        });
        Ok(TxId(self.transactions.len() as u64))
    }

    pub fn balance(&self, account: &AccountAddress, asset: &AssetId) -> Balance {
        self.balances
            .get(&(account.clone(), asset.clone()))
            .copied()
            .unwrap_or_default()
    }

    /// Balance of `account` right after transaction `at` was applied; `None` if `at` does not exist.
    pub fn balance_at(
        &self,
        account: &AccountAddress,
        asset: &AssetId,
        at: TxId,
    ) -> Option<Balance> {
        let count = Self::index(at)? + 1;
        let history = self.transactions.get(..count)?;
        let mut balance = Balance::ZERO;
        let postings = history
            .iter()
            .flat_map(|recorded| recorded.tx.postings())
            .filter(|posting| posting.asset() == asset);
        for posting in postings {
            // Replays postings in the order `apply` checked them, so overflow cannot occur here.
            if posting.source() == account {
                balance = balance.debit(posting.amount()).ok()?;
            }
            if posting.destination() == account {
                balance = balance.credit(posting.amount()).ok()?;
            }
        }
        Some(balance)
    }

    /// Sum of the current balances of all accounts matching `pattern` in `asset`.
    pub fn aggregate_balance(
        &self,
        pattern: &AccountPattern,
        asset: &AssetId,
    ) -> Result<Balance, BalanceError> {
        self.balances
            .iter()
            .filter(|((account, balance_asset), _)| {
                balance_asset == asset && pattern.matches(account)
            })
            .try_fold(Balance::ZERO, |sum, (_, balance)| sum.checked_add(*balance))
    }

    pub fn transaction(&self, id: TxId) -> Option<&Transaction> {
        self.recorded(id).map(|recorded| &recorded.tx)
    }

    /// The transaction that `id` reverses, if `id` is a reversal.
    pub fn reverts(&self, id: TxId) -> Option<TxId> {
        self.recorded(id)?.reverts
    }

    /// The reversal of `id`, if it has been reverted.
    pub fn reverted_by(&self, id: TxId) -> Option<TxId> {
        self.recorded(id)?.reverted_by
    }

    fn recorded(&self, id: TxId) -> Option<&Recorded> {
        self.transactions.get(Self::index(id)?)
    }

    fn recorded_mut(&mut self, id: TxId) -> Option<&mut Recorded> {
        self.transactions.get_mut(Self::index(id)?)
    }

    fn index(id: TxId) -> Option<usize> {
        usize::try_from(id.0).ok()?.checked_sub(1)
    }

    /// A frozen account cannot be a posting source; it can still receive.
    pub fn freeze(&mut self, account: AccountAddress) {
        self.frozen.insert(account);
    }

    pub fn unfreeze(&mut self, account: &AccountAddress) {
        self.frozen.remove(account);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Posting;

    fn account(raw: &str) -> AccountAddress {
        raw.parse().unwrap()
    }

    fn usdc() -> AssetId {
        "USDC@ethereum/6".parse().unwrap()
    }

    fn dai() -> AssetId {
        "DAI@ethereum/18".parse().unwrap()
    }

    fn posting(source: &str, destination: &str, amount: u128, asset: AssetId) -> Posting {
        let source = if source == "world" {
            AccountAddress::world()
        } else {
            account(source)
        };
        Posting::new(source, account(destination), amount, asset).unwrap()
    }

    fn tx(postings: Vec<Posting>) -> Transaction {
        Transaction::new(postings, Metadata::new()).unwrap()
    }

    fn balance(value: i128) -> Balance {
        if value < 0 {
            Balance::ZERO.debit(value.unsigned_abs()).unwrap()
        } else {
            Balance::ZERO.credit(value as u128).unwrap()
        }
    }

    /// Journal with users:1:wallet funded with `amount` USDC from world.
    fn funded(amount: u128) -> Journal {
        let mut journal = Journal::default();
        journal
            .apply(tx(vec![posting("world", "users:1:wallet", amount, usdc())]))
            .unwrap();
        journal
    }

    #[test]
    fn unknown_balance_is_zero() {
        assert_eq!(
            Journal::default().balance(&account("users:1:wallet"), &usdc()),
            Balance::ZERO
        );
    }

    #[test]
    fn world_may_go_negative() {
        let journal = funded(100);
        assert_eq!(
            journal.balance(&AccountAddress::world(), &usdc()),
            balance(-100)
        );
        assert_eq!(
            journal.balance(&account("users:1:wallet"), &usdc()),
            balance(100)
        );
    }

    #[test]
    fn applies_all_postings_of_a_transaction() {
        let mut journal = funded(100);
        journal
            .apply(tx(vec![
                posting("users:1:wallet", "merchants:7:wallet", 99, usdc()),
                posting("users:1:wallet", "platform:fees", 1, usdc()),
            ]))
            .unwrap();
        assert_eq!(
            journal.balance(&account("users:1:wallet"), &usdc()),
            Balance::ZERO
        );
        assert_eq!(
            journal.balance(&account("merchants:7:wallet"), &usdc()),
            balance(99)
        );
        assert_eq!(
            journal.balance(&account("platform:fees"), &usdc()),
            balance(1)
        );
    }

    #[test]
    fn balances_are_tracked_per_asset() {
        let mut journal = funded(100);
        journal
            .apply(tx(vec![posting("world", "users:1:wallet", 5, dai())]))
            .unwrap();
        assert_eq!(
            journal.balance(&account("users:1:wallet"), &usdc()),
            balance(100)
        );
        assert_eq!(
            journal.balance(&account("users:1:wallet"), &dai()),
            balance(5)
        );
    }

    #[test]
    fn assigns_monotonic_ids_and_records_transactions() {
        let mut journal = Journal::default();
        let first = journal
            .apply(tx(vec![posting("world", "users:1:wallet", 1, usdc())]))
            .unwrap();
        let second = journal
            .apply(tx(vec![posting("world", "users:1:wallet", 2, usdc())]))
            .unwrap();
        assert!(first < second);
        assert_eq!(
            journal.transaction(first).unwrap().postings()[0].amount(),
            1
        );
        assert_eq!(
            journal.transaction(second).unwrap().postings()[0].amount(),
            2
        );
    }

    #[test]
    fn rejects_insufficient_funds() {
        let mut journal = funded(100);
        let result = journal.apply(tx(vec![posting(
            "users:1:wallet",
            "users:2:wallet",
            101,
            usdc(),
        )]));
        assert_eq!(
            result,
            Err(ApplyError::InsufficientFunds {
                account: account("users:1:wallet"),
                asset: usdc()
            })
        );
    }

    #[test]
    fn checks_funds_on_the_net_result_of_the_transaction() {
        let mut journal = Journal::default();
        // users:1 receives 50 and spends 50 in the same transaction.
        journal
            .apply(tx(vec![
                posting("world", "users:1:wallet", 50, usdc()),
                posting("users:1:wallet", "users:2:wallet", 50, usdc()),
            ]))
            .unwrap();
        assert_eq!(
            journal.balance(&account("users:2:wallet"), &usdc()),
            balance(50)
        );
    }

    #[test]
    fn failed_transaction_leaves_no_trace() {
        let mut journal = funded(100);
        let result = journal.apply(tx(vec![
            posting("users:1:wallet", "users:2:wallet", 60, usdc()),
            posting("users:1:wallet", "users:3:wallet", 60, usdc()),
        ]));
        assert!(result.is_err());
        assert_eq!(
            journal.balance(&account("users:1:wallet"), &usdc()),
            balance(100)
        );
        assert_eq!(
            journal.balance(&account("users:2:wallet"), &usdc()),
            Balance::ZERO
        );
        assert_eq!(
            journal.balance(&account("users:3:wallet"), &usdc()),
            Balance::ZERO
        );

        // The failed attempt consumed no id.
        let next = journal
            .apply(tx(vec![posting("world", "users:1:wallet", 1, usdc())]))
            .unwrap();
        assert_eq!(next, TxId(2));
        assert!(journal.transaction(TxId(3)).is_none());
    }

    #[test]
    fn frozen_account_cannot_be_a_source() {
        let mut journal = funded(100);
        journal.freeze(account("users:1:wallet"));
        let result = journal.apply(tx(vec![posting(
            "users:1:wallet",
            "users:2:wallet",
            1,
            usdc(),
        )]));
        assert_eq!(
            result,
            Err(ApplyError::AccountFrozen(account("users:1:wallet")))
        );
    }

    #[test]
    fn frozen_account_can_receive() {
        let mut journal = funded(100);
        journal.freeze(account("users:2:wallet"));
        journal
            .apply(tx(vec![posting(
                "users:1:wallet",
                "users:2:wallet",
                1,
                usdc(),
            )]))
            .unwrap();
        assert_eq!(
            journal.balance(&account("users:2:wallet"), &usdc()),
            balance(1)
        );
    }

    #[test]
    fn unfrozen_account_can_send_again() {
        let mut journal = funded(100);
        journal.freeze(account("users:1:wallet"));
        journal.unfreeze(&account("users:1:wallet"));
        assert!(
            journal
                .apply(tx(vec![posting(
                    "users:1:wallet",
                    "users:2:wallet",
                    1,
                    usdc()
                )]))
                .is_ok()
        );
    }

    #[test]
    fn revert_applies_mirrored_postings_and_links_both_transactions() {
        let mut journal = funded(100);
        let payment = journal
            .apply(tx(vec![
                posting("users:1:wallet", "merchants:7:wallet", 99, usdc()),
                posting("users:1:wallet", "platform:fees", 1, usdc()),
            ]))
            .unwrap();
        let reversal = journal.revert(payment, Metadata::new()).unwrap();

        assert_eq!(
            journal.balance(&account("users:1:wallet"), &usdc()),
            balance(100)
        );
        assert_eq!(
            journal.balance(&account("merchants:7:wallet"), &usdc()),
            Balance::ZERO
        );
        assert_eq!(
            journal.balance(&account("platform:fees"), &usdc()),
            Balance::ZERO
        );
        let mirrored = journal.transaction(reversal).unwrap().postings();
        assert_eq!(
            mirrored[0],
            posting("merchants:7:wallet", "users:1:wallet", 99, usdc())
        );
        assert_eq!(
            mirrored[1],
            posting("platform:fees", "users:1:wallet", 1, usdc())
        );
        assert_eq!(journal.reverts(reversal), Some(payment));
        assert_eq!(journal.reverted_by(payment), Some(reversal));
        assert_eq!(journal.reverts(payment), None);
    }

    #[test]
    fn a_transaction_can_be_reverted_only_once() {
        let mut journal = funded(100);
        let id = journal
            .apply(tx(vec![posting(
                "users:1:wallet",
                "users:2:wallet",
                10,
                usdc(),
            )]))
            .unwrap();
        journal.revert(id, Metadata::new()).unwrap();
        assert_eq!(
            journal.revert(id, Metadata::new()),
            Err(RevertError::AlreadyReverted(id))
        );
    }

    #[test]
    fn reverting_an_unknown_transaction_fails() {
        let mut journal = funded(100);
        assert_eq!(
            journal.revert(TxId(42), Metadata::new()),
            Err(RevertError::NotFound(TxId(42)))
        );
    }

    #[test]
    fn revert_follows_the_usual_rules_and_leaves_no_trace_on_failure() {
        let mut journal = funded(100);
        let id = journal
            .apply(tx(vec![posting(
                "users:1:wallet",
                "users:2:wallet",
                10,
                usdc(),
            )]))
            .unwrap();
        // users:2 has already spent the money, so the reversal cannot take it back.
        journal
            .apply(tx(vec![posting(
                "users:2:wallet",
                "users:3:wallet",
                10,
                usdc(),
            )]))
            .unwrap();
        assert_eq!(
            journal.revert(id, Metadata::new()),
            Err(RevertError::Apply(ApplyError::InsufficientFunds {
                account: account("users:2:wallet"),
                asset: usdc()
            }))
        );
        assert_eq!(journal.reverted_by(id), None);
        assert_eq!(
            journal.balance(&account("users:1:wallet"), &usdc()),
            balance(90)
        );
    }

    #[test]
    fn balance_at_replays_history_up_to_a_transaction() {
        let mut journal = Journal::default();
        let first = journal
            .apply(tx(vec![posting("world", "users:1:wallet", 100, usdc())]))
            .unwrap();
        let second = journal
            .apply(tx(vec![posting(
                "users:1:wallet",
                "users:2:wallet",
                30,
                usdc(),
            )]))
            .unwrap();
        journal
            .apply(tx(vec![posting("world", "users:1:wallet", 5, dai())]))
            .unwrap();

        let user = account("users:1:wallet");
        assert_eq!(
            journal.balance_at(&user, &usdc(), first),
            Some(balance(100))
        );
        assert_eq!(
            journal.balance_at(&user, &usdc(), second),
            Some(balance(70))
        );
        assert_eq!(
            journal.balance_at(&AccountAddress::world(), &usdc(), second),
            Some(balance(-100))
        );
        assert_eq!(
            journal.balance_at(&user, &dai(), second),
            Some(Balance::ZERO)
        );
        assert_eq!(journal.balance_at(&user, &usdc(), TxId(99)), None);
    }

    #[test]
    fn aggregates_balances_by_pattern() {
        let mut journal = Journal::default();
        journal
            .apply(tx(vec![
                posting("world", "users:1:wallet", 10, usdc()),
                posting("world", "users:2:wallet", 20, usdc()),
                posting("world", "users:2:pending", 40, usdc()),
                posting("world", "users:3:wallet", 7, dai()),
            ]))
            .unwrap();

        let wallets: AccountPattern = "users:*:wallet".parse().unwrap();
        assert_eq!(
            journal.aggregate_balance(&wallets, &usdc()),
            Ok(balance(30))
        );
        assert_eq!(journal.aggregate_balance(&wallets, &dai()), Ok(balance(7)));
        let everything: AccountPattern = "users:*:*".parse().unwrap();
        assert_eq!(
            journal.aggregate_balance(&everything, &usdc()),
            Ok(balance(70))
        );
        let nothing: AccountPattern = "merchants:*:wallet".parse().unwrap();
        assert_eq!(
            journal.aggregate_balance(&nothing, &usdc()),
            Ok(Balance::ZERO)
        );
    }

    #[test]
    fn overflow_rejects_the_transaction() {
        let mut journal = funded(u128::MAX);
        let result = journal.apply(tx(vec![posting("world", "users:1:wallet", 1, usdc())]));
        assert_eq!(result, Err(ApplyError::Overflow));
        assert_eq!(
            journal.balance(&account("users:1:wallet"), &usdc()),
            Balance::ZERO.credit(u128::MAX).unwrap()
        );
    }
}
