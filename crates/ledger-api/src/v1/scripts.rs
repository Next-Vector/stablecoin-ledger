//! Ledger scripts: dry-run (FR-65) and the stored-script registry (FR-66). Running a script as a
//! transaction goes through `POST /v1/transactions`.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use ledger_core::Metadata;
use ledger_script::Vars;
use ledger_script::ast::Script;
use ledger_store::Page;

use super::dto::{
    DryRunRequest, DryRunResponse, ListScriptsQuery, ListVersionsQuery, PostingBody,
    RegisterScriptRequest, ScriptPageResponse, ScriptRef, ScriptResponse, ScriptRun,
    ScriptVersionsResponse,
};
use super::idempotency::{self, Outcome};
use crate::AppState;
use crate::error::{ApiError, ErrorBody, script_invalid, script_not_found};
use crate::extract::{Mutation, PathParams, QueryParams};

/// Dry-run a script
///
/// Compiles the script against current balances and checks the ledger rules, returning the
/// postings and the resulting balances. Nothing is written.
#[utoipa::path(
    post, path = "/v1/scripts/dry-run", tag = "scripts",
    request_body = DryRunRequest,
    security(("api_key" = [])),
    responses(
        (status = 200, body = DryRunResponse),
        (status = 400, description = "VALIDATION_ERROR or SCRIPT_INVALID", body = ErrorBody),
        (status = 404, description = "SCRIPT_NOT_FOUND", body = ErrorBody),
        (status = 422, description = "INSUFFICIENT_FUNDS, ACCOUNT_FROZEN or SCRIPT_REJECTED", body = ErrorBody),
    )
)]
pub async fn dry_run(
    State(state): State<AppState>,
    Mutation { body, .. }: Mutation<DryRunRequest>,
) -> Result<Json<DryRunResponse>, ApiError> {
    let (script, vars, audit) = load(&state, body.into_run()?).await?;
    let outcome = state.store.dry_run(&script, &vars, audit).await?;
    let mut balances: BTreeMap<String, super::dto::Balances> = BTreeMap::new();
    for change in outcome.balances {
        balances
            .entry(change.account.to_string())
            .or_default()
            .insert(change.asset.to_string(), change.balance.to_string());
    }
    Ok(Json(DryRunResponse {
        postings: outcome
            .transaction
            .postings()
            .iter()
            .map(PostingBody::from)
            .collect(),
        metadata: outcome.transaction.metadata().clone(),
        balances,
    }))
}

/// Register a script version
///
/// Stores the script under its name as the next version (starting at 1). It must parse.
#[utoipa::path(
    post, path = "/v1/scripts", tag = "scripts",
    params(("Idempotency-Key" = Option<String>, Header, description = "Retry-safe key (1–255 visible ASCII); a repeat returns the first result")),
    request_body = RegisterScriptRequest,
    security(("api_key" = [])),
    responses(
        (status = 201, body = ScriptResponse),
        (status = 400, description = "VALIDATION_ERROR or SCRIPT_INVALID", body = ErrorBody),
        (status = 409, description = "IDEMPOTENCY_CONFLICT: the key was used for a different request", body = ErrorBody),
    )
)]
pub async fn register_script(
    State(state): State<AppState>,
    Mutation { body, key }: Mutation<RegisterScriptRequest>,
) -> Result<Response, ApiError> {
    let operation = state
        .store
        .register_script(&body.name, &body.source, key.as_ref());
    let (script, replayed) = match idempotency::run(&state.store, key.as_ref(), operation).await? {
        Outcome::Done(script) => (script, false),
        Outcome::Replay(resources) => {
            let (name, version) = idempotency::recorded(resources.script)?;
            let script = state.store.script(&name, Some(version)).await?;
            (idempotency::recorded(script)?, true)
        }
    };
    Ok(idempotency::respond(
        (StatusCode::CREATED, Json(ScriptResponse::from(script))),
        replayed,
    ))
}

/// List scripts
///
/// The latest version of each stored script, by name, without sources.
#[utoipa::path(
    get, path = "/v1/scripts", tag = "scripts",
    params(ListScriptsQuery),
    security(("api_key" = [])),
    responses(
        (status = 200, body = ScriptPageResponse),
        (status = 400, description = "VALIDATION_ERROR", body = ErrorBody),
    )
)]
pub async fn list_scripts(
    State(state): State<AppState>,
    QueryParams(query): QueryParams<ListScriptsQuery>,
) -> Result<Json<ScriptPageResponse>, ApiError> {
    let (items, next_cursor) = state
        .store
        .scripts(query.cursor.as_deref(), page_size(query.limit)?)
        .await?;
    Ok(Json(ScriptPageResponse {
        items: items.into_iter().map(Into::into).collect(),
        next_cursor,
    }))
}

/// List versions of a script
///
/// Every version with its source, oldest first.
#[utoipa::path(
    get, path = "/v1/scripts/{name}/versions", tag = "scripts",
    params(("name" = String, Path, example = "checkout"), ListVersionsQuery),
    security(("api_key" = [])),
    responses(
        (status = 200, body = ScriptVersionsResponse),
        (status = 400, description = "VALIDATION_ERROR", body = ErrorBody),
        (status = 404, description = "SCRIPT_NOT_FOUND", body = ErrorBody),
    )
)]
pub async fn script_versions(
    State(state): State<AppState>,
    PathParams(name): PathParams<String>,
    QueryParams(query): QueryParams<ListVersionsQuery>,
) -> Result<Json<ScriptVersionsResponse>, ApiError> {
    let (items, next_cursor) = state
        .store
        .script_versions(&name, query.cursor, page_size(query.limit)?)
        .await?
        .ok_or_else(script_not_found)?;
    Ok(Json(ScriptVersionsResponse {
        items: items.into_iter().map(Into::into).collect(),
        next_cursor,
    }))
}

/// Parses the requested script (inline, or a stored version) and builds the audit metadata a run
/// records: the source, the variables and, for stored scripts, the name and version (FR-66).
pub async fn load(state: &AppState, run: ScriptRun) -> Result<(Script, Vars, Metadata), ApiError> {
    let mut audit = run.metadata;
    let source = match run.script {
        ScriptRef::Inline(source) => source,
        ScriptRef::Stored { name, version } => {
            let stored = state
                .store
                .script(&name, version)
                .await?
                .ok_or_else(script_not_found)?;
            audit.insert("script.name".to_owned(), stored.name);
            audit.insert("script.version".to_owned(), stored.version.to_string());
            stored.source
        }
    };
    let script = ledger_script::parse(&source).map_err(|error| script_invalid(&error))?;
    let vars_json = serde_json::to_string(&run.vars).map_err(|error| {
        tracing::error!(%error, "script variables do not serialize");
        ApiError::internal()
    })?;
    audit.insert("script.source".to_owned(), source);
    audit.insert("script.vars".to_owned(), vars_json);
    Ok((script, run.vars, audit))
}

fn page_size(limit: Option<u32>) -> Result<u32, ApiError> {
    let limit = limit.unwrap_or(Page::DEFAULT_LIMIT);
    Page::new(limit, None).map_err(|error| ApiError::invalid_field("limit", error))?;
    Ok(limit)
}
