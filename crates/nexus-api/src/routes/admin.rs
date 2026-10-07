//! Instance admin — overview stats + generic flag manipulation.
//!
//! User management (list, suspend, disable) and server listing live in
//! `admin_users.rs` which was committed first.  This module adds the two
//! endpoints that weren't covered there:
//!
//!   GET  /admin/overview         — aggregate instance stats for the dashboard
//!   PATCH /admin/users/:id       — raw flag bit manipulation (set/clear)
//!   DELETE /admin/servers/:id    — permanently delete a server

use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    middleware,
    routing::{delete, get, patch},
};
use nexus_common::{
    error::{NexusError, NexusResult},
    models::user::user_flags,
};
use nexus_db::repository::audit_log;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::sync::Arc;
use uuid::Uuid;

use crate::{AppState, middleware::AuthContext};
use nexus_common::snowflake;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/admin/overview", get(get_overview))
        .route("/admin/instance-audit", get(get_instance_audit))
        .route("/admin/users/{id}", patch(update_user_flags))
        .route("/admin/servers/{id}", delete(delete_server))
        .route_layer(middleware::from_fn(crate::middleware::identity_middleware))
}

// ── Auth guard ────────────────────────────────────────────────────────────────

async fn require_instance_admin(pool: &sqlx::AnyPool, user_id: Uuid) -> NexusResult<()> {
    let row = sqlx::query("SELECT flags FROM users WHERE id = $1::uuid")
        .bind(user_id.to_string())
        .fetch_one(pool)
        .await
        .map_err(|_| NexusError::Forbidden)?;
    let flags: i64 = row.try_get("flags").unwrap_or(0);
    if flags & user_flags::INSTANCE_ADMIN == 0 {
        return Err(NexusError::Forbidden);
    }
    Ok(())
}

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct InstanceOverview {
    total_users: i64,
    active_users_24h: i64,
    suspended_users: i64,
    total_servers: i64,
    total_messages: i64,
    voice_connections: usize,
    uptime_secs: u64,
    version: &'static str,
    gateway_online: bool,
}

#[derive(Deserialize)]
struct UpdateFlagsRequest {
    set_flags: Option<i64>,
    clear_flags: Option<i64>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /admin/overview — aggregate stats for the dashboard home page.
async fn get_overview(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> NexusResult<Json<InstanceOverview>> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;
    let pool = &state.db.pool;

    let total_users: i64 = sqlx::query("SELECT COUNT(*) AS c FROM users WHERE is_remote = FALSE")
        .fetch_one(pool)
        .await
        .map(|r| r.try_get("c").unwrap_or(0))
        .unwrap_or(0);

    let active_users_24h: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM users \
         WHERE is_remote = FALSE AND updated_at > NOW() - INTERVAL '24 hours'",
    )
    .fetch_one(pool)
    .await
    .map(|r| r.try_get("c").unwrap_or(0))
    .unwrap_or(0);

    let suspended_users: i64 = sqlx::query(&format!(
        "SELECT COUNT(*) AS c FROM users WHERE flags & {} != 0",
        user_flags::SUSPENDED
    ))
    .fetch_one(pool)
    .await
    .map(|r| r.try_get("c").unwrap_or(0))
    .unwrap_or(0);

    let total_servers: i64 = sqlx::query("SELECT COUNT(*) AS c FROM servers")
        .fetch_one(pool)
        .await
        .map(|r| r.try_get("c").unwrap_or(0))
        .unwrap_or(0);

    let total_messages: i64 = sqlx::query("SELECT COUNT(*) AS c FROM messages")
        .fetch_one(pool)
        .await
        .map(|r| r.try_get("c").unwrap_or(0))
        .unwrap_or(0);

    let voice_connections = state.voice_state.stats().await.total_connections;
    let uptime_secs = state.started_at.elapsed().as_secs();

    Ok(Json(InstanceOverview {
        total_users,
        active_users_24h,
        suspended_users,
        total_servers,
        total_messages,
        voice_connections,
        uptime_secs,
        version: env!("CARGO_PKG_VERSION"),
        gateway_online: true,
    }))
}

