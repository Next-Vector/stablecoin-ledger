//! The `create-api-key` / `revoke-api-key` operator commands against the run-integration database.

use std::process::{Command, Output};

use ledger_store::{AuthFailure, Role, authenticate};
use sqlx::PgPool;

fn ledger_api(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ledger-api"))
        .args(args)
        .output()
        .expect("run the ledger-api binary")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[tokio::test]
async fn created_keys_authenticate_until_revoked() {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must point at the run-integration database");
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .execute(&pool)
        .await
        .unwrap();
    ledger_store::migrate(&pool).await.unwrap();

    let created = ledger_api(&[
        "create-api-key",
        "--role",
        "write",
        "--name",
        "merchant-backend",
    ]);
    assert!(
        created.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let text = stdout(&created);
    let key = text
        .split_whitespace()
        .find(|word| word.starts_with("sl_"))
        .expect("the key is printed")
        .to_owned();
    let auth = authenticate(&pool, &key).await.unwrap().unwrap();
    assert_eq!(auth.role, Role::Write);
    assert!(text.contains(&auth.key_id));

    let revoked = ledger_api(&["revoke-api-key", &auth.key_id]);
    assert!(revoked.status.success());
    assert!(stdout(&revoked).contains("revoked"));
    assert_eq!(
        authenticate(&pool, &key).await.unwrap(),
        Err(AuthFailure::Revoked(auth.key_id.clone()))
    );

    let again = ledger_api(&["revoke-api-key", &auth.key_id]);
    assert_eq!(
        again.status.code(),
        Some(1),
        "revoking an unknown or revoked key fails"
    );
}

#[test]
fn invalid_arguments_print_usage() {
    for args in [
        &["create-api-key"][..],
        &["create-api-key", "--role", "root", "--name", "x"],
        &["revoke-api-key"],
    ] {
        let output = ledger_api(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("usage: ledger-api"),
            "{args:?}"
        );
    }
}
