//! Automatic expiry of overdue holds (FR-07): a due hold is voided as `expired`, its funds go back
//! to the owner and `hold.expired` is emitted in the same DB transaction; holds that are not due
//! are left alone, and concurrent sweepers expire each hold exactly once.

mod common;

use std::collections::BTreeSet;

use ledger_core::{
    AccountAddress, AssetId, Balance, HoldId, HoldStatus, LedgerStore, Metadata, Posting,
    Transaction,
};
use ledger_store::PgStore;
use sqlx::PgPool;

/// 2100-01-01, far enough ahead never to be due.
const FUTURE: u64 = 4_102_444_800;
/// 2001-09-09, long past.
const PAST: u64 = 1_000_000_000;

fn account(raw: &str) -> AccountAddress {
    raw.parse().unwrap()
}

fn usdc() -> AssetId {
    "USDC@ethereum/6".parse().unwrap()
}

fn credit(value: u128) -> Balance {
    Balance::ZERO.credit(value).unwrap()
}

async fn balance(store: &PgStore, raw: &str) -> Balance {
    store.balance(&account(raw), &usdc()).await.unwrap()
}

async fn funded(amount: u128) -> (PgStore, PgPool) {
    let pool = common::fresh_database().await;
    let store = PgStore::new(pool.clone());
    let posting =
        Posting::new(account("world"), account("users:1:wallet"), amount, usdc()).unwrap();
    store
        .apply(Transaction::new(vec![posting], Metadata::new()).unwrap())
        .await
        .unwrap();
    (store, pool)
}

async fn hold(store: &PgStore, amount: u128, expires_at: Option<u64>) -> HoldId {
    let (hold, _) = store
        .hold(
            account("users:1:wallet"),
            amount,
            usdc(),
            expires_at,
            Metadata::new(),
            None,
        )
        .await
        .unwrap();
    hold.id()
}

async fn status(store: &PgStore, id: HoldId) -> HoldStatus {
    store.find_hold(id).await.unwrap().unwrap().status()
}

#[tokio::test]
async fn a_due_hold_is_expired_and_its_funds_return() {
    let (store, pool) = funded(100).await;
    let due = hold(&store, 30, Some(PAST)).await;
    assert_eq!(balance(&store, "users:1:wallet").await, credit(70));

    let expired = store.expire_due_holds(10).await.unwrap();

    assert_eq!(expired, vec![due]);
    assert_eq!(status(&store, due).await, HoldStatus::Expired);
    assert_eq!(balance(&store, "users:1:wallet").await, credit(100));
    assert_eq!(
        balance(&store, &format!("holds:{}", due.0)).await,
        Balance::ZERO
    );

    // The release is journaled like any transaction, and hold.expired names it.
    let (data,): (serde_json::Value,) = sqlx::query_as(
        "SELECT data FROM webhook_events WHERE type = 'hold.expired' AND resource = $1",
    )
    .bind(format!("hold:{}", due.0))
    .fetch_one(&pool)
    .await
    .unwrap();
    let (release,): (i64,) = sqlx::query_as("SELECT max(id) FROM transactions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        data,
        serde_json::json!({
            "hold_id": due.0,
            "account": "users:1:wallet",
            "asset": "USDC@ethereum/6",
            "amount": "30",
            "transaction_id": release,
        })
    );

    // Nothing is due any more; an expired hold is not expired again.
    assert!(store.expire_due_holds(10).await.unwrap().is_empty());
    assert_eq!(balance(&store, "users:1:wallet").await, credit(100));
}

#[tokio::test]
async fn holds_not_yet_due_are_untouched() {
    let (store, pool) = funded(100).await;
    let later = hold(&store, 10, Some(FUTURE)).await;
    let open_ended = hold(&store, 20, None).await;
    let voided = hold(&store, 30, Some(PAST)).await;
    store.void(voided, Metadata::new(), None).await.unwrap();

    assert!(store.expire_due_holds(10).await.unwrap().is_empty());

    assert_eq!(status(&store, later).await, HoldStatus::Active);
    assert_eq!(status(&store, open_ended).await, HoldStatus::Active);
    assert_eq!(status(&store, voided).await, HoldStatus::Voided);
    assert_eq!(balance(&store, "users:1:wallet").await, credit(70));
    let (events,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM webhook_events WHERE type = 'hold.expired'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(events, 0);
}

#[tokio::test]
async fn a_sweep_stops_at_its_limit() {
    let (store, _) = funded(100).await;
    for _ in 0..3 {
        hold(&store, 10, Some(PAST)).await;
    }

    assert_eq!(store.expire_due_holds(2).await.unwrap().len(), 2);
    assert_eq!(store.expire_due_holds(2).await.unwrap().len(), 1);
    assert_eq!(balance(&store, "users:1:wallet").await, credit(100));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sweepers_expire_each_hold_once() {
    let (store, pool) = funded(1_000).await;
    let mut due = BTreeSet::new();
    for _ in 0..20 {
        due.insert(hold(&store, 10, Some(PAST)).await);
    }

    let sweepers: Vec<_> = (0..4)
        .map(|_| {
            let store = store.clone();
            tokio::spawn(async move { store.expire_due_holds(20).await.unwrap() })
        })
        .collect();
    let mut expired = Vec::new();
    for sweeper in sweepers {
        expired.extend(sweeper.await.unwrap());
    }

    let unique: BTreeSet<HoldId> = expired.iter().copied().collect();
    assert_eq!(unique.len(), expired.len(), "a hold was expired twice");
    assert_eq!(unique, due);
    assert_eq!(balance(&store, "users:1:wallet").await, credit(1_000));
    let (events,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM webhook_events WHERE type = 'hold.expired'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(events, 20);
}
