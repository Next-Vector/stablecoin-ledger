//! Wire types of the /v1 API and their conversion to and from ledger types. Amounts and
//! balances are decimal strings because JSON numbers cannot hold `u128` (spec §8).

use std::collections::BTreeMap;
use std::str::FromStr;

use ledger_core::{
    AccountAddress, AssetId, Balance, Hold, HoldStatus, Metadata, Posting, Transaction, TxId,
};
use ledger_script::Vars;
use ledger_store::{Account, HistoryPage, RecordedTransaction, ScriptInfo, StoredScript};
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
    /// 1 to 256 postings, applied atomically. Exactly one of `postings`, `script` and
    /// `script_name` is required.
    pub postings: Option<Vec<PostingBody>>,
    /// An inline ledger script (spec §5.7), compiled with `vars`.
    pub script: Option<String>,
    /// A stored script, run at its latest version unless `script_version` is given.
    pub script_name: Option<String>,
    pub script_version: Option<u32>,
    /// Script variable values by name (without `$`).
    #[schema(value_type = Option<Object>, example = json!({"buyer": "42", "price": "USDC@ethereum/6 100000000"}))]
    pub vars: Option<Vars>,
    /// Merged over metadata the script sets; keys starting with `script.` are reserved for the
    /// audit record of script runs.
    #[serde(default)]
    pub metadata: Metadata,
}

/// `POST /v1/scripts/dry-run`: a script to evaluate without writing.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DryRunRequest {
    /// Exactly one of `script` and `script_name` is required.
    pub script: Option<String>,
    pub script_name: Option<String>,
    pub script_version: Option<u32>,
    #[schema(value_type = Option<Object>)]
    pub vars: Option<Vars>,
    #[serde(default)]
    pub metadata: Metadata,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DryRunResponse {
    pub postings: Vec<PostingBody>,
    pub metadata: Metadata,
    /// Resulting balances of the accounts the postings move: account → asset → balance.
    #[schema(value_type = Object, example = json!({"merchants:7:wallet": {"USDC@ethereum/6": "99000000"}}))]
    pub balances: BTreeMap<String, Balances>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisterScriptRequest {
    /// 1–64 characters of a-z, 0-9, _ and -.
    #[schema(example = "checkout")]
    pub name: String,
    /// The script text; it must parse.
    pub source: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ScriptResponse {
    pub name: String,
    pub version: u32,
    pub source: String,
    /// Unix time in seconds.
    pub created_at: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ScriptSummary {
    pub name: String,
    /// The latest version.
    pub version: u32,
    pub created_at: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ScriptPageResponse {
    /// By name.
    pub items: Vec<ScriptSummary>,
    /// Pass as `cursor` for the next page; null on the last page.
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ScriptVersionsResponse {
    /// Oldest first.
    pub items: Vec<ScriptResponse>,
    pub next_cursor: Option<u32>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct ListScriptsQuery {
    /// Page size, 1 to 500 (default 50).
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct ListVersionsQuery {
    /// Page size, 1 to 500 (default 50).
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    pub cursor: Option<u32>,
}

/// Which script to run.
pub enum ScriptRef {
    Inline(String),
    Stored { name: String, version: Option<u32> },
}

/// A request to run a script: which one, its variables and extra metadata.
pub struct ScriptRun {
    pub script: ScriptRef,
    pub vars: Vars,
    pub metadata: Metadata,
}

pub enum TransactionInput {
    Postings(Transaction),
    Script(ScriptRun),
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
    pub fn into_input(self) -> Result<TransactionInput, ApiError> {
        let script = script_ref(self.script, self.script_name, self.script_version)?;
        match (self.postings, script) {
            (Some(postings), None) => {
                if self.vars.is_some() {
                    return Err(ApiError::invalid_field(
                        "vars",
                        "is only allowed with a script",
                    ));
                }
                Ok(TransactionInput::Postings(transaction(
                    &postings,
                    self.metadata,
                )?))
            }
            (None, Some(script)) => Ok(TransactionInput::Script(ScriptRun {
                script,
                vars: self.vars.unwrap_or_default(),
                metadata: self.metadata,
            })),
            _ => Err(ApiError::validation(
                "exactly one of postings, script and script_name is required",
            )),
        }
    }
}

impl DryRunRequest {
    pub fn into_run(self) -> Result<ScriptRun, ApiError> {
        let script =
            script_ref(self.script, self.script_name, self.script_version)?.ok_or_else(|| {
                ApiError::validation("exactly one of script and script_name is required")
            })?;
        Ok(ScriptRun {
            script,
            vars: self.vars.unwrap_or_default(),
            metadata: self.metadata,
        })
    }
}

fn script_ref(
    script: Option<String>,
    name: Option<String>,
    version: Option<u32>,
) -> Result<Option<ScriptRef>, ApiError> {
    match (script, name, version) {
        (None, None, None) => Ok(None),
        (Some(source), None, None) => Ok(Some(ScriptRef::Inline(source))),
        (None, Some(name), version) => Ok(Some(ScriptRef::Stored { name, version })),
        (Some(_), Some(_), _) => Err(ApiError::validation(
            "give either script or script_name, not both",
        )),
        (_, None, Some(_)) => Err(ApiError::invalid_field(
            "script_version",
            "is only allowed with script_name",
        )),
    }
}

fn transaction(postings: &[PostingBody], metadata: Metadata) -> Result<Transaction, ApiError> {
    let postings = postings
        .iter()
        .enumerate()
        .map(|(i, posting)| posting.to_posting(&format!("postings[{i}]")))
        .collect::<Result<Vec<_>, _>>()?;
    Transaction::new(postings, metadata).map_err(|error| ApiError::invalid_field("postings", error))
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

impl From<StoredScript> for ScriptResponse {
    fn from(script: StoredScript) -> Self {
        Self {
            name: script.name,
            version: script.version,
            source: script.source,
            created_at: script.created_at,
        }
    }
}

impl From<ScriptInfo> for ScriptSummary {
    fn from(script: ScriptInfo) -> Self {
        Self {
            name: script.name,
            version: script.version,
            created_at: script.created_at,
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
