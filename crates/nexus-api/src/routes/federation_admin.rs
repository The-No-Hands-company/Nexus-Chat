//! Instance-admin federation management routes — v0.8.5.
//!
//! All endpoints under `/api/v1/admin/federation/…` require the caller to hold
//! the `INSTANCE_ADMIN` user flag.  These routes are *not* server-scoped; they
//! operate at the Nexus *instance* level and let admins manage which remote
//! servers this instance federates with.
//!
//! ## Endpoints
//!
//! | Method   | Path                                            | Description                          |
//! |----------|-------------------------------------------------|--------------------------------------|
//! | GET      | `/admin/federation/status`                      | Own instance's federation summary    |
//! | GET/PATCH | `/admin/federation/identity`                   | Read/update own /.well-known metadata |
//! | GET      | `/admin/federation/peers`                       | List all known peers + health        |
//! | POST     | `/admin/federation/peers`                       | Initiate peering with a remote       |
//! | GET      | `/admin/federation/peers/{domain}/health`       | Live-ping a peer                     |
//! | PATCH    | `/admin/federation/peers/{domain}/trust`        | Update trust score                   |
//! | POST     | `/admin/federation/peers/{domain}/block`        | Block a peer                         |
//! | POST     | `/admin/federation/peers/{domain}/unblock`      | Unblock a peer                       |
//! | DELETE   | `/admin/federation/peers/{domain}`              | Remove a peer entirely               |
//! | GET      | `/admin/federation/requests`                    | List pending peer requests           |
//! | POST     | `/admin/federation/requests/{id}/accept`        | Accept an inbound request            |
//! | POST     | `/admin/federation/requests/{id}/reject`        | Reject an inbound request            |
//! | GET      | `/admin/federation/audit`                       | Federation audit log                 |
//! | GET      | `/federation/search`                            | Cross-instance user search (auth'd)  |

use axum::http::HeaderMap;
use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{delete, get, patch, post},
};
use chrono::{DateTime, Utc};
use nexus_common::{
    error::{NexusError, NexusResult},
    models::user::user_flags,
    snowflake,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    AppState,
    middleware::{AuthContext, check_rate_limit_with_fallback, extract_client_tag},
};
use sqlx::Row;

// ─── Router ───────────────────────────────────────────────────────────────────

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // Instance status + identity
        .route("/admin/federation/status", get(get_federation_status))
        .route(
            "/admin/federation/identity",
            get(get_identity).patch(update_identity),
        )
        // Peer management
        .route("/admin/federation/peers", get(list_peers).post(add_peer))
        .route("/admin/federation/peers/{domain}/health", get(peer_health))
        .route(
            "/admin/federation/peers/{domain}/trust",
            patch(update_trust),
        )
        .route("/admin/federation/peers/{domain}/block", post(block_peer))
        .route(
            "/admin/federation/peers/{domain}/unblock",
            post(unblock_peer),
        )
        .route("/admin/federation/peers/{domain}", delete(remove_peer))
        // Peering requests
        .route("/admin/federation/requests", get(list_requests))
        .route(
            "/admin/federation/requests/{request_id}/accept",
            post(accept_request),
        )
        .route(
            "/admin/federation/requests/{request_id}/reject",
            post(reject_request),
        )
        // Audit log
        .route("/admin/federation/audit", get(get_audit_log))
        // Cross-instance user search (accessible to any authenticated user)
        .route("/federation/search", get(search_federated_users))
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ))
}

// ─── Auth helper ──────────────────────────────────────────────────────────────

