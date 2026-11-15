//! FR-06: transaction history with filters and cursor pagination.

mod common;

use ledger_core::{LedgerStore, Metadata, Posting, Transaction, TxId};
use ledger_store::{HistoryFilter, Page, PgStore};
use sqlx::PgPool;

fn tx(postings: &[(&str, &str, u128, &str)], metadata: &[(&str, &str)]) -> Transaction {
    let postings = postings
        .iter()
        .map(|&(from, to, amount, asset)| {
            Posting::new(
                from.parse().unwrap(),
                to.parse().unwrap(),
                amount,
                asset.parse().unwrap(),
            )
            .unwrap()
        })
        .collect();
    let metadata: Metadata = metadata
        .iter()
        .map(|&(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    Transaction::new(postings, metadata).unwrap()
}

const USDC: &str = "USDC@ethereum/6";
const DAI: &str = "DAI@ethereum/18";

/// 1: fund users:1 (USDC), 2: fund users:2 (DAI), 3: users:1 pays merchants:7, 4: users:2 pays users:1 (DAI).
async fn seeded() -> (PgStore, PgPool) {
    let pool = common::fresh_database().await;
    let store = PgStore::new(pool.clone());
    for t in [
        tx(
            &[("world", "users:1:wallet", 100, USDC)],
            &[("kind", "deposit")],
        ),
        tx(
            &[("world", "users:2:wallet", 50, DAI)],
            &[("kind", "deposit")],
        ),
        tx(
            &[
                ("users:1:wallet", "merchants:7:wallet", 99, USDC),
                ("users:1:wallet", "platform:fees", 1, USDC),
            ],
            &[("order_id", "8812")],
        ),
        tx(&[("users:2:wallet", "users:1:wallet", 5, DAI)], &[]),
    ] {
        store.apply(t).await.unwrap();
    }
    (store, pool)
}

fn ids(page: &ledger_store::HistoryPage) -> Vec<u64> {
    page.items.iter().map(|item| item.id.0).collect()
}

fn account(raw: &str) -> HistoryFilter {
    HistoryFilter {
        account: Some(raw.parse().unwrap()),
        ..HistoryFilter::default()
    }
}

#[tokio::test]
async fn lists_newest_first_with_postings_and_metadata() {
    let (store, _) = seeded().await;
    let page = store
        .transactions(&HistoryFilter::default(), &Page::default())
        .await
        .unwrap();

    assert_eq!(ids(&page), vec![4, 3, 2, 1]);
    assert_eq!(page.next_cursor, None);
    let payment = &page.items[1];
    assert_eq!(payment.transaction.postings().len(), 2);
    assert_eq!(
        payment.transaction.postings()[0].destination().as_str(),
        "merchants:7:wallet"
    );
    assert_eq!(payment.transaction.postings()[1].amount(), 1);
    assert_eq!(
        payment
            .transaction
            .metadata()
            .get("order_id")
            .map(String::as_str),
        Some("8812")
    );
    assert_eq!(payment.reverts, None);
    assert!(
        payment.created_at > 1_700_000_000,
        "created_at is Unix seconds"
    );
}

#[tokio::test]
async fn cursor_pagination_walks_every_transaction_once() {
    let (store, _) = seeded().await;
    let mut seen = Vec::new();
    let mut before = None;
    loop {
        let page = store
            .transactions(&HistoryFilter::default(), &Page::new(3, before).unwrap())
            .await
            .unwrap();
        seen.extend(ids(&page));
        match page.next_cursor {
            Some(cursor) => before = Some(cursor),
            None => break,
        }
    }
    assert_eq!(seen, vec![4, 3, 2, 1]);

    let first = store
        .transactions(&HistoryFilter::default(), &Page::new(2, None).unwrap())
        .await
        .unwrap();
    assert_eq!(
        (ids(&first), first.next_cursor),
        (vec![4, 3], Some(TxId(3)))
    );
    let exact = store
        .transactions(&HistoryFilter::default(), &Page::new(4, None).unwrap())
        .await
        .unwrap();
    assert_eq!(
        exact.next_cursor, None,
        "no cursor when the last page is exactly full"
    );
}

#[tokio::test]
async fn filters_by_account_as_source_or_destination() {
    let (store, _) = seeded().await;
    let page = store
        .transactions(&account("users:1:wallet"), &Page::default())
        .await
        .unwrap();
    assert_eq!(ids(&page), vec![4, 3, 1]);
    let page = store
        .transactions(&account("merchants:7:wallet"), &Page::default())
        .await
        .unwrap();
    assert_eq!(ids(&page), vec![3]);
    let page = store
        .transactions(&account("users:9:wallet"), &Page::default())
        .await
        .unwrap();
    assert!(page.items.is_empty());
}

#[tokio::test]
async fn filters_by_asset_and_combines_filters() {
    let (store, _) = seeded().await;
    let dai = HistoryFilter {
        asset: Some(DAI.parse().unwrap()),
        ..HistoryFilter::default()
    };
    assert_eq!(
        ids(&store.transactions(&dai, &Page::default()).await.unwrap()),
        vec![4, 2]
    );

    let users1_dai = HistoryFilter {
        asset: Some(DAI.parse().unwrap()),
        ..account("users:1:wallet")
    };
    assert_eq!(
        ids(&store
            .transactions(&users1_dai, &Page::default())
            .await
            .unwrap()),
        vec![4]
    );
}

#[tokio::test]
async fn filters_by_metadata_pair() {
    let (store, _) = seeded().await;
    let deposits = HistoryFilter {
        metadata: Some(("kind".into(), "deposit".into())),
        ..HistoryFilter::default()
    };
    assert_eq!(
        ids(&store
            .transactions(&deposits, &Page::default())
            .await
            .unwrap()),
        vec![2, 1]
    );
    let order = HistoryFilter {
        metadata: Some(("order_id".into(), "8812".into())),
        ..account("users:1:wallet")
    };
    assert_eq!(
        ids(&store.transactions(&order, &Page::default()).await.unwrap()),
        vec![3]
    );
}

#[tokio::test]
async fn filters_by_period_and_reports_reversals() {
    let pool = common::fresh_database().await;
    let store = PgStore::new(pool.clone());
    // Journal rows with fixed timestamps (the table is append-only, so they are written as-is).
    sqlx::raw_sql(
        "INSERT INTO transactions (id, created_at) VALUES \
           (1, to_timestamp(1000)), (2, to_timestamp(2000)), (3, to_timestamp(3000)); \
         INSERT INTO transactions (id, reverts, created_at) VALUES (4, 3, to_timestamp(4000)); \
         INSERT INTO postings (tx_id, idx, source, destination, asset, amount) VALUES \
           (1, 0, 'world', 'users:1:wallet', 'USDC@ethereum/6', 10), \
           (2, 0, 'world', 'users:1:wallet', 'USDC@ethereum/6', 20), \
           (3, 0, 'users:1:wallet', 'users:2:wallet', 'USDC@ethereum/6', 5), \
           (4, 0, 'users:2:wallet', 'users:1:wallet', 'USDC@ethereum/6', 5);",
    )
    .execute(&pool)
    .await
    .unwrap();

    let period = HistoryFilter {
        from: Some(2000),
        to: Some(3999),
        ..HistoryFilter::default()
    };
    let page = store.transactions(&period, &Page::default()).await.unwrap();
    assert_eq!(ids(&page), vec![3, 2], "from is inclusive, to is exclusive");
    assert_eq!(page.items[0].created_at, 3000);

    let all = store
        .transactions(&HistoryFilter::default(), &Page::default())
        .await
        .unwrap();
    assert_eq!(all.items[0].reverts, Some(TxId(3)));
}

#[test]
fn page_size_is_bounded() {
    assert!(Page::new(1, None).is_ok());
    assert!(Page::new(Page::MAX_LIMIT, None).is_ok());
    assert!(Page::new(0, None).is_err());
    assert!(Page::new(Page::MAX_LIMIT + 1, None).is_err());
    assert_eq!(Page::default().limit(), Page::DEFAULT_LIMIT);
}
