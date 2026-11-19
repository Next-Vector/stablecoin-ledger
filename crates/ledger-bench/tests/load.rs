//! A short run of the load generator against a real ledger-api on the run-integration database:
//! it seeds the wallets, measures both modes, and the journal agrees with what it reports.

use std::time::Duration;

use ledger_api::{AppState, app};
use ledger_bench::{Config, run, seed};
use ledger_store::{Role, SecretBox, create_api_key};
use sqlx::PgPool;

async fn server() -> (String, String, PgPool) {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must point at the run-integration database");
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .execute(&pool)
        .await
        .unwrap();
    ledger_store::migrate(&pool).await.unwrap();
    let key = create_api_key(&pool, "bench", Role::Write)
        .await
        .unwrap()
        .plaintext;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = app(AppState::new(pool.clone(), SecretBox::new([3; 32])));
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (base, key, pool)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seeds_runs_both_modes_and_matches_the_journal() {
    let (url, key, pool) = server().await;
    let config = Config {
        url,
        key,
        accounts: 20,
        concurrency: 4,
        duration: Duration::from_millis(700),
        rate: Some(50),
    };

    seed(&config).await.unwrap();
    let full = run(&config, None).await.unwrap();
    assert!(full.succeeded > 0, "{full:?}");
    assert_eq!(full.failed, 0, "{full:?}");
    assert!(full.throughput() > 0.0);
    assert!(full.latency.p99 >= full.latency.p50);

    let paced = run(&config, config.rate).await.unwrap();
    assert_eq!(paced.failed, 0, "{paced:?}");
    // About 50/s for 0.7 s; allow for timer slack.
    assert!((20..=45).contains(&paced.succeeded), "{paced:?}");

    // Seeding wrote one transaction, every successful transfer one more.
    let (transactions,): (i64,) = sqlx::query_as("SELECT count(*) FROM transactions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(transactions as u64, 1 + full.succeeded + paced.succeeded);
}
