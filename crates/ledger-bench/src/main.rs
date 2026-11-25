//! ledger-bench: seeds `bench:{i}:wallet` accounts, then measures transfer throughput at full
//! speed and, with `--rate`, latency at a fixed request rate (NFR-04, NFR-05).

use std::process::ExitCode;

use ledger_bench::{Config, USAGE, run, seed};

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = match Config::from_args(&args) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if let Err(error) = measure(&config).await {
        eprintln!("ledger-bench failed: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn measure(config: &Config) -> Result<(), String> {
    println!("seeding {} wallets…", config.accounts);
    seed(config).await?;
    println!(
        "throughput, {} in flight, {} s:",
        config.concurrency,
        config.duration.as_secs()
    );
    println!("  {}", run(config, None).await?);
    if let Some(rate) = config.rate {
        println!(
            "latency at {rate} requests/s, {} s:",
            config.duration.as_secs()
        );
        println!("  {}", run(config, Some(rate)).await?);
    }
    Ok(())
}
