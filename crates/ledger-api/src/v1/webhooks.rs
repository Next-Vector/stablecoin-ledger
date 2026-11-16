//! Webhook subscriptions (FR-50), secret rotation (FR-54), the delivery log and redelivery
//! (FR-56), and SSRF checks on receiver URLs (FR-57). Secrets are returned only when created or
//! rotated; at rest they are sealed with the service key.

use axum::Json;
use axum::extract::State;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use ledger_store::{
    DeliveryAttempt, DeliveryStatus, EventDelivery, EventType, WebhookChanges, WebhookEndpoint,
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use super::dto::EmptyRequest;
use super::idempotency::{self, Outcome};
use crate::AppState;
use crate::error::{ApiError, ErrorBody};
use crate::extract::{Mutation, PathParams, QueryParams};
use crate::ssrf::{self, UrlPolicy};

const MAX_URL_LEN: usize = 2048;

/// Event types a subscription can receive (FR-51).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum EventTypeBody {
    #[serde(rename = "transaction.created")]
    TransactionCreated,
    #[serde(rename = "transaction.reverted")]
    TransactionReverted,
    #[serde(rename = "deposit.seen")]
    DepositSeen,
    #[serde(rename = "deposit.confirmed")]
    DepositConfirmed,
    #[serde(rename = "deposit.orphaned")]
    DepositOrphaned,
    #[serde(rename = "withdrawal.broadcast")]
    WithdrawalBroadcast,
    #[serde(rename = "withdrawal.confirmed")]
    WithdrawalConfirmed,
    #[serde(rename = "withdrawal.failed")]
    WithdrawalFailed,
    #[serde(rename = "hold.expired")]
    HoldExpired,
    #[serde(rename = "account.frozen")]
    AccountFrozen,
    #[serde(rename = "account.unfrozen")]
    AccountUnfrozen,
    #[serde(rename = "reconciliation.mismatch")]
    ReconciliationMismatch,
}

impl From<EventTypeBody> for EventType {
    fn from(event: EventTypeBody) -> Self {
        match event {
            EventTypeBody::TransactionCreated => EventType::TransactionCreated,
            EventTypeBody::TransactionReverted => EventType::TransactionReverted,
            EventTypeBody::DepositSeen => EventType::DepositSeen,
            EventTypeBody::DepositConfirmed => EventType::DepositConfirmed,
            EventTypeBody::DepositOrphaned => EventType::DepositOrphaned,
            EventTypeBody::WithdrawalBroadcast => EventType::WithdrawalBroadcast,
            EventTypeBody::WithdrawalConfirmed => EventType::WithdrawalConfirmed,
            EventTypeBody::WithdrawalFailed => EventType::WithdrawalFailed,
            EventTypeBody::HoldExpired => EventType::HoldExpired,
            EventTypeBody::AccountFrozen => EventType::AccountFrozen,
            EventTypeBody::AccountUnfrozen => EventType::AccountUnfrozen,
            EventTypeBody::ReconciliationMismatch => EventType::ReconciliationMismatch,
        }
    }
}

