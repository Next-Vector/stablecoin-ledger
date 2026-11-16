//! PostgreSQL storage for the ledger journal and materialized balances.

mod accounts;
mod api_keys;
mod history;
mod holds;

pub use accounts::Account;
pub use api_keys::{
    ApiKeyError, AuthFailure, Authenticated, GeneratedKey, Role, authenticate, create_api_key,
    revoke_api_key,
};
pub use history::{HistoryFilter, HistoryPage, InvalidPageSize, Page, RecordedTransaction};

use std::collections::{BTreeSet, HashMap, HashSet};

use ledger_core::{
    AccountAddress, ApplyError, AssetId, Balance, LedgerStore, Metadata, Posting, RevertError,
    StoreError, Transaction, TxId, plan,
};
use sqlx::migrate::MigrateError;
use sqlx::{PgConnection, PgPool};

/// The outcome of an operation a ledger rule can reject: `Rejected` carries the rule's error
/// (e.g. [`RevertError`], [`ledger_core::HoldError`]); nothing was written in either case.
#[derive(Debug, thiserror::Error)]
pub enum OpError<E> {
    #[error(transparent)]
    Rejected(E),
    #[error("storage backend failed: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl<E: From<ApplyError>> From<StoreError> for OpError<E> {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Apply(rule) => OpError::Rejected(rule.into()),
            StoreError::Backend(source) => OpError::Backend(source),
        }
    }
}

impl<E> From<sqlx::Error> for OpError<E> {
    fn from(error: sqlx::Error) -> Self {
        OpError::Backend(Box::new(error))
    }
}

/// Applies all pending schema migrations embedded from `migrations/`.
pub async fn migrate(pool: &PgPool) -> Result<(), MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}

/// Recomputes every balance from the posting journal (NFR-08) and returns how many balance rows
/// were wrong (changed, missing, or non-zero without any postings).
///
/// Runs in one DB transaction holding a lock that blocks concurrent `apply` calls (they write
/// `balances` first), so the journal snapshot and the balances it is compared to stay consistent.
pub async fn rebuild_balances(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let mut db = pool.begin().await?;
    sqlx::query("LOCK TABLE balances IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *db)
        .await?;
    let (corrected,): (i64,) = sqlx::query_as(
        "WITH totals AS ( \
             SELECT account, asset, sum(delta) AS balance FROM ( \
                 SELECT source AS account, asset, -amount AS delta FROM postings \
                 UNION ALL \
                 SELECT destination, asset, amount FROM postings \
             ) AS movements GROUP BY account, asset \
         ), expected AS ( \
             SELECT coalesce(t.account, b.account) AS account, coalesce(t.asset, b.asset) AS asset, \
                    coalesce(t.balance, 0) AS balance \
             FROM totals AS t FULL JOIN balances AS b ON b.account = t.account AND b.asset = t.asset \
         ), corrected AS ( \
             INSERT INTO balances (account, asset, balance) \
             SELECT e.account, e.asset, e.balance FROM expected AS e \
             LEFT JOIN balances AS b ON b.account = e.account AND b.asset = e.asset \
             WHERE b.account IS NULL OR b.balance <> e.balance \
             ON CONFLICT (account, asset) \
             DO UPDATE SET balance = EXCLUDED.balance, version = balances.version + 1 \
             RETURNING 1 \
         ) SELECT count(*) FROM corrected",
    )
    .fetch_one(&mut *db)
    .await?;
    db.commit().await?;
    Ok(u64::try_from(corrected).unwrap_or_default())
}

/// Ledger store backed by PostgreSQL. Cheap to clone: it shares the connection pool.
#[derive(Debug, Clone)]
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl LedgerStore for PgStore {
    /// One DB transaction; see [`apply_in`].
    async fn apply(&self, tx: Transaction) -> Result<TxId, StoreError> {
        let mut db = self.pool.begin().await.map_err(backend)?;
        let id = apply_in(&mut db, &tx, None).await?;
        db.commit().await.map_err(backend)?;
        tx_id(id)
    }

    async fn balance(
        &self,
        account: &AccountAddress,
        asset: &AssetId,
    ) -> Result<Balance, StoreError> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT balance::text FROM balances WHERE account = $1 AND asset = $2")
                .bind(account.as_str())
                .bind(asset.to_string())
                .fetch_optional(&self.pool)
                .await
                .map_err(backend)?;
        match row {
            Some((balance,)) => balance.parse().map_err(backend),
            None => Ok(Balance::ZERO),
        }
    }
}

