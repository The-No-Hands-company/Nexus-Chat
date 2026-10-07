//! Message bookmark routes — save, list, remove.
//!
//! Users can bookmark any message with an optional personal note.
//! Bookmarks are private — only visible to the bookmarking user.
//!
//! POST   /users/@me/bookmarks            — Bookmark a message
//! DELETE /users/@me/bookmarks/:msg_id    — Remove bookmark
//! GET    /users/@me/bookmarks            — List bookmarks with hydrated messages

use axum::http::HeaderMap;
use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    middleware,
    routing::{delete, get},
};
use chrono::{DateTime, Utc};
use nexus_common::error::{NexusError, NexusResult};
use nexus_common::snowflake;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::HashMap;
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
            "/users/@me/bookmarks",
            get(list_bookmarks).post(add_bookmark),
        )
        .route("/users/@me/bookmarks/{message_id}", delete(remove_bookmark))
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ))
}

// ============================================================
// Models
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bookmark {
    pub id: Uuid,
    pub user_id: Uuid,
    pub message_id: Uuid,
    pub channel_id: Uuid,
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Hydrated message content (best effort — null if message was deleted)
    pub message: Option<BookmarkedMessagePreview>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookmarkedMessagePreview {
    pub id: Uuid,
    pub content: String,
    pub author_id: Uuid,
    pub author_username: String,
    pub channel_id: Uuid,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct AddBookmarkRequest {
    message_id: Uuid,
    note: Option<String>,
    channel_id: Uuid,
}

// ============================================================
// DB row helpers
// ============================================================

#[derive(sqlx::FromRow)]
struct BookmarkRow {
    id: String,
    user_id: String,
    message_id: String,
    channel_id: String,
    note: Option<String>,
    created_at: String,
}

fn parse_dt(s: &str) -> Result<DateTime<Utc>, String> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
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

impl TryFrom<BookmarkRow> for Bookmark {
    type Error = NexusError;
    fn try_from(r: BookmarkRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: r
                .id
                .parse::<Uuid>()
                .map_err(|e| NexusError::Internal(anyhow::anyhow!(e)))?,
            user_id: r
                .user_id
                .parse::<Uuid>()
                .map_err(|e| NexusError::Internal(anyhow::anyhow!(e)))?,
            message_id: r
                .message_id
                .parse::<Uuid>()
                .map_err(|e| NexusError::Internal(anyhow::anyhow!(e)))?,
            channel_id: r
                .channel_id
                .parse::<Uuid>()
                .map_err(|e| NexusError::Internal(anyhow::anyhow!(e)))?,
            note: r.note,
            created_at: parse_dt(&r.created_at)
                .map_err(|e| NexusError::Internal(anyhow::anyhow!(e)))?,
            message: None, // hydrated separately
        })
    }
}

// ============================================================
// Handlers
// ============================================================

