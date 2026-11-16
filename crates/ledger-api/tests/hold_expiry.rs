//! The hold sweeper that runs inside ledger-api (FR-07): it expires due holds on its interval and
//! stops on shutdown, with the run-integration database.

use std::time::Duration;

use ledger_api::expiry::HoldSweeper;
use ledger_core::{
    AccountAddress, AssetId, HoldStatus, LedgerStore, Metadata, Posting, Transaction,
};
use ledger_store::PgStore;
use sqlx::PgPool;

async fn store() -> (PgStore, PgPool) {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must point at the run-integration database");
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .execute(&pool)
        .await
        .unwrap();
    ledger_store::migrate(&pool).await.unwrap();
    (PgStore::new(pool.clone()), pool)
}

fn wallet() -> AccountAddress {
    "users:1:wallet".parse().unwrap()
}

fn usdc() -> AssetId {
    "USDC@ethereum/6".parse().unwrap()
}

#[tokio::test]
async fn the_sweeper_expires_due_holds_until_shutdown() {
    let (store, pool) = store().await;
    let posting = Posting::new(AccountAddress::world(), wallet(), 100, usdc()).unwrap();
    store
        .apply(Transaction::new(vec![posting], Metadata::new()).unwrap())
        .await
        .unwrap();
    let (due, _) = store
        .hold(
            wallet(),
            40,
            usdc(),
            Some(1_000_000_000),
            Metadata::new(),
            None,
        )
        .await
        .unwrap();

    let (stop, stopped) = tokio::sync::watch::channel(false);
    let sweeping = tokio::spawn(HoldSweeper::new(pool, Duration::from_millis(20)).run(stopped));

    let expired = async {
        while store.find_hold(due.id()).await.unwrap().unwrap().status() != HoldStatus::Expired {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), expired)
        .await
        .expect("the sweeper expires the due hold");
    assert_eq!(
        store.balance(&wallet(), &usdc()).await.unwrap(),
        ledger_core::Balance::ZERO.credit(100).unwrap()
    );

    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), sweeping)
        .await
        .expect("the sweeper stops on shutdown")
        .unwrap();
}