impl From<EventType> for EventTypeBody {
    fn from(event: EventType) -> Self {
        match event {
            EventType::TransactionCreated => EventTypeBody::TransactionCreated,
            EventType::TransactionReverted => EventTypeBody::TransactionReverted,
            EventType::DepositSeen => EventTypeBody::DepositSeen,
            EventType::DepositConfirmed => EventTypeBody::DepositConfirmed,
            EventType::DepositOrphaned => EventTypeBody::DepositOrphaned,
            EventType::WithdrawalBroadcast => EventTypeBody::WithdrawalBroadcast,
            EventType::WithdrawalConfirmed => EventTypeBody::WithdrawalConfirmed,
            EventType::WithdrawalFailed => EventTypeBody::WithdrawalFailed,
            EventType::HoldExpired => EventTypeBody::HoldExpired,
            EventType::AccountFrozen => EventTypeBody::AccountFrozen,
            EventType::AccountUnfrozen => EventTypeBody::AccountUnfrozen,
            EventType::ReconciliationMismatch => EventTypeBody::ReconciliationMismatch,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateWebhookRequest {
    /// An http(s) URL that receives the events, at most 2048 characters.
    #[schema(example = "https://merchant.example/ledger-webhooks")]
    pub url: String,
    /// At least one event type, without repeats.
    pub event_types: Vec<EventTypeBody>,
    /// Default true.
    pub enabled: Option<bool>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateWebhookRequest {
    pub url: Option<String>,
    pub event_types: Option<Vec<EventTypeBody>>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct WebhookResponse {
    pub id: u64,
    pub url: String,
    pub event_types: Vec<EventTypeBody>,
    pub enabled: bool,
    /// After a rotation: Unix seconds until which the previous secret still signs.
    pub previous_secret_expires_at: Option<i64>,
    /// Unix seconds.
    pub created_at: i64,
    /// The signing secret; present only in the responses that create or rotate it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "whsec_…")]
    pub secret: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct WebhookPageResponse {
    pub items: Vec<WebhookResponse>,
    /// Pass as `cursor` for the next page; null on the last page.
    pub next_cursor: Option<u64>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct ListWebhooksQuery {
    /// Page size, 1 to 500 (default 50).
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    pub cursor: Option<u64>,
}

/// Create a webhook subscription
///
/// Returns the signing secret once; store it to verify `X-Ledger-Signature`.
#[utoipa::path(
    post, path = "/v1/webhooks", tag = "webhooks",
    params(("Idempotency-Key" = Option<String>, Header, description = "Retry-safe key (1–255 visible ASCII); a repeat returns the first result")),
    request_body = CreateWebhookRequest,
    security(("api_key" = [])),
    responses(
        (status = 201, body = WebhookResponse),
        (status = 400, description = "VALIDATION_ERROR", body = ErrorBody),
        (status = 409, description = "IDEMPOTENCY_CONFLICT: the key was used for a different request", body = ErrorBody),
    )
)]
pub async fn create_webhook(
    State(state): State<AppState>,
    Mutation { body, key }: Mutation<CreateWebhookRequest>,
) -> Result<Response, ApiError> {
    validate_url(&state, &body.url).await?;
    let event_types = event_types(body.event_types)?;
    let operation = state.store.create_webhook(
        &state.secrets,
        &body.url,
        &event_types,
        body.enabled.unwrap_or(true),
        key.as_ref(),
    );
    with_secret(&state, StatusCode::CREATED, key.as_ref(), operation).await
}

/// List webhook subscriptions
#[utoipa::path(
    get, path = "/v1/webhooks", tag = "webhooks",
    params(ListWebhooksQuery),
    security(("api_key" = [])),
    responses(
        (status = 200, body = WebhookPageResponse),
        (status = 400, description = "VALIDATION_ERROR", body = ErrorBody),
    )
)]
pub async fn list_webhooks(
    State(state): State<AppState>,
    QueryParams(query): QueryParams<ListWebhooksQuery>,
) -> Result<Json<WebhookPageResponse>, ApiError> {
    let limit = query.limit.unwrap_or(ledger_store::Page::DEFAULT_LIMIT);
    ledger_store::Page::new(limit, None)
        .map_err(|error| ApiError::invalid_field("limit", error))?;
    let (items, next_cursor) = state.store.webhooks(query.cursor, limit).await?;
    Ok(Json(WebhookPageResponse {
        items: items.into_iter().map(|e| response(e, None)).collect(),
        next_cursor,
    }))
}

/// Update a webhook subscription
///
/// Changes only the given fields.
#[utoipa::path(
    patch, path = "/v1/webhooks/{id}", tag = "webhooks",
    params(("id" = u64, Path), ("Idempotency-Key" = Option<String>, Header, description = "Retry-safe key (1–255 visible ASCII); a repeat returns the first result")),
    request_body = UpdateWebhookRequest,
    security(("api_key" = [])),
    responses(
        (status = 200, body = WebhookResponse),
        (status = 400, description = "VALIDATION_ERROR", body = ErrorBody),
        (status = 404, description = "WEBHOOK_NOT_FOUND", body = ErrorBody),
        (status = 409, description = "IDEMPOTENCY_CONFLICT: the key was used for a different request", body = ErrorBody),
    )
)]
pub async fn update_webhook(
    State(state): State<AppState>,
    PathParams(id): PathParams<u64>,
    Mutation { body, key }: Mutation<UpdateWebhookRequest>,
) -> Result<Response, ApiError> {
    if let Some(url) = &body.url {
        validate_url(&state, url).await?;
    }
    let changes = WebhookChanges {
        url: body.url,
        event_types: body.event_types.map(event_types).transpose()?,
        enabled: body.enabled,
    };
    let operation = state.store.update_webhook(id, &changes, key.as_ref());
    let (endpoint, replayed) = match idempotency::run(&state.store, key.as_ref(), operation).await?
    {
        Outcome::Done(endpoint) => (endpoint, false),
        Outcome::Replay(resources) => (replayed_endpoint(&state, resources.webhook).await?, true),
    };
    Ok(idempotency::respond(
        Json(response(endpoint, None)),
        replayed,
    ))
}

/// Delete a webhook subscription
#[utoipa::path(
    delete, path = "/v1/webhooks/{id}", tag = "webhooks",
    params(("id" = u64, Path), ("Idempotency-Key" = Option<String>, Header, description = "Retry-safe key (1–255 visible ASCII); a repeat returns the first result")),
    security(("api_key" = [])),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, description = "WEBHOOK_NOT_FOUND", body = ErrorBody),
        (status = 409, description = "IDEMPOTENCY_CONFLICT: the key was used for a different request", body = ErrorBody),
    )
)]
pub async fn delete_webhook(
    State(state): State<AppState>,
    PathParams(id): PathParams<u64>,
    Mutation { key, .. }: Mutation<EmptyRequest>,
) -> Result<Response, ApiError> {
    let operation = state.store.delete_webhook(id, key.as_ref());
    let replayed = matches!(
        idempotency::run(&state.store, key.as_ref(), operation).await?,
        Outcome::Replay(_)
    );
    Ok(idempotency::respond(StatusCode::NO_CONTENT, replayed))
}

