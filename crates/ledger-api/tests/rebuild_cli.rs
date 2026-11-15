//! The `ledger-api rebuild-balances` maintenance command against the run-integration database.

use std::process::Command;

use sqlx::PgPool;

fn ledger_api(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ledger-api"))
        .args(args)
        .output()
        .expect("run the ledger-api binary")
}

#[tokio::test]
async fn rebuild_balances_command_repairs_the_balances_table() {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must point at the run-integration database");
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .execute(&pool)
        .await
        .unwrap();
    ledger_store::migrate(&pool).await.unwrap();
    sqlx::raw_sql(
        "INSERT INTO transactions DEFAULT VALUES; \
         INSERT INTO postings (tx_id, idx, source, destination, asset, amount) \
           SELECT max(id), 0, 'world', 'users:1:wallet', 'USDC@ethereum/6', 100 FROM transactions; \
         INSERT INTO balances (account, asset, balance) VALUES ('users:1:wallet', 'USDC@ethereum/6', 7);",
    )
    .execute(&pool)
    .await
    .unwrap();

    let output = ledger_api(&["rebuild-balances"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("corrected 2 balance rows"),
        "stdout: {stdout}"
    );

    let (balance,): (String,) =
        sqlx::query_as("SELECT balance::text FROM balances WHERE account = 'users:1:wallet'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(balance, "100");
}

#[test]
fn unknown_command_prints_usage_and_fails() {
    let output = ledger_api(&["no-such-command"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage: ledger-api"));
}
