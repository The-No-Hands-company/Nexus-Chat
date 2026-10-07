//! Inline bot suggestion routes.
//!
//! GET  /channels/{id}/inline-query              — proxy query to a bot's callback URL
//! GET  /channels/{id}/bots/inline-triggers      — list bots with triggers active in this channel
//! POST /bots/@me/inline-triggers               — bot registers a trigger prefix
//! DELETE /bots/@me/inline-triggers/{tid}       — bot removes a trigger

use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    middleware,
    routing::{delete, get, post},
};
use nexus_common::error::{NexusError, NexusResult};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

use crate::{AppState, middleware::AuthContext};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/channels/{channel_id}/inline-query", get(inline_query))
        .route(
            "/channels/{channel_id}/bots/inline-triggers",
            get(list_channel_triggers),
        )
        .route("/bots/@me/inline-triggers", post(register_trigger))
        .route(
            "/bots/@me/inline-triggers/{trigger_id}",
            delete(remove_trigger),
        )
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ))
}

// ─────────────────────────────────────────────────────────────────────────────
// Models
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct InlineQueryParams {
    query: String,
    bot_id: Option<Uuid>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct InlineSuggestion {
    pub title: String,
    pub description: Option<String>,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview_url: Option<String>,
}

#[derive(Debug, Serialize)]
struct InlineQueryResponse {
    bot_id: Uuid,
    suggestions: Vec<InlineSuggestion>,
}

#[derive(Debug, Serialize)]
struct BotTrigger {
    id: Uuid,
    bot_id: Uuid,
    prefix: String,
    description: String,
    bot_name: String,
    bot_avatar: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RegisterTriggerRequest {
    prefix: String,
    description: String,
    callback_url: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Handlers
// ─────────────────────────────────────────────────────────────────────────────

/// GET /api/v1/channels/:channel_id/inline-query?query=…[&bot_id=…]
///
/// Resolves which bot to query (via `bot_id` param or by matching the query prefix
/// against registered `bot_inline_triggers`), then proxies the query to that bot's
/// `callback_url` and returns the results.
async fn inline_query(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
    Query(params): Query<InlineQueryParams>,
) -> NexusResult<Json<Vec<InlineQueryResponse>>> {
    // Lightweight existence + membership check
    let channel_row = sqlx::query(
        "SELECT id::text AS id, server_id::text AS server_id FROM channels WHERE id = $1::uuid",
    )
    .bind(channel_id.to_string())
    .fetch_optional(&state.db.pool)
    .await
    .map_err(NexusError::Database)?
    .ok_or(NexusError::NotFound {
        resource: "Channel".into(),
    })?;

    let server_id_str: Option<String> = channel_row
        .try_get::<Option<String>, _>("server_id")
        .unwrap_or(None);

    if let Some(sid_str) = server_id_str
        && let Ok(server_id) = sid_str.parse::<Uuid>() {
            sqlx::query(
                "SELECT id FROM server_members WHERE user_id = $1::uuid AND server_id = $2::uuid",
            )
            .bind(auth.user_id.to_string())
            .bind(server_id.to_string())
            .fetch_optional(&state.db.pool)
            .await
            .map_err(NexusError::Database)?
            .ok_or(NexusError::Forbidden)?;
        }

    let query_text = params.query.trim().to_owned();
    if query_text.is_empty() {
        return Ok(Json(vec![]));
    }

    // Determine which bot(s) to query
    let triggers: Vec<(Uuid, String)> = if let Some(bot_id) = params.bot_id {
        // Use specified bot; find any trigger prefix for it
        let row = sqlx::query(
            "SELECT bot_id::text AS bot_id, callback_url FROM bot_inline_triggers WHERE bot_id = $1::uuid LIMIT 1",
        )
        .bind(bot_id.to_string())
        .fetch_optional(&state.db.pool)
        .await
        .map_err(NexusError::Database)?;

        if let Some(r) = row {
            let bid: Uuid = r
                .try_get::<String, _>("bot_id")
                .unwrap_or_default()
                .parse()
                .unwrap_or(bot_id);
            let cb: String = r.try_get("callback_url").unwrap_or_default();
            vec![(bid, cb)]
        } else {
            return Ok(Json(vec![]));
        }
    } else {
        // Match by prefix: e.g. "@gif cats" → prefix "@gif"
        let prefix = query_text
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();
        let rows = sqlx::query(
            "SELECT DISTINCT ON (bot_id) bot_id::text AS bot_id, callback_url FROM bot_inline_triggers WHERE prefix = $1",
        )
        .bind(prefix)
        .fetch_all(&state.db.pool)
        .await
        .map_err(NexusError::Database)?;

        rows.iter()
            .filter_map(|r| {
                let bid: Uuid = r
                    .try_get::<String, _>("bot_id")
                    .unwrap_or_default()
                    .parse()
                    .ok()?;
                let cb: String = r.try_get("callback_url").ok()?;
                Some((bid, cb))
            })
            .collect()
    };

    // Proxy to each matching bot
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| NexusError::Internal(e.into()))?;

    let mut results = Vec::new();
    for (bot_id, callback_url) in triggers {
        match http
            .get(&callback_url)
            .query(&[
                ("query", query_text.as_str()),
                ("channel_id", channel_id.to_string().as_str()),
                ("user_id", auth.user_id.to_string().as_str()),
            ])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<Vec<InlineSuggestion>>().await {
                    Ok(suggestions) => results.push(InlineQueryResponse {
                        bot_id,
                        suggestions,
                    }),
                    Err(e) => {
                        tracing::warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "Bot returned invalid inline suggestions");
                    }
                }
            }
            Ok(resp) => {
                tracing::warn!(status = %resp.status(), "Bot inline query returned non-success HTTP status");
            }
            Err(e) => {
                tracing::warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "Bot inline query failed");
            }
        }
    }

    Ok(Json(results))
}

