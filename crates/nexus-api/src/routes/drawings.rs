//! Drawing & annotation routes — 17-03: In-Chat Drawing & Annotation.
//!
//! POST   /channels/:id/drawings           — Create drawing
//! GET    /channels/:id/drawings           — List drawings in channel
//! GET    /channels/:id/whiteboards        — List active whiteboards
//! GET    /drawings/:id                    — Get single drawing
//! PATCH  /drawings/:id                    — Update drawing data
//! DELETE /drawings/:id                    — Delete drawing

use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    middleware,
    routing::{get, post},
};
use nexus_common::error::{NexusError, NexusResult};
use nexus_common::snowflake;
use nexus_db::repository::{channels, drawings, members};
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

use crate::{AppState, middleware::AuthContext};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/channels/{channel_id}/drawings",
            post(create_drawing).get(list_drawings),
        )
        .route("/channels/{channel_id}/whiteboards", get(list_whiteboards))
        .route(
            "/drawings/{id}",
            get(get_drawing)
                .patch(update_drawing)
                .delete(delete_drawing),
        )
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ))
}

// ── Request / Response ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CreateDrawingRequest {
    drawing_data: serde_json::Value,
    width: Option<i32>,
    height: Option<i32>,
    preview_key: Option<String>,
    message_id: Option<Uuid>,
    is_whiteboard: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct UpdateDrawingRequest {
    drawing_data: serde_json::Value,
    preview_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

// ── Access guard ──────────────────────────────────────────────────────────────

/// Verify that `user_id` may read/write the channel identified by `channel_id`.
/// For server channels the caller must be a server member; for DMs they must
/// be a participant.  Returns `NotFound` (not `Forbidden`) on a missing channel
/// so callers cannot probe for channel existence.
async fn require_channel_access(
    state: &AppState,
    channel_id: Uuid,
    user_id: Uuid,
) -> NexusResult<()> {
    let channel = channels::find_by_id(&state.db.pool, channel_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .ok_or(NexusError::NotFound { resource: "Channel".into() })?;
    match channel.server_id {
        Some(server_id) => {
            if !members::is_member(&state.db.pool, user_id, server_id)
                .await
                .map_err(|e| NexusError::Internal(e.into()))?
            {
                return Err(NexusError::Forbidden);
            }
        }
        None => {
            if !channels::is_dm_participant(&state.db.pool, channel_id, user_id)
                .await
                .map_err(|e| NexusError::Internal(e.into()))?
            {
                return Err(NexusError::Forbidden);
            }
        }
    }
    Ok(())
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn create_drawing(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(channel_id): Path<Uuid>,
    Json(body): Json<CreateDrawingRequest>,
) -> NexusResult<Json<nexus_common::models::multimedia::Drawing>> {
    let width = body.width.unwrap_or(800).clamp(1, 4096);
    let height = body.height.unwrap_or(600).clamp(1, 4096);
    let drawing_data_str =
        serde_json::to_string(&body.drawing_data).map_err(|_| NexusError::Validation {
            message: "Invalid drawing data".into(),
        })?;

    let drawing = drawings::create_drawing(
        &state.db.pool,
        snowflake::generate_id(),
        channel_id,
        ctx.user_id,
        &drawing_data_str,
        width,
        height,
        body.preview_key.as_deref(),
        body.message_id,
        body.is_whiteboard.unwrap_or(false),
    )
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(drawing))
}

async fn list_drawings(
    Extension(ctx): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
    Query(q): Query<ListQuery>,
) -> NexusResult<Json<Vec<nexus_common::models::multimedia::Drawing>>> {
    require_channel_access(&state, channel_id, ctx.user_id).await?;
    let limit = q.limit.unwrap_or(50).min(100);
    let offset = q.offset.unwrap_or(0);
    drawings::list_by_channel(&state.db.pool, channel_id, limit, offset)
        .await
        .map(Json)
        .map_err(|e| NexusError::Internal(e.into()))
}

async fn list_whiteboards(
    Extension(ctx): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
) -> NexusResult<Json<Vec<nexus_common::models::multimedia::Drawing>>> {
    require_channel_access(&state, channel_id, ctx.user_id).await?;
    drawings::list_whiteboards(&state.db.pool, channel_id)
        .await
        .map(Json)
        .map_err(|e| NexusError::Internal(e.into()))
}

async fn get_drawing(
    Extension(ctx): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> NexusResult<Json<nexus_common::models::multimedia::Drawing>> {
    let drawing = drawings::find_drawing(&state.db.pool, id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .ok_or(NexusError::NotFound { resource: "Drawing".into() })?;
    require_channel_access(&state, drawing.channel_id, ctx.user_id).await?;
    Ok(Json(drawing))
}

async fn update_drawing(
    Extension(ctx): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateDrawingRequest>,
) -> NexusResult<Json<nexus_common::models::multimedia::Drawing>> {
    let drawing = drawings::find_drawing(&state.db.pool, id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .ok_or(NexusError::NotFound { resource: "Drawing".into() })?;
    if drawing.author_id != ctx.user_id {
        return Err(NexusError::Forbidden);
    }
    let drawing_data_str =
        serde_json::to_string(&body.drawing_data).map_err(|_| NexusError::Validation {
            message: "Invalid drawing data".into(),
        })?;
    drawings::update_drawing_data(
        &state.db.pool,
        id,
        &drawing_data_str,
        body.preview_key.as_deref(),
    )
    .await
    .map(Json)
    .map_err(|e| NexusError::Internal(e.into()))
}

async fn delete_drawing(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> NexusResult<Json<serde_json::Value>> {
    let deleted = drawings::delete_drawing(&state.db.pool, id, ctx.user_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;
    if !deleted {
        return Err(NexusError::NotFound {
            resource: "Drawing".into(),
        });
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}
