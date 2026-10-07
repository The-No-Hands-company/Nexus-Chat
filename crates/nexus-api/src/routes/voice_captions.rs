//! Voice caption routes — 18-04: Voice Accessibility.
//!
//! POST /channels/:id/captions        — Submit a caption segment
//! PATCH /captions/:id/finalise       — Finalise an interim caption
//! GET  /channels/:id/captions        — List recent captions for a channel

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    middleware,
    routing::post,
};
use nexus_common::error::{NexusError, NexusResult};
use nexus_common::snowflake;
use nexus_db::repository::{channels, members, voice_captions};
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

use crate::{AppState, middleware::AuthContext};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/channels/{id}/captions",
            post(submit_caption).get(list_captions),
        )
        .route("/captions/{id}/finalise", post(finalise_caption))
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ))
}

// ── Request bodies ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SubmitCaptionRequest {
    text: String,
    language: Option<String>,
    is_final: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct FinaliseCaptionRequest {
    text: String,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn submit_caption(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(channel_id): Path<Uuid>,
    Json(body): Json<SubmitCaptionRequest>,
) -> NexusResult<Json<nexus_common::models::accessibility::VoiceCaption>> {
    metrics::counter!("nexus_voice_captions_requests_total", "route" => "submit").increment(1);

    ensure_channel_access(&state, ctx.user_id, channel_id).await?;

    if body.text.is_empty() || body.text.len() > 5000 {
        metrics::counter!("nexus_voice_captions_requests_total", "route" => "submit", "outcome" => "invalid_text").increment(1);
        return Err(NexusError::Validation {
            message: "Caption text must be 1-5000 characters".into(),
        });
    }

    let lang = body.language.as_deref().unwrap_or("en");
    let is_final = body.is_final.unwrap_or(false);

    let caption = voice_captions::create_caption(
        &state.db.pool,
        snowflake::generate_id(),
        channel_id,
        ctx.user_id,
        &body.text,
        lang,
        is_final,
    )
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    metrics::counter!("nexus_voice_captions_requests_total", "route" => "submit", "outcome" => "ok").increment(1);

    Ok(Json(caption))
}

async fn finalise_caption(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<FinaliseCaptionRequest>,
) -> NexusResult<Json<nexus_common::models::accessibility::VoiceCaption>> {
    metrics::counter!("nexus_voice_captions_requests_total", "route" => "finalise").increment(1);

    if body.text.is_empty() || body.text.len() > 5000 {
        metrics::counter!("nexus_voice_captions_requests_total", "route" => "finalise", "outcome" => "invalid_text").increment(1);
        return Err(NexusError::Validation {
            message: "Caption text must be 1-5000 characters".into(),
        });
    }

    // Fetch the existing caption to verify ownership
    let caption = voice_captions::find_by_id(&state.db.pool, id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .ok_or(NexusError::NotFound {
            resource: format!("caption {id}"),
        })?;

    // Only the original speaker can finalize their caption
    if caption.speaker_id.to_string() != ctx.user_id.to_string() {
        tracing::warn!(caption_id = %id, "Denied caption finalization: user is not the caption author");
        metrics::counter!("nexus_voice_captions_requests_total", "route" => "finalise", "outcome" => "forbidden_not_author").increment(1);
        return Err(NexusError::Forbidden);
    }

    let finalised = voice_captions::finalise_caption(&state.db.pool, id, &body.text)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .ok_or(NexusError::NotFound {
            resource: format!("caption {id}"),
        })?;

    metrics::counter!("nexus_voice_captions_requests_total", "route" => "finalise", "outcome" => "ok").increment(1);

    Ok(Json(finalised))
}

async fn list_captions(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(channel_id): Path<Uuid>,
) -> NexusResult<Json<Vec<nexus_common::models::accessibility::VoiceCaption>>> {
    metrics::counter!("nexus_voice_captions_requests_total", "route" => "list").increment(1);

    ensure_channel_access(&state, ctx.user_id, channel_id).await?;

    let captions = voice_captions::list_channel_captions(&state.db.pool, channel_id, 100)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    metrics::counter!("nexus_voice_captions_requests_total", "route" => "list", "outcome" => "ok")
        .increment(1);
    Ok(Json(captions))
}

async fn ensure_channel_access(
    state: &AppState,
    user_id: Uuid,
    channel_id: Uuid,
) -> NexusResult<()> {
    let channel = channels::find_by_id(&state.db.pool, channel_id)
        .await
        .map_err(NexusError::Database)?
        .ok_or(NexusError::NotFound {
            resource: format!("channel {channel_id}"),
        })?;

    if let Some(server_id) = channel.server_id {
        let is_member = members::is_member(&state.db.pool, user_id, server_id)
            .await
            .map_err(NexusError::Database)?;
        if !is_member {
            return Err(NexusError::Forbidden);
        }
    } else {
        let is_dm_participant: (bool,) = sqlx::query_as(
            "SELECT EXISTS(SELECT 1 FROM dm_participants WHERE channel_id = $1::uuid AND user_id = $2::uuid)",
        )
        .bind(channel_id.to_string())
        .bind(user_id.to_string())
        .fetch_one(&state.db.pool)
        .await
        .map_err(NexusError::Database)?;
        if !is_dm_participant.0 {
            return Err(NexusError::Forbidden);
        }
    }

    Ok(())
}
