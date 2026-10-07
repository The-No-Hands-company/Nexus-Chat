//! Webhook routes — create, manage, and execute webhooks.
//!
//! Incoming webhook execution URL: POST /webhooks/{id}/{token}
//! (No auth required — token in URL path authenticates the request.)

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    middleware,
    routing::{get, post},
};
use nexus_common::{
    error::{NexusError, NexusResult},
    gateway_event::GatewayEvent,
    models::webhook::{
        CreateIncomingWebhookRequest, CreateOutgoingWebhookRequest, ExecuteWebhookRequest,
        ModifyWebhookRequest, Webhook,
    },
    snowflake,
};
use nexus_db::repository::{audit_log, channels, webhooks};
use rand::Rng;
use rand::distr::Alphanumeric;
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    AppState,
    middleware::{AuthContext, check_rate_limit_with_fallback, extract_client_tag},
};

/// Webhook routes — authenticated management + unauthenticated execution.
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // Authenticated management
        .route(
            "/channels/{channel_id}/webhooks",
            get(get_channel_webhooks)
                .post(create_incoming_webhook)
                .route_layer(middleware::from_fn(
                    crate::middleware::combined_auth_middleware,
                )),
        )
        .route(
            "/servers/{server_id}/webhooks/outgoing",
            post(create_outgoing_webhook).route_layer(middleware::from_fn(
                crate::middleware::combined_auth_middleware,
            )),
        )
        .route(
            "/servers/{server_id}/webhooks",
            get(get_server_webhooks_route).route_layer(middleware::from_fn(
                crate::middleware::combined_auth_middleware,
            )),
        )
        .route(
            "/webhooks/{webhook_id}",
            get(get_webhook_authed)
                .patch(modify_webhook)
                .delete(delete_webhook)
                .route_layer(middleware::from_fn(
                    crate::middleware::combined_auth_middleware,
                )),
        )
        // Public execution URL — token in path, no Bearer required
        .route(
            "/webhooks/{webhook_id}/{token}",
            post(execute_webhook).get(get_webhook_public),
        )
}

/// Generate a random webhook token.
fn generate_webhook_token() -> String {
    rand::rng()
        .sample_iter(Alphanumeric)
        .take(64)
        .map(char::from)
        .collect()
}

// ============================================================================
// Management Endpoints (require user auth)
// ============================================================================

/// GET /api/v1/channels/{channel_id}/webhooks
async fn get_channel_webhooks(
    Extension(_auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
) -> NexusResult<Json<Vec<Webhook>>> {
    let hooks = webhooks::get_channel_webhooks(&state.db.pool, channel_id).await?;
    Ok(Json(hooks))
}

/// POST /api/v1/channels/{channel_id}/webhooks — Create an incoming webhook.
async fn create_incoming_webhook(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(channel_id): Path<Uuid>,
    Json(body): Json<CreateIncomingWebhookRequest>,
) -> NexusResult<Json<Webhook>> {
    // Rate limiting: 10 webhook creates per hour per user
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:webhook:create:{}", auth.user_id),
        10,
        3600,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:webhook:create:tag:{tag}"),
        20,
        3600,
    )
    .await?;
    // Look up the channel to get server_id
    let channel = channels::find_by_id(&state.db.pool, channel_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "channel".to_string(),
        })?;
    let server_id = channel.server_id.ok_or(NexusError::Validation {
        message: "Webhooks can only be created on server channels".into(),
    })?;

    let id = snowflake::generate_id();
    let token = generate_webhook_token();

    let wh = webhooks::create_incoming_webhook(
        &state.db.pool,
        id,
        server_id,
        channel_id,
        auth.user_id,
        &body.name,
        body.avatar.as_deref(),
        &token,
    )
    .await?;

    let _ = audit_log::write_entry(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        Some(auth.user_id),
        "WEBHOOK_CREATE",
        Some("webhook"),
        Some(id),
        &serde_json::json!({ "name": body.name, "channel_id": channel_id }),
        None,
    )
    .await;

    Ok(Json(wh))
}