/// POST /api/v1/users/@me/bookmarks
async fn add_bookmark(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<AddBookmarkRequest>,
) -> NexusResult<Json<Bookmark>> {
    // ── Rate limiting: 20 bookmarks per user per minute ────────────
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:bookmark:add:{}", ctx.user_id),
        20,
        60,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:bookmark:add:tag:{tag}"),
        40,
        60,
    )
    .await?;

    let message_row: (String, Option<String>) = sqlx::query_as(
        r#"
        SELECT m.channel_id::text AS channel_id, c.server_id::text AS server_id
        FROM messages m
        INNER JOIN channels c ON c.id = m.channel_id
        WHERE m.id = $1 AND m.deleted_at IS NULL
        "#,
    )
    .bind(body.message_id.to_string())
    .fetch_optional(&state.db.pool)
    .await
    .map_err(|e| NexusError::Internal(e.into()))?
    .ok_or(NexusError::NotFound {
        resource: "Message".into(),
    })?;

    let actual_channel_id = message_row
        .0
        .parse::<Uuid>()
        .map_err(|e| NexusError::Internal(e.into()))?;
    if body.channel_id != actual_channel_id {
        return Err(NexusError::Validation {
            message: "channel_id does not match message channel".into(),
        });
    }

    if let Some(server_id_str) = message_row.1 {
        let server_id = server_id_str
            .parse::<Uuid>()
            .map_err(|e| NexusError::Internal(e.into()))?;
        if !nexus_db::repository::members::is_member(&state.db.pool, ctx.user_id, server_id)
            .await
            .map_err(|e| NexusError::Internal(e.into()))?
        {
            return Err(NexusError::Forbidden);
        }
    } else {
        let is_dm_participant: (bool,) = sqlx::query_as(
            "SELECT EXISTS(SELECT 1 FROM dm_participants WHERE channel_id = $1::uuid AND user_id = $2::uuid)",
        )
        .bind(actual_channel_id.to_string())
        .bind(ctx.user_id.to_string())
        .fetch_one(&state.db.pool)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;
        if !is_dm_participant.0 {
            return Err(NexusError::Forbidden);
        }
    }

    let bm_id = snowflake::generate_id();

    let row: BookmarkRow = sqlx::query_as(
        r#"
        INSERT INTO message_bookmarks (id, user_id, message_id, channel_id, note, created_at)
        VALUES ($1, $2, $3, $4, $5, NOW())
        ON CONFLICT (user_id, message_id) DO UPDATE SET note = EXCLUDED.note
        RETURNING id::text, user_id::text, message_id::text, channel_id::text, note, created_at::text
        "#,
    )
    .bind(bm_id.to_string())
    .bind(ctx.user_id.to_string())
    .bind(body.message_id.to_string())
    .bind(actual_channel_id.to_string())
    .bind(body.note.as_deref())
    .fetch_one(&state.db.pool)
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    let mut bm = Bookmark::try_from(row)?;
    bm.message = hydrate_message(&state, body.message_id).await;
    Ok(Json(bm))
}

/// DELETE /api/v1/users/@me/bookmarks/{message_id}
async fn remove_bookmark(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(message_id): Path<Uuid>,
) -> NexusResult<Json<serde_json::Value>> {
    let result =
        sqlx::query("DELETE FROM message_bookmarks WHERE user_id = $1 AND message_id = $2")
            .bind(ctx.user_id.to_string())
            .bind(message_id.to_string())
            .execute(&state.db.pool)
            .await
            .map_err(|e| NexusError::Internal(e.into()))?;

    if result.rows_affected() == 0 {
        return Err(NexusError::NotFound {
            resource: "Bookmark".into(),
        });
    }

    Ok(Json(serde_json::json!({ "ok": true })))
}

