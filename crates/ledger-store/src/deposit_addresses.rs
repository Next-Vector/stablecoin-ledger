//! Deposit addresses (FR-20). The ledger never generates or watches addresses — that is the chain
//! scanning module's job — it only records which account an address deposits into. An address is
//! an opaque string, so any network's format fits.

use ledger_core::{AccountAddress, StoreError};

use crate::idempotency::{self, IdempotencyKey, Recorded};
use crate::{OpError, PgStore, backend};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepositAddress {
    pub id: u64,
    pub network: String,
    /// Exactly as registered.
    pub address: String,
    /// The wallet account deposits end up in.
    pub account: AccountAddress,
    /// Unix seconds.
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DepositAddressError {
    #[error("no asset is registered on network {0}")]
    UnknownNetwork(String),
    #[error("address {0} is already registered")]
    AddressTaken(String),
    #[error("account {0} already has a deposit address on this network")]
    AccountHasAddress(AccountAddress),
}

type Row = (i64, String, String, String, i64);

const COLUMNS: &str =
    "id, network, address, account, floor(extract(epoch FROM created_at))::bigint";

impl PgStore {
    /// Records that `address` on `network` deposits into `account`. An address belongs to one
    /// account and an account has one address per network; the network must have an asset.
    pub async fn register_deposit_address(
        &self,
        network: &str,
        address: &str,
        account: &AccountAddress,
        key: Option<&IdempotencyKey>,
    ) -> Result<DepositAddress, OpError<DepositAddressError>> {
        let mut db = self.pool.begin().await?;
        let inserted: Result<Option<Row>, sqlx::Error> =
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "INSERT INTO deposit_addresses (network, address, account) \
             SELECT $1, $2, $3 WHERE EXISTS (SELECT 1 FROM assets WHERE network = $1) \
             RETURNING {COLUMNS}"
            )))
            .bind(network)
            .bind(address)
            .bind(account.as_str())
            .fetch_optional(&mut *db)
            .await;
        let row = match inserted {
            Ok(Some(row)) => row,
            Ok(None) => {
                return Err(OpError::Rejected(DepositAddressError::UnknownNetwork(
                    network.to_owned(),
                )));
            }
            Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
                return Err(OpError::Rejected(match error.constraint() {
                    Some("deposit_addresses_account_key") => {
                        DepositAddressError::AccountHasAddress(account.clone())
                    }
                    _ => DepositAddressError::AddressTaken(address.to_owned()),
                }));
            }
            Err(error) => return Err(error.into()),
        };
        idempotency::record(
            &mut db,
            key,
            Recorded {
                deposit_address: Some(row.0),
                ..Recorded::default()
            },
        )
        .await?;
        db.commit().await?;
        deposit_address(row).map_err(|error| OpError::Backend(Box::new(error)))
    }

    /// Registrations by id, at most `limit` after `after`, optionally only those of `network`,
    /// `address` (exact match) and `account`; the next cursor when more exist.
    pub async fn deposit_addresses(
        &self,
        network: Option<&str>,
        address: Option<&str>,
        account: Option<&AccountAddress>,
        after: Option<u64>,
        limit: u32,
    ) -> Result<(Vec<DepositAddress>, Option<u64>), StoreError> {
        let mut rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM deposit_addresses \
             WHERE ($1::text IS NULL OR network = $1) AND ($2::text IS NULL OR address = $2) \
               AND ($3::text IS NULL OR account = $3) AND ($4::bigint IS NULL OR id > $4) \
             ORDER BY id LIMIT $5"
        )))
        .bind(network)
        .bind(address)
        .bind(account.map(AccountAddress::as_str))
        .bind(after.map(i64::try_from).transpose().map_err(backend)?)
        .bind(i64::from(limit) + 1)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        let more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let items = rows
            .into_iter()
            .map(deposit_address)
            .collect::<Result<Vec<_>, _>>()?;
        let next = more.then(|| items.last().map(|item| item.id)).flatten();
        Ok((items, next))
    }

    pub async fn deposit_address(&self, id: u64) -> Result<Option<DepositAddress>, StoreError> {
        let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM deposit_addresses WHERE id = $1"
        )))
        .bind(i64::try_from(id).map_err(backend)?)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        row.map(deposit_address).transpose()
    }
}

fn deposit_address(
    (id, network, address, account, created_at): Row,
) -> Result<DepositAddress, StoreError> {
    Ok(DepositAddress {
        id: u64::try_from(id).map_err(backend)?,
        network,
        address,
        account: account.parse().map_err(backend)?,
        created_at,
    })
}
