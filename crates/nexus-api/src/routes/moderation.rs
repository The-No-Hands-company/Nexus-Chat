//! Moderation routes — audit log, kick, ban/unban, timeout, message reports,
//! and server-level content filters.
//!
//! All endpoints require authentication.  Actions that modify server state
//! additionally require the relevant permission (KICK_MEMBERS, BAN_MEMBERS,
//! MANAGE_MESSAGES, MANAGE_SERVER) or server ownership.

use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    middleware,
    routing::{delete, get, post, put},
};
use chrono::{Duration, Utc};
use nexus_common::{
    error::{NexusError, NexusResult},
    gateway_event::{GatewayEvent, event_types},
    models::server::Server,
    permissions::Permissions,
    snowflake,
};
use nexus_db::repository::{audit_log, members, moderation, roles, servers};
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

use crate::middleware::{check_rate_limit_with_fallback, extract_client_tag};
use crate::{AppState, middleware::AuthContext};
use axum::http::HeaderMap;

/// All moderation routes (require authentication).
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // ── Audit log ────────────────────────────────────────────────────────
        .route("/servers/{server_id}/audit-log", get(get_audit_log))
        // ── Kick ─────────────────────────────────────────────────────────────
        .route(
            "/servers/{server_id}/members/{user_id}/kick",
            post(kick_member),
        )
        // ── Bans ─────────────────────────────────────────────────────────────
        .route("/servers/{server_id}/bans", get(list_bans))
        .route(
            "/servers/{server_id}/bans/{user_id}",
            put(ban_member).delete(unban_member),
        )
        // ── Timeout ──────────────────────────────────────────────────────────
        .route(
            "/servers/{server_id}/members/{user_id}/timeout",
            put(set_timeout).delete(lift_timeout),
        )
        // ── Reports ──────────────────────────────────────────────────────────
        .route(
            "/channels/{channel_id}/messages/{message_id}/report",
            post(report_message),
        )
        .route("/servers/{server_id}/reports", get(list_reports))
        .route(
            "/servers/{server_id}/reports/{report_id}/resolve",
            post(resolve_report),
        )
        .route(
            "/servers/{server_id}/reports/{report_id}/dismiss",
            post(dismiss_report),
        )
        // ── Word filters ─────────────────────────────────────────────────────
        .route(
            "/servers/{server_id}/word-filters",
            get(list_word_filters).post(add_word_filter),
        )
        .route(
            "/servers/{server_id}/word-filters/{filter_id}",
            delete(remove_word_filter),
        )
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ))
}

// ── Permission helper ────────────────────────────────────────────────────────

/// Ensure the calling user has `required` permission in `server`, or return
/// `MissingPermission` / `Forbidden`.
///
/// The server owner implicitly has every permission.  For all other members,
/// the effective permission set is the union of all their assigned roles plus
/// the @everyone role.  `ADMINISTRATOR` overrides every other check.
async fn require_server_permission(
    pool: &sqlx::AnyPool,
    server: &Server,
    user_id: Uuid,
    required: Permissions,
) -> NexusResult<()> {
    if server.owner_id == user_id {
        return Ok(());
    }

    let member = members::find_member(pool, user_id, server.id)
        .await?
        .ok_or(NexusError::Forbidden)?;

    let all_roles = roles::list_server_roles(pool, server.id).await?;

    let effective = all_roles.iter().fold(Permissions::empty(), |acc, role| {
        if role.is_default || member.roles.contains(&role.id) {
            acc | Permissions::from_bits_truncate(role.permissions)
        } else {
            acc
        }
    });

    if effective.has(required) {
        Ok(())
    } else {
        Err(NexusError::MissingPermission {
            permission: format!("{required:?}"),
        })
    }
}

/// Fetch a server by ID and return `NotFound` if it doesn't exist.
async fn get_server_or_404(pool: &sqlx::AnyPool, server_id: Uuid) -> NexusResult<Server> {
    servers::find_by_id(pool, server_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Server".into(),
        })
}

