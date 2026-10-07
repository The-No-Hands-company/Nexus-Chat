//! # nexus-api
//!
//! REST API layer for Nexus. Provides HTTP endpoints for all CRUD operations,
//! authentication, and client-facing functionality.
#![allow(clippy::pedantic)]

pub mod email;
pub mod middleware;
pub mod push_sender;
pub mod routes;

use axum::Router;
use metrics_exporter_prometheus::PrometheusHandle;
use nexus_common::gateway_event::GatewayEvent;
use nexus_db::{Database, search::SearchClient, storage::StorageClient};
use nexus_federation::{ServerKeyPair, client::FederationClient};
use nexus_voice::state::VoiceStateManager;
use std::{sync::Arc, time::Instant};
use tokio::sync::broadcast;

use crate::email::EmailService;

/// Shared application state available to all route handlers.
#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    /// Broadcast sender to push events to the WebSocket gateway.
    /// API mutations (message create, channel update, etc.) use this
    /// to notify all connected clients in real-time.
    pub gateway_tx: broadcast::Sender<GatewayEvent>,
    /// Voice state manager — shared with the voice server for REST-based
    /// voice operations (state queries, moderation actions).
    pub voice_state: VoiceStateManager,
    /// MinIO / S3-compatible object storage client for file uploads.
    pub storage: StorageClient,
    /// MeiliSearch client for full-text message search.
    pub search: SearchClient,
    // ── v0.8 Federation ──────────────────────────────────────────────────────
    /// Public server name used in federation (e.g. "nexus.example.com").
    pub server_name: String,
    /// Active Ed25519 signing key for all outbound federation requests.
    pub federation_key: Arc<ServerKeyPair>,
    /// Signed HTTP client for outbound server-to-server federation requests.
    pub federation_client: Arc<FederationClient>,
    /// Monotonic timestamp captured at process start — used to compute uptime.
    pub started_at: Instant,
    /// Prometheus metrics handle — render current metrics text with `prometheus.render()`.
    pub prometheus: PrometheusHandle,
    /// Email delivery service (Resend). Disabled when `NEXUS__EMAIL__API_KEY` is unset.
    pub email: EmailService,
    /// Web Push sender (VAPID). `None` when `NEXUS__PUSH__VAPID_PRIVATE_KEY` is not set.
    /// Stored here so the P-256 key material is loaded once at startup, not per-request.
    pub push: Option<crate::push_sender::PushSender>,
    /// VAPID public key (base64url) — derived from `push` and served at
    /// `GET /push/vapid-public-key` so browser clients can subscribe.
    pub vapid_public_key: Option<String>,
}

/// Span for one HTTP request: method and matched route template only. Never the
/// URI, query string, or path-parameter values (zero-retention log rule).
pub(crate) fn request_span<B>(req: &axum::http::Request<B>) -> tracing::Span {
    let route = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|m| m.as_str())
        .unwrap_or("-");
    tracing::info_span!("request", method = %req.method(), route = %route)
}

#[cfg(test)]
mod request_span_tests {
    use super::request_span;
    use axum::{routing::get, Router};
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    #[derive(Clone)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn span_has_no_query_or_path_values() {
        let buf = Buf(Arc::new(Mutex::new(Vec::new())));
        let w = buf.clone();
        let sub = tracing_subscriber::fmt()
            .with_writer(move || w.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        let _g = tracing::subscriber::set_default(sub);

        let app = Router::new()
            .route(
                "/users/{id}/x",
                get(|| async {
                    tracing::info!("handled");
                    "ok"
                }),
            )
            .layer(tower_http::trace::TraceLayer::new_for_http().make_span_with(request_span));
        let req = axum::http::Request::builder()
            .uri("/users/user-canary-77/x?q=secret-canary")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(out.contains("handled"), "capture saw nothing: {out}");
        assert!(out.contains("method=GET"), "{out}");
        assert!(out.contains("route=/users/{id}/x"), "{out}");
        assert!(!out.contains("secret-canary"), "{out}");
        assert!(!out.contains("user-canary-77"), "{out}");
    }
}

