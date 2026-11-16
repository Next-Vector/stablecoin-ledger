//! The `allow-negative` / `disallow-negative` operator commands (§4.3 rule 2): let a system
//! account such as a gas expense account go negative, against the run-integration database.

use std::process::{Command, Output};

use sqlx::PgPool;

fn ledger_api(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ledger-api"))
        .args(args)
        .output()
        .expect("run the ledger-api binary")
}

async fn allowed(pool: &PgPool, address: &str) -> Option<bool> {
    sqlx::query_as::<_, (bool,)>("SELECT allow_negative FROM accounts WHERE address = $1")
        .bind(address)
        .fetch_optional(pool)
        .await
        .unwrap()
        .map(|(allowed,)| allowed)
}

#[tokio::test]
async fn the_flag_is_set_and_cleared_by_an_operator() {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must point at the run-integration database");
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .execute(&pool)
        .await
        .unwrap();
    ledger_store::migrate(&pool).await.unwrap();

    let on = ledger_api(&["allow-negative", "platform:gas:ethereum"]);
    assert!(
        on.status.success(),
        "{}",
        String::from_utf8_lossy(&on.stderr)
    );
    assert_eq!(allowed(&pool, "platform:gas:ethereum").await, Some(true));

    let off = ledger_api(&["disallow-negative", "platform:gas:ethereum"]);
    assert!(
        off.status.success(),
        "{}",
        String::from_utf8_lossy(&off.stderr)
    );
    assert_eq!(allowed(&pool, "platform:gas:ethereum").await, Some(false));
}

#[test]
fn invalid_arguments_print_usage() {
    for args in [
        &["allow-negative"][..],
        &["allow-negative", "not an account"],
        &["disallow-negative", "a", "b"],
    ] {
        let output = ledger_api(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("usage: ledger-api"),
            "{args:?}"
        );
    }
}