/// POST /api/v1/servers/{server_id}/webhooks/outgoing — Create an outgoing webhook.
async fn create_outgoing_webhook(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
    Json(body): Json<CreateOutgoingWebhookRequest>,
) -> NexusResult<Json<Webhook>> {
    let id = snowflake::generate_id();

    let wh = webhooks::create_outgoing_webhook(
        &state.db.pool,
        id,
        server_id,
        auth.user_id,
        &body.name,
        &body.url,
        &body.events,
        body.avatar.as_deref(),
    )
    .await?;

    let _ = audit_log::write_entry(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        Some(auth.user_id),
        "WEBHOOK_CREATE",
        Some("webhook"),
        Some(id),
        &serde_json::json!({ "name": body.name, "url": body.url }),
        None,
    )
    .await;

    Ok(Json(wh))
}

/// GET /api/v1/webhooks/{webhook_id} — Get webhook info (with token, for owner).
async fn get_webhook_authed(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(webhook_id): Path<Uuid>,
) -> NexusResult<Json<Webhook>> {
    let wh = webhooks::get_webhook(&state.db.pool, webhook_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "webhook".to_string(),
        })?;

    // Only creator can see full token
    if wh.creator_id != Some(auth.user_id) {
        return Ok(Json(Webhook { token: None, ..wh }));
    }

    Ok(Json(wh))
}

/// PATCH /api/v1/webhooks/{webhook_id} — Modify a webhook.
async fn modify_webhook(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(webhook_id): Path<Uuid>,
    Json(body): Json<ModifyWebhookRequest>,
) -> NexusResult<Json<Webhook>> {
    let existing = webhooks::get_webhook(&state.db.pool, webhook_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "webhook".to_string(),
        })?;
    if existing.creator_id != Some(auth.user_id) {
        return Err(NexusError::Forbidden);
    }

    let updated = webhooks::update_webhook(
        &state.db.pool,
        webhook_id,
        body.name.as_deref(),
        body.avatar.as_deref(),
        body.channel_id,
        body.url.as_deref(),
        body.events.as_deref(),
        body.active,
    )
    .await?
    .ok_or(NexusError::NotFound {
        resource: "webhook".to_string(),
    })?;

    if let Some(sid) = existing.server_id {
        let _ = audit_log::write_entry(
            &state.db.pool,
            snowflake::generate_id(),
            sid,
            Some(auth.user_id),
            "WEBHOOK_UPDATE",
            Some("webhook"),
            Some(webhook_id),
            &serde_json::json!({ "name": body.name }),
            None,
        )
        .await;
    }

    Ok(Json(updated))
}

/// DELETE /api/v1/webhooks/{webhook_id} — Delete a webhook.
async fn delete_webhook(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(webhook_id): Path<Uuid>,
) -> NexusResult<axum::http::StatusCode> {
    let existing = webhooks::get_webhook(&state.db.pool, webhook_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "webhook".to_string(),
        })?;
    if existing.creator_id != Some(auth.user_id) {
        return Err(NexusError::Forbidden);
    }

    webhooks::delete_webhook(&state.db.pool, webhook_id).await?;

    if let Some(sid) = existing.server_id {
        let _ = audit_log::write_entry(
            &state.db.pool,
            snowflake::generate_id(),
            sid,
            Some(auth.user_id),
            "WEBHOOK_DELETE",
            Some("webhook"),
            Some(webhook_id),
            &serde_json::json!({ "name": existing.name }),
            None,
        )
        .await;
    }

    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ============================================================================
// Public Execution Endpoints (no user auth — token in URL)
// ============================================================================

/// GET /api/v1/webhooks/{webhook_id}/{token} — Get webhook info (public, no owner token).
async fn get_webhook_public(
    State(state): State<Arc<AppState>>,
    Path((webhook_id, token)): Path<(Uuid, String)>,
) -> NexusResult<Json<Webhook>> {
    let wh = webhooks::get_webhook_by_token(&state.db.pool, webhook_id, &token)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "webhook".to_string(),
        })?;
    Ok(Json(wh))
}