/// GET /api/v1/users/@me/bookmarks
async fn list_bookmarks(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
) -> NexusResult<Json<Vec<Bookmark>>> {
    let rows: Vec<BookmarkRow> = sqlx::query_as(
        r#"
        SELECT id::text, user_id::text, message_id::text, channel_id::text,
               note, created_at::text
        FROM message_bookmarks
        WHERE user_id = $1
        ORDER BY created_at DESC
        LIMIT 200
        "#,
    )
    .bind(ctx.user_id.to_string())
    .fetch_all(&state.db.pool)
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    let mut bookmarks = Vec::with_capacity(rows.len());
    let mut message_ids = Vec::with_capacity(rows.len());
    for row in rows {
        let bm = Bookmark::try_from(row)?;
        message_ids.push(bm.message_id);
        bookmarks.push(bm);
    }

    let mut preview_by_message_id: HashMap<Uuid, BookmarkedMessagePreview> = HashMap::new();
    if !message_ids.is_empty() {
        let mut qb = sqlx::QueryBuilder::new(
            "SELECT m.id::text AS id, m.content AS content, m.author_id::text AS author_id, \
                    COALESCE(u.username, '') AS username, m.channel_id::text AS channel_id, m.created_at::text AS created_at \
             FROM messages m \
             LEFT JOIN users u ON u.id = m.author_id \
             WHERE m.deleted_at IS NULL AND m.id IN (",
        );
        {
            let mut separated = qb.separated(", ");
            for message_id in &message_ids {
                separated.push_bind(message_id.to_string());
            }
        }
        qb.push(")");

        let rows = qb
            .build()
            .fetch_all(&state.db.pool)
            .await
            .map_err(|e| NexusError::Internal(e.into()))?;

        for row in rows {
            let id = match row
                .try_get::<String, _>("id")
                .ok()
                .and_then(|s| s.parse::<Uuid>().ok())
            {
                Some(v) => v,
                None => continue,
            };
            let author_id = match row
                .try_get::<String, _>("author_id")
                .ok()
                .and_then(|s| s.parse::<Uuid>().ok())
            {
                Some(v) => v,
                None => continue,
            };
            let channel_id = match row
                .try_get::<String, _>("channel_id")
                .ok()
                .and_then(|s| s.parse::<Uuid>().ok())
            {
                Some(v) => v,
                None => continue,
            };

            let created_at = row
                .try_get::<String, _>("created_at")
                .ok()
                .and_then(|s| parse_dt(&s).ok())
                .unwrap_or_else(Utc::now);

            let preview = BookmarkedMessagePreview {
                id,
                content: row.try_get::<String, _>("content").unwrap_or_default(),
                author_id,
                author_username: row.try_get::<String, _>("username").unwrap_or_default(),
                channel_id,
                created_at,
            };
            preview_by_message_id.insert(id, preview);
        }
    }

    for bm in &mut bookmarks {
        bm.message = preview_by_message_id.get(&bm.message_id).cloned();
    }

    Ok(Json(bookmarks))
}

// ============================================================
// Hydration helper
// ============================================================

#[derive(sqlx::FromRow)]
struct HydrateRow {
    id: String,
    content: String,
    author_id: String,
    username: Option<String>,
    channel_id: String,
}

/// Fetch a message preview for display in the bookmark list.
/// Returns `None` if the message has been deleted — bookmarks of deleted
/// messages are kept (user may still want the note) but have `null` content.
async fn hydrate_message(state: &AppState, message_id: Uuid) -> Option<BookmarkedMessagePreview> {
    let row: Option<HydrateRow> = sqlx::query_as(
        r#"
        SELECT m.id::text AS id, m.content AS content, m.author_id::text AS author_id,
               u.username AS username, m.channel_id::text AS channel_id
        FROM messages m
        JOIN users u ON u.id = m.author_id
        WHERE m.id = $1 AND m.deleted_at IS NULL
        "#,
    )
    .bind(message_id.to_string())
    .fetch_optional(&state.db.pool)
    .await
    .ok()
    .flatten();

    let HydrateRow {
        id: id_str,
        content,
        author_id: author_id_str,
        username: username_opt,
        channel_id: channel_id_str,
    } = row?;

    let parse_dt_local = |s: &str| -> Option<DateTime<Utc>> {
        chrono::DateTime::parse_from_rfc3339(s)
            .map(|dt| dt.with_timezone(&Utc))
            .ok()
    };

    // Fetch created_at separately — avoids a 6-tuple which gets complex
    let created_at_str: Option<(String,)> =
        sqlx::query_as("SELECT created_at::text FROM messages WHERE id = $1")
            .bind(message_id.to_string())
            .fetch_optional(&state.db.pool)
            .await
            .ok()
            .flatten();

    Some(BookmarkedMessagePreview {
        id: id_str.parse().ok()?,
        content,
        author_id: author_id_str.parse().ok()?,
        author_username: username_opt.unwrap_or_default(),
        channel_id: channel_id_str.parse().ok()?,
        created_at: created_at_str
            .and_then(|(s,)| parse_dt_local(&s))
            .unwrap_or_else(Utc::now),
    })
}