/// PATCH /admin/users/:id — set or clear arbitrary flag bits.
///
/// Complement to the typed suspend/unsuspend/disable endpoints in admin_users.rs.
/// Useful for granting INSTANCE_ADMIN or other flags not covered by dedicated
/// action endpoints.
async fn update_user_flags(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(user_id): Path<Uuid>,
    Json(body): Json<UpdateFlagsRequest>,
) -> NexusResult<Json<serde_json::Value>> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    match (body.set_flags, body.clear_flags) {
        (None, None) => {
            return Err(NexusError::Validation {
                message: "Provide set_flags or clear_flags".into(),
            });
        }
        (Some(set), None) => {
            sqlx::query(
                "UPDATE users SET flags = flags | $1, updated_at = NOW() WHERE id = $2::uuid",
            )
            .bind(set)
            .bind(user_id.to_string())
            .execute(&state.db.pool)
            .await
            .map_err(|e| NexusError::Internal(e.into()))?;
        }
        (None, Some(clear)) => {
            sqlx::query(
                "UPDATE users SET flags = flags & ~$1, updated_at = NOW() WHERE id = $2::uuid",
            )
            .bind(clear)
            .bind(user_id.to_string())
            .execute(&state.db.pool)
            .await
            .map_err(|e| NexusError::Internal(e.into()))?;
        }
        (Some(set), Some(clear)) => {
            sqlx::query(
                "UPDATE users SET flags = (flags & ~$1) | $2, updated_at = NOW() WHERE id = $3::uuid")
                .bind(clear).bind(set).bind(user_id.to_string())
                .execute(&state.db.pool).await
                .map_err(|e| NexusError::Internal(e.into()))?;
        }
    }

    tracing::info!(admin = %auth.user_id, target = %user_id,
        set = ?body.set_flags, clear = ?body.clear_flags, "Admin updated user flags");

    // Write audit log for this critical instance-level operation
    let _ = audit_log::write_instance_entry(
        &state.db.pool,
        snowflake::generate_id(),
        auth.user_id,
        "USER_FLAGS_UPDATE",
        Some("user"),
        Some(user_id),
        &serde_json::json!({
            "set_flags": body.set_flags,
            "clear_flags": body.clear_flags,
        }),
        None,
    )
    .await;

    Ok(Json(serde_json::json!({ "updated": true })))
}

/// DELETE /admin/servers/:id — permanently destroy a server and all its data.
async fn delete_server(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
) -> NexusResult<Json<serde_json::Value>> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    let affected = sqlx::query("DELETE FROM servers WHERE id = $1::uuid")
        .bind(server_id.to_string())
        .execute(&state.db.pool)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .rows_affected();

    if affected == 0 {
        return Err(NexusError::NotFound {
            resource: "Server".into(),
        });
    }

    // Write audit log entry
    let _ = audit_log::write_entry(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        Some(auth.user_id),
        "SERVER_DELETE",
        Some("server"),
        Some(server_id),
        &serde_json::json!({ "affected_rows": affected }),
        Some("Instance admin permanently deleted server and all associated data"),
    )
    .await;

    tracing::warn!(admin = %auth.user_id, server = %server_id, "Admin deleted server");
    Ok(Json(serde_json::json!({ "deleted": true })))
}

// ── GET /admin/instance-audit ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct InstanceAuditQuery {
    action: Option<String>,
    actor_id: Option<Uuid>,
    target_type: Option<String>,
    #[serde(default = "default_audit_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
}
fn default_audit_limit() -> i64 {
    100
}

/// GET /admin/instance-audit — recent instance-wide admin actions.
///
/// Returns entries from `instance_audit_log`: USER_SUSPEND, USER_DISABLE,
/// ADMIN_GRANT, FLAG_UPDATE, SERVER_DELETE, etc.  Filterable by action,
/// actor_id, and target_type.
async fn get_instance_audit(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Query(q): Query<InstanceAuditQuery>,
) -> NexusResult<Json<serde_json::Value>> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    let entries = nexus_db::repository::audit_log::list_instance_entries(
        &state.db.pool,
        q.action.as_deref(),
        q.actor_id,
        q.target_type.as_deref(),
        None,
        q.limit.clamp(1, 200),
        q.offset.max(0),
    )
    .await
    .map_err(NexusError::Database)?;

    let items: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "id":          e.id,
                "actor_id":    e.actor_id,
                "action":      e.action,
                "target_type": e.target_type,
                "target_id":   e.target_id,
                "changes":     e.changes,
                "reason":      e.reason,
                "created_at":  e.created_at,
            })
        })
        .collect();

    Ok(Json(
        serde_json::json!({ "entries": items, "total": items.len() }),
    ))
}
