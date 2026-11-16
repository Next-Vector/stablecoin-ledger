//! Integration tests for the PgStore operations behind the REST API: single transaction lookup,
//! reversal (FR-03), account view and point-in-time balances (FR-04), aggregated balances (FR-05),
//! holds (FR-07) and freezing (FR-08).

mod common;

use ledger_core::{
    AccountAddress, ApplyError, AssetId, Balance, HoldError, HoldId, HoldStatus, LedgerStore,
    Metadata, Posting, RevertError, Transaction, TxId,
};
use ledger_store::{OpError, PgStore};
use sqlx::PgPool;

fn account(raw: &str) -> AccountAddress {
    raw.parse().unwrap()
}

fn usdc() -> AssetId {
    "USDC@ethereum/6".parse().unwrap()
}

fn dai() -> AssetId {
    "DAI@ethereum/18".parse().unwrap()
}

fn tx(postings: &[(&str, &str, u128)]) -> Transaction {
    let postings = postings
        .iter()
        .map(|&(from, to, amount)| {
            Posting::new(account(from), account(to), amount, usdc()).unwrap()
        })
        .collect();
    Transaction::new(postings, Metadata::new()).unwrap()
}

fn credit(value: u128) -> Balance {
    Balance::ZERO.credit(value).unwrap()
}

fn note(value: &str) -> Metadata {
    Metadata::from([("note".to_owned(), value.to_owned())])
}

async fn balance(store: &PgStore, raw: &str) -> Balance {
    store.balance(&account(raw), &usdc()).await.unwrap()
}

async fn funded() -> (PgStore, PgPool) {
    let pool = common::fresh_database().await;
    let store = PgStore::new(pool.clone());
    store
        .apply(tx(&[("world", "users:1:wallet", 100)]))
        .await
        .unwrap();
    (store, pool)
}

#[tokio::test]
async fn looks_up_a_single_transaction() {
    let (store, _) = funded().await;
    let found = store.transaction(TxId(1)).await.unwrap().unwrap();
    assert_eq!(found.id, TxId(1));
    assert_eq!(found.transaction, tx(&[("world", "users:1:wallet", 100)]));
    assert_eq!(found.reverts, None);
    assert_eq!(found.reverted_by, None);
    assert!(store.transaction(TxId(99)).await.unwrap().is_none());
}

#[tokio::test]
async fn reverts_a_transaction_once_with_mirrored_postings() {
    let (store, _) = funded().await;
    let spend = store
        .apply(tx(&[("users:1:wallet", "merchants:1:wallet", 30)]))
        .await
        .unwrap();

    let reversal = store.revert(spend, note("refund")).await.unwrap();

    assert_eq!(balance(&store, "users:1:wallet").await, credit(100));
    assert_eq!(balance(&store, "merchants:1:wallet").await, Balance::ZERO);
    let recorded = store.transaction(reversal).await.unwrap().unwrap();
    assert_eq!(recorded.reverts, Some(spend));
    assert_eq!(recorded.transaction.metadata(), &note("refund"));
    assert_eq!(
        recorded.transaction.postings()[0].source(),
        &account("merchants:1:wallet")
    );
    let original = store.transaction(spend).await.unwrap().unwrap();
    assert_eq!(original.reverted_by, Some(reversal));

    assert!(matches!(
        store.revert(spend, Metadata::new()).await,
        Err(OpError::Rejected(RevertError::AlreadyReverted(id))) if id == spend
    ));
    assert!(matches!(
        store.revert(TxId(99), Metadata::new()).await,
        Err(OpError::Rejected(RevertError::NotFound(TxId(99))))
    ));
}

#[tokio::test]
async fn a_reversal_follows_the_funds_rule() {
    let (store, _) = funded().await;
    let spend = store
        .apply(tx(&[("users:1:wallet", "merchants:1:wallet", 30)]))
        .await
        .unwrap();
    store
        .apply(tx(&[("merchants:1:wallet", "users:2:wallet", 30)]))
        .await
        .unwrap();

    assert!(matches!(
        store.revert(spend, Metadata::new()).await,
        Err(OpError::Rejected(RevertError::Apply(
            ApplyError::InsufficientFunds { .. }
        )))
    ));
    assert!(
        store
            .transaction(spend)
            .await
            .unwrap()
            .unwrap()
            .reverted_by
            .is_none()
    );
}