// ── Audit log ─────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AuditLogQuery {
    action: Option<String>,
    actor: Option<Uuid>,
    limit: Option<i64>,
}

/// GET /api/v1/servers/{server_id}/audit-log
///
/// Returns the most recent audit log entries for a server.
/// Requires VIEW_AUDIT_LOG permission (or ownership).
async fn get_audit_log(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
    Query(q): Query<AuditLogQuery>,
) -> NexusResult<Json<Vec<audit_log::AuditLogEntry>>> {
    let server = get_server_or_404(&state.db.pool, server_id).await?;
    require_server_permission(
        &state.db.pool,
        &server,
        auth.user_id,
        Permissions::VIEW_AUDIT_LOG,
    )
    .await?;

    let limit = q.limit.unwrap_or(50).clamp(1, 100);
    let entries = audit_log::list_entries(
        &state.db.pool,
        server_id,
        q.action.as_deref(),
        q.actor,
        limit,
    )
    .await?;

    Ok(Json(entries))
}

// ── Kick ──────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ReasonBody {
    reason: Option<String>,
}

/// POST /api/v1/servers/{server_id}/members/{user_id}/kick
///
/// Remove a member from the server.  The target can rejoin via an invite.
/// Requires KICK_MEMBERS permission.
async fn kick_member(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((server_id, target_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<ReasonBody>,
) -> NexusResult<Json<serde_json::Value>> {
    let server = get_server_or_404(&state.db.pool, server_id).await?;
    require_server_permission(
        &state.db.pool,
        &server,
        auth.user_id,
        Permissions::KICK_MEMBERS,
    )
    .await?;

    // Rate limiting: 10 kicks per user per 5 minutes
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:kick:user:{}", auth.user_id),
        10,
        300,
    )
    .await?;
    check_rate_limit_with_fallback(state.db.redis.as_ref(), format!("rl:kick:tag:{tag}"), 20, 300)
        .await?;

    // Can't kick the owner
    if target_id == server.owner_id {
        return Err(NexusError::Forbidden);
    }
    // Can't kick yourself
    if target_id == auth.user_id {
        return Err(NexusError::Validation {
            message: "You cannot kick yourself".into(),
        });
    }

    // Verify target is a member
    if members::find_member(&state.db.pool, target_id, server_id)
        .await?
        .is_none()
    {
        return Err(NexusError::NotFound {
            resource: "Member".into(),
        });
    }

    members::remove_member(&state.db.pool, target_id, server_id).await?;

    // Audit log
    let _ = audit_log::write_entry(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        Some(auth.user_id),
        "MEMBER_KICK",
        Some("member"),
        Some(target_id),
        &serde_json::json!({}),
        body.reason.as_deref(),
    )
    .await;

    tracing::info!(
        server_id = %server_id,
        "Member kicked");

    Ok(Json(serde_json::json!({ "kicked": true })))
}

// ── Bans ──────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct BanBody {
    reason: Option<String>,
    /// If set, the ban expires after this many seconds.  Omit for a permanent ban.
    expires_in_secs: Option<u64>,
}

