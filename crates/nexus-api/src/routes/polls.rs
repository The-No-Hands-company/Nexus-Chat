//! Poll routes — create, vote, retract, results, and end polls.
//!
//! Polls can be standalone or attached to a message in a channel.
//! They support multiple choice, anonymous voting, and auto-expiry.
//!
//! POST   /channels/:id/polls                          — Create poll
//! POST   /channels/:id/polls/:poll_id/vote            — Cast vote
//! DELETE /channels/:id/polls/:poll_id/vote            — Retract vote
//! GET    /channels/:id/polls/:poll_id/results         — Get results
//! POST   /channels/:id/polls/:poll_id/end             — End early (MANAGE_MESSAGES)

use axum::http::HeaderMap;
use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    middleware,
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use nexus_common::{
    error::{NexusError, NexusResult},
    gateway_event::GatewayEvent,
    permissions::Permissions,
    snowflake,
};
use nexus_db::repository::{channels, members, roles, servers};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    AppState,
    middleware::{AuthContext, check_rate_limit_with_fallback, extract_client_tag},
};

// ============================================================
// Router
// ============================================================

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/channels/{channel_id}/polls",
            get(list_polls).post(create_poll),
        )
        .route(
            "/channels/{channel_id}/polls/{poll_id}/vote",
            post(cast_vote).delete(retract_vote),
        )
        .route(
            "/channels/{channel_id}/polls/{poll_id}/results",
            get(get_results),
        )
        .route("/channels/{channel_id}/polls/{poll_id}/end", post(end_poll))
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ))
}

// ============================================================
// Models
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Poll {
    pub id: Uuid,
    pub channel_id: Uuid,
    pub message_id: Option<Uuid>,
    pub author_id: Uuid,
    pub question: String,
    pub options: Vec<String>,
    pub ends_at: Option<DateTime<Utc>>,
    pub allow_multiselect: bool,
    pub is_anonymous: bool,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct PollOptionResult {
    index: i32,
    label: String,
    vote_count: i64,
    /// voter_ids is only populated when `is_anonymous = false`
    voter_ids: Vec<Uuid>,
}

#[derive(Debug, Serialize)]
struct PollResults {
    poll: Poll,
    options: Vec<PollOptionResult>,
    total_voters: i64,
}

#[derive(Debug, Deserialize)]
struct CreatePollRequest {
    question: String,
    options: Vec<String>,
    ends_at: Option<DateTime<Utc>>,
    allow_multiselect: Option<bool>,
    is_anonymous: Option<bool>,
    /// Optional: attach this poll to an existing message
    message_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
struct VoteRequest {
    option_indices: Vec<i32>,
}

// ============================================================
// DB row types (AnyPool-compatible — all timestamps as String)
// ============================================================

#[derive(sqlx::FromRow)]
struct PollRow {
    id: String,
    channel_id: String,
    message_id: Option<String>,
    author_id: String,
    question: String,
    options: String, // jsonb as text
    ends_at: Option<String>,
    allow_multiselect: bool,
    is_anonymous: bool,
    status: String,
    created_at: String,
    updated_at: String,
}

fn parse_dt(s: &str) -> Result<DateTime<Utc>, String> {
    // Try RFC 3339 first
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    // Normalise Postgres TIMESTAMPTZ "2026-02-22 14:00:00+00" → "2026-02-22T14:00:00+00:00"
    let normalised = s.replacen(' ', "T", 1);
    let with_colon = if normalised.ends_with("+00") {
        format!("{normalised}:00")
    } else {
        normalised
    };
    chrono::DateTime::parse_from_rfc3339(&with_colon)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| format!("cannot parse timestamp '{s}': {e}"))
}

impl TryFrom<PollRow> for Poll {
    type Error = NexusError;

    fn try_from(r: PollRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: r
                .id
                .parse::<Uuid>()
                .map_err(|e| NexusError::Internal(anyhow::anyhow!(e)))?,
            channel_id: r
                .channel_id
                .parse::<Uuid>()
                .map_err(|e| NexusError::Internal(anyhow::anyhow!(e)))?,
            message_id: r.message_id.and_then(|s| s.parse().ok()),
            author_id: r
                .author_id
                .parse::<Uuid>()
                .map_err(|e| NexusError::Internal(anyhow::anyhow!(e)))?,
            question: r.question,
            options: serde_json::from_str::<Vec<String>>(&r.options).unwrap_or_default(),
            ends_at: r.ends_at.as_deref().and_then(|s| parse_dt(s).ok()),
            allow_multiselect: r.allow_multiselect,
            is_anonymous: r.is_anonymous,
            status: r.status,
            created_at: parse_dt(&r.created_at)
                .map_err(|e| NexusError::Internal(anyhow::anyhow!(e)))?,
            updated_at: parse_dt(&r.updated_at)
                .map_err(|e| NexusError::Internal(anyhow::anyhow!(e)))?,
        })
    }
}