#[tokio::test]
async fn concurrent_reverts_of_one_transaction_succeed_once() {
    let (store, _) = funded().await;
    let spend = store
        .apply(tx(&[("users:1:wallet", "merchants:1:wallet", 30)]))
        .await
        .unwrap();
    let attempts: Vec<_> = (0..10)
        .map(|_| {
            let store = store.clone();
            tokio::spawn(async move { store.revert(spend, Metadata::new()).await })
        })
        .collect();
    let mut succeeded = 0;
    for attempt in attempts {
        match attempt.await.unwrap() {
            Ok(_) => succeeded += 1,
            Err(OpError::Rejected(RevertError::AlreadyReverted(_))) => {}
            Err(other) => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(succeeded, 1);
    assert_eq!(balance(&store, "users:1:wallet").await, credit(100));
}

#[tokio::test]
async fn shows_an_account_with_its_balances_now_and_at_a_past_transaction() {
    let (store, _) = funded().await;
    let dai_deposit = Transaction::new(
        vec![Posting::new(AccountAddress::world(), account("users:1:wallet"), 7, dai()).unwrap()],
        Metadata::new(),
    )
    .unwrap();
    let second = store.apply(dai_deposit).await.unwrap();
    store
        .apply(tx(&[("users:1:wallet", "users:2:wallet", 40)]))
        .await
        .unwrap();

    let now = store
        .account(&account("users:1:wallet"), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(now.address, account("users:1:wallet"));
    assert!(!now.frozen);
    assert_eq!(now.balances, vec![(dai(), credit(7)), (usdc(), credit(60))]);

    let past = store
        .account(&account("users:1:wallet"), Some(second))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        past.balances,
        vec![(dai(), credit(7)), (usdc(), credit(100))]
    );

    let before_dai = store
        .account(&account("users:1:wallet"), Some(TxId(1)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before_dai.balances, vec![(usdc(), credit(100))]);

    assert!(
        store
            .account(&account("users:404:wallet"), None)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn aggregates_balances_by_pattern() {
    let (store, _) = funded().await;
    store
        .apply(tx(&[
            ("world", "users:2:wallet", 5),
            ("world", "users:2:pending", 9),
        ]))
        .await
        .unwrap();

    let pattern = "users:*:wallet".parse().unwrap();
    assert_eq!(
        store.aggregate_balances(&pattern, None).await.unwrap(),
        vec![(usdc(), credit(105))]
    );
    assert_eq!(
        store
            .aggregate_balances(&pattern, Some(&dai()))
            .await
            .unwrap(),
        vec![]
    );
    let everything = "world".parse().unwrap();
    assert_eq!(
        store.aggregate_balances(&everything, None).await.unwrap(),
        vec![(usdc(), Balance::ZERO.debit(114).unwrap())]
    );
}

#[tokio::test]
async fn a_frozen_account_cannot_send_but_can_receive_until_unfrozen() {
    let (store, _) = funded().await;
    store
        .set_frozen(&account("users:1:wallet"), true)
        .await
        .unwrap();
    assert!(
        store
            .account(&account("users:1:wallet"), None)
            .await
            .unwrap()
            .unwrap()
            .frozen
    );

    assert!(matches!(
        store
            .apply(tx(&[("users:1:wallet", "users:2:wallet", 1)]))
            .await,
        Err(ledger_core::StoreError::Apply(ApplyError::AccountFrozen(_)))
    ));
    store
        .apply(tx(&[("world", "users:1:wallet", 1)]))
        .await
        .unwrap();

    store
        .set_frozen(&account("users:1:wallet"), false)
        .await
        .unwrap();
    store
        .apply(tx(&[("users:1:wallet", "users:2:wallet", 1)]))
        .await
        .unwrap();
}

#[tokio::test]
async fn an_account_frozen_before_any_posting_exists() {
    let (store, _) = funded().await;
    store
        .set_frozen(&account("users:9:wallet"), true)
        .await
        .unwrap();
    let frozen = store
        .account(&account("users:9:wallet"), None)
        .await
        .unwrap()
        .unwrap();
    assert!(frozen.frozen);
    assert!(frozen.balances.is_empty());
}

#[tokio::test]
async fn holds_reserve_then_capture_partially() {
    let (store, _) = funded().await;
    let (hold, _) = store
        .hold(account("users:1:wallet"), 30, usdc(), None, note("order"))
        .await
        .unwrap();
    assert_eq!(hold.id(), HoldId(1));
    assert_eq!(hold.status(), HoldStatus::Active);
    assert_eq!(balance(&store, "users:1:wallet").await, credit(70));
    assert_eq!(balance(&store, "holds:1").await, credit(30));

    let (captured, tx_id) = store
        .capture(
            hold.id(),
            account("merchants:1:wallet"),
            20,
            Metadata::new(),
        )
        .await
        .unwrap();
    assert_eq!(captured.status(), HoldStatus::Captured);
    assert_eq!(
        store
            .transaction(tx_id)
            .await
            .unwrap()
            .unwrap()
            .transaction
            .postings()
            .len(),
        2
    );
    assert_eq!(balance(&store, "merchants:1:wallet").await, credit(20));
    assert_eq!(balance(&store, "users:1:wallet").await, credit(80));
    assert_eq!(balance(&store, "holds:1").await, Balance::ZERO);

    assert!(matches!(
        store.void(hold.id(), Metadata::new()).await,
        Err(OpError::Rejected(HoldError::NotActive(
            _,
            HoldStatus::Captured
        )))
    ));
}

#[tokio::test]
async fn void_returns_everything_and_rules_are_enforced() {
    let (store, _) = funded().await;
    assert!(matches!(
        store
            .hold(
                account("users:1:wallet"),
                101,
                usdc(),
                None,
                Metadata::new()
            )
            .await,
        Err(OpError::Rejected(HoldError::Apply(
            ApplyError::InsufficientFunds { .. }
        )))
    ));
    assert!(matches!(
        store
            .hold(account("users:1:wallet"), 0, usdc(), None, Metadata::new())
            .await,
        Err(OpError::Rejected(HoldError::ZeroAmount))
    ));

    let (hold, _) = store
        .hold(account("users:1:wallet"), 30, usdc(), None, Metadata::new())
        .await
        .unwrap();
    assert!(matches!(
        store
            .capture(
                hold.id(),
                account("merchants:1:wallet"),
                31,
                Metadata::new()
            )
            .await,
        Err(OpError::Rejected(HoldError::ExceedsHold {
            requested: 31,
            held: 30
        }))
    ));
    let (voided, _) = store.void(hold.id(), Metadata::new()).await.unwrap();
    assert_eq!(voided.status(), HoldStatus::Voided);
    assert_eq!(balance(&store, "users:1:wallet").await, credit(100));

    assert!(matches!(
        store.void(HoldId(99), Metadata::new()).await,
        Err(OpError::Rejected(HoldError::NotFound(HoldId(99))))
    ));
}

#[tokio::test]
async fn a_hold_past_its_deadline_cannot_be_captured_but_can_be_voided() {
    let (store, _) = funded().await;
    let (hold, _) = store
        .hold(
            account("users:1:wallet"),
            30,
            usdc(),
            Some(1),
            Metadata::new(),
        )
        .await
        .unwrap();
    assert_eq!(hold.expires_at(), Some(1));
    assert!(matches!(
        store
            .capture(
                hold.id(),
                account("merchants:1:wallet"),
                30,
                Metadata::new()
            )
            .await,
        Err(OpError::Rejected(HoldError::NotActive(
            _,
            HoldStatus::Expired
        )))
    ));
    store.void(hold.id(), Metadata::new()).await.unwrap();
    assert_eq!(balance(&store, "users:1:wallet").await, credit(100));
}

#[tokio::test]
async fn concurrent_settlements_of_one_hold_succeed_once() {
    let (store, _) = funded().await;
    let (hold, _) = store
        .hold(account("users:1:wallet"), 30, usdc(), None, Metadata::new())
        .await
        .unwrap();
    let id = hold.id();
    let attempts: Vec<_> = (0..10)
        .map(|i| {
            let store = store.clone();
            tokio::spawn(async move {
                if i % 2 == 0 {
                    store.void(id, Metadata::new()).await
                } else {
                    store
                        .capture(id, account("merchants:1:wallet"), 30, Metadata::new())
                        .await
                }
            })
        })
        .collect();
    let mut succeeded = 0;
    for attempt in attempts {
        match attempt.await.unwrap() {
            Ok(_) => succeeded += 1,
            Err(OpError::Rejected(HoldError::NotActive(..))) => {}
            Err(other) => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(succeeded, 1);
    assert_eq!(balance(&store, "holds:1").await, Balance::ZERO);
}
