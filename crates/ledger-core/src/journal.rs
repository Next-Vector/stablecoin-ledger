use std::collections::{HashMap, HashSet};

use crate::{AccountAddress, AssetId, Balance, BalanceError, Transaction};

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

type BalanceKey = (AccountAddress, AssetId);

/// In-memory double-entry journal: applies transactions atomically and keeps balances.
#[derive(Debug, Default)]
pub struct Journal {
    balances: HashMap<BalanceKey, Balance>,
    frozen: HashSet<AccountAddress>,
    transactions: Vec<Transaction>,
}

impl Journal {
    /// Applies all postings of `tx` or none of them, and returns the id assigned to it.
    pub fn apply(&mut self, tx: Transaction) -> Result<TxId, ApplyError> {
        let mut staged: HashMap<BalanceKey, Balance> = HashMap::new();
        for posting in tx.postings() {
            if self.frozen.contains(posting.source()) {
                return Err(ApplyError::AccountFrozen(posting.source().clone()));
            }
            let source = (posting.source().clone(), posting.asset().clone());
            let destination = (posting.destination().clone(), posting.asset().clone());
            let debited = self
                .staged_balance(&staged, &source)
                .debit(posting.amount())?;
            staged.insert(source, debited);
            let credited = self
                .staged_balance(&staged, &destination)
                .credit(posting.amount())?;
            staged.insert(destination, credited);
        }

        if let Some(((account, asset), _)) = staged.iter().find(|((account, _), balance)| {
            balance.is_negative() && !Self::allows_negative(account)
        }) {
            return Err(ApplyError::InsufficientFunds {
                account: account.clone(),
                asset: asset.clone(),
            });
        }

        self.balances.extend(staged);
        self.transactions.push(tx);
        Ok(TxId(self.transactions.len() as u64))
    }

    pub fn balance(&self, account: &AccountAddress, asset: &AssetId) -> Balance {
        self.balances
            .get(&(account.clone(), asset.clone()))
            .copied()
            .unwrap_or_default()
    }

    pub fn transaction(&self, id: TxId) -> Option<&Transaction> {
        let index = usize::try_from(id.0).ok()?.checked_sub(1)?;
        self.transactions.get(index)
    }

    /// A frozen account cannot be a posting source; it can still receive.
    pub fn freeze(&mut self, account: AccountAddress) {
        self.frozen.insert(account);
    }

    pub fn unfreeze(&mut self, account: &AccountAddress) {
        self.frozen.remove(account);
    }

    fn staged_balance(&self, staged: &HashMap<BalanceKey, Balance>, key: &BalanceKey) -> Balance {
        staged
            .get(key)
            .copied()
            .unwrap_or_else(|| self.balances.get(key).copied().unwrap_or_default())
    }

    /// Only `world` may go negative for now; system accounts with `allow_negative` come with
    /// the features that need them (e.g. gas accounting).
    fn allows_negative(account: &AccountAddress) -> bool {
        account.is_world()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Metadata, Posting};

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
