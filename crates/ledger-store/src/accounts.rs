//! Account reads (FR-04), aggregated balances by pattern (FR-05) and freezing (FR-08).

use ledger_core::{AccountAddress, AccountPattern, AssetId, Balance, Metadata, StoreError, TxId};

use std::convert::Infallible;

use crate::idempotency::{self, IdempotencyKey};
use crate::{OpError, PgStore, backend};

/// An account as the API shows it. Accounts exist implicitly from their first posting; a row in
/// `accounts` (metadata, flags) exists only once something was set on it, e.g. a freeze.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub address: AccountAddress,
    pub metadata: Metadata,
    pub frozen: bool,
    /// One entry per asset the account has ever moved, sorted by asset.
    pub balances: Vec<(AssetId, Balance)>,
}

impl PgStore {
    /// The account with its current balances, or with its balances right after transaction `at`.
    /// `None` if the account never moved funds and has no flags or metadata.
    pub async fn account(
        &self,
        address: &AccountAddress,
        at: Option<TxId>,
    ) -> Result<Option<Account>, StoreError> {
        let flags: Option<(bool, Vec<String>, Vec<String>)> = sqlx::query_as(
            "SELECT frozen, \
                    coalesce((SELECT array_agg(key ORDER BY key) FROM jsonb_each_text(metadata)), '{}'), \
                    coalesce((SELECT array_agg(value ORDER BY key) FROM jsonb_each_text(metadata)), '{}') \
             FROM accounts WHERE address = $1",
        )
        .bind(address.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        let moved: (bool,) =
            sqlx::query_as("SELECT EXISTS (SELECT 1 FROM balances WHERE account = $1)")
                .bind(address.as_str())
                .fetch_one(&self.pool)
                .await
                .map_err(backend)?;
        if flags.is_none() && !moved.0 {
            return Ok(None);
        }

        let rows: Vec<(String, String)> = match at {
            None => sqlx::query_as(
                "SELECT asset, balance::text FROM balances WHERE account = $1 ORDER BY asset",
            )
            .bind(address.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(backend)?,
            Some(at) => sqlx::query_as(
                "SELECT asset, sum(delta)::text FROM ( \
                     SELECT asset, -amount AS delta FROM postings WHERE source = $1 AND tx_id <= $2 \
                     UNION ALL \
                     SELECT asset, amount FROM postings WHERE destination = $1 AND tx_id <= $2 \
                 ) AS movements GROUP BY asset ORDER BY asset",
            )
            .bind(address.as_str())
            .bind(i64::try_from(at.0).map_err(backend)?)
            .fetch_all(&self.pool)
            .await
            .map_err(backend)?,
        };

        let (frozen, keys, values) = flags.unwrap_or_default();
        Ok(Some(Account {
            address: address.clone(),
            metadata: keys.into_iter().zip(values).collect(),
            frozen,
            balances: parse_balances(rows)?,
        }))
    }

    /// The summed current balance of every account matching `pattern`, per asset (only `asset`
    /// if given), sorted by asset.
    pub async fn aggregate_balances(
        &self,
        pattern: &AccountPattern,
        asset: Option<&AssetId>,
    ) -> Result<Vec<(AssetId, Balance)>, StoreError> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT asset, sum(balance)::text FROM balances \
             WHERE account ~ $1 AND ($2::text IS NULL OR asset = $2) \
             GROUP BY asset ORDER BY asset",
        )
        .bind(pattern_regex(pattern))
        .bind(asset.map(AssetId::to_string))
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        parse_balances(rows)
    }

    /// Freezes or unfreezes an account; a frozen account cannot be a posting source (FR-08).
    /// Works for accounts that have not moved funds yet.
    pub async fn set_frozen(
        &self,
        address: &AccountAddress,
        frozen: bool,
        key: Option<&IdempotencyKey>,
    ) -> Result<(), OpError<Infallible>> {
        let mut db = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO accounts (address, frozen) VALUES ($1, $2) \
             ON CONFLICT (address) DO UPDATE SET frozen = EXCLUDED.frozen",
        )
        .bind(address.as_str())
        .bind(frozen)
        .execute(&mut *db)
        .await?;
        idempotency::record(&mut db, key, None, None, None).await?;
        db.commit().await?;
        Ok(())
    }
}

/// An anchored regex equivalent to the pattern: segments are `[a-z0-9_]+` (no regex
/// metacharacters), and `*` matches exactly one segment.
fn pattern_regex(pattern: &AccountPattern) -> String {
    let segments: Vec<&str> = pattern
        .as_str()
        .split(':')
        .map(|segment| {
            if segment == "*" {
                "[a-z0-9_]+"
            } else {
                segment
            }
        })
        .collect();
    format!("^{}$", segments.join(":"))
}

fn parse_balances(rows: Vec<(String, String)>) -> Result<Vec<(AssetId, Balance)>, StoreError> {
    rows.into_iter()
        .map(|(asset, balance)| {
            Ok((
                asset.parse().map_err(backend)?,
                balance.parse().map_err(backend)?,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pattern_becomes_an_anchored_regex_with_one_segment_per_wildcard() {
        let regex = |raw: &str| pattern_regex(&raw.parse().unwrap());
        assert_eq!(regex("users:*:wallet"), "^users:[a-z0-9_]+:wallet$");
        assert_eq!(regex("world"), "^world$");
        assert_eq!(regex("*"), "^[a-z0-9_]+$");
    }
}