/// PUT /api/v1/servers/{server_id}/bans/{user_id}
///
/// Ban a user from the server, optionally for a limited time.
/// Also removes them if they are currently a member.
/// Requires BAN_MEMBERS permission.
async fn ban_member(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((server_id, target_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<BanBody>,
) -> NexusResult<Json<moderation::BanRow>> {
    let server = get_server_or_404(&state.db.pool, server_id).await?;
    require_server_permission(
        &state.db.pool,
        &server,
        auth.user_id,
        Permissions::BAN_MEMBERS,
    )
    .await?;

    // Rate limiting: 10 bans per user per 5 minutes
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:ban:user:{}", auth.user_id),
        10,
        300,
    )
    .await?;
    check_rate_limit_with_fallback(state.db.redis.as_ref(), format!("rl:ban:tag:{tag}"), 20, 300)
        .await?;

    if target_id == server.owner_id {
        return Err(NexusError::Forbidden);
    }
    if target_id == auth.user_id {
        return Err(NexusError::Validation {
            message: "You cannot ban yourself".into(),
        });
    }

    let expires_at = body
        .expires_in_secs
        .map(|secs| Utc::now() + Duration::seconds(secs as i64));

    // Also kick if currently a member
    let _ = members::remove_member(&state.db.pool, target_id, server_id).await;

    let ban = moderation::create_ban(
        &state.db.pool,
        target_id,
        server_id,
        auth.user_id,
        body.reason.as_deref(),
        expires_at,
    )
    .await?;

    let _ = audit_log::write_entry(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        Some(auth.user_id),
        "MEMBER_BAN",
        Some("member"),
        Some(target_id),
        &serde_json::json!({
            "temporary": expires_at.is_some(),
            "expires_at": expires_at.map(|t| t.to_rfc3339()),
        }),
        body.reason.as_deref(),
    )
    .await;

    tracing::info!(
        server_id = %server_id,
        temp     = expires_at.is_some(),
        "Member banned");

    Ok(Json(ban))
}

/// DELETE /api/v1/servers/{server_id}/bans/{user_id}
///
/// Remove a ban (unban).  Requires BAN_MEMBERS permission.
async fn unban_member(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((server_id, target_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<ReasonBody>,
) -> NexusResult<Json<serde_json::Value>> {
    let server = get_server_or_404(&state.db.pool, server_id).await?;
    require_server_permission(
        &state.db.pool,
        &server,
        auth.user_id,
        Permissions::BAN_MEMBERS,
    )
    .await?;

    // Rate limiting: 10 unbans per user per 5 minutes
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:unban:user:{}", auth.user_id),
        10,
        300,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:unban:tag:{tag}"),
        20,
        300,
    )
    .await?;

    let removed = moderation::remove_ban(&state.db.pool, target_id, server_id).await?;
    if !removed {
        return Err(NexusError::NotFound {
            resource: "Ban".into(),
        });
    }

    let _ = audit_log::write_entry(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        Some(auth.user_id),
        "MEMBER_UNBAN",
        Some("member"),
        Some(target_id),
        &serde_json::json!({}),
        body.reason.as_deref(),
    )
    .await;

    Ok(Json(serde_json::json!({ "unbanned": true })))
}

/// GET /api/v1/servers/{server_id}/bans
///
/// List all active bans for a server.  Requires BAN_MEMBERS permission.
async fn list_bans(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
) -> NexusResult<Json<Vec<moderation::BanRow>>> {
    let server = get_server_or_404(&state.db.pool, server_id).await?;
    require_server_permission(
        &state.db.pool,
        &server,
        auth.user_id,
        Permissions::BAN_MEMBERS,
    )
    .await?;

    let bans = moderation::list_bans(&state.db.pool, server_id).await?;
    Ok(Json(bans))
}

// ── Timeouts ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TimeoutBody {
    /// Timeout duration in seconds (max: 7 days = 604800).
    duration_secs: u64,
    reason: Option<String>,
}

/// PUT /api/v1/servers/{server_id}/members/{user_id}/timeout
///
/// Put a member in a communication timeout.  While timed out the member
/// cannot send messages or join voice channels.
/// Requires KICK_MEMBERS permission (same tier as kicking).
async fn set_timeout(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((server_id, target_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<TimeoutBody>,
) -> NexusResult<Json<serde_json::Value>> {
    // Cap at 28 days
    let duration_secs = body.duration_secs.min(28 * 24 * 3600);
    if duration_secs == 0 {
        return Err(NexusError::Validation {
            message: "Timeout duration must be greater than zero".into(),
        });
    }

    let server = get_server_or_404(&state.db.pool, server_id).await?;
    require_server_permission(
        &state.db.pool,
        &server,
        auth.user_id,
        Permissions::KICK_MEMBERS,
    )
    .await?;

    // Rate limiting: 10 timeouts per user per 5 minutes
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:timeout:user:{}", auth.user_id),
        10,
        300,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:timeout:tag:{tag}"),
        20,
        300,
    )
    .await?;

    if target_id == server.owner_id {
        return Err(NexusError::Forbidden);
    }
    if target_id == auth.user_id {
        return Err(NexusError::Validation {
            message: "You cannot timeout yourself".into(),
        });
    }

    if members::find_member(&state.db.pool, target_id, server_id)
        .await?
        .is_none()
    {
        return Err(NexusError::NotFound {
            resource: "Member".into(),
        });
    }

    let expires_at = Utc::now() + Duration::seconds(duration_secs as i64);
    moderation::set_timeout(
        &state.db.pool,
        target_id,
        server_id,
        auth.user_id,
        expires_at,
        body.reason.as_deref(),
    )
    .await?;

    let _ = audit_log::write_entry(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        Some(auth.user_id),
        "MEMBER_TIMEOUT",
        Some("member"),
        Some(target_id),
        &serde_json::json!({
            "expires_at": expires_at.to_rfc3339(),
            "duration_secs": duration_secs,
        }),
        body.reason.as_deref(),
    )
    .await;

    tracing::info!(
        server_id = %server_id,
        duration_secs,
        "Member timed out");

    // Notify connected clients so they update the member's timeout state in real-time.
    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: event_types::SERVER_MEMBER_UPDATE.to_string(),
        data: serde_json::json!({
            "server_id": server_id,
            "user_id": target_id,
            "communication_disabled_until": expires_at.to_rfc3339(),
        }),
        server_id: Some(server_id),
        channel_id: None,
        user_id: Some(target_id),
    });

    Ok(Json(serde_json::json!({
        "timed_out": true,
        "expires_at": expires_at.to_rfc3339(),
    })))
}