// ============================================================
// Permission helper
// ============================================================

async fn require_manage_messages(
    state: &AppState,
    user_id: Uuid,
    channel_id: Uuid,
) -> NexusResult<()> {
    let channel = channels::find_by_id(&state.db.pool, channel_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Channel".into(),
        })?;

    let server_id = channel.server_id.ok_or(NexusError::MissingPermission {
        permission: "MANAGE_MESSAGES".into(),
    })?;

    let server = servers::find_by_id(&state.db.pool, server_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Server".into(),
        })?;

    if server.owner_id == user_id {
        return Ok(());
    }

    let member = members::find_member(&state.db.pool, user_id, server_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .ok_or(NexusError::MissingPermission {
            permission: "MANAGE_MESSAGES".into(),
        })?;

    let all_roles = roles::list_server_roles(&state.db.pool, server_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    let base = all_roles
        .iter()
        .find(|r| r.is_default)
        .map(|r| Permissions::from_bits_truncate(r.permissions))
        .unwrap_or_else(Permissions::empty);

    let effective = all_roles
        .iter()
        .filter(|r| !r.is_default && member.roles.contains(&r.id))
        .map(|r| Permissions::from_bits_truncate(r.permissions))
        .fold(base, |acc, rp| acc | rp);

    if effective.has(Permissions::MANAGE_MESSAGES) {
        Ok(())
    } else {
        Err(NexusError::MissingPermission {
            permission: "MANAGE_MESSAGES".into(),
        })
    }
}

// ============================================================
// Handlers
// ============================================================

/// GET /api/v1/channels/{channel_id}/polls
///
/// Returns all open (or recently ended) polls in the channel, ordered by
/// creation time ascending.  Clients load this once on channel open and
/// then keep state up-to-date via `POLL_VOTE_ADD` / `POLL_ENDED` gateway events.
async fn list_polls(
    State(state): State<Arc<AppState>>,
    Extension(_ctx): Extension<AuthContext>,
    Path(channel_id): Path<Uuid>,
) -> NexusResult<Json<Vec<Poll>>> {
    let rows: Vec<PollRow> = sqlx::query_as(
        r#"
        SELECT id::text, channel_id::text, message_id::text, author_id::text,
               question, options::text, ends_at::text, allow_multiselect, is_anonymous,
               status, created_at::text, updated_at::text
        FROM polls
        WHERE channel_id = $1
        ORDER BY created_at ASC
        "#,
    )
    .bind(channel_id.to_string())
    .fetch_all(&state.db.pool)
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    let polls: Result<Vec<Poll>, _> = rows.into_iter().map(Poll::try_from).collect();
    Ok(Json(polls?))
}

/// POST /api/v1/channels/{channel_id}/polls
async fn create_poll(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    headers: HeaderMap,
    Path(channel_id): Path<Uuid>,
    Json(body): Json<CreatePollRequest>,
) -> NexusResult<Json<Poll>> {
    // Rate limiting: 10 polls per hour per user, 20 per IP
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:polls:user:{}", ctx.user_id),
        10,
        3600,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:polls:tag:{tag}"),
        20,
        3600,
    )
    .await?;

    if body.question.trim().is_empty() {
        return Err(NexusError::Validation {
            message: "Poll question cannot be empty".into(),
        });
    }
    if body.options.len() < 2 {
        return Err(NexusError::Validation {
            message: "Polls require at least 2 options".into(),
        });
    }
    if body.options.len() > 10 {
        return Err(NexusError::Validation {
            message: "Polls may have at most 10 options".into(),
        });
    }
    if let Some(ends_at) = body.ends_at
        && ends_at <= Utc::now() {
            return Err(NexusError::Validation {
                message: "ends_at must be in the future".into(),
            });
        }

    let poll_id = snowflake::generate_id();
    let options_json =
        serde_json::to_string(&body.options).map_err(|e| NexusError::Internal(e.into()))?;

    let row: PollRow = sqlx::query_as(
        r#"
        INSERT INTO polls
            (id, channel_id, message_id, author_id, question, options, ends_at,
             allow_multiselect, is_anonymous, status, created_at, updated_at)
        VALUES
            ($1, $2, $3, $4, $5, $6::jsonb, $7,
             $8, $9, 'open', NOW(), NOW())
        RETURNING
            id::text, channel_id::text, message_id::text, author_id::text,
            question, options::text, ends_at::text, allow_multiselect, is_anonymous,
            status, created_at::text, updated_at::text
        "#,
    )
    .bind(poll_id.to_string())
    .bind(channel_id.to_string())
    .bind(body.message_id.map(|id| id.to_string()))
    .bind(ctx.user_id.to_string())
    .bind(&body.question)
    .bind(&options_json)
    .bind(body.ends_at.map(|dt| dt.to_rfc3339()))
    .bind(body.allow_multiselect.unwrap_or(false))
    .bind(body.is_anonymous.unwrap_or(false))
    .fetch_one(&state.db.pool)
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    let poll = Poll::try_from(row)?;

    // Emit gateway event
    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: nexus_common::gateway_event::event_types::POLL_CREATE.into(),
        data: serde_json::to_value(&poll).unwrap_or_default(),
        server_id: None,
        channel_id: None,
        user_id: None,
    });

    Ok(Json(poll))
}