/// Verify that `auth` has the `INSTANCE_ADMIN` flag.
///
/// Returns `Ok(Uuid)` with the admin's user ID on success, or a 403 error
/// if the user is not an instance admin.
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

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Write a row to the federation audit log.
async fn write_audit(
    pool: &sqlx::AnyPool,
    admin_id: Uuid,
    action: &str,
    target_domain: &str,
    details: Value,
) {
    if let Err(e) = sqlx::query(
        r#"INSERT INTO federation_audit_log (id, admin_id, action, target_domain, details)
           VALUES ($1::uuid, $2::uuid, $3, $4, $5)"#,
    )
    .bind(snowflake::generate_id().to_string())
    .bind(admin_id.to_string())
    .bind(action)
    .bind(target_domain)
    .bind(serde_json::to_string(&details).unwrap_or_else(|_| "{}".to_string()))
    .execute(pool)
    .await
    {
        warn!("Failed to write federation audit log: {}", e);
    }
}

// ─── GET /admin/federation/status ────────────────────────────────────────────

async fn get_federation_status(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> NexusResult<impl IntoResponse> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    let peer_row = sqlx::query(
        r#"SELECT
               COUNT(*)                                     AS peer_count,
               COUNT(*) FILTER (WHERE is_healthy = true AND is_blocked = false) AS healthy_count
           FROM federated_servers"#,
    )
    .fetch_one(&state.db.pool)
    .await
    .map_err(NexusError::from)?;

    let peer_count: i64 = peer_row.try_get("peer_count").unwrap_or(0);
    let healthy_count: i64 = peer_row.try_get("healthy_count").unwrap_or(0);

    let req_row = sqlx::query(
        r#"SELECT
               COUNT(*) FILTER (WHERE direction = 'inbound')  AS inbound_pending,
               COUNT(*) FILTER (WHERE direction = 'outbound') AS outbound_pending
           FROM federation_peer_requests
           WHERE status = 'pending'"#,
    )
    .fetch_one(&state.db.pool)
    .await
    .map_err(NexusError::from)?;

    let inbound_pending: i64 = req_row.try_get("inbound_pending").unwrap_or(0);
    let outbound_pending: i64 = req_row.try_get("outbound_pending").unwrap_or(0);

    let uptime_secs = state.started_at.elapsed().as_secs();
    let version = concat!("Nexus ", env!("CARGO_PKG_VERSION"));

    Ok(Json(json!({
        "server_name":              state.server_name,
        "software_version":         version,
        "federation_enabled":       true,
        "peer_count":               peer_count,
        "healthy_peer_count":       healthy_count,
        "pending_inbound_requests": inbound_pending,
        "pending_outbound_requests": outbound_pending,
        "uptime_seconds":           uptime_secs,
    })))
}

// ─── GET /admin/federation/identity ──────────────────────────────────────────

/// Reads the instance identity stored in `instance_settings` or returns
/// sensible defaults derived from environment variables.
async fn get_identity(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> NexusResult<impl IntoResponse> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    let row = sqlx::query(
        r#"SELECT value::text AS value FROM instance_settings WHERE key = 'federation_identity'"#,
    )
    .fetch_optional(&state.db.pool)
    .await
    .map_err(NexusError::from)?;

    let identity: Value = row
        .and_then(|r| {
            let v: Option<Value> = r
                .try_get::<String, _>("value")
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok());
            v
        })
        .unwrap_or_else(|| {
            json!({
                "display_name":       null,
                "description":        null,
                "admin_contact":      null,
                "federation_policy":  "open",
            })
        });

    Ok(Json(identity))
}

// ─── PATCH /admin/federation/identity ────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct UpdateIdentityBody {
    display_name: Option<String>,
    description: Option<String>,
    admin_contact: Option<String>,
    /// `"open"` | `"allowlist"` | `"closed"`
    federation_policy: Option<String>,
}

