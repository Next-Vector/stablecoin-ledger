//! Ledger API binary.
//!
//!   ledger-api [serve]                                   run the HTTP API (default)
//!   ledger-api migrate                                   apply pending database migrations
//!   ledger-api rebuild-balances                          recompute balances from the journal (NFR-08)
//!   ledger-api create-api-key --role <role> --name <n>   issue an API key (read | write | admin)
//!   ledger-api revoke-api-key <key_id>                   revoke an API key
//!
//! Configuration comes from the environment and is validated before anything starts.

use std::process::ExitCode;

use ledger_api::config::{Config, LogFormat};
use ledger_api::dispatcher::{Dispatcher, DispatcherConfig};
use ledger_api::expiry::HoldSweeper;
use ledger_api::{AppState, app};
use ledger_store::Role;
use sqlx::PgPool;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: ledger-api [serve | migrate | rebuild-balances | \
create-api-key --role <read|write|admin> --name <name> | revoke-api-key <key_id>]";

type Failure = Box<dyn std::error::Error>;

#[derive(Debug)]
enum Command {
    Serve,
    Migrate,
    RebuildBalances,
    CreateApiKey { role: Role, name: String },
    RevokeApiKey { key_id: String },
}

impl Command {
    fn parse(args: &[String]) -> Option<Self> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match args.as_slice() {
            [] | ["serve"] => Some(Self::Serve),
            ["migrate"] => Some(Self::Migrate),
            ["rebuild-balances"] => Some(Self::RebuildBalances),
            ["revoke-api-key", key_id] => Some(Self::RevokeApiKey {
                key_id: (*key_id).to_owned(),
            }),
            ["create-api-key", flags @ ..] => {
                let (mut role, mut name) = (None, None);
                for pair in flags.chunks(2) {
                    match pair {
                        ["--role", value] => role = Some(value.parse().ok()?),
                        ["--name", value] => name = Some((*value).to_owned()),
                        _ => return None,
                    }
                }
                Some(Self::CreateApiKey {
                    role: role?,
                    name: name?,
                })
            }
            _ => None,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Serve => "serve",
            Self::Migrate => "migrate",
            Self::RebuildBalances => "rebuild-balances",
            Self::CreateApiKey { .. } => "create-api-key",
            Self::RevokeApiKey { .. } => "revoke-api-key",
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = Command::parse(&args) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    init_tracing(config.log_format);

    let name = command.name();
    let result = match command {
        Command::Serve => serve(&config).await,
        Command::Migrate => migrate(&config).await,
        Command::RebuildBalances => rebuild_balances(&config).await,
        Command::CreateApiKey { role, name } => create_api_key(&config, role, &name).await,
        Command::RevokeApiKey { key_id } => revoke_api_key(&config, &key_id).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, command = name, "command failed");
            eprintln!("{name} failed: {error}");
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
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let dispatcher = Dispatcher::new(
        pool.clone(),
        config.webhook_secrets.clone(),
        DispatcherConfig {
            url_policy: config.webhook_url_policy,
            ..DispatcherConfig::default()
        },
    )?;
    let dispatching = tokio::spawn(dispatcher.run(stopped.clone()));
    let sweeping =
        tokio::spawn(HoldSweeper::new(pool.clone(), config.hold_expiry_interval).run(stopped));
    tracing::info!(addr = %config.bind_addr, "listening");
    axum::serve(
        listener,
        app(AppState::new(pool.clone(), config.webhook_secrets.clone())
            .with_url_policy(config.webhook_url_policy)),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        stop.send(true).ok();
    })
    .await?;
    // In-flight requests and the current dispatch and expiry rounds have finished; release the
    // connections.
    dispatching.await.ok();
    sweeping.await.ok();
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

async fn create_api_key(config: &Config, role: Role, name: &str) -> Result<(), Failure> {
    let pool = PgPool::connect(&config.database_url).await?;
    let key = ledger_store::create_api_key(&pool, name, role).await?;
    tracing::info!(key_id = %key.key_id, role = role.as_str(), name, "API key created");
    println!("created {} key {} ({name})", role.as_str(), key.key_id);
    println!("{}", key.plaintext);
    println!("store it now: the key cannot be shown again");
    Ok(())
}

async fn revoke_api_key(config: &Config, key_id: &str) -> Result<(), Failure> {
    let pool = PgPool::connect(&config.database_url).await?;
    if !ledger_store::revoke_api_key(&pool, key_id).await? {
        return Err(format!("no active API key {key_id}").into());
    }
    tracing::info!(key_id, "API key revoked");
    println!("revoked API key {key_id}");
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
