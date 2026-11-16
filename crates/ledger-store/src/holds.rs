//! Holds in PostgreSQL (FR-07). The rules are `ledger_core::Hold`'s; this module only loads and
//! locks the hold row and applies the transaction the rule builds, in one DB transaction.

use ledger_core::{
    AccountAddress, AssetId, Hold, HoldError, HoldId, HoldStatus, Metadata, StoreError, TxId,
};
use sqlx::PgConnection;

use crate::idempotency::{self, IdempotencyKey};
use crate::{OpError, PgStore, apply_in, backend, tx_id};

type HoldRow = (String, String, String, String, Option<i64>, i64);

impl PgStore {
    /// Reserves `amount` of `asset` from `owner` until captured, voided or expired; `expires_at`
    /// is Unix seconds. Returns the hold and the reserving transaction.
    pub async fn hold(
        &self,
        owner: AccountAddress,
        amount: u128,
        asset: AssetId,
        expires_at: Option<u64>,
        metadata: Metadata,
        key: Option<&IdempotencyKey>,
    ) -> Result<(Hold, TxId), OpError<HoldError>> {
        let mut db = self.pool.begin().await?;
        let (id,): (i64,) = sqlx::query_as("SELECT nextval(pg_get_serial_sequence('holds', 'id'))")
            .fetch_one(&mut *db)
            .await?;
        let hold = Hold::new(
            HoldId(u64::try_from(id).map_err(backend)?),
            owner,
            amount,
            asset,
            HoldStatus::Active,
            expires_at,
        )
        .map_err(OpError::Rejected)?;
        let reservation = hold.reservation(metadata).map_err(OpError::Rejected)?;
        let reserved = apply_in(&mut db, &reservation, None).await?;
        sqlx::query(
            "INSERT INTO holds (id, owner, asset, amount, status, expires_at) \
             VALUES ($1, $2, $3, $4::numeric, $5, to_timestamp($6))",
        )
        .bind(id)
        .bind(hold.owner().as_str())
        .bind(hold.asset().to_string())
        .bind(hold.amount().to_string())
        .bind(hold.status().as_str())
        .bind(expires_at.map(i64::try_from).transpose().map_err(backend)?)
        .execute(&mut *db)
        .await?;
        idempotency::record(&mut db, key, Some(reserved), Some(id)).await?;
        db.commit().await?;
        Ok((hold, tx_id(reserved)?))
    }

    /// Sends `amount` of an active hold to `destination` and returns the rest to the owner.
    /// A hold past its deadline counts as expired and cannot be captured.
    pub async fn capture(
        &self,
        id: HoldId,
        destination: AccountAddress,
        amount: u128,
        metadata: Metadata,
        key: Option<&IdempotencyKey>,
    ) -> Result<(Hold, TxId), OpError<HoldError>> {
        let mut db = self.pool.begin().await?;
        let (hold, now) = load_hold(&mut db, id, true)
            .await?
            .ok_or(OpError::Rejected(HoldError::NotFound(id)))?;
        if hold.is_due(now) {
            return Err(OpError::Rejected(HoldError::NotActive(
                id,
                HoldStatus::Expired,
            )));
        }
        let capture = hold
            .capture(destination, amount, metadata)
            .map_err(OpError::Rejected)?;
        let settled = settle(&mut db, hold, &capture, HoldStatus::Captured, key).await?;
        db.commit().await?;
        Ok(settled)
    }

    /// Returns the whole held amount to the owner.
    pub async fn void(
        &self,
        id: HoldId,
        metadata: Metadata,
        key: Option<&IdempotencyKey>,
    ) -> Result<(Hold, TxId), OpError<HoldError>> {
        let mut db = self.pool.begin().await?;
        let (hold, _) = load_hold(&mut db, id, true)
            .await?
            .ok_or(OpError::Rejected(HoldError::NotFound(id)))?;
        let release = hold.release(metadata).map_err(OpError::Rejected)?;
        let settled = settle(&mut db, hold, &release, HoldStatus::Voided, key).await?;
        db.commit().await?;
        Ok(settled)
    }

    /// A hold in its current state, or `None` if it does not exist.
    pub async fn find_hold(&self, id: HoldId) -> Result<Option<Hold>, StoreError> {
        let mut db = self.pool.acquire().await.map_err(backend)?;
        Ok(load_hold(&mut db, id, false).await?.map(|(hold, _)| hold))
    }
}

/// Loads a hold with the database clock in Unix seconds; with `lock`, row-locks it so it
/// settles once.
async fn load_hold(
    db: &mut PgConnection,
    id: HoldId,
    lock: bool,
) -> Result<Option<(Hold, u64)>, StoreError> {
    let sql = format!(
        "SELECT owner, asset, amount::text, status, \
                floor(extract(epoch FROM expires_at))::bigint, \
                floor(extract(epoch FROM now()))::bigint \
         FROM holds WHERE id = $1{}",
        if lock { " FOR UPDATE" } else { "" }
    );
    let row: Option<HoldRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(i64::try_from(id.0).map_err(backend)?)
        .fetch_optional(&mut *db)
        .await
        .map_err(backend)?;
    let Some((owner, asset, amount, status, expires_at, now)) = row else {
        return Ok(None);
    };
    let hold = Hold::new(
        id,
        owner.parse().map_err(backend)?,
        amount.parse().map_err(backend)?,
        asset.parse().map_err(backend)?,
        status.parse().map_err(backend)?,
        expires_at.map(u64::try_from).transpose().map_err(backend)?,
    )
    .map_err(backend)?;
    Ok(Some((hold, u64::try_from(now).map_err(backend)?)))
}

/// Applies the settling transaction and records the hold's final status.
async fn settle(
    db: &mut PgConnection,
    hold: Hold,
    tx: &ledger_core::Transaction,
    status: HoldStatus,
    key: Option<&IdempotencyKey>,
) -> Result<(Hold, TxId), OpError<HoldError>> {
    let settled = apply_in(db, tx, None).await?;
    sqlx::query("UPDATE holds SET status = $2 WHERE id = $1")
        .bind(i64::try_from(hold.id().0).map_err(backend)?)
        .bind(status.as_str())
        .execute(&mut *db)
        .await?;
    let hold_id = i64::try_from(hold.id().0).map_err(backend)?;
    idempotency::record(db, key, Some(settled), Some(hold_id)).await?;
    let hold = Hold::new(
        hold.id(),
        hold.owner().clone(),
        hold.amount(),
        hold.asset().clone(),
        status,
        hold.expires_at(),
    )
    .map_err(OpError::Rejected)?;
    Ok((hold, tx_id(settled)?))
}
