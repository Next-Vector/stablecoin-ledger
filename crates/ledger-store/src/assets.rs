//! The asset registry (§4.2): the assets the ledger accounts for. Filled by the operator; deposit
//! address registration accepts only networks that have an asset. Chain details (chain id,
//! contract, finality) belong to the chain scanning module, not to the ledger.

use ledger_core::AssetId;

use crate::{OpError, PgStore};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AssetError {
    #[error("asset {0} is already registered")]
    AlreadyRegistered(AssetId),
}

impl PgStore {
    pub async fn register_asset(&self, asset: &AssetId) -> Result<(), OpError<AssetError>> {
        let inserted =
            sqlx::query("INSERT INTO assets (id, network, decimals) VALUES ($1, $2, $3)")
                .bind(asset.to_string())
                .bind(asset.network())
                .bind(i16::from(asset.decimals()))
                .execute(&self.pool)
                .await;
        match inserted {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(error)) if error.is_unique_violation() => Err(
                OpError::Rejected(AssetError::AlreadyRegistered(asset.clone())),
            ),
            Err(error) => Err(error.into()),
        }
    }
}
