//! Acceptance criterion (spec §10, NFR-01): 1,000 parallel debits of 1 USDC from an account holding
//! 500 USDC yield exactly 500 successes and 500 INSUFFICIENT_FUNDS, and the balance never goes negative.

mod common;

use ledger_core::{
    AccountAddress, ApplyError, AssetId, Balance, LedgerStore, Metadata, Posting, StoreError,
    Transaction,
};
use ledger_store::PgStore;
use sqlx::postgres::PgPoolOptions;

const ONE_USDC: u128 = 1_000_000;
const FUNDED_USDC: u128 = 500;
const DEBITS: usize = 1_000;
/// Enough connections for real contention on the payer's balance row.
const CONNECTIONS: u32 = 50;

fn usdc() -> AssetId {
    "USDC@ethereum/6".parse().unwrap()
}

fn transfer(from: &str, to: &str, amount: u128) -> Transaction {
    let posting = Posting::new(from.parse().unwrap(), to.parse().unwrap(), amount, usdc()).unwrap();
    Transaction::new(vec![posting], Metadata::new()).unwrap()
}

#[tokio::test]
async fn thousand_parallel_debits_never_overdraw() {
    common::fresh_database().await;
    let pool = PgPoolOptions::new()
        .max_connections(CONNECTIONS)
        .connect(&std::env::var("DATABASE_URL").unwrap())
        .await
        .unwrap();
    let store = PgStore::new(pool.clone());
    store
        .apply(transfer("world", "users:1:wallet", FUNDED_USDC * ONE_USDC))
        .await
        .unwrap();

    let tasks: Vec<_> = (0..DEBITS)
        .map(|i| {
            let store = store.clone();
            let merchant = format!("merchants:{}:wallet", i % 10);
            tokio::spawn(async move {
                store
                    .apply(transfer("users:1:wallet", &merchant, ONE_USDC))
                    .await
            })
        })
        .collect();

    let (mut succeeded, mut insufficient) = (0, 0);
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => succeeded += 1,
            Err(StoreError::Apply(ApplyError::InsufficientFunds { .. })) => insufficient += 1,
            Err(other) => panic!("unexpected error under contention: {other}"),
        }
    }

    assert_eq!((succeeded, insufficient), (500, 500));
    let payer: AccountAddress = "users:1:wallet".parse().unwrap();
    assert_eq!(store.balance(&payer, &usdc()).await.unwrap(), Balance::ZERO);

    let mut received = Balance::ZERO;
    for m in 0..10 {
        let merchant: AccountAddress = format!("merchants:{m}:wallet").parse().unwrap();
        received = received
            .checked_add(store.balance(&merchant, &usdc()).await.unwrap())
            .unwrap();
    }
    assert_eq!(
        received,
        Balance::ZERO.credit(FUNDED_USDC * ONE_USDC).unwrap()
    );

    let (journal_debits,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM postings WHERE source = 'users:1:wallet'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(journal_debits, 500);
}
