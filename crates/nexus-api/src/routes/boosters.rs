//! Server supporter / boost tier routes — v0.15 Community Ecosystem.
//!
//! POST   /servers/{id}/boost               — add a boost slot
//! DELETE /servers/{id}/boost/{slot}        — remove a boost slot
//! GET    /servers/{id}/boosters            — list active boosters
//! GET    /servers/{id}/boost-tier          — get current tier + perks
//! PATCH  /servers/{id}/vanity-url          — set vanity invite code (tier 2+)

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    middleware,
    routing::{delete, get, patch, post},
};
use chrono::{DateTime, Utc};
use nexus_common::{
    error::{NexusError, NexusResult},
    gateway_event::{GatewayEvent, event_types},
    permissions::Permissions,
};
use nexus_db::repository::{members, roles};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    AppState,
    middleware::{AuthContext, check_rate_limit_with_fallback, extract_client_tag},
};

// Boost count thresholds
const TIER1_THRESHOLD: i64 = 2;
const TIER2_THRESHOLD: i64 = 7;
const TIER3_THRESHOLD: i64 = 14;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/servers/{server_id}/boost", post(add_boost))
        .route("/servers/{server_id}/boost/{slot}", delete(remove_boost))
        .route("/servers/{server_id}/boosters", get(list_boosters))
        .route("/servers/{server_id}/boost-tier", get(get_boost_tier))
        .route("/servers/{server_id}/vanity-url", patch(set_vanity_url))
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ))
}