/// Rotate a webhook secret
///
/// Returns the new secret once. The previous secret keeps signing deliveries for 24 hours, so
/// receivers can switch without dropping events.
#[utoipa::path(
    post, path = "/v1/webhooks/{id}/rotate-secret", tag = "webhooks",
    params(("id" = u64, Path), ("Idempotency-Key" = Option<String>, Header, description = "Retry-safe key (1–255 visible ASCII); a repeat returns the first result")),
    security(("api_key" = [])),
    responses(
        (status = 200, body = WebhookResponse),
        (status = 404, description = "WEBHOOK_NOT_FOUND", body = ErrorBody),
        (status = 409, description = "IDEMPOTENCY_CONFLICT: the key was used for a different request", body = ErrorBody),
    )
)]
pub async fn rotate_secret(
    State(state): State<AppState>,
    PathParams(id): PathParams<u64>,
    Mutation { key, .. }: Mutation<EmptyRequest>,
) -> Result<Response, ApiError> {
    let operation = state
        .store
        .rotate_webhook_secret(&state.secrets, id, key.as_ref());
    with_secret(&state, StatusCode::OK, key.as_ref(), operation).await
}

/// Where a delivery stands.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryStatusBody {
    Pending,
    Succeeded,
    Failed,
}

impl From<DeliveryStatus> for DeliveryStatusBody {
    fn from(status: DeliveryStatus) -> Self {
        match status {
            DeliveryStatus::Pending => Self::Pending,
            DeliveryStatus::Succeeded => Self::Succeeded,
            DeliveryStatus::Failed => Self::Failed,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeliveryAttemptResponse {
    pub id: u64,
    /// The delivery this attempt belongs to; a redelivery starts a new one.
    pub delivery_id: u64,
    #[schema(example = "evt_0d1f…")]
    pub event_id: String,
    pub event_type: EventTypeBody,
    /// The attempt's number within its delivery, from 1.
    pub attempt: u32,
    /// Whether the receiver answered 2xx.
    pub succeeded: bool,
    /// The receiver's status code; null when there was no response.
    pub status_code: Option<u16>,
    pub latency_ms: u64,
    /// Why the attempt failed; null on success.
    pub error: Option<String>,
    /// Unix seconds when the attempt ended.
    pub attempted_at: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeliveryAttemptPageResponse {
    /// Newest first.
    pub items: Vec<DeliveryAttemptResponse>,
    /// Pass as `cursor` for the next (older) page; null on the last page.
    pub next_cursor: Option<u64>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct DeliveryLogQuery {
    /// Page size, 1 to 500 (default 50).
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    pub cursor: Option<u64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EventDeliveryResponse {
    pub id: u64,
    pub webhook_id: u64,
    pub status: DeliveryStatusBody,
    /// Attempts made so far.
    pub attempts: u32,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RedeliveryResponse {
    pub event_id: String,
    /// Every delivery of the event, oldest first, the queued ones included.
    pub deliveries: Vec<EventDeliveryResponse>,
}

/// Webhook delivery log
///
/// Every delivery attempt to this subscription, newest first: response code, latency and error.
#[utoipa::path(
    get, path = "/v1/webhooks/{id}/deliveries", tag = "webhooks",
    params(("id" = u64, Path), DeliveryLogQuery),
    security(("api_key" = [])),
    responses(
        (status = 200, body = DeliveryAttemptPageResponse),
        (status = 400, description = "VALIDATION_ERROR", body = ErrorBody),
        (status = 404, description = "WEBHOOK_NOT_FOUND", body = ErrorBody),
    )
)]
pub async fn delivery_log(
    State(state): State<AppState>,
    PathParams(id): PathParams<u64>,
    QueryParams(query): QueryParams<DeliveryLogQuery>,
) -> Result<Json<DeliveryAttemptPageResponse>, ApiError> {
    let limit = query.limit.unwrap_or(ledger_store::Page::DEFAULT_LIMIT);
    ledger_store::Page::new(limit, None)
        .map_err(|error| ApiError::invalid_field("limit", error))?;
    if state.store.webhook(id).await?.is_none() {
        return Err(ledger_store::WebhookError::NotFound(id).into());
    }
    let (items, next_cursor) = state
        .store
        .delivery_attempts(id, query.cursor, limit)
        .await?;
    Ok(Json(DeliveryAttemptPageResponse {
        items: items
            .into_iter()
            .map(attempt_response)
            .collect::<Result<_, _>>()?,
        next_cursor,
    }))
}

/// Redeliver a webhook event
///
/// Queues the event again, with a fresh retry schedule, for every subscription it was delivered
/// to; a subscription that still has a pending delivery of it is left as is.
#[utoipa::path(
    post, path = "/v1/webhook-events/{id}/redeliver", tag = "webhooks",
    params(("id" = String, Path), ("Idempotency-Key" = Option<String>, Header, description = "Retry-safe key (1–255 visible ASCII); a repeat returns the first result")),
    security(("api_key" = [])),
    responses(
        (status = 202, body = RedeliveryResponse),
        (status = 404, description = "WEBHOOK_EVENT_NOT_FOUND", body = ErrorBody),
        (status = 409, description = "IDEMPOTENCY_CONFLICT: the key was used for a different request", body = ErrorBody),
    )
)]
pub async fn redeliver(
    State(state): State<AppState>,
    PathParams(event_id): PathParams<String>,
    Mutation { key, .. }: Mutation<EmptyRequest>,
) -> Result<Response, ApiError> {
    let operation = state.store.redeliver(&event_id, key.as_ref());
    // The key covers the path, so a replay is about this same event.
    let replayed = matches!(
        idempotency::run(&state.store, key.as_ref(), operation).await?,
        Outcome::Replay(_)
    );
    let deliveries = state.store.event_deliveries(&event_id).await?;
    let body = RedeliveryResponse {
        event_id,
        deliveries: deliveries.into_iter().map(delivery_response).collect(),
    };
    Ok(idempotency::respond(
        (StatusCode::ACCEPTED, Json(body)),
        replayed,
    ))
}

fn attempt_response(attempt: DeliveryAttempt) -> Result<DeliveryAttemptResponse, ApiError> {
    let event_type: EventType = attempt.event_type.parse().map_err(|_| {
        tracing::error!(event_type = %attempt.event_type, "stored event type is unknown");
        ApiError::internal()
    })?;
    Ok(DeliveryAttemptResponse {
        id: attempt.id,
        delivery_id: attempt.delivery_id,
        event_id: attempt.event_id,
        event_type: event_type.into(),
        attempt: attempt.attempt,
        succeeded: attempt.succeeded,
        status_code: attempt.status_code,
        latency_ms: attempt.latency_ms,
        error: attempt.error,
        attempted_at: attempt.attempted_at,
    })
}

fn delivery_response(delivery: EventDelivery) -> EventDeliveryResponse {
    EventDeliveryResponse {
        id: delivery.id,
        webhook_id: delivery.endpoint_id,
        status: delivery.status.into(),
        attempts: delivery.attempts,
    }
}

/// Runs an operation that returns a subscription with its new secret. A replay answers with the
/// same subscription and its current secret.
async fn with_secret(
    state: &AppState,
    status: StatusCode,
    key: Option<&ledger_store::IdempotencyKey>,
    operation: impl Future<
        Output = Result<
            (WebhookEndpoint, String),
            ledger_store::OpError<ledger_store::WebhookError>,
        >,
    >,
) -> Result<Response, ApiError> {
    let ((endpoint, secret), replayed) =
        match idempotency::run(&state.store, key, operation).await? {
            Outcome::Done(created) => (created, false),
            Outcome::Replay(resources) => {
                let endpoint = replayed_endpoint(state, resources.webhook).await?;
                let secret = state
                    .store
                    .signing_secrets(&state.secrets, endpoint.id)
                    .await?
                    .into_iter()
                    .next();
                ((endpoint, idempotency::recorded(secret)?), true)
            }
        };
    Ok(idempotency::respond(
        (status, Json(response(endpoint, Some(secret)))).into_response(),
        replayed,
    ))
}

async fn replayed_endpoint(state: &AppState, id: Option<u64>) -> Result<WebhookEndpoint, ApiError> {
    let id = idempotency::recorded(id)?;
    state
        .store
        .webhook(id)
        .await?
        .ok_or_else(|| ledger_store::WebhookError::NotFound(id).into())
}

fn response(endpoint: WebhookEndpoint, secret: Option<String>) -> WebhookResponse {
    WebhookResponse {
        id: endpoint.id,
        url: endpoint.url,
        event_types: endpoint.event_types.into_iter().map(Into::into).collect(),
        enabled: endpoint.enabled,
        previous_secret_expires_at: endpoint.previous_secret_expires_at,
        created_at: endpoint.created_at,
        secret,
    }
}

/// An http(s) URL with a host, at most 2048 characters, that in production resolves to public
/// addresses only (FR-57).
async fn validate_url(state: &AppState, url: &str) -> Result<(), ApiError> {
    if url.len() > MAX_URL_LEN {
        return Err(ApiError::invalid_field(
            "url",
            format!("must be at most {MAX_URL_LEN} characters"),
        ));
    }
    let uri: Uri = url
        .parse()
        .map_err(|_| ApiError::invalid_field("url", "is not a valid URL"))?;
    let scheme_ok = matches!(uri.scheme_str(), Some("http" | "https"));
    let host_ok = uri.host().is_some_and(|host| !host.is_empty());
    if !scheme_ok || !host_ok {
        return Err(ApiError::invalid_field(
            "url",
            "must be an http or https URL with a host",
        ));
    }
    if state.url_policy == UrlPolicy::PublicOnly {
        ssrf::check_receiver(url)
            .await
            .map_err(|reason| ApiError::invalid_field("url", reason))?;
    }
    Ok(())
}

/// At least one event type, without repeats.
fn event_types(events: Vec<EventTypeBody>) -> Result<Vec<EventType>, ApiError> {
    if events.is_empty() {
        return Err(ApiError::invalid_field("event_types", "must not be empty"));
    }
    let mut unique: Vec<EventType> = Vec::with_capacity(events.len());
    for event in events.into_iter().map(EventType::from) {
        if unique.contains(&event) {
            return Err(ApiError::invalid_field(
                "event_types",
                format!("{event} is listed twice"),
            ));
        }
        unique.push(event);
    }
    Ok(unique)
}