/// POST /api/v1/channels/{channel_id}/polls/{poll_id}/vote
async fn cast_vote(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    headers: HeaderMap,
    Path((channel_id, poll_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<VoteRequest>,
) -> NexusResult<Json<serde_json::Value>> {
    // Rate limiting: 30 votes per minute per user (prevents vote spam)
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:vote:user:{}", ctx.user_id),
        30,
        60,
    )
    .await?;
    check_rate_limit_with_fallback(state.db.redis.as_ref(), format!("rl:vote:tag:{tag}"), 60, 60)
        .await?;

    if body.option_indices.is_empty() {
        return Err(NexusError::Validation {
            message: "option_indices must not be empty".into(),
        });
    }

    // Fetch poll — check it's open and belongs to the channel
    let row: Option<PollRow> = sqlx::query_as(
        r#"
        SELECT id::text, channel_id::text, message_id::text, author_id::text,
               question, options::text, ends_at::text, allow_multiselect, is_anonymous,
               status, created_at::text, updated_at::text
        FROM polls
        WHERE id = $1 AND channel_id = $2 AND status = 'open'
        "#,
    )
    .bind(poll_id.to_string())
    .bind(channel_id.to_string())
    .fetch_optional(&state.db.pool)
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    let poll = match row {
        Some(r) => Poll::try_from(r)?,
        None => {
            return Err(NexusError::NotFound {
                resource: "Poll".into(),
            });
        }
    };

    // Check not expired
    if let Some(ends_at) = poll.ends_at
        && Utc::now() > ends_at {
            return Err(NexusError::Validation {
                message: "This poll has ended".into(),
            });
        }

    // Validate indices
    let num_options = poll.options.len() as i32;
    let unique_indices: Vec<i32> = body
        .option_indices
        .iter()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    for &idx in &unique_indices {
        if idx < 0 || idx >= num_options {
            return Err(NexusError::Validation {
                message: format!("option_index {idx} is out of range (0..{num_options})"),
            });
        }
    }

    // Enforce single-choice constraint
    if !poll.allow_multiselect && unique_indices.len() > 1 {
        return Err(NexusError::Validation {
            message: "This poll does not allow multiple selections".into(),
        });
    }

    // Insert votes in one query — ignore duplicates (idempotent re-vote for same option).
    let now = Utc::now().to_rfc3339();
    let mut qb = sqlx::QueryBuilder::new(
        "INSERT INTO poll_votes (id, poll_id, user_id, option_index, voted_at) ",
    );
    qb.push_values(unique_indices.iter(), |mut b, idx| {
        b.push_bind(snowflake::generate_id().to_string())
            .push_bind(poll_id.to_string())
            .push_bind(ctx.user_id.to_string())
            .push_bind(*idx)
            .push_bind(&now);
    });
    qb.push(" ON CONFLICT (poll_id, user_id, option_index) DO NOTHING");
    qb.build()
        .execute(&state.db.pool)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: nexus_common::gateway_event::event_types::POLL_VOTE_ADD.into(),
        data: serde_json::json!({
            "poll_id": poll_id,
            "channel_id": channel_id,
            "user_id": ctx.user_id,
            "option_indices": unique_indices,
        }),
        server_id: None,
        channel_id: None,
        user_id: None,
    });

    Ok(Json(serde_json::json!({ "ok": true })))
}

