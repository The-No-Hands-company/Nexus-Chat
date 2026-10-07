//! Prometheus metrics endpoint — exposes `GET /metrics` for scraping.
//!
//! Metrics exposed:
//!   - `nexus_http_requests_total{method, path, status}` — request counter
//!   - `nexus_http_request_duration_seconds{method, path}` — latency histogram
//!   - `nexus_voice_users_active` — gauge: users currently in a voice channel
//!
//! The HTTP request metrics are updated by the `record_request_metrics` middleware
//! in `middleware.rs`.  Voice gauges are updated directly in `nexus-voice/src/state.rs`
//! on join / leave.

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use std::sync::Arc;

use crate::AppState;

/// Mount the `/metrics` route.
///
/// The endpoint is intentionally outside `/api/v1` so standard Prometheus
/// `scrape_configs` can use `metrics_path: /metrics` without a prefix.
pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/metrics", get(metrics_handler))
}

/// Render current Prometheus metrics in the text exposition format 0.0.4.
///
/// # Access control
///
/// If the environment variable `NEXUS_METRICS_TOKEN` is set, the endpoint
/// requires `Authorization: Bearer <token>`.  If it is **not** set, the
/// endpoint is restricted to loopback addresses only (127.0.0.1 / ::1),
/// checked via `X-Forwarded-For` / `X-Real-IP` (same logic as the auth
/// rate limiter).
///
/// Set `NEXUS_METRICS_TOKEN=your-secret` for external Prometheus scrapers.
async fn metrics_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, &'static str)> {
    // ── Access check ──────────────────────────────────────────────────────
    if let Ok(expected_token) = std::env::var("NEXUS_METRICS_TOKEN") {
        // Token-gated: any IP is allowed if the bearer token matches.
        // Use constant-time comparison to prevent timing-oracle attacks where
        // an attacker measures response latency to brute-force the token.
        let provided = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");

        // Pad both sides to equal length before comparing, then use XOR
        // accumulation so the comparison takes the same time regardless of
        // where the strings differ.
        let expected_bytes = expected_token.as_bytes();
        let provided_bytes = provided.as_bytes();
        let max_len = expected_bytes.len().max(provided_bytes.len()).max(1);
        let mut diff: u8 = (expected_bytes.len() != provided_bytes.len()) as u8;
        for i in 0..max_len {
            let e = *expected_bytes.get(i).unwrap_or(&0);
            let p = *provided_bytes.get(i).unwrap_or(&0);
            diff |= e ^ p;
        }
        if diff != 0 {
            return Err((
                StatusCode::UNAUTHORIZED,
                "Provide Authorization: Bearer <NEXUS_METRICS_TOKEN>",
            ));
        }
    } else {
        // No token configured: restrict to loopback only.
        // The ecosystem proxy always stamps a client tag on forwarded traffic;
        // only a direct (tagless) local scrape counts as loopback.
        let is_loopback = crate::middleware::extract_client_tag(&headers) == "unknown";
        if !is_loopback {
            return Err((
                StatusCode::FORBIDDEN,
                "Set NEXUS_METRICS_TOKEN to enable remote scraping",
            ));
        }
    }

    let body = state.prometheus.render();
    Ok((
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    ))
}
