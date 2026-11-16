//! Wire types of the /v1 API and their conversion to and from ledger types. Amounts and
//! balances are decimal strings because JSON numbers cannot hold `u128` (spec §8).

use std::collections::BTreeMap;
use std::str::FromStr;

use ledger_core::{
    AccountAddress, AssetId, Balance, Hold, HoldStatus, Metadata, Posting, Transaction, TxId,
};
use ledger_store::{Account, HistoryPage, RecordedTransaction};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::error::ApiError;

/// A movement of `amount` base units of `asset` from `source` to `destination`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PostingBody {
    #[schema(example = "users:42:wallet")]
    pub source: String,
    #[schema(example = "merchants:7:wallet")]
    pub destination: String,
    /// Base units as a decimal string, greater than zero.
    #[schema(example = "99000000")]
    pub amount: String,
    #[schema(example = "USDC@ethereum/6")]
    pub asset: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateTransactionRequest {
    /// 1 to 256 postings, applied atomically.
    pub postings: Vec<PostingBody>,
    #[serde(default)]
    pub metadata: Metadata,
}

/// Optional body of actions that only take metadata (revert, void).
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MetadataRequest {
    #[serde(default)]
    pub metadata: Metadata,
}

/// The body of actions that take no input (freeze, unfreeze): omit it or send `{}`.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EmptyRequest {}

