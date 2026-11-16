//! Transaction history with filters and cursor pagination (FR-06), newest first.

use std::collections::BTreeMap;

use ledger_core::{AccountAddress, AssetId, Metadata, Posting, StoreError, Transaction, TxId};

use crate::{PgStore, backend, posting, tx_id};

/// Every field narrows the result; `None` means no restriction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryFilter {
    /// Transactions with a posting from or to this account.
    pub account: Option<AccountAddress>,
    /// Transactions with a posting in this asset (combined with `account`: in the same posting).
    pub asset: Option<AssetId>,
    /// Created at or after this Unix time (seconds), inclusive.
    pub from: Option<i64>,
    /// Created before this Unix time (seconds), exclusive.
    pub to: Option<i64>,
    /// Transactions whose metadata contains this key with this value.
    pub metadata: Option<(String, String)>,
}

/// A page request: at most `limit` transactions with ids below `before` (the previous page's cursor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    limit: u32,
    before: Option<TxId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("page size {0} is out of range; it must be between 1 and {max}", max = Page::MAX_LIMIT)]
pub struct InvalidPageSize(pub u32);

impl Page {
    pub const DEFAULT_LIMIT: u32 = 50;
    /// Hard upper bound on page size (OWASP API4: no unbounded lists).
    pub const MAX_LIMIT: u32 = 500;

    pub fn new(limit: u32, before: Option<TxId>) -> Result<Self, InvalidPageSize> {
        if !(1..=Self::MAX_LIMIT).contains(&limit) {
            return Err(InvalidPageSize(limit));
        }
        Ok(Self { limit, before })
    }

    pub fn limit(&self) -> u32 {
        self.limit
    }

    pub fn before(&self) -> Option<TxId> {
        self.before
    }
}

impl Default for Page {
    fn default() -> Self {
        Self {
            limit: Self::DEFAULT_LIMIT,
            before: None,
        }
    }
}

/// A transaction as stored in the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedTransaction {
    pub id: TxId,
    pub transaction: Transaction,
    /// The transaction this one reverses, if it is a reversal.
    pub reverts: Option<TxId>,
    /// The reversal of this transaction, if it has been reverted.
    pub reverted_by: Option<TxId>,
    /// Unix time (seconds) when the transaction was written.
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryPage {
    pub items: Vec<RecordedTransaction>,
    /// Pass as `before` to get the next (older) page; `None` on the last page.
    pub next_cursor: Option<TxId>,
}

type HeaderRow = (i64, Option<i64>, Option<i64>, i64, Vec<String>, Vec<String>);

/// The columns of a [`HeaderRow`], selected from `transactions AS t`.
const HEADER_COLUMNS: &str = "t.id, t.reverts, \
    (SELECT r.id FROM transactions AS r WHERE r.reverts = t.id), \
    floor(extract(epoch FROM t.created_at))::bigint, \
    coalesce((SELECT array_agg(key ORDER BY key) FROM jsonb_each_text(t.metadata)), '{}'), \
    coalesce((SELECT array_agg(value ORDER BY key) FROM jsonb_each_text(t.metadata)), '{}')";

impl PgStore {
    /// A single transaction by id, or `None` if it does not exist.
    pub async fn transaction(&self, id: TxId) -> Result<Option<RecordedTransaction>, StoreError> {
        let id = i64::try_from(id.0).map_err(backend)?;
        let header: Option<HeaderRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {HEADER_COLUMNS} FROM transactions AS t WHERE t.id = $1"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        Ok(self.hydrate(header.into_iter().collect()).await?.pop())
    }

    pub async fn transactions(
        &self,
        filter: &HistoryFilter,
        page: &Page,
    ) -> Result<HistoryPage, StoreError> {
        let account = filter.account.as_ref().map(AccountAddress::to_string);
        let asset = filter.asset.as_ref().map(AssetId::to_string);
        let (meta_key, meta_value) = filter.metadata.clone().unzip();
        let before = page
            .before
            .map(|id| i64::try_from(id.0))
            .transpose()
            .map_err(backend)?;

        // One row more than the page size tells whether an older page exists.
        let mut headers: Vec<HeaderRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {HEADER_COLUMNS} \
             FROM transactions AS t \
             WHERE ($1::bigint IS NULL OR t.id < $1) \
               AND ($2::bigint IS NULL OR t.created_at >= to_timestamp($2)) \
               AND ($3::bigint IS NULL OR t.created_at < to_timestamp($3)) \
               AND ($4::text IS NULL OR t.metadata @> jsonb_build_object($4::text, $5::text)) \
               AND ($6::text IS NULL \
                    OR EXISTS (SELECT 1 FROM postings AS p WHERE p.tx_id = t.id AND p.source = $6 \
                               AND ($7::text IS NULL OR p.asset = $7)) \
                    OR EXISTS (SELECT 1 FROM postings AS p WHERE p.tx_id = t.id AND p.destination = $6 \
                               AND ($7::text IS NULL OR p.asset = $7))) \
               AND ($7::text IS NULL \
                    OR EXISTS (SELECT 1 FROM postings AS p WHERE p.tx_id = t.id AND p.asset = $7)) \
             ORDER BY t.id DESC \
             LIMIT $8"
        )))
        .bind(before)
        .bind(filter.from)
        .bind(filter.to)
        .bind(meta_key)
        .bind(meta_value)
        .bind(account)
        .bind(asset)
        .bind(i64::from(page.limit) + 1)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;

        let has_more = headers.len() > page.limit as usize;
        headers.truncate(page.limit as usize);

        let items = self.hydrate(headers).await?;
        let next_cursor = if has_more {
            items.last().map(|item| item.id)
        } else {
            None
        };
        Ok(HistoryPage { items, next_cursor })
    }

    /// Loads the postings of `headers` and assembles the transactions, in the same order.
    async fn hydrate(
        &self,
        headers: Vec<HeaderRow>,
    ) -> Result<Vec<RecordedTransaction>, StoreError> {
        let ids: Vec<i64> = headers.iter().map(|header| header.0).collect();
        let rows: Vec<(i64, String, String, String, String)> = sqlx::query_as(
            "SELECT tx_id, source, destination, asset, amount::text FROM postings \
             WHERE tx_id = ANY($1) ORDER BY tx_id, idx",
        )
        .bind(&ids)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        let mut postings: BTreeMap<i64, Vec<Posting>> = BTreeMap::new();
        for (tx_id, source, destination, asset, amount) in rows {
            postings.entry(tx_id).or_default().push(posting(
                &source,
                &destination,
                &asset,
                &amount,
            )?);
        }

        let mut items = Vec::with_capacity(headers.len());
        for (id, reverts, reverted_by, created_at, keys, values) in headers {
            let metadata: Metadata = keys.into_iter().zip(values).collect();
            let transaction = Transaction::new(postings.remove(&id).unwrap_or_default(), metadata)
                .map_err(backend)?;
            items.push(RecordedTransaction {
                id: tx_id(id)?,
                transaction,
                reverts: reverts.map(tx_id).transpose()?,
                reverted_by: reverted_by.map(tx_id).transpose()?,
                created_at,
            });
        }
        Ok(items)
    }
}