async fn update_identity(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    headers: HeaderMap,
    Json(body): Json<UpdateIdentityBody>,
) -> NexusResult<impl IntoResponse> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    // Rate limit identity updates
    let _ip = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:fedadmin:user:{}:identity", auth.user_id),
        30,  // 30 updates per admin
        300, // per 5 minutes
    )
    .await?;

    // Validate federation_policy
    if let Some(ref policy) = body.federation_policy
        && !matches!(policy.as_str(), "open" | "allowlist" | "closed") {
            return Err(NexusError::Validation {
                message: "federation_policy must be 'open', 'allowlist', or 'closed'".to_string(),
            });
        }

    let value = json!({
        "display_name":      body.display_name,
        "description":       body.description,
        "admin_contact":     body.admin_contact,
        "federation_policy": body.federation_policy.unwrap_or_else(|| "open".into()),
    });

    sqlx::query(
        r#"INSERT INTO instance_settings (key, value)
           VALUES ('federation_identity', $1)
           ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = NOW()"#,
    )
    .bind(serde_json::to_string(&value).unwrap_or_default())
    .execute(&state.db.pool)
    .await
    .map_err(NexusError::from)?;

    write_audit(
        &state.db.pool,
        auth.user_id,
        "identity_updated",
        &state.server_name,
        value.clone(),
    )
    .await;

    info!("Instance federation identity updated");
    Ok(Json(value))
}

// ─── GET /admin/federation/peers ─────────────────────────────────────────────

async fn list_peers(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> NexusResult<impl IntoResponse> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    let rows = sqlx::query(
        r#"SELECT id, server_name, display_name, description, admin_contact,
                  software_version, user_count, trust_score, federation_policy,
                  is_blocked, is_healthy, latency_ms, last_seen_at, last_error,
                  first_seen_at, server_type
           FROM federated_servers
           ORDER BY trust_score DESC, server_name"#,
    )
    .fetch_all(&state.db.pool)
    .await
    .map_err(NexusError::from)?;

    let peers: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id":               r.try_get::<String, _>("id").unwrap_or_default(),
                "server_name":      r.try_get::<String, _>("server_name").unwrap_or_default(),
                "display_name":     r.try_get::<Option<String>, _>("display_name").unwrap_or(None),
                "description":      r.try_get::<Option<String>, _>("description").unwrap_or(None),
                "admin_contact":    r.try_get::<Option<String>, _>("admin_contact").unwrap_or(None),
                "software_version": r.try_get::<Option<String>, _>("software_version").unwrap_or(None),
                "user_count":       r.try_get::<Option<i64>, _>("user_count").unwrap_or(None),
                "trust_score":      r.try_get::<i16, _>("trust_score").unwrap_or(50),
                "federation_policy":r.try_get::<String, _>("federation_policy").unwrap_or_else(|_| "open".into()),
                "is_blocked":       r.try_get::<bool, _>("is_blocked").unwrap_or(false),
                "is_healthy":       r.try_get::<bool, _>("is_healthy").unwrap_or(true),
                "latency_ms":       r.try_get::<Option<i32>, _>("latency_ms").unwrap_or(None),
                "last_seen_at":     r.try_get::<Option<String>, _>("last_seen_at").unwrap_or(None),
                "last_error":       r.try_get::<Option<String>, _>("last_error").unwrap_or(None),
                "first_seen_at":    r.try_get::<Option<String>, _>("first_seen_at").unwrap_or(None),
                "server_type":      r.try_get::<String, _>("server_type").unwrap_or_else(|_| "nexus".into()),
            })
        })
        .collect();

    Ok(Json(json!({ "peers": peers, "total": peers.len() })))
}

// ─── POST /admin/federation/peers ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct AddPeerBody {
    /// The remote server's domain name (e.g. `"nexus.other.tld"`).
    domain: String,
    /// Optional human message included in the outbound peering request.
    message: Option<String>,
    /// Initial trust score (defaults to 50).
    trust_score: Option<i16>,
}

