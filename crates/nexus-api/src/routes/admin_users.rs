//! Instance-admin user management routes.
//!
//! All routes require the `INSTANCE_ADMIN` user flag. They expose instance-wide
//! operations that go beyond per-server moderation: listing all users, suspending
//! or unsuspending accounts, and viewing instance-level statistics.
//!
//! Routes:
//! | Method | Path                                  | Description                    |
//! |--------|---------------------------------------|--------------------------------|
//! | GET    | /admin/users                          | List all local users (paginated)|
//! | GET    | /admin/users/stats                    | User count + registration rate |
//! | GET    | /admin/users/{user_id}                | Get a specific user            |
//! | POST   | /admin/users/{user_id}/suspend        | Set SUSPENDED flag             |
//! | POST   | /admin/users/{user_id}/unsuspend      | Clear SUSPENDED flag           |
//! | POST   | /admin/users/{user_id}/disable        | Set DISABLED flag (soft-delete)|
//! | GET    | /admin/servers                        | List all servers               |

use axum::http::HeaderMap;
use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    middleware,
    routing::{get, post},
};
use nexus_common::snowflake;
use nexus_common::{
    error::{NexusError, NexusResult},
    models::user::user_flags,
};
use nexus_db::repository::{audit_log, users};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    AppState,
    middleware::{AuthContext, check_rate_limit_with_fallback, extract_client_tag},
};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/admin/users", get(list_users))
        .route("/admin/users/stats", get(user_stats))
        .route("/admin/users/{id}", get(get_user))
        .route("/admin/users/{user_id}/suspend", post(suspend_user))
        .route("/admin/users/{user_id}/unsuspend", post(unsuspend_user))
        .route("/admin/users/{user_id}/disable", post(disable_user))
        .route("/admin/servers", get(list_servers))
        .route_layer(middleware::from_fn(crate::middleware::identity_middleware))
}

// ── Auth guard ────────────────────────────────────────────────────────────────

async fn require_instance_admin(pool: &sqlx::AnyPool, user_id: Uuid) -> NexusResult<()> {
    let user = users::find_by_id(pool, user_id)
        .await
        .map_err(NexusError::from)?
        .ok_or(NexusError::InvalidCredentials)?;

    if user.flags & user_flags::INSTANCE_ADMIN == 0 {
        return Err(NexusError::Forbidden);
    }
    Ok(())
}

// ── Request / response types ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct ListUsersQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
    q: Option<String>,
}
fn default_limit() -> i64 {
    50
}

#[derive(Serialize)]
struct AdminUserView {
    id: Uuid,
    username: String,
    display_name: Option<String>,
    email_hash: Option<String>,
    flags: i64,
    is_suspended: bool,
    is_disabled: bool,
    is_bot: bool,
    is_staff: bool,
    created_at: String,
}

impl From<nexus_common::models::user::User> for AdminUserView {
    fn from(u: nexus_common::models::user::User) -> Self {
        Self {
            id: u.id,
            username: u.username,
            display_name: u.display_name,
            email_hash: None, // never expose email in admin views
            flags: u.flags,
            is_suspended: u.flags & user_flags::SUSPENDED != 0,
            is_disabled: u.flags & user_flags::DISABLED != 0,
            is_bot: u.flags & user_flags::BOT != 0,
            is_staff: u.flags & user_flags::STAFF != 0,
            created_at: u.created_at.to_rfc3339(),
        }
    }
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /admin/users — list all local users with optional search and pagination
async fn list_users(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListUsersQuery>,
) -> NexusResult<Json<serde_json::Value>> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    let limit = params.limit.clamp(1, 200);
    let offset = params.offset.max(0);

    let user_list = users::list_users(&state.db.pool, limit, offset, params.q.as_deref())
        .await
        .map_err(NexusError::from)?;

    let total = users::count_users(&state.db.pool)
        .await
        .map_err(NexusError::from)?;

    let views: Vec<AdminUserView> = user_list.into_iter().map(Into::into).collect();