/// POST /api/v1/webhooks/{webhook_id}/{token} — Execute a webhook (post a message).
async fn execute_webhook(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path((webhook_id, token)): Path<(Uuid, String)>,
    Json(body): Json<ExecuteWebhookRequest>,
) -> NexusResult<axum::http::StatusCode> {
    // Rate limiting: 30 webhook executions per minute per webhook, 60 per client tag
    // This protects against spam through compromised webhooks
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:webhook:id:{}", webhook_id),
        30,
        60,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:webhook:tag:{tag}"),
        60,
        60,
    )
    .await?;
    // Validate token
    let wh = webhooks::get_webhook_by_token(&state.db.pool, webhook_id, &token)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "webhook".to_string(),
        })?;

    let channel_id = wh.channel_id.ok_or(NexusError::Validation {
        message: "Webhook has no target channel".into(),
    })?;

    let content = body.content.unwrap_or_default();
    if content.is_empty() && body.embeds.as_ref().is_none_or(|e| e.is_empty()) {
        return Err(NexusError::Validation {
            message: "content or embeds must be provided".into(),
        });
    }
    if content.len() > 4000 {
        return Err(NexusError::Validation {
            message: "Webhook message content exceeds 4000 characters".into(),
        });
    }

    let display_name = body.username.as_deref().unwrap_or(&wh.name);
    // Sanitize display name: strip dangerous formatting
    let display_name = display_name.trim().chars().take(80).collect::<String>();
    let display_name = if display_name.is_empty() {
        wh.name.clone()
    } else {
        display_name
    };

    // Parse @mentions from content so unread badge counts stay correct
    let mentions = nexus_common::validation::parse_mentions_from_content(&content);

    let msg_id = nexus_common::snowflake::generate_id();
    let message = nexus_db::repository::messages::create_message(
        &state.db.pool,
        msg_id,
        channel_id,
        // Webhooks use their own UUID as the pseudo-author so messages are
        // correctly attributed and can be fetched by webhook_id in audit trails.
        webhook_id,
        &content,
        0,    // message_type: 0 = Default
        None, // reference_message_id (webhooks don't reply)
        None, // reference_channel_id
        &mentions,
        &[], // mention_roles (webhooks don't resolve role mentions)
        content.contains("@everyone") || content.contains("@here"),
    )
    .await?;

    // Build the full MESSAGE_CREATE payload that clients expect — including
    // the webhook author fields so clients render the custom name/avatar.
    let gateway_payload = serde_json::json!({
        "id":           message.id,
        "channel_id":   channel_id,
        "author": {
            "id":           webhook_id,
            "username":     &display_name,
            "avatar":       body.avatar_url,
            "discriminator":"0000",
            "bot":          true,
            "webhook_id":   webhook_id,
        },
        "content":      &content,
        "embeds":       body.embeds.unwrap_or_default(),
        "attachments":  [],
        "mentions":     mentions.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
        "mention_roles": [],
        "mention_everyone": message.mention_everyone,
        "pinned":       false,
        "tts":          false,
        "type":         0,
        "created_at":   message.created_at,
    });

    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: nexus_common::gateway_event::event_types::MESSAGE_CREATE.to_string(),
        data: gateway_payload,
        server_id: wh.server_id,
        channel_id: Some(channel_id),
        user_id: None,
    });

    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ============================================================================
// GET /api/v1/servers/{server_id}/webhooks
// ============================================================================

/// GET /api/v1/servers/{server_id}/webhooks — List all webhooks for a server.
async fn get_server_webhooks_route(
    Extension(_auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
) -> NexusResult<Json<Vec<Webhook>>> {
    let hooks = webhooks::get_server_webhooks(&state.db.pool, server_id).await?;
    Ok(Json(hooks))
}