impl PgStore {
    /// Reverses transaction `id` with mirrored postings (FR-03), at most once. Concurrent
    /// reversals of the same transaction are serialized by a lock on its row; the unique
    /// `transactions.reverts` constraint backs that up.
    pub async fn revert(&self, id: TxId, metadata: Metadata) -> Result<TxId, OpError<RevertError>> {
        let original = i64::try_from(id.0).map_err(backend)?;
        let mut db = self.pool.begin().await?;
        // Lock first, check after: a statement's snapshot is taken before it waits for a lock,
        // so the check must be a separate statement to see a reversal committed meanwhile.
        let found: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM transactions WHERE id = $1 FOR UPDATE")
                .bind(original)
                .fetch_optional(&mut *db)
                .await?;
        if found.is_none() {
            return Err(OpError::Rejected(RevertError::NotFound(id)));
        }
        let (reverted,): (bool,) =
            sqlx::query_as("SELECT EXISTS (SELECT 1 FROM transactions WHERE reverts = $1)")
                .bind(original)
                .fetch_one(&mut *db)
                .await?;
        if reverted {
            return Err(OpError::Rejected(RevertError::AlreadyReverted(id)));
        }
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT source, destination, asset, amount::text FROM postings \
             WHERE tx_id = $1 ORDER BY idx",
        )
        .bind(original)
        .fetch_all(&mut *db)
        .await?;
        let postings = rows
            .into_iter()
            .map(|(source, destination, asset, amount)| {
                posting(&source, &destination, &asset, &amount)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let reversal = Transaction::new(postings, Metadata::new())
            .map_err(backend)?
            .reversed(metadata);
        let reversal_id = apply_in(&mut db, &reversal, Some(original)).await?;
        db.commit().await?;
        Ok(tx_id(reversal_id)?)
    }
}

/// Rebuilds a posting read back from the journal.
pub(crate) fn posting(
    source: &str,
    destination: &str,
    asset: &str,
    amount: &str,
) -> Result<Posting, StoreError> {
    Posting::new(
        source.parse().map_err(backend)?,
        destination.parse().map_err(backend)?,
        amount.parse().map_err(backend)?,
        asset.parse().map_err(backend)?,
    )
    .map_err(backend)
}

/// Applies `tx` inside the caller's DB transaction (so holds and reversals can add their own
/// writes atomically) and returns the new transaction id. The caller commits.
///
/// Locks the touched balance rows in a deterministic order (no deadlocks), checks the ledger
/// rules with `ledger_core::plan`, then appends the journal and updates balances. The transaction
/// row is inserted only after the locks are held, so per account the id order matches the order
/// in which balances changed.
async fn apply_in(
    db: &mut PgConnection,
    tx: &Transaction,
    reverts: Option<i64>,
) -> Result<i64, StoreError> {
    let keys: BTreeSet<(String, String)> = tx
        .postings()
        .iter()
        .flat_map(|p| [(p.source(), p.asset()), (p.destination(), p.asset())])
        .map(|(account, asset)| (account.to_string(), asset.to_string()))
        .collect();
    let (accounts, assets): (Vec<String>, Vec<String>) = keys.into_iter().unzip();

    sqlx::query(
        "INSERT INTO balances (account, asset) \
             SELECT * FROM UNNEST($1::text[], $2::text[]) ORDER BY 1, 2 \
             ON CONFLICT DO NOTHING",
    )
    .bind(&accounts)
    .bind(&assets)
    .execute(&mut *db)
    .await
    .map_err(backend)?;

    let locked: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT account, asset, balance::text FROM balances \
             WHERE (account, asset) IN (SELECT * FROM UNNEST($1::text[], $2::text[])) \
             ORDER BY account, asset FOR UPDATE",
    )
    .bind(&accounts)
    .bind(&assets)
    .fetch_all(&mut *db)
    .await
    .map_err(backend)?;
    let mut current = HashMap::with_capacity(locked.len());
    for (account, asset, balance) in locked {
        current.insert(
            (account, asset),
            balance.parse::<Balance>().map_err(backend)?,
        );
    }

