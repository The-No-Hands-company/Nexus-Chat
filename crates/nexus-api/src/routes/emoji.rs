//! Custom emoji routes — upload, list, rename, delete server emoji.
//!
//! POST   /servers/:id/emojis            — Upload a custom emoji
//! GET    /servers/:id/emojis            — List server emoji
//! GET    /servers/:id/emojis/:emoji_id  — Get emoji details
//! PATCH  /servers/:id/emojis/:emoji_id  — Rename emoji
//! DELETE /servers/:id/emojis/:emoji_id  — Delete emoji

use axum::{
    Json, Router,
    extract::{Extension, Multipart, Path, State},
    middleware,
    routing::get,
};
use nexus_common::gateway_event::GatewayEvent;
use nexus_common::{
    error::{NexusError, NexusResult},
    models::rich::{ServerEmoji, UpdateEmojiRequest},
    validation::validate_request,
};
use nexus_db::repository::emoji;
use std::sync::Arc;
use uuid::Uuid;

use crate::{AppState, middleware::AuthContext};

/// Maximum emoji size: 256 KiB
const MAX_EMOJI_BYTES: usize = 256 * 1024;

/// Maximum emoji per server (free tier)
const MAX_EMOJI_PER_SERVER: i64 = 50;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/servers/{server_id}/emojis",
            get(list_emoji).post(create_emoji),
        )
        .route(
            "/servers/{server_id}/emojis/{emoji_id}",
            get(get_emoji).patch(update_emoji).delete(delete_emoji),
        )
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ))
}

// ============================================================
// POST /servers/:server_id/emojis — multipart upload
// ============================================================

async fn create_emoji(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
    mut multipart: Multipart,
) -> NexusResult<Json<ServerEmoji>> {
    let mut file_data: Option<Vec<u8>> = None;
    let mut content_type = String::from("image/png");
    let mut emoji_name = String::new();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| NexusError::Validation {
            message: format!("Multipart error: {e}"),
        })?
    {
        match field.name() {
            Some("image") => {
                if let Some(ct) = field.content_type() {
                    // Strict allowlist: only raster image formats. SVG and other
                    // scriptable types are excluded because MinIO serves them
                    // verbatim from storage.tnhc.dev, which shares the session
                    // cookie domain, enabling XSS.
                    const ALLOWED: &[&str] =
                        &["image/png", "image/gif", "image/webp", "image/jpeg", "image/avif"];
                    if !ALLOWED.contains(&ct) {
                        return Err(NexusError::Validation {
                            message: format!("Content type '{}' is not allowed for emoji", ct),
                        });
                    }
                    content_type = ct.to_string();
                }
                let bytes = field.bytes().await.map_err(|e| NexusError::Validation {
                    message: format!("Failed to read emoji: {e}"),
                })?;
                if bytes.len() > MAX_EMOJI_BYTES {
                    return Err(NexusError::Validation {
                        message: format!(
                            "Emoji too large: {} bytes (max {})",
                            bytes.len(),
                            MAX_EMOJI_BYTES
                        ),
                    });
                }
                file_data = Some(bytes.to_vec());
            }
            Some("name") => {
                emoji_name = field.text().await.unwrap_or_default().trim().to_string();
            }
            _ => {}
        }
    }

    // Validate name
    if emoji_name.len() < 2 || emoji_name.len() > 32 {
        return Err(NexusError::Validation {
            message: "Emoji name must be 2-32 characters".into(),
        });
    }

    let data = file_data.ok_or(NexusError::Validation {
        message: "No image field in request".into(),
    })?;

    // Check server emoji limit
    let count = emoji::count_for_server(&state.db.pool, server_id).await?;
    if count >= MAX_EMOJI_PER_SERVER {
        return Err(NexusError::LimitReached {
            message: format!("Server has reached the emoji limit ({MAX_EMOJI_PER_SERVER})"),
        });
    }

    // Detect animated (very simple: check GIF magic bytes)
    let animated = data.starts_with(b"GIF");

    let emoji_id = Uuid::new_v4();
    let ext = if animated { "gif" } else { "webp" };
    let storage_key = format!("emoji/{}/{}.{}", server_id, emoji_id, ext);

    // Upload to MinIO
    state
        .storage
        .put_object(&storage_key, data, &content_type)
        .await
        .map_err(NexusError::Internal)?;

    let url = state
        .storage
        .presigned_get_url(&storage_key, 3600 * 24 * 365) // 1-year URL
        .await
        .ok();

    let row = emoji::create_emoji(
        &state.db.pool,
        emoji_id,
        server_id,
        auth.user_id,
        &emoji_name,
        &storage_key,
        url.as_deref(),
        animated,
    )
    .await?;

    let se: ServerEmoji = row.into();

    // Broadcast emoji update
    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: "GUILD_EMOJIS_UPDATE".into(),
        data: serde_json::json!({ "server_id": server_id, "emoji": &se }),
        server_id: Some(server_id),
        channel_id: None,
        user_id: Some(auth.user_id),
    });

    Ok(Json(se))
}

// ============================================================
// GET /servers/:server_id/emojis
// ============================================================

async fn list_emoji(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
) -> NexusResult<Json<Vec<ServerEmoji>>> {
    let _ = auth;
    let rows = emoji::list_for_server(&state.db.pool, server_id).await?;
    let list: Vec<ServerEmoji> = rows.into_iter().map(Into::into).collect();
    Ok(Json(list))
}

// ============================================================
// GET /servers/:server_id/emojis/:emoji_id
// ============================================================

async fn get_emoji(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((_server_id, emoji_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<Json<ServerEmoji>> {
    let _ = auth;
    let row = emoji::find_by_id(&state.db.pool, emoji_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Emoji".into(),
        })?;
    Ok(Json(row.into()))
}

// ============================================================
// PATCH /servers/:server_id/emojis/:emoji_id
// ============================================================

async fn update_emoji(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((server_id, emoji_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<UpdateEmojiRequest>,
) -> NexusResult<Json<ServerEmoji>> {
    let _ = auth;
    validate_request(&body)?;

    let name = body.name.ok_or(NexusError::Validation {
        message: "name is required".into(),
    })?;

    let row = emoji::update_emoji(&state.db.pool, emoji_id, server_id, &name).await?;

    let se: ServerEmoji = row.into();

    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: "GUILD_EMOJIS_UPDATE".into(),
        data: serde_json::json!({ "server_id": server_id, "emoji": &se }),
        server_id: Some(server_id),
        channel_id: None,
        user_id: Some(auth.user_id),
    });

    Ok(Json(se))
}

// ============================================================
// DELETE /servers/:server_id/emojis/:emoji_id
// ============================================================

async fn delete_emoji(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((server_id, emoji_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<Json<serde_json::Value>> {
    let storage_key = emoji::delete_emoji(&state.db.pool, emoji_id, server_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Emoji".into(),
        })?;

    // Remove from storage (best-effort)
    let _ = state.storage.delete_object(&storage_key).await;

    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: "GUILD_EMOJIS_UPDATE".into(),
        data: serde_json::json!({ "server_id": server_id, "deleted_emoji_id": emoji_id }),
        server_id: Some(server_id),
        channel_id: None,
        user_id: Some(auth.user_id),
    });

    Ok(Json(serde_json::json!({ "deleted": true })))
}
