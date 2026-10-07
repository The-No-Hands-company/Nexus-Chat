//! Channel routes — CRUD for channels within a server.

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    middleware,
    routing::get,
};
use nexus_common::permissions::Permissions;
use nexus_common::{
    error::{NexusError, NexusResult},
    models::channel::{CreateChannelRequest, UpdateChannelRequest},
    snowflake,
    validation::validate_request,
};
use nexus_db::repository::{audit_log, channels, members, roles, servers};
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    AppState,
    middleware::{AuthContext, check_rate_limit_with_fallback, extract_client_tag},
};

/// Verify that `user_id` holds `required` permission in a server.
///
/// Short-circuits for the server owner (always allowed). Otherwise resolves
/// the member's role set, computes the effective permission bitfield, and
/// checks the required bit (or ADMINISTRATOR).
async fn require_server_permission(
    pool: &sqlx::AnyPool,
    server_id: Uuid,
    server_owner_id: Uuid,
    user_id: Uuid,
    required: Permissions,
) -> NexusResult<()> {
    // Server owner always has all permissions.
    if user_id == server_owner_id {
        return Ok(());
    }

    // Caller must actually be a member of the server.
    let member = members::find_member(pool, user_id, server_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .ok_or_else(|| NexusError::MissingPermission {
            permission: format!("{required:?}"),
        })?;

    // Fetch every role defined in the server so we can look up permissions.
    let all_server_roles = roles::list_server_roles(pool, server_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    // Base: @everyone role permissions (is_default = true).
    let base = all_server_roles
        .iter()
        .find(|r| r.is_default)
        .map(|r| Permissions::from_bits_truncate(r.permissions))
        .unwrap_or_else(Permissions::empty);

    // OR in permissions from each role the member holds.
    let effective = all_server_roles
        .iter()
        .filter(|r| !r.is_default && member.roles.contains(&r.id))
        .map(|r| Permissions::from_bits_truncate(r.permissions))
        .fold(base, |acc, rp| acc | rp);

    if effective.has(required) {
        Ok(())
    } else {
        Err(NexusError::MissingPermission {
            permission: format!("{required:?}"),
        })
    }
}

/// Channel routes.
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/servers/{server_id}/channels",
            get(list_channels).post(create_channel),
        )
        .route(
            "/channels/{channel_id}",
            get(get_channel)
                .patch(update_channel)
                .delete(delete_channel),
        )
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ))
}

/// GET /api/v1/servers/:server_id/channels
async fn list_channels(
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
) -> NexusResult<Json<Vec<nexus_common::models::channel::Channel>>> {
    let channel_list = channels::list_server_channels(&state.db.pool, server_id).await?;
    Ok(Json(channel_list))
}

/// POST /api/v1/servers/:server_id/channels
async fn create_channel(
    headers: axum::http::HeaderMap,
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
    Json(body): Json<CreateChannelRequest>,
) -> NexusResult<Json<nexus_common::models::channel::Channel>> {
    validate_request(&body)?;

    // ── Rate limiting: 10 channel creates per user per minute ────────────
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:channel:create:{}", auth.user_id),
        10,
        60,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:channel:create:tag:{tag}"),
        20,
        60,
    )
    .await?;

    // Verify server exists and user has permission
    let server = servers::find_by_id(&state.db.pool, server_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Server".into(),
        })?;

    require_server_permission(
        &state.db.pool,
        server_id,
        server.owner_id,
        auth.user_id,
        Permissions::MANAGE_CHANNELS,
    )
    .await?;

    let channel_id = snowflake::generate_id();
    let channel_type_str = serde_json::to_value(body.channel_type)
        .map_err(|e| NexusError::Internal(e.into()))?
        .as_str()
        .unwrap_or("text")
        .to_string();

    let channel = channels::create_channel(
        &state.db.pool,
        channel_id,
        Some(server_id),
        body.parent_id,
        &channel_type_str,
        Some(&body.name),
        body.topic.as_deref(),
        body.position.unwrap_or(0),
    )
    .await?;

    tracing::info!(
        channel_id = %channel_id,
        server_id = %server_id,
        name = %body.name,
        "Channel created"
    );

    let _ = audit_log::write_entry(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        Some(auth.user_id),
        "CHANNEL_CREATE",
        Some("channel"),
        Some(channel_id),
        &serde_json::json!({ "name": body.name, "type": channel_type_str }),
        None,
    )
    .await;

    Ok(Json(channel))
}

