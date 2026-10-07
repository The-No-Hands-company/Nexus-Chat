//! Session management routes — list and revoke active login sessions.
//!
//! Endpoints:
//!   GET    /auth/sessions           — list all active sessions for the current user
//!   DELETE /auth/sessions/{id}      — revoke a specific session
//!   DELETE /auth/sessions           — revoke all sessions except the current one

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::HeaderMap,
    http::StatusCode,
    middleware,
    routing::{delete, get},
};
use nexus_common::error::{NexusError, NexusResult};
use nexus_db::repository::sessions;
use serde::Serialize;
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    AppState,
    middleware::{AuthContext, check_rate_limit_with_fallback, extract_client_tag},
};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/auth/sessions",
            get(list_sessions).delete(revoke_all_sessions),
        )
        .route("/auth/sessions/{id}", delete(revoke_session))
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ))
}

// ── Response types ────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct SessionsResponse {
    sessions: Vec<sessions::SessionRow>,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// GET /api/v1/auth/sessions
///
/// List all active (non-expired) sessions for the authenticated user.
async fn list_sessions(
    Extension(auth_ctx): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> NexusResult<Json<SessionsResponse>> {
    let rows = sessions::list_sessions(&state.db.pool, auth_ctx.user_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(SessionsResponse { sessions: rows }))
}

/// DELETE /api/v1/auth/sessions/{id}
///
/// Revoke the session identified by `id`.
/// Returns 404 if the session does not exist or belongs to another user.
async fn revoke_session(
    Extension(auth_ctx): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(session_id_str): Path<String>,
) -> NexusResult<StatusCode> {
    // Rate limiting: 10 session revocations per user per 5 minutes
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:session_revoke:user:{}", auth_ctx.user_id),
        10,
        300,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:session_revoke:tag:{tag}"),
        20,
        300,
    )
    .await?;

    let session_id: Uuid = session_id_str.parse().map_err(|_| NexusError::Validation {
        message: "Invalid session ID format".into(),
    })?;

    let deleted = sessions::revoke_session(&state.db.pool, auth_ctx.user_id, session_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(NexusError::NotFound {
            resource: "Session".into(),
        })
    }
}

/// DELETE /api/v1/auth/sessions
///
/// Revoke all sessions except the current one (identified by the `jti` claim
/// in the caller's access token).
async fn revoke_all_sessions(
    Extension(auth_ctx): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> NexusResult<Json<RevokedCountResponse>> {
    // Rate limiting: 5 bulk session revocations per user per hour (high impact operation)
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:session_revoke_all:user:{}", auth_ctx.user_id),
        5,
        3600,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:session_revoke_all:tag:{tag}"),
        10,
        3600,
    )
    .await?;

    // Use the JTI from the access token claims to exclude the current session.
    let current_session_id = auth_ctx.session_id.unwrap_or(Uuid::nil());

    let count = sessions::revoke_all_except(&state.db.pool, auth_ctx.user_id, current_session_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(RevokedCountResponse { revoked: count }))
}

#[derive(Serialize)]
struct RevokedCountResponse {
    revoked: u64,
}