/// Build the complete API router with all routes and middleware.
pub fn build_router(state: AppState) -> Router {
    // Create the Arc up-front so we can share it as an Axum Extension
    // (`combined_auth_middleware` reads it to look up bot tokens).
    let arc_state = Arc::new(state);

    let api_routes = Router::new()
        .merge(routes::users::router())
        .merge(routes::servers::router())
        .merge(routes::channels::router())
        .merge(routes::messages::router())
        .merge(routes::dms::router())
        .merge(routes::voice::router())
        .merge(routes::health::router())
        // v0.4 Rich Features
        .merge(routes::uploads::router())
        .merge(routes::threads::router())
        .merge(routes::emoji::router())
        .merge(routes::search::router())
        .merge(routes::presence::router())
        .merge(routes::relationships::router())
        // v0.5 Encryption
        .merge(routes::keys::router())
        .merge(routes::e2ee::router())
        .merge(routes::phantom::router())
        .merge(routes::verification::router())
        // v0.7 Extensibility
        .merge(routes::bots::router())
        .merge(routes::webhooks::router())
        .merge(routes::slash_commands::router())
        .merge(routes::extensibility::router())
        // v0.8 Federation — client-facing directory endpoints
        .merge(routes::directory::router())
        // v0.9.7 Account Security
        .merge(routes::sessions::router()) // session listing + revocation
        .merge(routes::email_verification::router()) // email verify + resend
        // v0.9.8 Moderation & Safety
        .merge(routes::moderation::router()) // audit log, kick, ban, timeout, reports, word filters
        // v0.12 Channel Type Completion
        .merge(routes::forum::router()) // forum posts + tags
        .merge(routes::stages::router()) // stage instances + speaker management
        // v0.13 Engagement Features
        .merge(routes::polls::router()) // polls — create, vote, results, end
        .merge(routes::scheduled_messages::router()) // scheduled messages CRUD
        .merge(routes::bookmarks::router()) // message bookmarks
        .merge(routes::drafts::router()) // per-channel drafts
        // v0.14 Platform Differentiation
        .merge(routes::forward::router()) // message forwarding
        .merge(routes::events::router()) // server events + RSVP
        .merge(routes::stickers::router()) // sticker packs
        .merge(routes::inline_query::router()) // inline bot suggestions
        // v0.15 Community Ecosystem
        .merge(routes::badges::router()) // user badges
        .merge(routes::boosters::router()) // server supporter tiers
        .merge(routes::canvas::router()) // canvas document channels
        // v0.16 Discovery & Monetization
        .merge(routes::discovery::router()) // server discovery browsing
        .merge(routes::monetization::router()) // creator monetization
        // v1.5 Collaboration & Productivity
        .merge(routes::tasks::router()) // task boards + checklists
        .merge(routes::calendar::router()) // calendar events + RSVP + ICS
        .merge(routes::file_versions::router()) // file versioning + quotas
        .merge(routes::ai_assists::router()) // on-device AI preferences + digests
        // v1.6 Multimedia & Expression
        .merge(routes::voice_notes::router()) // voice & video notes
        .merge(routes::stories::router()) // stories & ephemeral status
        .merge(routes::drawings::router()) // in-chat drawing & annotation
        .merge(routes::voice_settings::router()) // voice settings & music queue
        .merge(routes::media_gallery::router()) // media gallery browsing + filters
        .merge(routes::experience::router()) // full vs messaging-first profile
        // v1.7 Accessibility & Inclusivity
        .merge(routes::accessibility::router()) // accessibility settings
        .merge(routes::voice_captions::router()) // voice channel live captions
        .merge(routes::translations::router()) // message translations + TTS
        // v1.8 Ecosystem & Onboarding
        .merge(routes::importer::router()) // data import & bulk invite
        .merge(routes::onboarding::router()) // onboarding wizard & templates
        .merge(routes::admin_analytics::router()) // admin analytics snapshots
        .merge(routes::marketplace::router()) // plugin marketplace
        // v1.9 Scalability & Performance Hardening
        .merge(routes::scalability::router()) // scaling configs, SFU, federation batches, voice quality
        // v2.0 AI & Intelligence Layer
        .merge(routes::ai_layer::router()) // AI suggestions, toxicity, raid detection, consent
        // v2.1 Voice & Real-Time Collaboration
        .merge(routes::voice_collab::router()) // video layouts, live streams, breakout rooms, spatial audio
        // v2.2 User Growth & Retention
        .merge(routes::growth::router()) // recommendations, gamification, achievements, offline queue
        // v2.x Sustainability & Extensibility
        .merge(routes::sustainability::router()) // governance, protocol versions, security audits, tutorials
        // v0.8.5 Federation UX — admin management + cross-instance search
        .merge(routes::federation_admin::router()) // federation admin panel API
        .merge(routes::admin::router()) // instance admin: users, servers, overview
        .merge(routes::admin_users::router()) // instance-admin user/server management
        .merge(routes::push::router()) // Web Push subscriptions + VAPID key
        // Make Arc<AppState> available as an Axum Extension so that
        // `combined_auth_middleware` can perform DB lookups for bot tokens
        // without requiring `from_fn_with_state` on every sub-router.
        .layer(axum::Extension(arc_state.clone()));

    Router::new()
        .nest("/api/v1", api_routes)
        // v0.8 Federation — server-to-server endpoints (live outside /api/v1)
        .merge(routes::federation::federation_router())
        // Prometheus metrics — at /metrics (outside /api/v1 for standard scrape_configs)
        .merge(routes::metrics::router())
        // Local file serving (lite mode — no-op in full mode)
        .merge(routes::files::router())
        // Global request body limit (32 MiB).  Individual upload routes may raise
        // this with an inner RequestBodyLimitLayer; auth routes should keep it low.
        .layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024))
        .layer(build_cors_layer())
        .layer(
            tower_http::trace::TraceLayer::new_for_http()
                // One span per request carrying ONLY the method and the matched route
                // template (never the URI, query string, or path parameter values).
                .make_span_with(request_span)
                // Emit a record per response with status + latency_ms as structured fields
                .on_response(
                    tower_http::trace::DefaultOnResponse::new()
                        .level(tracing::Level::INFO)
                        .latency_unit(tower_http::LatencyUnit::Millis),
                )
                // Emit a record on request body failures
                .on_failure(
                    tower_http::trace::DefaultOnFailure::new().level(tracing::Level::ERROR),
                ),
        )
        .layer(tower_http::compression::CompressionLayer::new())
        .layer(axum::middleware::from_fn(middleware::security_headers))
        .layer(axum::middleware::from_fn(
            middleware::record_request_metrics,
        ))
        .with_state(arc_state)
}