/// GET /api/v1/channels/:channel_id
async fn get_channel(
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
) -> NexusResult<Json<nexus_common::models::channel::Channel>> {
    let channel = channels::find_by_id(&state.db.pool, channel_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Channel".into(),
        })?;

    Ok(Json(channel))
}

/// PATCH /api/v1/channels/:channel_id
async fn update_channel(
    headers: axum::http::HeaderMap,
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
    Json(body): Json<UpdateChannelRequest>,
) -> NexusResult<Json<nexus_common::models::channel::Channel>> {
    validate_request(&body)?;

    // ── Rate limiting: 5 channel updates per user per minute ────────────
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:channel:update:{}", auth.user_id),
        5,
        60,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:channel:update:tag:{tag}"),
        10,
        60,
    )
    .await?;

    let channel = channels::find_by_id(&state.db.pool, channel_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Channel".into(),
        })?;

    // Resolve the server this channel belongs to so we can check permissions.
    let server_id = channel.server_id.ok_or(NexusError::MissingPermission {
        permission: "MANAGE_CHANNELS".into(),
    })?;
    let server = servers::find_by_id(&state.db.pool, server_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Server".into(),
        })?;

    require_server_permission(
        &state.db.pool,
        server_id,
        server.owner_id,
        auth.user_id,
        Permissions::MANAGE_CHANNELS,
    )
    .await?;

    let updated = channels::update_channel(
        &state.db.pool,
        channel_id,
        body.name.as_deref(),
        body.topic.as_deref(),
        body.position,
        body.nsfw,
        body.rate_limit_per_user,
        body.is_stream,
    )
    .await?;

    let _ = audit_log::write_entry(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        Some(auth.user_id),
        "CHANNEL_UPDATE",
        Some("channel"),
        Some(channel_id),
        &serde_json::json!({ "name": body.name, "topic": body.topic }),
        None,
    )
    .await;

    Ok(Json(updated))
}

/// DELETE /api/v1/channels/:channel_id
async fn delete_channel(
    headers: axum::http::HeaderMap,
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
) -> NexusResult<Json<serde_json::Value>> {
    // ── Rate limiting: 5 channel deletes per user per minute ────────────
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:channel:delete:{}", auth.user_id),
        5,
        60,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:channel:delete:tag:{tag}"),
        10,
        60,
    )
    .await?;

    // Load the channel first so we can find its server and return 404 if missing.
    let channel = channels::find_by_id(&state.db.pool, channel_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Channel".into(),
        })?;

    let server_id = channel.server_id.ok_or(NexusError::MissingPermission {
        permission: "MANAGE_CHANNELS".into(),
    })?;
    let server = servers::find_by_id(&state.db.pool, server_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Server".into(),
        })?;

    require_server_permission(
        &state.db.pool,
        server_id,
        server.owner_id,
        auth.user_id,
        Permissions::MANAGE_CHANNELS,
    )
    .await?;

    channels::delete_channel(&state.db.pool, channel_id).await?;

    let _ = audit_log::write_entry(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        Some(auth.user_id),
        "CHANNEL_DELETE",
        Some("channel"),
        Some(channel_id),
        &serde_json::json!({ "name": channel.name }),
        None,
    )
    .await;

    tracing::info!(channel_id = %channel_id, "Channel deleted");

    Ok(Json(serde_json::json!({ "deleted": true })))
}