/// GET /api/v1/channels/:channel_id/bots/inline-triggers
///
/// Returns all bots that have registered triggers visible to users in this channel.
async fn list_channel_triggers(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
) -> NexusResult<Json<Vec<BotTrigger>>> {
    let channel_row = sqlx::query(
        "SELECT id::text AS id, server_id::text AS server_id FROM channels WHERE id = $1::uuid",
    )
    .bind(channel_id.to_string())
    .fetch_optional(&state.db.pool)
    .await
    .map_err(NexusError::Database)?
    .ok_or(NexusError::NotFound {
        resource: "Channel".into(),
    })?;

    let server_id_str: Option<String> = channel_row
        .try_get::<Option<String>, _>("server_id")
        .unwrap_or(None);

    if let Some(sid_str) = server_id_str
        && let Ok(server_id) = sid_str.parse::<Uuid>() {
            sqlx::query(
                "SELECT id FROM server_members WHERE user_id = $1::uuid AND server_id = $2::uuid",
            )
            .bind(auth.user_id.to_string())
            .bind(server_id.to_string())
            .fetch_optional(&state.db.pool)
            .await
            .map_err(NexusError::Database)?
            .ok_or(NexusError::Forbidden)?;
        }

    // Join bot_inline_triggers → users (bots are users with is_bot=true)
    let rows = sqlx::query(
        r#"SELECT bit.id::text AS id, bit.bot_id::text AS bot_id,
                  bit.prefix, bit.description,
                  u.username AS bot_name, u.avatar_url AS bot_avatar
           FROM bot_inline_triggers bit
           JOIN users u ON u.id = bit.bot_id
           ORDER BY bit.prefix"#,
    )
    .fetch_all(&state.db.pool)
    .await
    .map_err(NexusError::Database)?;

    let triggers: Vec<BotTrigger> = rows
        .iter()
        .filter_map(|r| {
            let id: Uuid = r
                .try_get::<String, _>("id")
                .unwrap_or_default()
                .parse()
                .ok()?;
            let bot_id: Uuid = r
                .try_get::<String, _>("bot_id")
                .unwrap_or_default()
                .parse()
                .ok()?;
            Some(BotTrigger {
                id,
                bot_id,
                prefix: r.try_get("prefix").unwrap_or_default(),
                description: r.try_get("description").unwrap_or_default(),
                bot_name: r.try_get("bot_name").unwrap_or_default(),
                bot_avatar: r.try_get::<Option<String>, _>("bot_avatar").unwrap_or(None),
            })
        })
        .collect();

    Ok(Json(triggers))
}

/// POST /api/v1/bots/@me/inline-triggers — bot registers a new trigger prefix
async fn register_trigger(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<RegisterTriggerRequest>,
) -> NexusResult<Json<serde_json::Value>> {
    // Verify caller is actually a bot
    let bot_row = sqlx::query("SELECT is_bot FROM users WHERE id = $1::uuid")
        .bind(auth.user_id.to_string())
        .fetch_optional(&state.db.pool)
        .await
        .map_err(NexusError::Database)?;

    let is_bot = bot_row.and_then(|r| r.try_get::<Option<bool>, _>("is_bot").ok().flatten());

    match is_bot {
        Some(true) => {}
        _ => return Err(NexusError::Forbidden),
    }

    if body.prefix.trim().is_empty() || body.prefix.len() > 32 {
        return Err(NexusError::Validation {
            message: "prefix must be 1–32 characters".into(),
        });
    }
    if body.description.len() > 200 {
        return Err(NexusError::Validation {
            message: "description too long".into(),
        });
    }
    if body.callback_url.len() > 512 {
        return Err(NexusError::Validation {
            message: "callback_url too long".into(),
        });
    }
    if !body.callback_url.starts_with("https://") {
        return Err(NexusError::Validation {
            message: "callback_url must use HTTPS".into(),
        });
    }

    let trigger_id = nexus_common::snowflake::generate_id();
    sqlx::query(
        "INSERT INTO bot_inline_triggers (id, bot_id, prefix, description, callback_url) \
         VALUES ($1::uuid, $2::uuid, $3, $4, $5) \
         ON CONFLICT (bot_id, prefix) DO UPDATE \
         SET description = EXCLUDED.description, callback_url = EXCLUDED.callback_url",
    )
    .bind(trigger_id.to_string())
    .bind(auth.user_id.to_string())
    .bind(body.prefix.trim().to_string())
    .bind(&body.description)
    .bind(&body.callback_url)
    .execute(&state.db.pool)
    .await
    .map_err(NexusError::Database)?;

    Ok(Json(
        serde_json::json!({ "id": trigger_id, "prefix": body.prefix.trim() }),
    ))
}

/// DELETE /api/v1/bots/@me/inline-triggers/:trigger_id — bot removes one trigger
async fn remove_trigger(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(trigger_id): Path<Uuid>,
) -> NexusResult<Json<serde_json::Value>> {
    let res =
        sqlx::query("DELETE FROM bot_inline_triggers WHERE id = $1::uuid AND bot_id = $2::uuid")
            .bind(trigger_id.to_string())
            .bind(auth.user_id.to_string())
            .execute(&state.db.pool)
            .await
            .map_err(NexusError::Database)?;

    if res.rows_affected() == 0 {
        return Err(NexusError::NotFound {
            resource: "Trigger".into(),
        });
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}
