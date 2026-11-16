//! The transactional outbox (FR-51, FR-52, FR-55): every change writes its webhook event in the
//! same DB transaction, with a unique id and a sequence per resource.

mod common;

use ledger_core::{AccountAddress, AssetId, LedgerStore, Metadata, Posting, Transaction};
use ledger_store::PgStore;
use sqlx::PgPool;

fn account(raw: &str) -> AccountAddress {
    raw.parse().unwrap()
}

fn usdc() -> AssetId {
    "USDC@ethereum/6".parse().unwrap()
}

fn tx(postings: &[(&str, &str, u128)], metadata: Metadata) -> Transaction {
    let postings = postings
        .iter()
        .map(|&(from, to, amount)| {
            Posting::new(account(from), account(to), amount, usdc()).unwrap()
        })
        .collect();
    Transaction::new(postings, metadata).unwrap()
}

#[derive(Debug)]
struct Event {
    id: String,
    kind: String,
    resource: String,
    sequence: i64,
    data: serde_json::Value,
}

async fn events(pool: &PgPool) -> Vec<Event> {
    let rows: Vec<(String, String, String, i64, String)> = sqlx::query_as(
        "SELECT id, type, resource, sequence, data::text FROM webhook_events ORDER BY created_at, resource, sequence",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    rows.into_iter()
        .map(|(id, kind, resource, sequence, data)| Event {
            id,
            kind,
            resource,
            sequence,
            data: serde_json::from_str(&data).unwrap(),
        })
        .collect()
}

async fn store() -> (PgStore, PgPool) {
    let pool = common::fresh_database().await;
    (PgStore::new(pool.clone()), pool)
}

#[tokio::test]
async fn a_transaction_writes_its_created_event_atomically() {
    let (store, pool) = store().await;
    let metadata = Metadata::from([("order_id".to_owned(), "8812".to_owned())]);
    let id = store
        .apply(tx(
            &[
                ("world", "users:1:wallet", 100),
                ("users:1:wallet", "platform:fees", 1),
            ],
            metadata,
        ))
        .await
        .unwrap();

    // A fresh connection sees it: the event is durable with the commit, not held in the process.
    let other = PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
        .await
        .unwrap();
    let events = events(&other).await;
    assert_eq!(events.len(), 1, "{events:?}");
    let event = &events[0];
    assert!(
        event.id.starts_with("evt_") && event.id.len() == 4 + 32,
        "{}",
        event.id
    );
    assert_eq!(event.kind, "transaction.created");
    assert_eq!(event.resource, format!("transaction:{}", id.0));
    assert_eq!(event.sequence, 1);
    assert_eq!(event.data["transaction_id"], id.0);
    assert_eq!(event.data["postings"][0]["source"], "world");
    assert_eq!(event.data["postings"][0]["amount"], "100");
    assert_eq!(event.data["postings"][1]["destination"], "platform:fees");
    assert_eq!(event.data["postings"][1]["asset"], "USDC@ethereum/6");
    assert_eq!(event.data["metadata"]["order_id"], "8812");
    assert_eq!(event.data["reverts"], serde_json::Value::Null);
    drop(pool);
}

#[tokio::test]
async fn a_rejected_change_writes_no_event() {
    let (store, pool) = store().await;
    assert!(
        store
            .apply(tx(
                &[("users:1:wallet", "users:2:wallet", 1)],
                Metadata::new()
            ))
            .await
            .is_err()
    );
    assert!(events(&pool).await.is_empty());
}

#[tokio::test]
async fn a_reversal_is_created_and_marks_the_original_reverted_in_order() {
    let (store, pool) = store().await;
    store
        .apply(tx(&[("world", "users:1:wallet", 100)], Metadata::new()))
        .await
        .unwrap();
    let spend = store
        .apply(tx(&[("users:1:wallet", "shop:1", 30)], Metadata::new()))
        .await
        .unwrap();
    let reversal = store.revert(spend, Metadata::new(), None).await.unwrap();

    let events = events(&pool).await;
    let on_spend: Vec<(&str, i64)> = events
        .iter()
        .filter(|e| e.resource == format!("transaction:{}", spend.0))
        .map(|e| (e.kind.as_str(), e.sequence))
        .collect();
    assert_eq!(
        on_spend,
        [("transaction.created", 1), ("transaction.reverted", 2)]
    );
    let reverted = events
        .iter()
        .find(|e| e.kind == "transaction.reverted")
        .unwrap();
    assert_eq!(reverted.data["transaction_id"], spend.0);
    assert_eq!(reverted.data["reversal_id"], reversal.0);
    let created = events
        .iter()
        .find(|e| e.resource == format!("transaction:{}", reversal.0))
        .unwrap();
    assert_eq!(created.kind, "transaction.created");
    assert_eq!(created.data["reverts"], spend.0);
}

#[tokio::test]
async fn holds_and_scripts_emit_through_the_journal() {
    let (store, pool) = store().await;
    store
        .apply(tx(&[("world", "users:1:wallet", 100)], Metadata::new()))
        .await
        .unwrap();
    let (hold, _) = store
        .hold(
            account("users:1:wallet"),
            30,
            usdc(),
            None,
            Metadata::new(),
            None,
        )
        .await
        .unwrap();
    store
        .capture(hold.id(), account("shop:1"), 20, Metadata::new(), None)
        .await
        .unwrap();
    let script = ledger_script::parse(
        "send [USDC@ethereum/6 5] (source = @users:1:wallet destination = @shop:2)",
    )
    .unwrap();
    store
        .run_script(&script, &ledger_script::Vars::new(), Metadata::new(), None)
        .await
        .unwrap();
    let kinds: Vec<String> = events(&pool).await.into_iter().map(|e| e.kind).collect();
    assert_eq!(kinds, vec!["transaction.created"; 4]);
}

#[tokio::test]
async fn freezing_emits_only_when_the_state_changes() {
    let (store, pool) = store().await;
    let wallet = account("users:1:wallet");
    store.set_frozen(&wallet, true, None).await.unwrap();
    store.set_frozen(&wallet, true, None).await.unwrap();
    store.set_frozen(&wallet, false, None).await.unwrap();
    store.set_frozen(&wallet, false, None).await.unwrap();
    store
        .set_frozen(&account("users:2:wallet"), false, None)
        .await
        .unwrap();

    let events = events(&pool).await;
    let seen: Vec<(&str, &str, i64)> = events
        .iter()
        .map(|e| (e.kind.as_str(), e.resource.as_str(), e.sequence))
        .collect();
    assert_eq!(
        seen,
        [
            ("account.frozen", "account:users:1:wallet", 1),
            ("account.unfrozen", "account:users:1:wallet", 2),
        ]
    );
    assert_eq!(events[0].data["account"], "users:1:wallet");
    assert_ne!(events[0].id, events[1].id);
}

#[tokio::test]
async fn concurrent_changes_to_one_resource_get_gapless_sequences() {
    let (store, pool) = store().await;
    let toggles: Vec<_> = (0..20)
        .map(|i| {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .set_frozen(&account("users:1:wallet"), i % 2 == 0, None)
                    .await
            })
        })
        .collect();
    for toggle in toggles {
        toggle.await.unwrap().unwrap();
    }
    let mut sequences: Vec<i64> = events(&pool).await.iter().map(|e| e.sequence).collect();
    sequences.sort_unstable();
    let expected: Vec<i64> = (1..=sequences.len() as i64).collect();
    assert_eq!(sequences, expected);
    assert!(!sequences.is_empty());
}

#[tokio::test]
async fn concurrent_first_freezes_of_a_new_account_emit_once() {
    let (store, pool) = store().await;
    let freezes: Vec<_> = (0..10)
        .map(|_| {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .set_frozen(&account("users:9:wallet"), true, None)
                    .await
            })
        })
        .collect();
    for freeze in freezes {
        freeze.await.unwrap().unwrap();
    }
    let kinds: Vec<String> = events(&pool).await.into_iter().map(|e| e.kind).collect();
    assert_eq!(kinds, ["account.frozen"]);
}