/// DELETE /api/v1/servers/{server_id}/members/{user_id}/timeout
///
/// Remove a member's timeout before it expires naturally.
/// Requires KICK_MEMBERS permission.
async fn lift_timeout(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((server_id, target_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<Json<serde_json::Value>> {
    let server = get_server_or_404(&state.db.pool, server_id).await?;
    require_server_permission(
        &state.db.pool,
        &server,
        auth.user_id,
        Permissions::KICK_MEMBERS,
    )
    .await?;

    let lifted = moderation::lift_timeout(&state.db.pool, target_id, server_id).await?;
    if !lifted {
        return Err(NexusError::NotFound {
            resource: "Timeout".into(),
        });
    }

    let _ = audit_log::write_entry(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        Some(auth.user_id),
        "MEMBER_TIMEOUT_REMOVE",
        Some("member"),
        Some(target_id),
        &serde_json::json!({}),
        None,
    )
    .await;

    // Notify connected clients that the timeout has been cleared.
    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: event_types::SERVER_MEMBER_UPDATE.to_string(),
        data: serde_json::json!({
            "server_id": server_id,
            "user_id": target_id,
            "communication_disabled_until": serde_json::Value::Null,
        }),
        server_id: Some(server_id),
        channel_id: None,
        user_id: Some(target_id),
    });

    Ok(Json(serde_json::json!({ "lifted": true })))
}

// ── Message reports ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ReportMessageBody {
    reason: String,
}