// ─────────────────────────────────────────────────────────────────────────────
// Public response models
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct BoosterEntry {
    pub id: Uuid,
    pub user_id: Uuid,
    pub server_id: Uuid,
    pub slot: i16,
    pub started_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct BoostTierInfo {
    pub tier: i16,
    pub booster_count: i64,
    pub extra_emoji_slots: i32,
    pub upload_limit_bytes: i64,
    pub vanity_url_available: bool,
    pub current_vanity_code: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal DB rows
// ─────────────────────────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct BoosterEntryRow {
    id: String,
    user_id: String,
    server_id: String,
    slot: i16,
    started_at: String,
    expires_at: Option<String>,
}

impl TryFrom<BoosterEntryRow> for BoosterEntry {
    type Error = NexusError;
    fn try_from(r: BoosterEntryRow) -> Result<Self, Self::Error> {
        Ok(BoosterEntry {
            id: r
                .id
                .parse()
                .map_err(|_| NexusError::Internal(anyhow::anyhow!("invalid id")))?,
            user_id: r
                .user_id
                .parse()
                .map_err(|_| NexusError::Internal(anyhow::anyhow!("invalid user_id")))?,
            server_id: r
                .server_id
                .parse()
                .map_err(|_| NexusError::Internal(anyhow::anyhow!("invalid server_id")))?,
            slot: r.slot,
            started_at: chrono::DateTime::parse_from_rfc3339(&r.started_at)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now()),
            expires_at: r
                .expires_at
                .as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc)),
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Request models
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SetVanityUrlRequest {
    pub code: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Tier helpers
// ─────────────────────────────────────────────────────────────────────────────

fn tier_from_count(count: i64) -> i16 {
    if count >= TIER3_THRESHOLD {
        3
    } else if count >= TIER2_THRESHOLD {
        2
    } else if count >= TIER1_THRESHOLD {
        1
    } else {
        0
    }
}

fn perks(tier: i16, vanity_code: Option<String>) -> BoostTierInfo {
    let (extra_emoji, upload_limit) = match tier {
        1 => (50, 25 * 1024 * 1024),
        2 => (100, 50 * 1024 * 1024),
        3 => (200, 100 * 1024 * 1024),
        _ => (0, 8 * 1024 * 1024),
    };
    BoostTierInfo {
        tier,
        booster_count: 0,
        extra_emoji_slots: extra_emoji,
        upload_limit_bytes: upload_limit,
        vanity_url_available: tier >= 2,
        current_vanity_code: vanity_code,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Handlers
// ─────────────────────────────────────────────────────────────────────────────

/// `POST /servers/{server_id}/boost` — add a boost (slot auto-assigned 1 or 2)
async fn add_boost(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    headers: axum::http::HeaderMap,
    Path(server_id): Path<Uuid>,
) -> NexusResult<Json<BoosterEntry>> {
    // Rate limiting: 2 boosts per server per day per user (economic abuse prevention)
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:boost:{server_id}:{}", auth.user_id),
        2,
        86400,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:boost:tag:{tag}"),
        5,
        86400,
    )
    .await?;
    let used_slots: Vec<(i16,)> = sqlx::query_as(
        r#"SELECT slot FROM server_boosters
           WHERE user_id = $1::uuid AND server_id = $2::uuid
             AND (expires_at IS NULL OR expires_at > NOW())"#,
    )
    .bind(auth.user_id.to_string())
    .bind(server_id.to_string())
    .fetch_all(&state.db.pool)
    .await
    .map_err(NexusError::Database)?;

    let used: Vec<i16> = used_slots.into_iter().map(|(s,)| s).collect();

    let slot: i16 = if !used.contains(&1) {
        1
    } else if !used.contains(&2) {
        2
    } else {
        return Err(NexusError::Validation {
            message: "Both boost slots already used on this server".to_string(),
        });
    };

    let boost_id = uuid::Uuid::new_v4().to_string();
    let row: BoosterEntryRow = sqlx::query_as(
        r#"INSERT INTO server_boosters (id, user_id, server_id, slot)
           VALUES ($1::uuid, $2::uuid, $3::uuid, $4)
           RETURNING id::text AS id, user_id::text AS user_id, server_id::text AS server_id, slot, started_at::text AS started_at, expires_at::text AS expires_at"#,
    )
    .bind(&boost_id)
    .bind(auth.user_id.to_string())
    .bind(server_id.to_string())
    .bind(slot)
    .fetch_one(&state.db.pool)
    .await
    .map_err(NexusError::Database)?;

    let entry = BoosterEntry::try_from(row)?;

    recalculate_tier(&state, server_id).await?;

    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: event_types::SERVER_BOOST.to_string(),
        data: serde_json::to_value(&entry).unwrap_or_default(),
        server_id: Some(server_id),
        channel_id: None,
        user_id: Some(auth.user_id),
    });

    Ok(Json(entry))
}

/// `DELETE /servers/{server_id}/boost/{slot}` — remove a specific boost slot
async fn remove_boost(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    headers: axum::http::HeaderMap,
    Path((server_id, slot)): Path<(Uuid, i16)>,
) -> NexusResult<Json<serde_json::Value>> {
    // Rate limiting: 10 boost removals per day per user
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:boost:remove:{}", auth.user_id),
        10,
        86400,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:boost:remove:tag:{tag}"),
        20,
        86400,
    )
    .await?;
    let result = sqlx::query(
        r#"DELETE FROM server_boosters
           WHERE user_id = $1::uuid AND server_id = $2::uuid AND slot = $3"#,
    )
    .bind(auth.user_id.to_string())
    .bind(server_id.to_string())
    .bind(slot)
    .execute(&state.db.pool)
    .await
    .map_err(NexusError::Database)?;

    if result.rows_affected() == 0 {
        return Err(NexusError::NotFound {
            resource: "Boost slot not found".to_string(),
        });
    }

    recalculate_tier(&state, server_id).await?;

    Ok(Json(serde_json::json!({ "removed": true })))
}

/// `GET /servers/{server_id}/boosters` — list active boosters
async fn list_boosters(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
    Path(server_id): Path<Uuid>,
) -> NexusResult<Json<Vec<BoosterEntry>>> {
    let rows: Vec<BoosterEntryRow> = sqlx::query_as(
        r#"SELECT id::text AS id, user_id::text AS user_id, server_id::text AS server_id, slot, started_at::text AS started_at, expires_at::text AS expires_at
           FROM server_boosters
           WHERE server_id = $1::uuid AND (expires_at IS NULL OR expires_at > NOW())
           ORDER BY started_at ASC"#,
    )
    .bind(server_id.to_string())
    .fetch_all(&state.db.pool)
    .await
    .map_err(NexusError::Database)?;

    let entries: NexusResult<Vec<BoosterEntry>> =
        rows.into_iter().map(BoosterEntry::try_from).collect();
    Ok(Json(entries?))
}

/// `GET /servers/{server_id}/boost-tier` — current tier + perks
async fn get_boost_tier(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthContext>,
    Path(server_id): Path<Uuid>,
) -> NexusResult<Json<BoostTierInfo>> {
    let row: Option<(i16, i32, Option<String>)> = sqlx::query_as(
        "SELECT boost_tier, booster_count, vanity_code FROM servers WHERE id = $1::uuid",
    )
    .bind(server_id.to_string())
    .fetch_optional(&state.db.pool)
    .await
    .map_err(NexusError::Database)?;

    let (boost_tier, booster_count, vanity_code) = row.ok_or_else(|| NexusError::NotFound {
        resource: "Server not found".to_string(),
    })?;

    let mut info = perks(boost_tier, vanity_code);
    info.booster_count = booster_count as i64;
    Ok(Json(info))
}

/// `PATCH /servers/{server_id}/vanity-url` — set vanity invite code (tier 2+)
async fn set_vanity_url(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(server_id): Path<Uuid>,
    Json(body): Json<SetVanityUrlRequest>,
) -> NexusResult<Json<serde_json::Value>> {
    let row: Option<(i16, String)> =
        sqlx::query_as("SELECT boost_tier, owner_id::text FROM servers WHERE id = $1::uuid")
            .bind(server_id.to_string())
            .fetch_optional(&state.db.pool)
            .await
            .map_err(NexusError::Database)?;

    let (boost_tier, owner_id_str) = row.ok_or_else(|| NexusError::NotFound {
        resource: "Server not found".to_string(),
    })?;

    if boost_tier < 2 {
        return Err(NexusError::Forbidden);
    }

    let is_owner = owner_id_str.parse::<Uuid>().ok() == Some(auth.user_id);
    if !is_owner {
        let member = members::find_member(&state.db.pool, auth.user_id, server_id)
            .await
            .map_err(NexusError::Database)?
            .ok_or(NexusError::Forbidden)?;
        let all_roles = roles::list_server_roles(&state.db.pool, server_id)
            .await
            .map_err(NexusError::Database)?;
        let base = all_roles
            .iter()
            .find(|r| r.is_default)
            .map(|r| Permissions::from_bits_truncate(r.permissions))
            .unwrap_or_else(Permissions::empty);
        let bits = all_roles
            .iter()
            .filter(|r| !r.is_default && member.roles.contains(&r.id))
            .map(|r| Permissions::from_bits_truncate(r.permissions))
            .fold(base, |acc, p| acc | p);
        if !bits.contains(Permissions::MANAGE_SERVER) && !bits.contains(Permissions::ADMINISTRATOR)
        {
            return Err(NexusError::Forbidden);
        }
    }

    let code = body.code.trim().to_string();
    if code.len() < 2 || code.len() > 32 || !code.chars().all(|c| c.is_alphanumeric() || c == '-') {
        return Err(NexusError::Validation {
            message: "Vanity code must be 2-32 alphanumeric characters or hyphens".to_string(),
        });
    }

    sqlx::query("UPDATE servers SET vanity_code = $1 WHERE id = $2::uuid")
        .bind(&code)
        .bind(server_id.to_string())
        .execute(&state.db.pool)
        .await
        .map_err(NexusError::Database)?;

    Ok(Json(serde_json::json!({ "vanity_code": code })))
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal helpers
// ─────────────────────────────────────────────────────────────────────────────

async fn recalculate_tier(state: &AppState, server_id: Uuid) -> NexusResult<()> {
    let count_row: Option<(i64,)> = sqlx::query_as(
        r#"SELECT COUNT(*)
           FROM server_boosters
           WHERE server_id = $1::uuid
             AND (expires_at IS NULL OR expires_at > NOW())"#,
    )
    .bind(server_id.to_string())
    .fetch_optional(&state.db.pool)
    .await
    .map_err(NexusError::Database)?;

    let count = count_row.map(|(c,)| c).unwrap_or(0);
    let new_tier = tier_from_count(count) as i32;

    // Persist and check if tier changed
    let changed_row: Option<(i32,)> = sqlx::query_as(
        r#"UPDATE servers
           SET boost_tier    = $1,
               booster_count = $2
           WHERE id = $3::uuid
             AND (boost_tier != $1 OR booster_count != $2)
           RETURNING boost_tier"#,
    )
    .bind(new_tier)
    .bind(count as i32)
    .bind(server_id.to_string())
    .fetch_optional(&state.db.pool)
    .await
    .map_err(NexusError::Database)?;

    if changed_row.is_some() {
        let _ = state.gateway_tx.send(GatewayEvent {
            event_type: event_types::SERVER_TIER_UPDATE.to_string(),
            data: serde_json::json!({
                "server_id": server_id,
                "tier": new_tier,
                "booster_count": count,
            }),
            server_id: Some(server_id),
            channel_id: None,
            user_id: None,
        });
    }

    Ok(())
}
