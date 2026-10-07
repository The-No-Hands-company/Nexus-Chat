//! Web Push notification routes.
//!
//! Browser clients subscribe by calling:
//!   1. `GET /push/vapid-public-key` → get the server's VAPID public key
//!   2. Call `PushManager.subscribe({ applicationServerKey })` in the browser
//!   3. `POST /push/subscriptions` with the subscription object → registers it
//!   4. `DELETE /push/subscriptions` with the endpoint → unregisters it
//!
//! The server sends push notifications via `PushSender` (see push_sender.rs)
//! when relevant events occur (new @mention, DM, etc.).

use axum::{
    Json, Router,
    extract::{Extension, State},
    middleware,
    routing::{get, post},
};
use nexus_common::error::{NexusError, NexusResult};
use nexus_db::repository::push_subscriptions;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{AppState, middleware::AuthContext};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/push/vapid-public-key", get(get_vapid_public_key))
        .route("/push/subscriptions", post(subscribe).delete(unsubscribe))
        .route_layer(middleware::from_fn(crate::middleware::identity_middleware))
}

// ── Response types ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct VapidKeyResponse {
    /// Base64url-encoded uncompressed P-256 public key (65 bytes → 87 chars).
    public_key: String,
}

// ── Request types ──────────────────────────────────────────────────────────────

/// The subscription object sent by the browser after `PushManager.subscribe()`.
#[derive(Deserialize)]
struct SubscribeRequest {
    /// The push service endpoint URL.
    endpoint: String,
    /// Browser-generated keys (from PushSubscription.toJSON()).
    keys: PushKeys,
}

#[derive(Deserialize)]
struct PushKeys {
    /// Base64url-encoded P-256 DH public key.
    p256dh: String,
    /// Base64url-encoded 16-byte auth secret.
    auth: String,
}

#[derive(Deserialize)]
struct UnsubscribeRequest {
    /// The endpoint to remove.
    endpoint: String,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /push/vapid-public-key
///
/// Returns the server's VAPID public key (base64url). Clients pass this to
/// `PushManager.subscribe({ applicationServerKey })` to bind subscriptions
/// to this server, preventing other servers from sending pushes to our clients.
async fn get_vapid_public_key(
    State(state): State<Arc<AppState>>,
) -> NexusResult<Json<VapidKeyResponse>> {
    let public_key = state.vapid_public_key.clone().ok_or_else(|| {
        NexusError::Internal(anyhow::anyhow!(
            "VAPID keys not configured. Set NEXUS__PUSH__VAPID_PRIVATE_KEY."
        ))
    })?;

    Ok(Json(VapidKeyResponse { public_key }))
}

/// POST /push/subscriptions
///
/// Register a new push subscription for the authenticated user.
/// Safe to call on every page load — uses UPSERT so re-subscribing is harmless.
async fn subscribe(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<SubscribeRequest>,
) -> NexusResult<Json<serde_json::Value>> {
    // Validate endpoint looks like a URL
    if !body.endpoint.starts_with("https://") {
        return Err(NexusError::Validation {
            message: "Push endpoint must be an HTTPS URL".into(),
        });
    }
    if body.endpoint.len() > 2048 {
        return Err(NexusError::Validation {
            message: "Push endpoint URL too long".into(),
        });
    }
    if body.keys.p256dh.is_empty() || body.keys.auth.is_empty() {
        return Err(NexusError::Validation {
            message: "Push keys (p256dh and auth) are required".into(),
        });
    }

    push_subscriptions::upsert(
        &state.db.pool,
        auth.user_id,
        &body.endpoint,
        &body.keys.p256dh,
        &body.keys.auth,
    )
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    tracing::info!("Push subscription registered");

    Ok(Json(serde_json::json!({ "subscribed": true })))
}

/// DELETE /push/subscriptions
///
/// Unregister a push subscription (user explicitly unsubscribes or switches browser).
async fn unsubscribe(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<UnsubscribeRequest>,
) -> NexusResult<Json<serde_json::Value>> {
    let removed =
        push_subscriptions::delete_by_endpoint(&state.db.pool, auth.user_id, &body.endpoint)
            .await
            .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(serde_json::json!({ "unsubscribed": removed })))
}