/// POST /api/v1/channels/{channel_id}/messages/{message_id}/report
///
/// File a report on a message.  Any authenticated user can report;
/// the reporter must be a member of the server if it is a server channel.
async fn report_message(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((channel_id, message_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<ReportMessageBody>,
) -> NexusResult<Json<moderation::ReportRow>> {
    if body.reason.trim().is_empty() {
        return Err(NexusError::Validation {
            message: "Report reason cannot be empty".into(),
        });
    }
    if body.reason.len() > 500 {
        return Err(NexusError::Validation {
            message: "Report reason must be 500 characters or fewer".into(),
        });
    }

    let channel = nexus_db::repository::channels::find_by_id(&state.db.pool, channel_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Channel".into(),
        })?;

    // If it's a server channel, reporter must be a member
    if let Some(server_id) = channel.server_id
        && !members::is_member(&state.db.pool, auth.user_id, server_id).await? {
            return Err(NexusError::Forbidden);
        }

    let report = moderation::create_report(
        &state.db.pool,
        snowflake::generate_id(),
        message_id,
        channel_id,
        channel.server_id,
        auth.user_id,
        &body.reason,
    )
    .await
    .map_err(|e| {
        // Unique violation = user already has a pending report for this message
        if let sqlx::Error::Database(ref de) = e
            && de.code().is_some_and(|c| c == "23505") {
                return NexusError::AlreadyExists {
                    resource: "Report".into(),
                };
            }
        NexusError::Database(e)
    })?;

    Ok(Json(report))
}

#[derive(Deserialize)]
struct ReportFilterQuery {
    status: Option<String>,
}

/// GET /api/v1/servers/{server_id}/reports
///
/// List message reports.  Requires MANAGE_MESSAGES permission.
/// Optional `?status=pending|resolved|dismissed` filter.
async fn list_reports(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
    Query(q): Query<ReportFilterQuery>,
) -> NexusResult<Json<Vec<moderation::ReportRow>>> {
    let server = get_server_or_404(&state.db.pool, server_id).await?;
    require_server_permission(
        &state.db.pool,
        &server,
        auth.user_id,
        Permissions::MANAGE_MESSAGES,
    )
    .await?;

    let valid_status = |s: &str| matches!(s, "pending" | "resolved" | "dismissed");
    let status = q.status.as_deref().filter(|s| valid_status(s));

    let reports = moderation::list_reports(&state.db.pool, server_id, status).await?;
    Ok(Json(reports))
}

#[derive(Deserialize)]
struct ResolveReportBody {
    /// Human description of the action taken: "deleted_message", "timeout", "ban", etc.
    action: String,
}

/// POST /api/v1/servers/{server_id}/reports/{report_id}/resolve
///
/// Mark a report as resolved with an action description.
/// Requires MANAGE_MESSAGES permission.
async fn resolve_report(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((server_id, report_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<ResolveReportBody>,
) -> NexusResult<Json<serde_json::Value>> {
    // Rate limiting: 20 report resolutions per user per 5 minutes
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:report_resolve:user:{}", auth.user_id),
        20,
        300,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:report_resolve:tag:{tag}"),
        40,
        300,
    )
    .await?;

    let server = get_server_or_404(&state.db.pool, server_id).await?;
    require_server_permission(
        &state.db.pool,
        &server,
        auth.user_id,
        Permissions::MANAGE_MESSAGES,
    )
    .await?;

    let updated = moderation::resolve_report(
        &state.db.pool,
        report_id,
        server_id,
        auth.user_id,
        &body.action,
    )
    .await?;
    if !updated {
        return Err(NexusError::NotFound {
            resource: "Report".into(),
        });
    }

    let _ = audit_log::write_entry(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        Some(auth.user_id),
        "REPORT_RESOLVE",
        Some("report"),
        Some(report_id),
        &serde_json::json!({ "action": body.action }),
        None,
    )
    .await;

    Ok(Json(serde_json::json!({ "resolved": true })))
}

/// POST /api/v1/servers/{server_id}/reports/{report_id}/dismiss
///
/// Dismiss a report (no action taken).
/// Requires MANAGE_MESSAGES permission.
async fn dismiss_report(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((server_id, report_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<Json<serde_json::Value>> {
    // Rate limiting: 20 report dismissals per user per 5 minutes
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:report_dismiss:user:{}", auth.user_id),
        20,
        300,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:report_dismiss:tag:{tag}"),
        40,
        300,
    )
    .await?;

    let server = get_server_or_404(&state.db.pool, server_id).await?;
    require_server_permission(
        &state.db.pool,
        &server,
        auth.user_id,
        Permissions::MANAGE_MESSAGES,
    )
    .await?;

    let updated =
        moderation::dismiss_report(&state.db.pool, report_id, server_id, auth.user_id).await?;
    if !updated {
        return Err(NexusError::NotFound {
            resource: "Report".into(),
        });
    }

    let _ = audit_log::write_entry(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        Some(auth.user_id),
        "REPORT_DISMISS",
        Some("report"),
        Some(report_id),
        &serde_json::json!({}),
        None,
    )
    .await;

    Ok(Json(serde_json::json!({ "dismissed": true })))
}

// ── Word filters ──────────────────────────────────────────────────────────────

/// GET /api/v1/servers/{server_id}/word-filters
///
/// List all word filter rules for a server.
/// Requires MANAGE_SERVER or MANAGE_MESSAGES permission.
async fn list_word_filters(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
) -> NexusResult<Json<Vec<moderation::WordFilter>>> {
    let server = get_server_or_404(&state.db.pool, server_id).await?;
    require_server_permission(
        &state.db.pool,
        &server,
        auth.user_id,
        Permissions::MANAGE_SERVER,
    )
    .await?;

    let filters = moderation::list_filters(&state.db.pool, server_id).await?;
    Ok(Json(filters))
}

#[derive(Deserialize)]
struct AddFilterBody {
    pattern: String,
    /// "block" (default), "delete", or "warn"
    action: Option<String>,
}

/// POST /api/v1/servers/{server_id}/word-filters
///
/// Add a word/phrase filter rule.  Requires MANAGE_SERVER permission.
async fn add_word_filter(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(server_id): Path<Uuid>,
    Json(body): Json<AddFilterBody>,
) -> NexusResult<Json<moderation::WordFilter>> {
    // Rate limiting: 10 word filter changes per user per 5 minutes
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:word_filter:user:{}", auth.user_id),
        10,
        300,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:word_filter:tag:{tag}"),
        20,
        300,
    )
    .await?;

    let pattern = body.pattern.trim().to_lowercase();
    if pattern.is_empty() {
        return Err(NexusError::Validation {
            message: "Pattern cannot be empty".into(),
        });
    }
    if pattern.len() > 200 {
        return Err(NexusError::Validation {
            message: "Pattern must be 200 characters or fewer".into(),
        });
    }

    let action = body.action.as_deref().unwrap_or("block");
    if !matches!(action, "block" | "delete" | "warn") {
        return Err(NexusError::Validation {
            message: "action must be one of: block, delete, warn".into(),
        });
    }

    let server = get_server_or_404(&state.db.pool, server_id).await?;
    require_server_permission(
        &state.db.pool,
        &server,
        auth.user_id,
        Permissions::MANAGE_SERVER,
    )
    .await?;

    let filter = moderation::add_filter(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        &pattern,
        action,
        auth.user_id,
    )
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(ref de) = e
            && de.code().is_some_and(|c| c == "23505") {
                return NexusError::AlreadyExists {
                    resource: "Filter pattern".into(),
                };
            }
        NexusError::Database(e)
    })?;

    let _ = audit_log::write_entry(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        Some(auth.user_id),
        "WORD_FILTER_ADD",
        Some("server"),
        Some(server_id),
        &serde_json::json!({ "pattern": pattern, "action": action }),
        None,
    )
    .await;

    Ok(Json(filter))
}

/// DELETE /api/v1/servers/{server_id}/word-filters/{filter_id}
///
/// Remove a word filter rule.  Requires MANAGE_SERVER permission.
async fn remove_word_filter(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((server_id, filter_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<Json<serde_json::Value>> {
    // Rate limiting: 10 word filter changes per user per 5 minutes
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:word_filter:user:{}", auth.user_id),
        10,
        300,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:word_filter:tag:{tag}"),
        20,
        300,
    )
    .await?;

    let server = get_server_or_404(&state.db.pool, server_id).await?;
    require_server_permission(
        &state.db.pool,
        &server,
        auth.user_id,
        Permissions::MANAGE_SERVER,
    )
    .await?;

    let removed = moderation::remove_filter(&state.db.pool, filter_id, server_id).await?;
    if !removed {
        return Err(NexusError::NotFound {
            resource: "Filter".into(),
        });
    }

    let _ = audit_log::write_entry(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        Some(auth.user_id),
        "WORD_FILTER_REMOVE",
        Some("server"),
        Some(server_id),
        &serde_json::json!({ "filter_id": filter_id }),
        None,
    )
    .await;

    Ok(Json(serde_json::json!({ "removed": true })))
}
