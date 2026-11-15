//! REST API and webhook dispatcher for the ledger.
//!
//! Maintenance commands:
//!   ledger-api rebuild-balances   recompute the balances table from the posting journal (NFR-08)

use std::process::ExitCode;

use sqlx::PgPool;

const USAGE: &str = "usage: ledger-api rebuild-balances";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["rebuild-balances"] => match rebuild_balances().await {
            Ok(corrected) => {
                println!("rebuild-balances: corrected {corrected} balance rows");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("rebuild-balances failed: {error}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

async fn rebuild_balances() -> Result<u64, Box<dyn std::error::Error>> {
    let url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set")?;
    let pool = PgPool::connect(&url).await?;
    Ok(ledger_store::rebuild_balances(&pool).await?)
}