// ── CORS helper ─────────────────────────────────────────────────────────────

/// Build the CORS layer, honouring the `NEXUS_CORS_ORIGINS` environment variable.
///
/// | `NEXUS_CORS_ORIGINS` | Behaviour |
/// |---|---|
/// | Not set | `*` — permissive default (works for dev, lite mode, and initial self-hosted installs) |
/// | `*` | Explicitly permissive |
/// | `https://app.nexus.chat,https://admin.nexus.chat` | Only the listed origins |
///
/// Methods and headers are always `*`; the restriction is origin-only because
/// Nexus uses JWT bearer tokens (not cookies) so CORS origin alone cannot be
/// the only security boundary.
fn build_cors_layer() -> tower_http::cors::CorsLayer {
    use tower_http::cors::CorsLayer;

    if let Ok(origins_env) = std::env::var("NEXUS_CORS_ORIGINS") {
        // Explicit wildcard bypass:
        if origins_env.trim() == "*" {
            return CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any);
        }

        let allowed: Vec<axum::http::HeaderValue> = origins_env
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();

        if !allowed.is_empty() {
            return CorsLayer::new()
                .allow_origin(allowed)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any);
        }
    }

    // Default: restrictive — deny cross-origin requests unless an explicit
    // allow-list is configured. This is the right default for production.
    //
    // To allow all origins during local development or for a fully public API,
    // set: NEXUS_CORS_ORIGINS=*
    //
    // Note: Nexus uses JWT bearer tokens, not cookies, so CORS cannot protect
    // against CSRF on its own. However, a restrictive default prevents the
    // browser from forwarding authentication tokens to cross-origin endpoints
    // from pages the user did not explicitly navigate to.
    tracing::warn!(
        "NEXUS_CORS_ORIGINS is not set. Cross-origin requests will be denied by default. \
         Set NEXUS_CORS_ORIGINS=* for a fully public API, or list allowed origins."
    );
    CorsLayer::new()
        .allow_methods(tower_http::cors::Any)
        .allow_headers(tower_http::cors::Any)
    // No .allow_origin() call = browsers block cross-origin requests
}