/// Discover a remote server via `/.well-known/nexus/server`, store it in
/// `federated_servers` (if new), and create an outbound `federation_peer_requests` row.
async fn add_peer(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    headers: HeaderMap,
    Json(body): Json<AddPeerBody>,
) -> NexusResult<impl IntoResponse> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    // Rate limit peering attempts (external HTTP calls)
    let _ip = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:fedadmin:user:{}:add_peer", auth.user_id),
        10,  // 10 peering attempts per admin
        300, // per 5 minutes
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:fedadmin:tag:{_ip}:add_peer"),
        20, // 20 per IP
        300,
    )
    .await?;

    // Basic domain validation.
    let domain = body.domain.trim().to_lowercase();
    if domain.is_empty() || domain.contains('/') || domain.contains(' ') {
        return Err(NexusError::Validation {
            message: "Invalid domain name".to_string(),
        });
    }
    if domain == state.server_name {
        return Err(NexusError::Validation {
            message: "Cannot peer with yourself".to_string(),
        });
    }

    let trust_score = body.trust_score.unwrap_or(50).clamp(0, 100);

    // Ping the remote to discover its identity.
    let (latency_ms, well_known) =
        state
            .federation_client
            .ping(&domain)
            .await
            .map_err(|e| NexusError::Validation {
                message: format!("Could not reach {}: {}", domain, e),
            })?;

    let display_name = well_known.display_name.as_deref().unwrap_or("");
    let description = well_known.description.as_deref().unwrap_or("");

    // Upsert into federated_servers.
    sqlx::query(
        r#"INSERT INTO federated_servers
               (id, server_name, display_name, description, admin_contact,
                software_version, federation_policy, trust_score,
                is_healthy, latency_ms, last_seen_at)
           VALUES ($1::uuid, $2, $3, $4, $5, $6, $7, $8, true, $9, NOW())
           ON CONFLICT (server_name) DO UPDATE SET
               display_name      = EXCLUDED.display_name,
               description       = EXCLUDED.description,
               admin_contact     = EXCLUDED.admin_contact,
               software_version  = EXCLUDED.software_version,
               federation_policy = EXCLUDED.federation_policy,
               trust_score       = $8,
               is_healthy        = true,
               latency_ms        = $9,
               last_seen_at      = NOW(),
               last_error        = NULL"#,
    )
    .bind(snowflake::generate_id().to_string())
    .bind(&domain)
    .bind(&well_known.display_name)
    .bind(&well_known.description)
    .bind(&well_known.admin_contact)
    .bind(&well_known.software_version)
    .bind(&well_known.federation_policy)
    .bind(trust_score)
    .bind(latency_ms as i32)
    .execute(&state.db.pool)
    .await
    .map_err(NexusError::from)?;

    // Create an outbound peering request.
    let req_id = snowflake::generate_id().to_string();
    sqlx::query(
        r#"INSERT INTO federation_peer_requests
               (id, remote_domain, direction, acted_by, message, remote_display_name, remote_description)
           VALUES ($1::uuid, $2, 'outbound', $3::uuid, $4, $5, $6)"#)
        .bind(&req_id)
        .bind(&domain)
        .bind(auth.user_id.to_string())
        .bind(&body.message)
        .bind(display_name)
        .bind(description)
        .execute(&state.db.pool)
        .await
        .map_err(NexusError::from)?;

    write_audit(
        &state.db.pool,
        auth.user_id,
        "peer_added",
        &domain,
        json!({ "trust_score": trust_score, "latency_ms": latency_ms }),
    )
    .await;

    info!(
        "Admin initiated peering with {} (latency {}ms)",
        domain, latency_ms
    );

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "domain":       domain,
            "display_name": well_known.display_name,
            "description":  well_known.description,
            "latency_ms":   latency_ms,
            "trust_score":  trust_score,
            "request_id":   req_id,
        })),
    ))
}

// ─── GET /admin/federation/peers/{domain}/health ──────────────────────────────

