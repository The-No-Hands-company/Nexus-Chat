//! Search routes — MeiliSearch-powered full-text message and server search.
//!
//! GET /search/messages          — Search messages (scoped to accessible servers/channels)
//! GET /servers/:id/search       — Search messages within a server
//! GET /channels/:id/search      — Search messages within a channel (already in messages.rs,
//!                                  but here we provide the MeiliSearch-backed version)

use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    middleware,
    routing::get,
};
use nexus_common::error::{NexusError, NexusResult};
use nexus_db::repository::{channels, members};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    AppState,
    middleware::{AuthContext, check_rate_limit_with_fallback, extract_client_tag},
};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/search/messages", get(search_messages_global))
        .route("/servers/{server_id}/search", get(search_server_messages))
        .route(
            "/channels/{channel_id}/search/meili",
            get(search_channel_messages),
        )
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ))
}

// ============================================================
// Query params
// ============================================================

#[derive(Debug, Deserialize)]
struct SearchParams {
    /// The search query text
    q: String,
    /// Filter by author
    author_id: Option<Uuid>,
    /// Filter by channel (for global/server search)
    channel_id: Option<Uuid>,
    /// Results per page (max 50)
    limit: Option<usize>,
    /// Pagination offset
    offset: Option<usize>,
}

// ============================================================
// Response
// ============================================================

#[derive(Debug, Serialize)]
struct SearchResult {
    query: String,
    total_hits: Option<usize>,
    limit: usize,
    offset: usize,
    hits: Vec<serde_json::Value>,
}

// ============================================================
// GET /search/messages
// ============================================================

/// Global message search across all servers the user is a member of.
async fn search_messages_global(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<SearchParams>,
) -> NexusResult<Json<SearchResult>> {
    // Rate limit: 20 searches per user per 60 seconds.
    // Search is expensive (MeiliSearch + DB fan-out); protect against DoS.
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:search:user:{}", auth.user_id),
        20,
        60,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:search:tag:{tag}"),
        40,
        60,
    )
    .await?;

    // Enforce query length to prevent ReDoS / oversized MeiliSearch queries
    if params.q.len() > 500 {
        return Err(nexus_common::error::NexusError::Validation {
            message: "Search query must be 500 characters or fewer".into(),
        });
    }
    if params.q.trim().is_empty() {
        return Err(nexus_common::error::NexusError::Validation {
            message: "Search query cannot be empty".into(),
        });
    }

    metrics::counter!("nexus_search_requests_total", "scope" => "global").increment(1);

    let limit = params.limit.unwrap_or(20).min(50);
    let offset = params.offset.unwrap_or(0);

    if let Some(channel_id) = params.channel_id {
        let channel = channels::find_by_id(&state.db.pool, channel_id)
            .await
            .map_err(NexusError::Database)?
            .ok_or(NexusError::NotFound {
                resource: format!("channel {channel_id}"),
            })?;
        if let Some(server_id) = channel.server_id {
            if !members::is_member(&state.db.pool, auth.user_id, server_id).await? {
                metrics::counter!("nexus_search_requests_total", "scope" => "global", "outcome" => "forbidden").increment(1);
                tracing::warn!(%channel_id, %server_id, "Denied global search for inaccessible channel");
                return Err(NexusError::Forbidden);
            }
        } else {
            metrics::counter!("nexus_search_requests_total", "scope" => "global", "outcome" => "unsupported_dm").increment(1);
            return Err(NexusError::Validation {
                message: "Search within DM channels is not yet supported".into(),
            });
        }
    }

    let server_ids = members::list_server_ids_for_user(&state.db.pool, auth.user_id)
        .await
        .map_err(NexusError::Database)?;
    if server_ids.is_empty() {
        metrics::counter!("nexus_search_requests_total", "scope" => "global", "outcome" => "no_memberships").increment(1);
        return Ok(Json(SearchResult {
            query: params.q,
            total_hits: Some(0),
            limit,
            offset,
            hits: Vec::new(),
        }));
    }

    let window = (limit + offset).min(200);
    let mut merged_hits = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut total_hits = 0usize;

    for server_id in server_ids {
        let results = state
            .search
            .search_messages(
                &params.q,
                Some(server_id),
                params.channel_id,
                params.author_id,
                window,
                0,
            )
            .await
            .map_err(NexusError::Internal)?;

        total_hits += results.total_hits.unwrap_or(results.hits.len());
        for hit in results.hits {
            if seen_ids.insert(hit.id.clone()) {
                merged_hits.push(hit);
            }
        }
    }

    merged_hits.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    let hits: Vec<serde_json::Value> = merged_hits
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|h| serde_json::to_value(h).unwrap_or_default())
        .collect();

    metrics::counter!("nexus_search_requests_total", "scope" => "global", "outcome" => "ok")
        .increment(1);

    Ok(Json(SearchResult {
        query: params.q,
        total_hits: Some(total_hits),
        limit,
        offset,
        hits,
    }))
}