    Ok(Json(serde_json::json!({
        "users": views,
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}

/// GET /admin/users/stats — registration counts and flag breakdown
async fn user_stats(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> NexusResult<Json<serde_json::Value>> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    let row = sqlx::query(
        r#"SELECT
            COUNT(*)                                                         AS total,
            COUNT(*) FILTER (WHERE flags & $1 != 0)                         AS suspended,
            COUNT(*) FILTER (WHERE flags & $2 != 0)                         AS disabled,
            COUNT(*) FILTER (WHERE flags & $3 != 0)                         AS bots,
            COUNT(*) FILTER (WHERE created_at > NOW() - INTERVAL '24 hours') AS new_24h,
            COUNT(*) FILTER (WHERE created_at > NOW() - INTERVAL '7 days')  AS new_7d
           FROM users WHERE is_remote = FALSE"#,
    )
    .bind(user_flags::SUSPENDED)
    .bind(user_flags::DISABLED)
    .bind(user_flags::BOT)
    .fetch_one(&state.db.pool)
    .await
    .map_err(NexusError::from)?;

    Ok(Json(serde_json::json!({
        "total":      row.try_get::<i64, _>("total").unwrap_or(0),
        "suspended":  row.try_get::<i64, _>("suspended").unwrap_or(0),
        "disabled":   row.try_get::<i64, _>("disabled").unwrap_or(0),
        "bots":       row.try_get::<i64, _>("bots").unwrap_or(0),
        "new_24h":    row.try_get::<i64, _>("new_24h").unwrap_or(0),
        "new_7d":     row.try_get::<i64, _>("new_7d").unwrap_or(0),
    })))
}

/// GET /admin/users/:user_id
async fn get_user(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(user_id): Path<Uuid>,
) -> NexusResult<Json<AdminUserView>> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    let user = users::find_by_id(&state.db.pool, user_id)
        .await
        .map_err(NexusError::from)?
        .ok_or(NexusError::NotFound {
            resource: "User".into(),
        })?;

    Ok(Json(user.into()))
}

/// POST /admin/users/:user_id/suspend
async fn suspend_user(
    Extension(auth): Extension<AuthContext>,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Path(user_id): Path<Uuid>,
) -> NexusResult<Json<serde_json::Value>> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    // Rate limiting: 10 admin actions per minute per admin
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:admin:user:{}", auth.user_id),
        10,
        60,
    )
    .await?;
    check_rate_limit_with_fallback(state.db.redis.as_ref(), format!("rl:admin:tag:{tag}"), 20, 60)
        .await?;

    if user_id == auth.user_id {
        return Err(NexusError::Validation {
            message: "Cannot suspend your own account".into(),
        });
    }

    users::add_user_flags(&state.db.pool, user_id, user_flags::SUSPENDED)
        .await
        .map_err(NexusError::from)?;

    // Audit log
    let tag = extract_client_tag(&headers);
    let _ = audit_log::write_instance_entry(
        &state.db.pool,
        snowflake::generate_id(),
        auth.user_id,
        "USER_SUSPEND",
        Some("user"),
        Some(user_id),
        &serde_json::json!({"action": "suspended"}),
        None,
    )
    .await;

    tracing::warn!(
        admin = %auth.user_id,
        target = %user_id,
        "Instance admin suspended user account"
    );

    Ok(Json(serde_json::json!({ "suspended": true })))
}

/// POST /admin/users/:user_id/unsuspend
async fn unsuspend_user(
    Extension(auth): Extension<AuthContext>,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Path(user_id): Path<Uuid>,
) -> NexusResult<Json<serde_json::Value>> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    // Rate limiting: 10 admin actions per minute per admin
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:admin:user:{}", auth.user_id),
        10,
        60,
    )
    .await?;
    check_rate_limit_with_fallback(state.db.redis.as_ref(), format!("rl:admin:tag:{tag}"), 20, 60)
        .await?;

    users::remove_user_flags(&state.db.pool, user_id, user_flags::SUSPENDED)
        .await
        .map_err(NexusError::from)?;

    // Audit log
    let tag = extract_client_tag(&headers);
    let _ = audit_log::write_instance_entry(
        &state.db.pool,
        snowflake::generate_id(),
        auth.user_id,
        "USER_UNSUSPEND",
        Some("user"),
        Some(user_id),
        &serde_json::json!({"action": "unsuspended"}),
        None,
    )
    .await;

    tracing::info!(
        admin = %auth.user_id,
        target = %user_id,
        "Instance admin unsuspended user account"
    );

    Ok(Json(serde_json::json!({ "suspended": false })))
}

/// POST /admin/users/:user_id/disable — hard-disable (blocks login)
async fn disable_user(
    Extension(auth): Extension<AuthContext>,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Path(user_id): Path<Uuid>,
) -> NexusResult<Json<serde_json::Value>> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    // Rate limiting: 10 admin actions per minute per admin
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:admin:user:{}", auth.user_id),
        10,
        60,
    )
    .await?;
    check_rate_limit_with_fallback(state.db.redis.as_ref(), format!("rl:admin:tag:{tag}"), 20, 60)
        .await?;

    if user_id == auth.user_id {
        return Err(NexusError::Validation {
            message: "Cannot disable your own account".into(),
        });
    }

    users::add_user_flags(&state.db.pool, user_id, user_flags::DISABLED)
        .await
        .map_err(NexusError::from)?;

    // Audit log
    let tag = extract_client_tag(&headers);
    let _ = audit_log::write_instance_entry(
        &state.db.pool,
        snowflake::generate_id(),
        auth.user_id,
        "USER_DISABLE",
        Some("user"),
        Some(user_id),
        &serde_json::json!({"action": "disabled"}),
        None,
    )
    .await;

    tracing::warn!(
        admin = %auth.user_id,
        target = %user_id,
        "Instance admin disabled user account"
    );

    Ok(Json(serde_json::json!({ "disabled": true })))
}

/// GET /admin/servers — list all servers on this instance
async fn list_servers(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListUsersQuery>,
) -> NexusResult<Json<serde_json::Value>> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    let limit = params.limit.clamp(1, 200);
    let offset = params.offset.max(0);

    let rows = sqlx::query(
        "SELECT id::text, name, description, member_count, is_public, \
                created_at::text, owner_id::text \
         FROM servers \
         ORDER BY member_count DESC NULLS LAST, created_at DESC \
         LIMIT $1 OFFSET $2",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db.pool)
    .await
    .map_err(NexusError::from)?;

    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM servers")
        .fetch_one(&state.db.pool)
        .await
        .map_err(NexusError::from)?;

    let servers: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "id":           r.try_get::<String, _>("id").unwrap_or_default(),
                "name":         r.try_get::<String, _>("name").unwrap_or_default(),
                "description":  r.try_get::<Option<String>, _>("description").unwrap_or_default(),
                "member_count": r.try_get::<i64, _>("member_count").unwrap_or(0),
                "is_public":    r.try_get::<bool, _>("is_public").unwrap_or(false),
                "owner_id":     r.try_get::<String, _>("owner_id").unwrap_or_default(),
                "created_at":   r.try_get::<String, _>("created_at").unwrap_or_default(),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "servers": servers,
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}