async fn peer_health(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(domain): Path<String>,
) -> NexusResult<impl IntoResponse> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    match state.federation_client.ping(&domain).await {
        Ok((latency_ms, info)) => {
            // Update DB with fresh health data.
            let _ = sqlx::query(
                r#"UPDATE federated_servers
                   SET is_healthy = true, latency_ms = $1, last_seen_at = NOW(),
                       last_error = NULL, display_name = COALESCE($2, display_name),
                       software_version = COALESCE($3, software_version)
                   WHERE server_name = $4"#,
            )
            .bind(latency_ms as i32)
            .bind(&info.display_name)
            .bind(&info.software_version)
            .bind(&domain)
            .execute(&state.db.pool)
            .await;

            Ok(Json(json!({
                "domain":           domain,
                "status":           "online",
                "latency_ms":       latency_ms,
                "display_name":     info.display_name,
                "software_version": info.software_version,
                "user_count":       info.user_count,
                "federation_policy":info.federation_policy,
            })))
        }
        Err(e) => {
            let error_str = e.to_string();
            // Update DB to reflect offline.
            let _ = sqlx::query(
                r#"UPDATE federated_servers
                   SET is_healthy = false, last_error = $1 WHERE server_name = $2"#,
            )
            .bind(&error_str)
            .bind(&domain)
            .execute(&state.db.pool)
            .await;

            Ok(Json(json!({
                "domain":   domain,
                "status":   "offline",
                "error":    error_str,
            })))
        }
    }
}

// ─── PATCH /admin/federation/peers/{domain}/trust ────────────────────────────

#[derive(Debug, Deserialize)]
struct UpdateTrustBody {
    /// New trust score 0-100.
    trust_score: i16,
}

async fn update_trust(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(domain): Path<String>,
    Json(body): Json<UpdateTrustBody>,
) -> NexusResult<impl IntoResponse> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    let score = body.trust_score.clamp(0, 100);

    let result = sqlx::query(
        "UPDATE federated_servers SET trust_score = $1 WHERE server_name = $2 RETURNING id",
    )
    .bind(score)
    .bind(&domain)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(NexusError::from)?;

    if result.is_none() {
        return Err(NexusError::NotFound {
            resource: "Peer not found".to_string(),
        });
    }

    write_audit(
        &state.db.pool,
        auth.user_id,
        "trust_changed",
        &domain,
        json!({ "trust_score": score }),
    )
    .await;

    Ok(Json(json!({ "domain": domain, "trust_score": score })))
}

// ─── POST /admin/federation/peers/{domain}/block ──────────────────────────────

async fn block_peer(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    headers: HeaderMap,
    Path(domain): Path<String>,
) -> NexusResult<impl IntoResponse> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    // Rate limit block operations
    let _ip = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:fedadmin:user:{}:block", auth.user_id),
        50, // 50 blocks per admin
        60, // per minute
    )
    .await?;

    let result = sqlx::query(
        "UPDATE federated_servers SET is_blocked = true, trust_score = 0 WHERE server_name = $1 RETURNING id")
        .bind(&domain)
        .fetch_optional(&state.db.pool)
        .await
        .map_err(NexusError::from)?;

    if result.is_none() {
        // Create a blocked placeholder so future connections are rejected.
        sqlx::query(
            r#"INSERT INTO federated_servers (id, server_name, is_blocked, trust_score)
               VALUES ($1::uuid, $2, true, 0)
               ON CONFLICT (server_name) DO UPDATE SET is_blocked = true, trust_score = 0"#,
        )
        .bind(snowflake::generate_id().to_string())
        .bind(&domain)
        .execute(&state.db.pool)
        .await
        .map_err(NexusError::from)?;
    }

    write_audit(
        &state.db.pool,
        auth.user_id,
        "peer_blocked",
        &domain,
        json!({}),
    )
    .await;

    info!("Admin blocked peer {}", domain);
    Ok(Json(json!({ "domain": domain, "is_blocked": true })))
}

// ─── POST /admin/federation/peers/{domain}/unblock ───────────────────────────

