//! The `register-asset` operator command (§4.2): adds an asset to the registry the ledger accounts
//! for, against the run-integration database.

use std::process::{Command, Output};

use sqlx::PgPool;

fn ledger_api(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ledger-api"))
        .args(args)
        .output()
        .expect("run the ledger-api binary")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[tokio::test]
async fn assets_are_registered_once() {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must point at the run-integration database");
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .execute(&pool)
        .await
        .unwrap();
    ledger_store::migrate(&pool).await.unwrap();

    for asset in ["USDC@sepolia/6", "USDT@tron/6"] {
        let output = ledger_api(&["register-asset", "--asset", asset]);
        assert!(output.status.success(), "{asset}: {}", stderr(&output));
    }
    let rows: Vec<(String, String, i16)> =
        sqlx::query_as("SELECT id, network, decimals FROM assets ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![
            ("USDC@sepolia/6".into(), "sepolia".into(), 6),
            ("USDT@tron/6".into(), "tron".into(), 6),
        ]
    );

    let again = ledger_api(&["register-asset", "--asset", "USDC@sepolia/6"]);
    assert_eq!(again.status.code(), Some(1), "{}", stderr(&again));
    assert!(
        stderr(&again).contains("already registered"),
        "{}",
        stderr(&again)
    );
}

#[test]
fn invalid_asset_arguments_print_usage() {
    for args in [
        &["register-asset"][..],
        &["register-asset", "--asset", "not-an-asset"],
        &[
            "register-asset",
            "--asset",
            "USDC@sepolia/6",
            "--chain-id",
            "1",
        ],
    ] {
        let output = ledger_api(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stderr(&output).contains("usage: ledger-api"), "{args:?}");
    }
}
