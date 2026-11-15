//! PostgreSQL storage for the ledger journal and materialized balances.

use std::collections::{BTreeSet, HashMap, HashSet};

use ledger_core::{
    AccountAddress, AssetId, Balance, LedgerStore, StoreError, Transaction, TxId, plan,
};
use sqlx::PgPool;
use sqlx::migrate::MigrateError;

/// Applies all pending schema migrations embedded from `migrations/`.
pub async fn migrate(pool: &PgPool) -> Result<(), MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
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
    /// One DB transaction: lock the touched balance rows in a deterministic order (no deadlocks),
    /// check the ledger rules with `ledger_core::plan`, then append the journal and update balances.
    /// The transaction row is inserted only after the locks are held, so per account the id order
    /// matches the order in which balances changed.
    async fn apply(&self, tx: Transaction) -> Result<TxId, StoreError> {
        let mut db = self.pool.begin().await.map_err(backend)?;

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
            &tx,
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
            "INSERT INTO transactions (metadata) VALUES (jsonb_object($1::text[], $2::text[])) RETURNING id",
        )
        .bind(&keys)
        .bind(&values)
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

        db.commit().await.map_err(backend)?;
        Ok(TxId(u64::try_from(id).map_err(backend)?))
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

fn backend(error: impl std::error::Error + Send + Sync + 'static) -> StoreError {
    StoreError::Backend(Box::new(error))
}
