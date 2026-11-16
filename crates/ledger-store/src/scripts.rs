//! Running ledger scripts (FR-65) and the stored-script registry (FR-66).
//!
//! A script run locks every balance row the script can touch (known from the script and its
//! variables, not from balances) in one ordered statement, compiles against those locked
//! balances and applies the result in the same DB transaction. A dry run does the same
//! read-only, checking the ledger rules with `plan` and writing nothing.

use std::collections::BTreeSet;

use ledger_core::{ApplyError, BalanceChange, Metadata, Transaction, TxId, plan};
use ledger_script::ast::Script;
use ledger_script::{CompileError, ParseError, Vars};

use crate::idempotency::{self, IdempotencyKey, Recorded};
use crate::{OpError, PgStore, apply_in, backend, read_balances, source_flags, tx_id};

/// A version of a stored script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredScript {
    pub name: String,
    pub version: u32,
    pub source: String,
    /// Unix time in seconds.
    pub created_at: i64,
}

/// A stored script's latest version, without its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptInfo {
    pub name: String,
    pub version: u32,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegisterError {
    #[error("a script name must be 1 to 64 characters of a-z, 0-9, _ and -")]
    InvalidName,
    #[error(transparent)]
    Invalid(#[from] ParseError),
}

/// Why a script did not produce a transaction.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScriptError {
    #[error(transparent)]
    Compile(#[from] CompileError),
    #[error(transparent)]
    Apply(#[from] ApplyError),
}

/// What a script would do: its transaction and the resulting balances of the accounts it moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRun {
    pub transaction: Transaction,
    pub balances: Vec<BalanceChange>,
}

type ScriptRow = (String, i32, String, i64);
const SCRIPT_COLUMNS: &str = "name, version, source, floor(extract(epoch FROM created_at))::bigint";

impl PgStore {
    /// Stores `source` as the next version of script `name` (starting at 1); it must parse.
    pub async fn register_script(
        &self,
        name: &str,
        source: &str,
        key: Option<&IdempotencyKey>,
    ) -> Result<StoredScript, OpError<RegisterError>> {
        let valid_name = (1..=64).contains(&name.len())
            && name
                .bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'));
        if !valid_name {
            return Err(OpError::Rejected(RegisterError::InvalidName));
        }
        ledger_script::parse(source).map_err(|error| OpError::Rejected(error.into()))?;

        let mut db = self.pool.begin().await?;
        // Serializes registrations of one name, so versions are gapless and unique.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('script:' || $1, 0))")
            .bind(name)
            .execute(&mut *db)
            .await?;
        let row: ScriptRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "INSERT INTO scripts (name, version, source) \
             SELECT $1, coalesce(max(version), 0) + 1, $2 FROM scripts WHERE name = $1 \
             RETURNING {SCRIPT_COLUMNS}"
        )))
        .bind(name)
        .bind(source)
        .fetch_one(&mut *db)
        .await?;
        idempotency::record(
            &mut db,
            key,
            Recorded {
                script: Some((name, row.1)),
                ..Recorded::default()
            },
        )
        .await?;
        db.commit().await?;
        stored(row).map_err(|error| OpError::Backend(Box::new(error)))
    }

    /// A script version, or its latest version; `None` if there is no such script or version.
    pub async fn script(
        &self,
        name: &str,
        version: Option<u32>,
    ) -> Result<Option<StoredScript>, ledger_core::StoreError> {
        let version = version.map(i32::try_from).transpose().map_err(backend)?;
        let row: Option<ScriptRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {SCRIPT_COLUMNS} FROM scripts \
             WHERE name = $1 AND ($2::integer IS NULL OR version = $2) \
             ORDER BY version DESC LIMIT 1"
        )))
        .bind(name)
        .bind(version)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        row.map(stored).transpose()
    }

    /// Latest version of each script, by name, at most `limit` after `after`; the cursor of the
    /// next page is the last name when more exist.
    pub async fn scripts(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<(Vec<ScriptInfo>, Option<String>), ledger_core::StoreError> {
        let mut rows: Vec<ScriptRow> = sqlx::query_as(
            "SELECT DISTINCT ON (name) name, version, '', \
                    floor(extract(epoch FROM created_at))::bigint \
             FROM scripts WHERE ($1::text IS NULL OR name > $1) \
             ORDER BY name, version DESC LIMIT $2",
        )
        .bind(after)
        .bind(i64::from(limit) + 1)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        let more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let items = rows
            .into_iter()
            .map(|row| {
                stored(row).map(|script| ScriptInfo {
                    name: script.name,
                    version: script.version,
                    created_at: script.created_at,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let next = more
            .then(|| items.last().map(|item| item.name.clone()))
            .flatten();
        Ok((items, next))
    }

    /// Versions of a script in order, at most `limit` after version `after`; `None` if the
    /// script does not exist.
    pub async fn script_versions(
        &self,
        name: &str,
        after: Option<u32>,
        limit: u32,
    ) -> Result<Option<(Vec<StoredScript>, Option<u32>)>, ledger_core::StoreError> {
        if self.script(name, None).await?.is_none() {
            return Ok(None);
        }
        let mut rows: Vec<ScriptRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {SCRIPT_COLUMNS} FROM scripts \
             WHERE name = $1 AND ($2::integer IS NULL OR version > $2) \
             ORDER BY version LIMIT $3"
        )))
        .bind(name)
        .bind(after.map(i32::try_from).transpose().map_err(backend)?)
        .bind(i64::from(limit) + 1)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        let more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let items = rows
            .into_iter()
            .map(stored)
            .collect::<Result<Vec<_>, _>>()?;
        let next = more
            .then(|| items.last().map(|item| item.version))
            .flatten();
        Ok(Some((items, next)))
    }

    /// Compiles `script` against locked balances and applies it atomically. `audit` is merged
    /// over the script's own metadata.
    pub async fn run_script(
        &self,
        script: &Script,
        vars: &Vars,
        audit: Metadata,
        key: Option<&IdempotencyKey>,
    ) -> Result<TxId, OpError<ScriptError>> {
        let keys = touched(script, vars)?;
        let mut db = self.pool.begin().await?;
        let balances = read_balances(&mut db, &keys, true).await?;
        let tx = compile(script, vars, &balances, audit)?;
        let id = apply_in(&mut db, &tx, None).await?;
        idempotency::record(
            &mut db,
            key,
            Recorded {
                transaction: Some(id),
                ..Recorded::default()
            },
        )
        .await?;
        db.commit().await?;
        Ok(tx_id(id)?)
    }

    /// What [`PgStore::run_script`] would do now, without writing anything.
    pub async fn dry_run(
        &self,
        script: &Script,
        vars: &Vars,
        audit: Metadata,
    ) -> Result<DryRun, OpError<ScriptError>> {
        let keys = touched(script, vars)?;
        let mut db = self.pool.acquire().await?;
        let balances = read_balances(&mut db, &keys, false).await?;
        let transaction = compile(script, vars, &balances, audit)?;
        let sources: Vec<String> = transaction
            .postings()
            .iter()
            .map(|p| p.source().to_string())
            .collect();
        let flags = source_flags(&mut db, &sources, false).await?;
        let balances = plan(
            &transaction,
            |account, asset| current(&balances, account, asset),
            |account| flags.frozen.contains(account.as_str()),
            |account| flags.negative_allowed.contains(account.as_str()),
        )
        .map_err(|error| OpError::Rejected(error.into()))?;
        Ok(DryRun {
            transaction,
            balances,
        })
    }
}

type Balances = std::collections::HashMap<(String, String), ledger_core::Balance>;

fn touched(
    script: &Script,
    vars: &Vars,
) -> Result<BTreeSet<(String, String)>, OpError<ScriptError>> {
    Ok(ledger_script::touched(script, vars)
        .map_err(|error| OpError::Rejected(error.into()))?
        .into_iter()
        .map(|(account, asset)| (account.to_string(), asset.to_string()))
        .collect())
}

fn current(
    balances: &Balances,
    account: &ledger_core::AccountAddress,
    asset: &ledger_core::AssetId,
) -> ledger_core::Balance {
    balances
        .get(&(account.to_string(), asset.to_string()))
        .copied()
        .unwrap_or_default()
}

fn compile(
    script: &Script,
    vars: &Vars,
    balances: &Balances,
    audit: Metadata,
) -> Result<Transaction, OpError<ScriptError>> {
    let tx = ledger_script::compile(script, vars, |account, asset| {
        current(balances, account, asset)
    })
    .map_err(|error| OpError::Rejected(error.into()))?;
    let mut metadata = tx.metadata().clone();
    metadata.extend(audit);
    Ok(Transaction::new(tx.postings().to_vec(), metadata).map_err(backend)?)
}

fn stored(
    (name, version, source, created_at): ScriptRow,
) -> Result<StoredScript, ledger_core::StoreError> {
    Ok(StoredScript {
        name,
        version: u32::try_from(version).map_err(backend)?,
        source,
        created_at,
    })
}