async fn unblock_peer(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(domain): Path<String>,
) -> NexusResult<impl IntoResponse> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    sqlx::query(
        "UPDATE federated_servers SET is_blocked = false, trust_score = 25 WHERE server_name = $1",
    )
    .bind(&domain)
    .execute(&state.db.pool)
    .await
    .map_err(NexusError::from)?;

    write_audit(
        &state.db.pool,
        auth.user_id,
        "peer_unblocked",
        &domain,
        json!({}),
    )
    .await;

    Ok(Json(json!({ "domain": domain, "is_blocked": false })))
}

// ─── DELETE /admin/federation/peers/{domain} ─────────────────────────────────

async fn remove_peer(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(domain): Path<String>,
) -> NexusResult<impl IntoResponse> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    let result = sqlx::query("DELETE FROM federated_servers WHERE server_name = $1 RETURNING id")
        .bind(&domain)
        .fetch_optional(&state.db.pool)
        .await
        .map_err(NexusError::from)?;

    if result.is_none() {
        return Err(NexusError::NotFound {
            resource: "Peer not found".to_string(),
        });
    }

    write_audit(
        &state.db.pool,
        auth.user_id,
        "peer_removed",
        &domain,
        json!({}),
    )
    .await;

    info!("Admin removed peer {}", domain);
    Ok(StatusCode::NO_CONTENT)
}

// ─── GET /admin/federation/requests ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ListRequestsQuery {
    direction: Option<String>, // "inbound" | "outbound"
    status: Option<String>,    // "pending" | "accepted" | "rejected" | "cancelled"
}

async fn list_requests(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Query(q): Query<ListRequestsQuery>,
) -> NexusResult<impl IntoResponse> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    let rows = sqlx::query(
        r#"SELECT id, remote_domain, direction, status, message,
                  remote_display_name, remote_description,
                  acted_by, created_at, resolved_at
           FROM federation_peer_requests
           WHERE ($1::text IS NULL OR direction = $1)
             AND ($2::text IS NULL OR status    = $2)
           ORDER BY created_at DESC
           LIMIT 100"#,
    )
    .bind(q.direction)
    .bind(q.status.as_deref().or(Some("pending")))
    .fetch_all(&state.db.pool)
    .await
    .map_err(NexusError::from)?;

    let requests: Vec<Value> = rows.iter().map(|r| json!({
        "id":                   r.try_get::<String, _>("id").unwrap_or_default(),
        "remote_domain":        r.try_get::<String, _>("remote_domain").unwrap_or_default(),
        "direction":            r.try_get::<String, _>("direction").unwrap_or_default(),
        "status":               r.try_get::<String, _>("status").unwrap_or_default(),
        "message":              r.try_get::<Option<String>, _>("message").unwrap_or(None),
        "remote_display_name":  r.try_get::<Option<String>, _>("remote_display_name").unwrap_or(None),
        "remote_description":   r.try_get::<Option<String>, _>("remote_description").unwrap_or(None),
        "created_at":           r.try_get::<Option<String>, _>("created_at").unwrap_or(None),
        "resolved_at":          r.try_get::<Option<String>, _>("resolved_at").unwrap_or(None),
    })).collect();

    Ok(Json(
        json!({ "requests": requests, "total": requests.len() }),
    ))
}

// ─── POST /admin/federation/requests/{id}/accept ──────────────────────────────