#[derive(Debug, Serialize, ToSchema)]
pub struct TransactionResponse {
    pub id: u64,
    pub postings: Vec<PostingBody>,
    pub metadata: Metadata,
    /// The transaction this one reverses, if it is a reversal.
    pub reverts: Option<u64>,
    /// The reversal of this transaction, if it has been reverted.
    pub reverted_by: Option<u64>,
    /// Unix time in seconds.
    pub created_at: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TransactionPageResponse {
    /// Newest first.
    pub items: Vec<TransactionResponse>,
    /// Pass as `cursor` to get the next (older) page; null on the last page.
    pub next_cursor: Option<u64>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct ListTransactionsQuery {
    /// Transactions with a posting from or to this account.
    pub account: Option<String>,
    /// Transactions with a posting in this asset.
    pub asset: Option<String>,
    /// Created at or after this Unix time (seconds).
    pub from: Option<i64>,
    /// Created before this Unix time (seconds).
    pub to: Option<i64>,
    /// With `metadata_value`: transactions whose metadata has this key with that value.
    pub metadata_key: Option<String>,
    pub metadata_value: Option<String>,
    /// Page size, 1 to 500 (default 50).
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    pub cursor: Option<u64>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct AccountQuery {
    /// Show balances as they were right after this transaction.
    pub at: Option<u64>,
}

/// Balances by asset, as signed decimal strings.
pub type Balances = BTreeMap<String, String>;

#[derive(Debug, Serialize, ToSchema)]
pub struct AccountResponse {
    pub address: String,
    pub metadata: Metadata,
    /// A frozen account cannot be a posting source; it can still receive.
    pub frozen: bool,
    #[schema(example = json!({"USDC@ethereum/6": "60000000"}))]
    pub balances: Balances,
}

#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct BalancesQuery {
    /// Address pattern; a `*` segment matches exactly one segment, e.g. `users:*:wallet`.
    pub address: String,
    pub asset: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BalancesResponse {
    pub address: String,
    pub balances: Balances,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateHoldRequest {
    /// The account to reserve funds from.
    pub account: String,
    pub amount: String,
    pub asset: String,
    /// Unix time (seconds) after which the hold expires.
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub metadata: Metadata,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CaptureHoldRequest {
    pub destination: String,
    /// At most the held amount; the rest returns to the owner.
    pub amount: String,
    #[serde(default)]
    pub metadata: Metadata,
}

#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum HoldStatusBody {
    Active,
    Captured,
    Voided,
    Expired,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct HoldResponse {
    pub id: u64,
    /// The account the funds were reserved from and return to.
    pub owner: String,
    /// The technical account `holds:{id}` holding the funds.
    pub account: String,
    pub amount: String,
    pub asset: String,
    pub status: HoldStatusBody,
    pub expires_at: Option<u64>,
    /// The transaction of this action (reserve, capture or void).
    pub transaction_id: u64,
}

/// Parses `raw` as a `T`, reporting a failure against `field`.
pub fn parse<T: FromStr>(field: &str, raw: &str) -> Result<T, ApiError>
where
    T::Err: std::fmt::Display,
{
    raw.parse()
        .map_err(|error| ApiError::invalid_field(field, error))
}

/// A base-unit amount: a canonical decimal string of digits only (no sign, point or exponent).
pub fn parse_amount(field: &str, raw: &str) -> Result<u128, ApiError> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ApiError::invalid_field(
            field,
            "must be a decimal string of digits",
        ));
    }
    raw.parse()
        .map_err(|_| ApiError::invalid_field(field, "exceeds the maximum amount"))
}

impl CreateTransactionRequest {
    pub fn into_transaction(self) -> Result<Transaction, ApiError> {
        let postings = self
            .postings
            .iter()
            .enumerate()
            .map(|(i, posting)| posting.to_posting(&format!("postings[{i}]")))
            .collect::<Result<Vec<_>, _>>()?;
        Transaction::new(postings, self.metadata)
            .map_err(|error| ApiError::invalid_field("postings", error))
    }
}

impl PostingBody {
    fn to_posting(&self, field: &str) -> Result<Posting, ApiError> {
        Posting::new(
            parse::<AccountAddress>(&format!("{field}.source"), &self.source)?,
            parse::<AccountAddress>(&format!("{field}.destination"), &self.destination)?,
            parse_amount(&format!("{field}.amount"), &self.amount)?,
            parse::<AssetId>(&format!("{field}.asset"), &self.asset)?,
        )
        .map_err(|error| ApiError::invalid_field(field, error))
    }
}

impl From<&Posting> for PostingBody {
    fn from(posting: &Posting) -> Self {
        Self {
            source: posting.source().to_string(),
            destination: posting.destination().to_string(),
            amount: posting.amount().to_string(),
            asset: posting.asset().to_string(),
        }
    }
}

impl From<RecordedTransaction> for TransactionResponse {
    fn from(recorded: RecordedTransaction) -> Self {
        Self {
            id: recorded.id.0,
            postings: recorded
                .transaction
                .postings()
                .iter()
                .map(PostingBody::from)
                .collect(),
            metadata: recorded.transaction.metadata().clone(),
            reverts: recorded.reverts.map(|id| id.0),
            reverted_by: recorded.reverted_by.map(|id| id.0),
            created_at: recorded.created_at,
        }
    }
}

impl From<HistoryPage> for TransactionPageResponse {
    fn from(page: HistoryPage) -> Self {
        Self {
            items: page.items.into_iter().map(Into::into).collect(),
            next_cursor: page.next_cursor.map(|id: TxId| id.0),
        }
    }
}

pub fn balances(entries: Vec<(AssetId, Balance)>) -> Balances {
    entries
        .into_iter()
        .map(|(asset, balance)| (asset.to_string(), balance.to_string()))
        .collect()
}

impl From<Account> for AccountResponse {
    fn from(account: Account) -> Self {
        Self {
            address: account.address.to_string(),
            metadata: account.metadata,
            frozen: account.frozen,
            balances: balances(account.balances),
        }
    }
}

impl From<HoldStatus> for HoldStatusBody {
    fn from(status: HoldStatus) -> Self {
        match status {
            HoldStatus::Active => Self::Active,
            HoldStatus::Captured => Self::Captured,
            HoldStatus::Voided => Self::Voided,
            HoldStatus::Expired => Self::Expired,
        }
    }
}

impl HoldResponse {
    pub fn new(hold: &Hold, transaction: TxId) -> Self {
        Self {
            id: hold.id().0,
            owner: hold.owner().to_string(),
            account: hold.account().to_string(),
            amount: hold.amount().to_string(),
            asset: hold.asset().to_string(),
            status: hold.status().into(),
            expires_at: hold.expires_at(),
            transaction_id: transaction.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_are_canonical_digit_strings_within_u128() {
        assert_eq!(parse_amount("amount", "0").unwrap(), 0);
        assert_eq!(parse_amount("amount", "1500000").unwrap(), 1_500_000);
        assert_eq!(
            parse_amount("amount", &u128::MAX.to_string()).unwrap(),
            u128::MAX
        );
        for raw in ["", "+1", "-1", "1.5", "1e3", " 1", "0x10"] {
            assert!(parse_amount("amount", raw).is_err(), "{raw:?}");
        }
        assert!(parse_amount("amount", "340282366920938463463374607431768211456").is_err());
    }
}
