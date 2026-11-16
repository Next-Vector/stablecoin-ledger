//! The hold sweeper (FR-07): voids overdue holds as `expired` on a fixed interval. Several
//! sweepers (replicas) can run at once; the store settles each hold exactly once.

use std::time::Duration;

use ledger_store::PgStore;
use sqlx::PgPool;
use tokio::sync::watch;

/// Holds expired per store call; a full batch is followed by the next one without waiting.
const BATCH: u32 = 100;

pub struct HoldSweeper {
    store: PgStore,
    interval: Duration,
}

impl HoldSweeper {
    pub fn new(pool: PgPool, interval: Duration) -> Self {
        Self {
            store: PgStore::new(pool),
            interval,
        }
    }

    /// Sweeps until `shutdown` turns true, pausing `interval` once nothing more is due.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        while !*shutdown.borrow() {
            let wait = match self.store.expire_due_holds(BATCH).await {
                Ok(expired) => {
                    if !expired.is_empty() {
                        tracing::info!(count = expired.len(), "expired overdue holds");
                    }
                    if expired.len() == BATCH as usize {
                        Duration::ZERO
                    } else {
                        self.interval
                    }
                }
                Err(error) => {
                    tracing::error!(%error, "hold expiry sweep failed");
                    self.interval
                }
            };
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = shutdown.changed() => {}
            }
        }
        tracing::info!("hold sweeper stopped");
    }
}
