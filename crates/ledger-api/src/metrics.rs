//! Prometheus metrics: a process-wide recorder, HTTP RED metrics, and the `/metrics` exposition.

use std::sync::OnceLock;
use std::time::Instant;

use axum::extract::{MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

use crate::AppState;

/// The global recorder can be installed once per process; every router shares it.
pub fn handle() -> PrometheusHandle {
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    HANDLE
        .get_or_init(|| {
            PrometheusBuilder::new()
                .install_recorder()
                .expect("the Prometheus recorder is installed only here")
        })
        .clone()
}

/// Counts requests and records their latency, labelled by method, route template and status.
/// The route is the matched template (`/v1/transactions/{id}`), never the raw path, so label
/// cardinality stays bounded; unknown paths are all `unmatched`.
pub async fn track(request: Request, next: Next) -> Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "unmatched".to_owned(), |path| path.as_str().to_owned());
    let method = request.method().to_string();
    let started = Instant::now();
    let response = next.run(request).await;
    let labels = [
        ("method", method),
        ("route", route),
        ("status", response.status().as_u16().to_string()),
    ];
    metrics::counter!("http_requests_total", &labels).increment(1);
    metrics::histogram!("http_request_duration_seconds", &labels)
        .record(started.elapsed().as_secs_f64());
    response
}

/// Prometheus text exposition.
pub async fn render(State(state): State<AppState>) -> String {
    state.metrics.render()
}
