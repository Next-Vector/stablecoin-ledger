//! The storage port: what the rest of the system needs from a ledger store, whatever backs it.
//! Implemented by the in-memory [`Journal`] here and by PostgreSQL in `ledger-store`.

use std::future::{Future, ready};
use std::sync::Mutex;

use crate::{AccountAddress, ApplyError, AssetId, Balance, Journal, Transaction, TxId};

pub trait LedgerStore {
    /// Applies all postings of `tx` atomically, or none of them.
    fn apply(&self, tx: Transaction) -> impl Future<Output = Result<TxId, StoreError>> + Send;

    /// The current balance of `account` in `asset`; zero if it never moved.
    fn balance(
        &self,
        account: &AccountAddress,
        asset: &AssetId,
    ) -> impl Future<Output = Result<Balance, StoreError>> + Send;
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The transaction broke a ledger rule; nothing was written.
    #[error(transparent)]
    Apply(#[from] ApplyError),
    /// The storage backend failed (connection, query, corrupted data).
    #[error("storage backend failed: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl LedgerStore for Mutex<Journal> {
    fn apply(&self, tx: Transaction) -> impl Future<Output = Result<TxId, StoreError>> + Send {
        let result = match self.lock() {
            Ok(mut journal) => journal.apply(tx).map_err(StoreError::from),
            Err(poisoned) => Err(StoreError::Backend(poisoned.to_string().into())),
        };
        ready(result)
    }

    fn balance(
        &self,
        account: &AccountAddress,
        asset: &AssetId,
    ) -> impl Future<Output = Result<Balance, StoreError>> + Send {
        let result = match self.lock() {
            Ok(journal) => Ok(journal.balance(account, asset)),
            Err(poisoned) => Err(StoreError::Backend(poisoned.to_string().into())),
        };
        ready(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Metadata, Posting};

    fn usdc() -> AssetId {
        "USDC@ethereum/6".parse().unwrap()
    }

    fn deposit(amount: u128) -> Transaction {
        let posting = Posting::new(
            AccountAddress::world(),
            "users:1:wallet".parse().unwrap(),
            amount,
            usdc(),
        )
        .unwrap();
        Transaction::new(vec![posting], Metadata::new()).unwrap()
    }

    /// Exercises a store only through the port, as the API will.
    async fn deposit_and_read(store: &impl LedgerStore) -> (TxId, Balance) {
        let id = store.apply(deposit(100)).await.unwrap();
        let balance = store
            .balance(&"users:1:wallet".parse().unwrap(), &usdc())
            .await
            .unwrap();
        (id, balance)
    }

    #[tokio::test]
    async fn in_memory_journal_implements_the_store_port() {
        let store = Mutex::new(Journal::default());
        let (id, balance) = deposit_and_read(&store).await;
        assert_eq!(id, TxId(1));
        assert_eq!(balance, Balance::ZERO.credit(100).unwrap());
    }

    #[tokio::test]
    async fn rule_violations_surface_as_apply_errors() {
        let store = Mutex::new(Journal::default());
        let spend = Posting::new(
            "users:1:wallet".parse().unwrap(),
            "users:2:wallet".parse().unwrap(),
            1,
            usdc(),
        )
        .unwrap();
        let result = store
            .apply(Transaction::new(vec![spend], Metadata::new()).unwrap())
            .await;
        assert!(matches!(
            result,
            Err(StoreError::Apply(ApplyError::InsufficientFunds { .. }))
        ));
    }
}