// ============================================================
// GET /servers/:server_id/search
// ============================================================

async fn search_server_messages(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(server_id): Path<Uuid>,
    Query(params): Query<SearchParams>,
) -> NexusResult<Json<SearchResult>> {
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:search:user:{}", auth.user_id),
        20,
        60,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:search:tag:{tag}"),
        40,
        60,
    )
    .await?;
    if params.q.len() > 500 || params.q.trim().is_empty() {
        return Err(nexus_common::error::NexusError::Validation {
            message: "Search query must be 1-500 characters".into(),
        });
    }

    metrics::counter!("nexus_search_requests_total", "scope" => "server").increment(1);

    // Verify user is a member of the server before allowing search
    if !members::is_member(&state.db.pool, auth.user_id, server_id).await? {
        tracing::warn!(
            server_id = %server_id,
            "Denied message search: user is not a member of the server"
        );
        metrics::counter!("nexus_search_requests_total", "scope" => "server", "outcome" => "forbidden").increment(1);
        return Err(NexusError::Forbidden);
    }

    let limit = params.limit.unwrap_or(20).min(50);
    let offset = params.offset.unwrap_or(0);

    let results = state
        .search
        .search_messages(
            &params.q,
            Some(server_id),
            params.channel_id,
            params.author_id,
            limit,
            offset,
        )
        .await
        .map_err(NexusError::Internal)?;

    let hits: Vec<serde_json::Value> = results
        .hits
        .into_iter()
        .map(|h| serde_json::to_value(h).unwrap_or_default())
        .collect();

    metrics::counter!("nexus_search_requests_total", "scope" => "server", "outcome" => "ok")
        .increment(1);

    Ok(Json(SearchResult {
        query: params.q,
        total_hits: results.total_hits,
        limit,
        offset,
        hits,
    }))
}

// ============================================================
// GET /channels/:channel_id/search/meili
// ============================================================

async fn search_channel_messages(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
    Query(params): Query<SearchParams>,
) -> NexusResult<Json<SearchResult>> {
    metrics::counter!("nexus_search_requests_total", "scope" => "channel").increment(1);

    // Load the channel to get its server context
    let channel = channels::find_by_id(&state.db.pool, channel_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .ok_or(NexusError::NotFound {
            resource: format!("channel {channel_id}"),
        })?;

    // Verify access: user must be a member of the server (for server channels)
    if let Some(server_id) = channel.server_id {
        if !members::is_member(&state.db.pool, auth.user_id, server_id).await? {
            tracing::warn!(
                channel_id = %channel_id,
                server_id = %server_id,
                "Denied channel message search: user is not a member of the server"
            );
            metrics::counter!("nexus_search_requests_total", "scope" => "channel", "outcome" => "forbidden").increment(1);
            return Err(NexusError::Forbidden);
        }
    } else {
        // DM channel — verify the caller is a participant before searching.
        let is_participant: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM dm_participants WHERE channel_id = $1::uuid AND user_id = $2::uuid)",
        )
        .bind(channel_id.to_string())
        .bind(auth.user_id.to_string())
        .fetch_one(&state.db.pool)
        .await
        .unwrap_or(false);

        if !is_participant {
            tracing::warn!(
                channel_id = %channel_id,
                "Denied DM channel search: user is not a participant"
            );
            metrics::counter!(
                "nexus_search_requests_total",
                "scope" => "channel",
                "outcome" => "forbidden_dm"
            )
            .increment(1);
            return Err(NexusError::Forbidden);
        }

        metrics::counter!(
            "nexus_search_requests_total",
            "scope" => "channel",
            "outcome" => "dm_allowed"
        )
        .increment(1);
    }

    let limit = params.limit.unwrap_or(20).min(50);
    let offset = params.offset.unwrap_or(0);

    let results = state
        .search
        .search_messages(
            &params.q,
            None,
            Some(channel_id),
            params.author_id,
            limit,
            offset,
        )
        .await
        .map_err(NexusError::Internal)?;

    let hits: Vec<serde_json::Value> = results
        .hits
        .into_iter()
        .map(|h| serde_json::to_value(h).unwrap_or_default())
        .collect();

    metrics::counter!("nexus_search_requests_total", "scope" => "channel", "outcome" => "ok")
        .increment(1);

    Ok(Json(SearchResult {
        query: params.q,
        total_hits: results.total_hits,
        limit,
        offset,
        hits,
    }))
}