/// DELETE /api/v1/channels/{channel_id}/polls/{poll_id}/vote
async fn retract_vote(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path((channel_id, poll_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<Json<serde_json::Value>> {
    // Verify poll exists and is open
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM polls WHERE id = $1 AND channel_id = $2 AND status = 'open')",
    )
    .bind(poll_id.to_string())
    .bind(channel_id.to_string())
    .fetch_one(&state.db.pool)
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    if !exists {
        return Err(NexusError::NotFound {
            resource: "Poll".into(),
        });
    }

    sqlx::query("DELETE FROM poll_votes WHERE poll_id = $1 AND user_id = $2")
        .bind(poll_id.to_string())
        .bind(ctx.user_id.to_string())
        .execute(&state.db.pool)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: nexus_common::gateway_event::event_types::POLL_VOTE_REMOVE.into(),
        data: serde_json::json!({
            "poll_id": poll_id,
            "channel_id": channel_id,
            "user_id": ctx.user_id,
        }),
        server_id: None,
        channel_id: None,
        user_id: None,
    });

    Ok(Json(serde_json::json!({ "ok": true })))
}

/// GET /api/v1/channels/{channel_id}/polls/{poll_id}/results
async fn get_results(
    State(state): State<Arc<AppState>>,
    Extension(_ctx): Extension<AuthContext>,
    Path((channel_id, poll_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<Json<PollResults>> {
    let row: Option<PollRow> = sqlx::query_as(
        r#"
        SELECT id::text, channel_id::text, message_id::text, author_id::text,
               question, options::text, ends_at::text, allow_multiselect, is_anonymous,
               status, created_at::text, updated_at::text
        FROM polls
        WHERE id = $1 AND channel_id = $2
        "#,
    )
    .bind(poll_id.to_string())
    .bind(channel_id.to_string())
    .fetch_optional(&state.db.pool)
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    let poll = match row {
        Some(r) => Poll::try_from(r)?,
        None => {
            return Err(NexusError::NotFound {
                resource: "Poll".into(),
            });
        }
    };

    // Collect vote counts per option
    let vote_rows: Vec<(i32, i64, String)> = sqlx::query_as(
        r#"
        SELECT option_index, COUNT(*) AS vote_count, user_id::text
        FROM poll_votes
        WHERE poll_id = $1
        GROUP BY option_index, user_id
        "#,
    )
    .bind(poll_id.to_string())
    .fetch_all(&state.db.pool)
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    // Aggregate per option index
    let mut counts: std::collections::HashMap<i32, (i64, Vec<Uuid>)> =
        std::collections::HashMap::new();
    for (idx, _, voter_id_str) in &vote_rows {
        let entry = counts.entry(*idx).or_insert((0, vec![]));
        entry.0 += 1;
        if let Ok(uid) = voter_id_str.parse::<Uuid>() {
            entry.1.push(uid);
        }
    }

    let mut options: Vec<PollOptionResult> = poll
        .options
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let (vote_count, voter_ids_raw) =
                counts.get(&(i as i32)).cloned().unwrap_or((0, vec![]));
            PollOptionResult {
                index: i as i32,
                label: label.clone(),
                vote_count,
                voter_ids: if poll.is_anonymous {
                    vec![]
                } else {
                    voter_ids_raw
                },
            }
        })
        .collect();
    options.sort_by_key(|o| o.index);

    let total_voters: i64 =
        sqlx::query_scalar("SELECT COUNT(DISTINCT user_id) FROM poll_votes WHERE poll_id = $1")
            .bind(poll_id.to_string())
            .fetch_one(&state.db.pool)
            .await
            .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(PollResults {
        poll,
        options,
        total_voters,
    }))
}

/// POST /api/v1/channels/{channel_id}/polls/{poll_id}/end
///
/// End a poll early. Requires MANAGE_MESSAGES permission.
async fn end_poll(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path((channel_id, poll_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<Json<Poll>> {
    require_manage_messages(&state, ctx.user_id, channel_id).await?;

    let row: Option<PollRow> = sqlx::query_as(
        r#"
        UPDATE polls
        SET status = 'ended', updated_at = NOW()
        WHERE id = $1 AND channel_id = $2 AND status = 'open'
        RETURNING
            id::text, channel_id::text, message_id::text, author_id::text,
            question, options::text, ends_at::text, allow_multiselect, is_anonymous,
            status, created_at::text, updated_at::text
        "#,
    )
    .bind(poll_id.to_string())
    .bind(channel_id.to_string())
    .fetch_optional(&state.db.pool)
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    let poll = match row {
        Some(r) => Poll::try_from(r)?,
        None => {
            return Err(NexusError::NotFound {
                resource: "Poll or poll already ended".into(),
            });
        }
    };

    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: nexus_common::gateway_event::event_types::POLL_ENDED.into(),
        data: serde_json::to_value(&poll).unwrap_or_default(),
        server_id: None,
        channel_id: None,
        user_id: None,
    });

    Ok(Json(poll))
}