    let sources: Vec<String> = tx
        .postings()
        .iter()
        .map(|p| p.source().to_string())
        .collect();
    let frozen: HashSet<String> = sqlx::query_as::<_, (String,)>(
        "SELECT address FROM accounts WHERE frozen AND address = ANY($1) FOR SHARE",
    )
    .bind(&sources)
    .fetch_all(&mut *db)
    .await
    .map_err(backend)?
    .into_iter()
    .map(|(address,)| address)
    .collect();

    let changes = plan(
        tx,
        |account: &AccountAddress, asset: &AssetId| {
            current
                .get(&(account.to_string(), asset.to_string()))
                .copied()
                .unwrap_or_default()
        },
        |account| frozen.contains(account.as_str()),
    )?;

    let (keys, values): (Vec<String>, Vec<String>) = tx
        .metadata()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .unzip();
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO transactions (metadata, reverts) \
             VALUES (jsonb_object($1::text[], $2::text[]), $3) RETURNING id",
    )
    .bind(&keys)
    .bind(&values)
    .bind(reverts)
    .fetch_one(&mut *db)
    .await
    .map_err(backend)?;

    let postings = tx.postings();
    let indexes: Vec<i16> = (0..postings.len())
        .map(|i| i16::try_from(i).expect("at most 256 postings per transaction"))
        .collect();
    sqlx::query(
        "INSERT INTO postings (tx_id, idx, source, destination, asset, amount) \
             SELECT $1, idx, source, destination, asset, amount::numeric \
             FROM UNNEST($2::smallint[], $3::text[], $4::text[], $5::text[], $6::text[]) \
                  AS p(idx, source, destination, asset, amount)",
    )
    .bind(id)
    .bind(&indexes)
    .bind(
        postings
            .iter()
            .map(|p| p.source().to_string())
            .collect::<Vec<_>>(),
    )
    .bind(
        postings
            .iter()
            .map(|p| p.destination().to_string())
            .collect::<Vec<_>>(),
    )
    .bind(
        postings
            .iter()
            .map(|p| p.asset().to_string())
            .collect::<Vec<_>>(),
    )
    .bind(
        postings
            .iter()
            .map(|p| p.amount().to_string())
            .collect::<Vec<_>>(),
    )
    .execute(&mut *db)
    .await
    .map_err(backend)?;

    sqlx::query(
        "UPDATE balances AS b SET balance = v.balance::numeric, version = b.version + 1 \
             FROM UNNEST($1::text[], $2::text[], $3::text[]) AS v(account, asset, balance) \
             WHERE b.account = v.account AND b.asset = v.asset",
    )
    .bind(
        changes
            .iter()
            .map(|c| c.account.to_string())
            .collect::<Vec<_>>(),
    )
    .bind(
        changes
            .iter()
            .map(|c| c.asset.to_string())
            .collect::<Vec<_>>(),
    )
    .bind(
        changes
            .iter()
            .map(|c| c.balance.to_string())
            .collect::<Vec<_>>(),
    )
    .execute(&mut *db)
    .await
    .map_err(backend)?;

    Ok(id)
}

pub(crate) fn tx_id(id: i64) -> Result<TxId, StoreError> {
    u64::try_from(id).map(TxId).map_err(backend)
}

pub(crate) fn backend(error: impl std::error::Error + Send + Sync + 'static) -> StoreError {
    StoreError::Backend(Box::new(error))
}