async fn accept_request(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(request_id): Path<String>,
) -> NexusResult<impl IntoResponse> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    // Fetch and validate the request.
    let row = sqlx::query(
        r#"SELECT remote_domain, direction, status, remote_display_name, remote_description
           FROM federation_peer_requests WHERE id = $1::uuid"#,
    )
    .bind(&request_id)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(NexusError::from)?
    .ok_or_else(|| NexusError::NotFound {
        resource: "Request not found".to_string(),
    })?;

    let direction: String = row.try_get("direction").unwrap_or_default();
    let status: String = row.try_get("status").unwrap_or_default();
    let domain: String = row.try_get("remote_domain").unwrap_or_default();

    if direction != "inbound" {
        return Err(NexusError::Validation {
            message: "Only inbound requests can be accepted".to_string(),
        });
    }
    if status != "pending" {
        return Err(NexusError::Validation {
            message: format!("Request is already {}", status),
        });
    }

    // Mark as accepted.
    sqlx::query(
        r#"UPDATE federation_peer_requests
           SET status = 'accepted', acted_by = $1::uuid, resolved_at = NOW()
           WHERE id = $2::uuid"#,
    )
    .bind(auth.user_id.to_string())
    .bind(&request_id)
    .execute(&state.db.pool)
    .await
    .map_err(NexusError::from)?;

    // Ensure the peer is in federated_servers with a healthy trust score.
    sqlx::query(
        r#"INSERT INTO federated_servers (id, server_name, display_name, description, trust_score, is_healthy)
           VALUES ($1::uuid, $2, $3, $4, 50, true)
           ON CONFLICT (server_name) DO UPDATE SET
               trust_score = GREATEST(federated_servers.trust_score, 50),
               is_blocked  = false"#)
        .bind(snowflake::generate_id().to_string())
        .bind(&domain)
        .bind(row.try_get::<Option<String>, _>("remote_display_name").unwrap_or(None))
        .bind(row.try_get::<Option<String>, _>("remote_description").unwrap_or(None))
        .execute(&state.db.pool)
        .await
        .map_err(NexusError::from)?;

    write_audit(
        &state.db.pool,
        auth.user_id,
        "request_accepted",
        &domain,
        json!({ "request_id": request_id }),
    )
    .await;

    Ok(Json(
        json!({ "request_id": request_id, "domain": domain, "status": "accepted" }),
    ))
}

// ─── POST /admin/federation/requests/{id}/reject ──────────────────────────────

async fn reject_request(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(request_id): Path<String>,
) -> NexusResult<impl IntoResponse> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    let row = sqlx::query(
        "SELECT remote_domain, status FROM federation_peer_requests WHERE id = $1::uuid",
    )
    .bind(&request_id)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(NexusError::from)?
    .ok_or_else(|| NexusError::NotFound {
        resource: "Request not found".to_string(),
    })?;

    let status: String = row.try_get("status").unwrap_or_default();
    let domain: String = row.try_get("remote_domain").unwrap_or_default();

    if status != "pending" {
        return Err(NexusError::Validation {
            message: format!("Request is already {}", status),
        });
    }

    sqlx::query(
        r#"UPDATE federation_peer_requests
           SET status = 'rejected', acted_by = $1::uuid, resolved_at = NOW()
           WHERE id = $2::uuid"#,
    )
    .bind(auth.user_id.to_string())
    .bind(&request_id)
    .execute(&state.db.pool)
    .await
    .map_err(NexusError::from)?;

    write_audit(
        &state.db.pool,
        auth.user_id,
        "request_rejected",
        &domain,
        json!({ "request_id": request_id }),
    )
    .await;

    Ok(Json(
        json!({ "request_id": request_id, "domain": domain, "status": "rejected" }),
    ))
}

// ─── GET /admin/federation/audit ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct AuditQuery {
    domain: Option<String>,
    limit: Option<i64>,
    before: Option<DateTime<Utc>>,
}

