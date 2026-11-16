//! Writing webhook events to the transactional outbox (FR-51, FR-52, FR-55). Every function
//! here runs inside the caller's DB transaction, next to the change the event describes; the
//! payload is built in SQL from the rows just written, and the deliveries to the subscribed
//! endpoints are queued with it.

use ledger_core::{HoldId, StoreError};
use sqlx::PgConnection;

use crate::{EventType, backend};

/// Appends an event for `resource` with the next sequence of that resource. `data` is a SQL
/// expression producing the JSONB payload from the bound parameters `$3`, `$4`, ….
async fn emit(
    db: &mut PgConnection,
    kind: EventType,
    resource: &str,
    data: &'static str,
    bind: impl FnOnce(
        sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments>,
    ) -> sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments>,
) -> Result<(), StoreError> {
    // One writer per resource at a time, so sequences are gapless and in commit order.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('event:' || $1, 0))")
        .bind(resource)
        .execute(&mut *db)
        .await
        .map_err(backend)?;
    // The event, then one delivery per subscription enabled for its type (FR-53).
    let sql = format!(
        "WITH event AS ( \
             INSERT INTO webhook_events (id, type, resource, sequence, data) \
             SELECT 'evt_' || replace(gen_random_uuid()::text, '-', ''), $1, $2, \
                    coalesce((SELECT max(sequence) FROM webhook_events WHERE resource = $2), 0) + 1, \
                    {data} \
             RETURNING id \
         ) \
         INSERT INTO webhook_deliveries (event_id, endpoint_id) \
         SELECT event.id, endpoint.id FROM event \
         JOIN webhook_endpoints AS endpoint ON endpoint.enabled AND $1 = ANY (endpoint.event_types)"
    );
    let query = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(kind.as_str())
        .bind(resource);
    bind(query).execute(&mut *db).await.map_err(backend)?;
    Ok(())
}

/// `transaction.created` for a transaction just written, with its postings and metadata.
pub(crate) async fn transaction_created(db: &mut PgConnection, id: i64) -> Result<(), StoreError> {
    emit(
        db,
        EventType::TransactionCreated,
        &format!("transaction:{id}"),
        "(SELECT jsonb_build_object( \
             'transaction_id', t.id, \
             'postings', (SELECT jsonb_agg(jsonb_build_object( \
                     'source', p.source, 'destination', p.destination, \
                     'amount', p.amount::text, 'asset', p.asset) ORDER BY p.idx) \
                 FROM postings AS p WHERE p.tx_id = t.id), \
             'metadata', t.metadata, \
             'reverts', t.reverts) \
          FROM transactions AS t WHERE t.id = $3)",
        |query| query.bind(id),
    )
    .await
}

/// `transaction.reverted` on the original transaction, naming its reversal.
pub(crate) async fn transaction_reverted(
    db: &mut PgConnection,
    original: i64,
    reversal: i64,
) -> Result<(), StoreError> {
    emit(
        db,
        EventType::TransactionReverted,
        &format!("transaction:{original}"),
        "jsonb_build_object('transaction_id', $3::bigint, 'reversal_id', $4::bigint)",
        |query| query.bind(original).bind(reversal),
    )
    .await
}

/// `account.frozen` or `account.unfrozen`.
pub(crate) async fn account_freeze_changed(
    db: &mut PgConnection,
    address: &str,
    frozen: bool,
) -> Result<(), StoreError> {
    let kind = if frozen {
        EventType::AccountFrozen
    } else {
        EventType::AccountUnfrozen
    };
    emit(
        db,
        kind,
        &format!("account:{address}"),
        "jsonb_build_object('account', $3::text)",
        |query| query.bind(address.to_owned()),
    )
    .await
}

/// `hold.expired` for a hold just voided by expiry, naming the transaction that released it.
pub(crate) async fn hold_expired(
    db: &mut PgConnection,
    hold: HoldId,
    release: i64,
) -> Result<(), StoreError> {
    let hold = i64::try_from(hold.0).map_err(backend)?;
    emit(
        db,
        EventType::HoldExpired,
        &format!("hold:{hold}"),
        "(SELECT jsonb_build_object( \
             'hold_id', h.id, 'account', h.owner, 'asset', h.asset, \
             'amount', h.amount::text, 'transaction_id', $4::bigint) \
          FROM holds AS h WHERE h.id = $3)",
        |query| query.bind(hold).bind(release),
    )
    .await
}
