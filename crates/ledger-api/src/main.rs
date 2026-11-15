//! Ledger API binary.
//!
//!   ledger-api [serve]            run the HTTP API (default)
//!   ledger-api migrate            apply pending database migrations
//!   ledger-api rebuild-balances   recompute the balances table from the posting journal (NFR-08)
//!
//! Configuration comes from the environment and is validated before anything starts.

use std::process::ExitCode;

use ledger_api::config::{Config, LogFormat};
use ledger_api::{AppState, app};
use sqlx::PgPool;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: ledger-api [serve | migrate | rebuild-balances]";

type Failure = Box<dyn std::error::Error>;

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match args.as_slice() {
        [] => "serve",
        [command] => command.as_str(),
        _ => "",
    };
    if !matches!(command, "serve" | "migrate" | "rebuild-balances") {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    init_tracing(config.log_format);

    let result = match command {
        "serve" => serve(&config).await,
        "migrate" => migrate(&config).await,
        _ => rebuild_balances(&config).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, command, "command failed");
            eprintln!("{command} failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match format {
        LogFormat::Json => builder.json().init(),
        LogFormat::Pretty => builder.init(),
    }
}

async fn serve(config: &Config) -> Result<(), Failure> {
    let pool = PgPool::connect(&config.database_url).await?;
    let listener = tokio::net::TcpListener::bind(config.bind_addr).await?;
    tracing::info!(addr = %config.bind_addr, "listening");
    axum::serve(listener, app(AppState::new(pool.clone())))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    // In-flight requests have finished; release the database connections.
    pool.close().await;
    tracing::info!("stopped");
    Ok(())
}

async fn migrate(config: &Config) -> Result<(), Failure> {
    let pool = PgPool::connect(&config.database_url).await?;
    ledger_store::migrate(&pool).await?;
    tracing::info!("migrations applied");
    println!("migrate: schema is up to date");
    Ok(())
}

async fn rebuild_balances(config: &Config) -> Result<(), Failure> {
    let pool = PgPool::connect(&config.database_url).await?;
    let corrected = ledger_store::rebuild_balances(&pool).await?;
    println!("rebuild-balances: corrected {corrected} balance rows");
    Ok(())
}

/// Resolves on Ctrl-C or SIGTERM (Kubernetes / Docker stop), starting a graceful shutdown.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received, draining requests");
}