async fn get_audit_log(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Query(q): Query<AuditQuery>,
) -> NexusResult<impl IntoResponse> {
    require_instance_admin(&state.db.pool, auth.user_id).await?;

    let limit = q.limit.unwrap_or(50).min(200);

    let rows = sqlx::query(
        r#"SELECT id, admin_id, action, target_domain, details, created_at
           FROM federation_audit_log
           WHERE ($1::text     IS NULL OR target_domain = $1)
             AND ($2::timestamptz IS NULL OR created_at < $2)
           ORDER BY created_at DESC
           LIMIT $3"#,
    )
    .bind(q.domain)
    .bind(q.before.map(|d| d.to_rfc3339()))
    .bind(limit)
    .fetch_all(&state.db.pool)
    .await
    .map_err(NexusError::from)?;

    let entries: Vec<Value> = rows.iter().map(|r| json!({
        "id":            r.try_get::<String, _>("id").unwrap_or_default(),
        "admin_id":      r.try_get::<Option<String>, _>("admin_id").unwrap_or(None),
        "action":        r.try_get::<String, _>("action").unwrap_or_default(),
        "target_domain": r.try_get::<String, _>("target_domain").unwrap_or_default(),
        "details":       r.try_get::<Option<String>, _>("details").ok().flatten().and_then(|s| serde_json::from_str::<Value>(&s).ok()).unwrap_or(json!({})),
        "created_at":    r.try_get::<Option<String>, _>("created_at").unwrap_or(None),
    })).collect();

    Ok(Json(json!({ "entries": entries, "total": entries.len() })))
}

// ─── GET /federation/search ───────────────────────────────────────────────────

/// Cross-instance user search.  Any authenticated user can call this.
///
/// - `q=user@server.tld` — resolves the user's profile on the remote server.
/// - `q=username` — searches local users only.
#[derive(Debug, Deserialize)]
struct FederationSearchQuery {
    q: String,
}

async fn search_federated_users(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
    Query(q): Query<FederationSearchQuery>,
) -> NexusResult<impl IntoResponse> {
    let query = q.q.trim();

    // If the query looks like `user@server.tld`, resolve the remote profile.
    if let Some(at_pos) = query.rfind('@') {
        let localpart = &query[..at_pos];
        let remote_server = &query[at_pos + 1..];

        if !remote_server.is_empty() && remote_server != state.server_name {
            // Check the remote server isn't blocked.
            let blocked: bool =
                sqlx::query("SELECT is_blocked FROM federated_servers WHERE server_name = $1")
                    .bind(remote_server)
                    .fetch_optional(&state.db.pool)
                    .await
                    .map_err(NexusError::from)?
                    .and_then(|r| r.try_get::<bool, _>("is_blocked").ok())
                    .unwrap_or(false);

            if blocked {
                return Err(NexusError::Forbidden);
            }

            // Try to fetch the user profile from the remote server.
            let user_id_mxid = format!("@{}:{}", localpart, remote_server);

            // Use the federation client's authenticated GET.
            let profile: Value = state
                .federation_client
                .get_user_profile(remote_server, &user_id_mxid)
                .await
                .unwrap_or_else(|_| json!({}));

            return Ok(Json(json!({
                "type":    "federated",
                "user_id": user_id_mxid,
                "server":  remote_server,
                "profile": profile,
            })));
        }
    }

    // Fallback: local user search.
    let rows = sqlx::query(
        r#"SELECT id, username, display_name, avatar, flags
           FROM users
           WHERE (LOWER(username) LIKE $1 OR LOWER(display_name) LIKE $1)
             AND (flags & $2 = 0)
           ORDER BY username
           LIMIT 25"#,
    )
    .bind(format!("{}%", query.to_lowercase()))
    .bind(user_flags::BOT | user_flags::SUSPENDED | user_flags::DISABLED)
    .fetch_all(&state.db.pool)
    .await
    .map_err(NexusError::from)?;

    let users: Vec<Value> = rows.iter().map(|r| json!({
        "type":         "local",
        "user_id":      r.try_get::<String, _>("id").map(|_id| format!("@{}:{}", r.try_get::<String, _>("username").unwrap_or_default(), state.server_name)).unwrap_or_default(),
        "username":     r.try_get::<String, _>("username").unwrap_or_default(),
        "display_name": r.try_get::<Option<String>, _>("display_name").unwrap_or(None),
        "avatar":       r.try_get::<Option<String>, _>("avatar").unwrap_or(None),
    })).collect();

    Ok(Json(json!({ "type": "local", "results": users })))
}

// ─── Util ─────────────────────────────────────────────────────────────────────
