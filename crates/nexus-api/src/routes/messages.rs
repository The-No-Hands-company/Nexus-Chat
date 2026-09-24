//! Message routes — send, edit, delete, history, search, pins, reactions.
//!
//! This is the core of chat. Every message mutation emits a gateway event
//! so connected WebSocket clients see changes in real-time.

use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    middleware,
    routing::{delete, get, post, put},
};
use chrono::Utc;
use nexus_common::gateway_event::GatewayEvent;
use nexus_common::{
    error::{NexusError, NexusResult},
    models::message::{CreateMessageRequest, UpdateMessageRequest},
    snowflake,
    validation::validate_request,
};
use nexus_db::repository::{
    channels, members, messages, moderation, reactions, read_states, scylla_outbox, servers,
};
use scylla::SessionBuilder;
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};
use uuid::Uuid;

use crate::{
    AppState,
    middleware::{AuthContext, check_rate_limit_with_fallback, extract_client_ip},
};

/// Message routes.
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // Message CRUD
        .route(
            "/channels/{channel_id}/messages",
            get(get_messages).post(send_message),
        )
        .route(
            "/channels/{channel_id}/messages/{message_id}",
            get(get_message).patch(edit_message).delete(delete_message),
        )
        // Bulk delete
        .route(
            "/channels/{channel_id}/messages/bulk-delete",
            post(bulk_delete_messages),
        )
        // Pins
        .route("/channels/{channel_id}/pins", get(get_pinned_messages))
        .route(
            "/channels/{channel_id}/pins/{message_id}",
            put(pin_message).delete(unpin_message),
        )
        // Reactions
        .route(
            "/channels/{channel_id}/messages/{message_id}/reactions/{emoji}/@me",
            put(add_reaction).delete(remove_reaction),
        )
        .route(
            "/channels/{channel_id}/messages/{message_id}/reactions/{emoji}",
            get(get_reactors).delete(remove_all_emoji_reactions),
        )
        .route(
            "/channels/{channel_id}/messages/{message_id}/reactions",
            delete(remove_all_reactions),
        )
        // Read state
        .route("/channels/{channel_id}/ack/{message_id}", post(ack_message))
        // Search
        .route("/channels/{channel_id}/search", get(search_messages))
        // Announcement channel — publish (crosspost) a message
        .route(
            "/channels/{channel_id}/messages/{message_id}/crosspost",
            post(crosspost_message),
        )
        // Announcement channel follower — subscribe a channel to this announcement channel
        .route(
            "/channels/{channel_id}/followers",
            put(add_channel_follower),
        )
        // All routes require authentication
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ))
}

// ============================================================================
// Query parameters
// ============================================================================

#[derive(Debug, Deserialize)]
struct MessageHistoryParams {
    before: Option<Uuid>,
    after: Option<Uuid>,
    limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct SearchParams {
    query: String,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct BulkDeleteBody {
    messages: Vec<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScyllaReadStrategy {
    Off,
    Canary,
    Prefer,
}

impl ScyllaReadStrategy {
    fn as_label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Canary => "canary",
            Self::Prefer => "prefer",
        }
    }
}

// ============================================================================
// Message CRUD
// ============================================================================

/// POST /api/v1/channels/:channel_id/messages — Send a message.
async fn send_message(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(channel_id): Path<Uuid>,
    Json(body): Json<CreateMessageRequest>,
) -> NexusResult<Json<serde_json::Value>> {
    validate_request(&body)?;

    // ── Rate limiting: 30 messages per user per 10 seconds ─────────────────
    let ip = extract_client_ip(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:msg:user:{}", auth.user_id),
        30,
        10,
    )
    .await?;
    check_rate_limit_with_fallback(state.db.redis.as_ref(), format!("rl:msg:ip:{ip}"), 60, 10)
        .await?;

    // Verify channel exists
    let channel = channels::find_by_id(&state.db.pool, channel_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Channel".into(),
        })?;

    // If this is a server channel, verify membership, timeout status, and content filters
    // Also capture per-server spam settings for the check below.
    let mut spam_window_secs: u64 = 30;
    let mut spam_max_messages: u64 = 3;
    if let Some(server_id) = channel.server_id {
        let member = members::find_member(&state.db.pool, auth.user_id, server_id)
            .await?
            .ok_or(NexusError::Forbidden)?;

        // Timeout enforcement: member cannot send messages while timed out
        if let Some(disabled_until) = member.communication_disabled_until
            && disabled_until > Utc::now() {
                return Err(NexusError::Validation {
                    message: "You are currently timed out in this server".into(),
                });
            }

        // Word filter check
        if let Ok(Some((_pattern, action))) =
            moderation::check_content(&state.db.pool, server_id, &body.content).await
        {
            match action.as_str() {
                "warn" => {
                    // Log but allow — a future gateway event could warn the client
                    tracing::debug!(
                        user_id = %auth.user_id,
                        channel_id = %channel_id,
                        "Word filter matched (warn action)"
                    );
                }
                _ => {
                    // "block" and "delete" both prevent sending
                    return Err(NexusError::Validation {
                        message: "Message contains prohibited content".into(),
                    });
                }
            }
        }

        // Load per-server spam detection settings.
        if let Ok(Some(server)) =
            nexus_db::repository::servers::find_by_id(&state.db.pool, server_id).await
        {
            spam_window_secs = server.spam_window_secs.max(1) as u64;
            spam_max_messages = server.spam_max_messages.max(1) as u64;
        }
    }

    // Spam check: reject if the same content exceeds the per-server limit within the window
    if check_spam(
        &state.db.redis,
        auth.user_id,
        channel_id,
        &body.content,
        spam_window_secs,
        spam_max_messages,
    )
    .await
    {
        return Err(NexusError::RateLimited {
            retry_after_ms: spam_window_secs * 1_000,
        });
    }

    // Determine message type: 0 = Default, 1 = Reply (if reference provided)
    let message_type = if body.reference.is_some() { 1 } else { 0 };

    let (ref_msg_id, ref_ch_id) = match &body.reference {
        Some(r) => (Some(r.message_id), Some(r.channel_id)),
        None => (None, None),
    };

    // Parse mentions from content (basic @user_id pattern)
    let mentions = parse_mentions(&body.content);
    let mention_everyone = body.content.contains("@everyone");

    let message_id = snowflake::generate_id();
    let msg = messages::create_message(
        &state.db.pool,
        message_id,
        channel_id,
        auth.user_id,
        &body.content,
        message_type,
        ref_msg_id,
        ref_ch_id,
        &mentions,
        &[],
        mention_everyone,
    )
    .await?;

    // v0.14: persist topic and sticker_ids if provided
    if body.topic.is_some() || body.sticker_ids.as_ref().is_some_and(|v| !v.is_empty()) {
        let topic_val = body.topic.as_deref().unwrap_or("");
        let sticker_ids_str = body
            .sticker_ids
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");
        // Postgres array literal: '{uuid1,uuid2}'::uuid[]
        let sticker_literal = format!("{{{sticker_ids_str}}}");
        let sql = format!(
            "UPDATE messages SET \
                 topic = NULLIF($1, ''), \
                 sticker_ids = '{sticker_literal}'::uuid[] \
             WHERE id = $2::uuid"
        );
        let _ = sqlx::query(&sql)
            .bind(topic_val)
            .bind(message_id.to_string())
            .execute(&state.db.pool)
            .await;
    }

    // Disappearing messages — if the channel has a TTL set, write expires_at
    // We query the column added in migration 00015 directly to avoid changing
    // the Channel model and all its consumers.
    let disappear_secs: Option<(i64,)> =
        sqlx::query_as("SELECT disappear_after_seconds FROM channels WHERE id = $1::uuid")
            .bind(channel_id.to_string())
            .fetch_optional(&state.db.pool)
            .await
            .ok()
            .flatten();
    if let Some((secs,)) = disappear_secs
        && secs > 0 {
            let _ = sqlx::query(
                "UPDATE messages SET expires_at = NOW() + ($1 * INTERVAL '1 second') WHERE id = $2::uuid",
            )
            .bind(secs)
            .bind(message_id.to_string())
            .execute(&state.db.pool)
            .await;
        }

    // Increment mention counts for mentioned users
    for mentioned_user_id in &mentions {
        let _ =
            read_states::increment_mention_count(&state.db.pool, *mentioned_user_id, channel_id)
                .await;
    }

    let mut response = message_row_to_json(&msg, &[]);
    response["author_username"] = serde_json::Value::String(auth.username.clone());

    // Keep full-text index in sync after create.
    let doc = nexus_db::search::MessageDocument {
        id: msg.id.to_string(),
        channel_id: msg.channel_id.to_string(),
        server_id: channel.server_id.map(|x| x.to_string()),
        author_id: msg.author_id.to_string(),
        author_username: auth.username.clone(),
        content: msg.content.clone(),
        has_attachments: msg
            .attachments
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        has_embeds: msg
            .embeds
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        created_at: msg.created_at.timestamp(),
    };
    let _ = state
        .search
        .sync_message_index(&state.db.pool, msg.id, doc.clone())
        .await;

    if state.db.scylla_enabled {
        let _ = scylla_outbox::enqueue_upsert_message(&state.db.pool, &doc).await;
    }

    // Emit MESSAGE_CREATE event to gateway
    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: "MESSAGE_CREATE".into(),
        data: response.clone(),
        server_id: channel.server_id,
        channel_id: Some(channel_id),
        user_id: Some(auth.user_id),
    });

    // ── Web Push: notify mentioned users ─────────────────────────────────────
    // Fire-and-forget — push delivery must never block message delivery.
    // Uses the PushSender already initialised at startup (key material loaded once).
    if let Some(ref sender) = state.push {
        let sender = sender.clone();
        let pool = state.db.pool.clone();
        let author_name = auth.username.clone();
        let author_id = auth.user_id;
        // Truncate at 120 chars — enough for a notification preview without
        // leaking too much E2EE-adjacent content in push payloads.
        let content_preview: String = body.content.chars().take(120).collect();
        let channel_id_copy = channel_id;
        let mentions_copy = mentions.clone();
        let channel_type = channel.channel_type;

        tokio::spawn(async move {
            // ── Case 1: @mention in a server channel ─────────────────────
            for &mentioned_uid in &mentions_copy {
                if mentioned_uid == author_id {
                    continue; // never self-ping
                }
                let payload = crate::push_sender::PushPayload {
                    title: format!("{} mentioned you", author_name),
                    body: content_preview.clone(),
                    icon: Some("/icon-192.png".into()),
                    url: Some(format!("/channel/{channel_id_copy}")),
                    channel_id: Some(channel_id_copy),
                };
                if let Err(e) = sender.notify_user(&pool, mentioned_uid, &payload).await {
                    tracing::warn!(
                        user_id = %mentioned_uid,
                        error = %e,
                        "Push notification failed for @mention"
                    );
                }
            }

            // ── Case 2: DM / Group DM — notify all other participants ────
            // DMs always warrant a push regardless of @mention syntax.
            let is_dm = matches!(
                channel_type,
                nexus_common::models::channel::ChannelType::Dm
                    | nexus_common::models::channel::ChannelType::GroupDm
            );
            if is_dm {
                // Fetch other participants
                let others: Vec<(String,)> = sqlx::query_as(
                    "SELECT user_id::text FROM dm_participants \
                     WHERE channel_id = $1::uuid AND user_id != $2::uuid",
                )
                .bind(channel_id_copy.to_string())
                .bind(author_id.to_string())
                .fetch_all(&pool)
                .await
                .unwrap_or_default();

                for (uid_str,) in others {
                    let Ok(recipient_uid) = uid_str.parse::<Uuid>() else {
                        continue;
                    };
                    // Skip if already notified via @mention above
                    if mentions_copy.contains(&recipient_uid) {
                        continue;
                    }
                    let payload = crate::push_sender::PushPayload {
                        title: format!("Message from {}", author_name),
                        body: content_preview.clone(),
                        icon: Some("/icon-192.png".into()),
                        url: Some(format!("/channel/{channel_id_copy}")),
                        channel_id: Some(channel_id_copy),
                    };
                    if let Err(e) = sender.notify_user(&pool, recipient_uid, &payload).await {
                        tracing::warn!(
                            user_id = %recipient_uid,
                            error = %e,
                            "Push notification failed for DM"
                        );
                    }
                }
            }
        });
    }

    // ── Web Push: notify DM participants ──────────────────────────────────────
    // When a message arrives in a DM or group DM channel (no server_id),
    // push every other participant so they see it even when the app is closed.
    // Mentioned-user pushes above cover @mentions in server channels;
    // this block covers the direct-message case where there are no @mentions.
    if channel.server_id.is_none()
        && let Some(ref sender) = state.push {
            let sender = sender.clone();
            let pool = state.db.pool.clone();
            let author_name = auth.username.clone();
            let content_preview: String = body.content.chars().take(120).collect();
            let channel_id_copy = channel_id;
            let author_id = auth.user_id;

            tokio::spawn(async move {
                let participants = match nexus_db::repository::channels::list_dm_participants(
                    &pool,
                    channel_id_copy,
                )
                .await
                {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(
                            channel_id = %channel_id_copy,
                            error = %e,
                            "Failed to load DM participants for push notification"
                        );
                        return;
                    }
                };

                let payload = crate::push_sender::PushPayload {
                    title: author_name,
                    body: content_preview,
                    icon: Some("/icon-192.png".into()),
                    url: Some(format!("/channel/{channel_id_copy}")),
                    channel_id: Some(channel_id_copy),
                };

                for participant_uid in participants {
                    if participant_uid == author_id {
                        continue; // never self-ping
                    }
                    if let Err(e) = sender.notify_user(&pool, participant_uid, &payload).await {
                        tracing::warn!(
                            user_id = %participant_uid,
                            error = %e,
                            "Push notification failed for DM"
                        );
                    }
                }
            });
        }

    tracing::debug!(
        message_id = %message_id,
        channel_id = %channel_id,
        author = %auth.username,
        "Message sent"
    );

    Ok(Json(response))
}

/// GET /api/v1/channels/:channel_id/messages — Get message history.
async fn get_messages(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
    Query(params): Query<MessageHistoryParams>,
) -> NexusResult<Json<Vec<serde_json::Value>>> {
    // Verify channel exists
    let channel = channels::find_by_id(&state.db.pool, channel_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Channel".into(),
        })?;

    // If server channel, verify membership
    if let Some(server_id) = channel.server_id
        && !members::is_member(&state.db.pool, auth.user_id, server_id).await? {
            return Err(NexusError::Forbidden);
        }

    let limit = params.limit.unwrap_or(50).clamp(1, 100);
    let strategy = scylla_read_strategy(&state);
    if strategy != ScyllaReadStrategy::Off && params.before.is_none() && params.after.is_none() {
        match list_messages_from_scylla(&state, channel_id, auth.user_id, limit).await {
            Ok(rows) => {
                if !rows.is_empty() {
                    metrics::counter!(
                        "nexus_scylla_read_total",
                        "route" => "get_messages",
                        "strategy" => strategy.as_label(),
                        "outcome" => "hit",
                    )
                    .increment(1);
                    return Ok(Json(rows));
                }
                metrics::counter!(
                    "nexus_scylla_read_total",
                    "route" => "get_messages",
                    "strategy" => strategy.as_label(),
                    "outcome" => "empty",
                )
                .increment(1);
                if strategy == ScyllaReadStrategy::Prefer {
                    return Ok(Json(rows));
                }
            }
            Err(err) => {
                tracing::debug!(
                    channel_id = %channel_id,
                    error = %err,
                    strategy = strategy.as_label(),
                    "Scylla get_messages read failed; falling back to SQL"
                );
                metrics::counter!(
                    "nexus_scylla_read_total",
                    "route" => "get_messages",
                    "strategy" => strategy.as_label(),
                    "outcome" => "error",
                )
                .increment(1);
            }
        }
    }

    if strategy != ScyllaReadStrategy::Off {
        metrics::counter!(
            "nexus_scylla_read_total",
            "route" => "get_messages",
            "strategy" => strategy.as_label(),
            "outcome" => "sql_fallback",
        )
        .increment(1);
    }

    let rows = messages::list_channel_messages_with_author(
        &state.db.pool,
        channel_id,
        params.before,
        params.after,
        limit,
    )
    .await?;

    let message_ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    let reaction_counts_map =
        reactions::get_reaction_counts_for_messages(&state.db.pool, &message_ids)
            .await
            .unwrap_or_default();
    let my_reactions_map = reactions::get_user_reaction_emojis_for_messages(
        &state.db.pool,
        auth.user_id,
        &message_ids,
    )
    .await
    .unwrap_or_default();

    let mut result = Vec::with_capacity(rows.len());
    for row in &rows {
        let reaction_counts = reaction_counts_map
            .get(&row.id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let my_reactions = my_reactions_map
            .get(&row.id)
            .map(|set| set.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        result.push(message_with_author_to_json(
            row,
            reaction_counts,
            &my_reactions,
        ));
    }

    Ok(Json(result))
}

/// GET /api/v1/channels/:channel_id/messages/:message_id — Get a single message.
async fn get_message(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((channel_id, message_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<Json<serde_json::Value>> {
    // Verify channel exists and membership rules before exposing message details.
    let channel = channels::find_by_id(&state.db.pool, channel_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Channel".into(),
        })?;
    if let Some(server_id) = channel.server_id
        && !members::is_member(&state.db.pool, auth.user_id, server_id).await? {
            return Err(NexusError::Forbidden);
        }

    let strategy = scylla_read_strategy(&state);
    if strategy != ScyllaReadStrategy::Off {
        match get_message_from_scylla(&state, channel_id, message_id, auth.user_id).await {
            Ok(Some(msg)) => {
                metrics::counter!(
                    "nexus_scylla_read_total",
                    "route" => "get_message",
                    "strategy" => strategy.as_label(),
                    "outcome" => "hit",
                )
                .increment(1);
                return Ok(Json(msg));
            }
            Ok(None) => {
                metrics::counter!(
                    "nexus_scylla_read_total",
                    "route" => "get_message",
                    "strategy" => strategy.as_label(),
                    "outcome" => "empty",
                )
                .increment(1);
                if strategy == ScyllaReadStrategy::Prefer {
                    return Err(NexusError::NotFound {
                        resource: "Message".into(),
                    });
                }
            }
            Err(err) => {
                tracing::debug!(
                    message_id = %message_id,
                    channel_id = %channel_id,
                    error = %err,
                    strategy = strategy.as_label(),
                    "Scylla get_message read failed; falling back to SQL"
                );
                metrics::counter!(
                    "nexus_scylla_read_total",
                    "route" => "get_message",
                    "strategy" => strategy.as_label(),
                    "outcome" => "error",
                )
                .increment(1);
            }
        }
    }

    if strategy != ScyllaReadStrategy::Off {
        metrics::counter!(
            "nexus_scylla_read_total",
            "route" => "get_message",
            "strategy" => strategy.as_label(),
            "outcome" => "sql_fallback",
        )
        .increment(1);
    }

    let msg = messages::find_by_id(&state.db.pool, message_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Message".into(),
        })?;

    if msg.channel_id != channel_id {
        return Err(NexusError::NotFound {
            resource: "Message".into(),
        });
    }

    let reaction_counts = reactions::get_reaction_counts(&state.db.pool, message_id)
        .await
        .unwrap_or_default();

    Ok(Json(message_row_to_json(&msg, &reaction_counts)))
}

/// PATCH /api/v1/channels/:channel_id/messages/:message_id — Edit a message.
async fn edit_message(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((channel_id, message_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<UpdateMessageRequest>,
) -> NexusResult<Json<serde_json::Value>> {
    validate_request(&body)?;

    let msg = messages::find_by_id(&state.db.pool, message_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Message".into(),
        })?;

    // Only the author can edit their own message
    if msg.author_id != auth.user_id {
        return Err(NexusError::Forbidden);
    }

    if msg.channel_id != channel_id {
        return Err(NexusError::NotFound {
            resource: "Message".into(),
        });
    }

    let content = body.content.as_deref().ok_or(NexusError::Validation {
        message: "Content is required".into(),
    })?;

    // Fetch the channel before applying the edit so we can run the word filter.
    let channel = channels::find_by_id(&state.db.pool, channel_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Channel".into(),
        })?;

    // Word filter — reject / warn before persisting the edit.
    if let Some(server_id) = channel.server_id
        && let Ok(Some((_pattern, action))) =
            moderation::check_content(&state.db.pool, server_id, content).await
        {
            match action.as_str() {
                "warn" => tracing::debug!(
                    server_id = %server_id,
                    "word filter match on message edit (warn only)"
                ),
                _ => {
                    return Err(NexusError::Validation {
                        message: "Message contains prohibited content".into(),
                    });
                }
            }
        }

    let updated = messages::update_message(&state.db.pool, message_id, content).await?;

    // Keep full-text index in sync after edit.
    let doc = nexus_db::search::MessageDocument {
        id: updated.id.to_string(),
        channel_id: updated.channel_id.to_string(),
        server_id: channel.server_id.map(|x| x.to_string()),
        author_id: updated.author_id.to_string(),
        author_username: auth.username.clone(),
        content: updated.content.clone(),
        has_attachments: updated
            .attachments
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        has_embeds: updated
            .embeds
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        created_at: updated.created_at.timestamp(),
    };
    let _ = state
        .search
        .sync_message_index(&state.db.pool, updated.id, doc.clone())
        .await;

    if state.db.scylla_enabled {
        let _ = scylla_outbox::enqueue_upsert_message(&state.db.pool, &doc).await;
    }

    let response = message_row_to_json(&updated, &[]);

    // Emit MESSAGE_UPDATE event
    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: "MESSAGE_UPDATE".into(),
        data: response.clone(),
        server_id: channel.server_id,
        channel_id: Some(channel_id),
        user_id: Some(auth.user_id),
    });

    Ok(Json(response))
}

/// DELETE /api/v1/channels/:channel_id/messages/:message_id — Delete a message.
async fn delete_message(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((channel_id, message_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<Json<serde_json::Value>> {
    let msg = messages::find_by_id(&state.db.pool, message_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Message".into(),
        })?;

    if msg.channel_id != channel_id {
        return Err(NexusError::NotFound {
            resource: "Message".into(),
        });
    }

    // Author can delete their own, or MANAGE_MESSAGES permission in server channels
    let channel = channels::find_by_id(&state.db.pool, channel_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Channel".into(),
        })?;

    if msg.author_id != auth.user_id {
        // Check if user has MANAGE_MESSAGES permission
        if let Some(server_id) = channel.server_id {
            let server = nexus_db::repository::servers::find_by_id(&state.db.pool, server_id)
                .await?
                .ok_or(NexusError::NotFound {
                    resource: "Server".into(),
                })?;
            if server.owner_id != auth.user_id {
                return Err(NexusError::MissingPermission {
                    permission: "MANAGE_MESSAGES".into(),
                });
            }
        } else {
            return Err(NexusError::Forbidden);
        }
    }

    messages::delete_message(&state.db.pool, message_id).await?;

    // Keep full-text index in sync after delete.
    let _ = state
        .search
        .sync_message_delete(&state.db.pool, message_id)
        .await;
    if state.db.scylla_enabled {
        let _ = scylla_outbox::enqueue_delete_message(&state.db.pool, message_id).await;
    }

    // Write an audit log entry when a moderator deletes someone else's message
    if msg.author_id != auth.user_id
        && let Some(server_id) = channel.server_id {
            let _ = nexus_db::repository::audit_log::write_entry(
                &state.db.pool,
                snowflake::generate_id(),
                server_id,
                Some(auth.user_id),
                "MESSAGE_DELETE",
                Some("message"),
                Some(message_id),
                &serde_json::json!({ "channel_id": channel_id, "author_id": msg.author_id }),
                None,
            )
            .await;
        }

    // Emit MESSAGE_DELETE event
    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: "MESSAGE_DELETE".into(),
        data: serde_json::json!({
            "id": message_id,
            "channel_id": channel_id,
            "server_id": channel.server_id,
        }),
        server_id: channel.server_id,
        channel_id: Some(channel_id),
        user_id: Some(auth.user_id),
    });

    Ok(Json(serde_json::json!({ "deleted": true })))
}

/// POST /api/v1/channels/:channel_id/messages/bulk-delete
async fn bulk_delete_messages(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
    Json(body): Json<BulkDeleteBody>,
) -> NexusResult<Json<serde_json::Value>> {
    if body.messages.is_empty() || body.messages.len() > 100 {
        return Err(NexusError::Validation {
            message: "Must delete between 1 and 100 messages".into(),
        });
    }

    let channel = channels::find_by_id(&state.db.pool, channel_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Channel".into(),
        })?;

    // Must be server owner or have MANAGE_MESSAGES
    if let Some(server_id) = channel.server_id {
        let server = nexus_db::repository::servers::find_by_id(&state.db.pool, server_id)
            .await?
            .ok_or(NexusError::NotFound {
                resource: "Server".into(),
            })?;
        if server.owner_id != auth.user_id {
            return Err(NexusError::MissingPermission {
                permission: "MANAGE_MESSAGES".into(),
            });
        }
    } else {
        return Err(NexusError::Forbidden);
    }

    let deleted = messages::bulk_delete_messages(&state.db.pool, &body.messages).await?;

    // Keep full-text index in sync after bulk delete.
    for mid in &body.messages {
        let _ = state.search.sync_message_delete(&state.db.pool, *mid).await;
        if state.db.scylla_enabled {
            let _ = scylla_outbox::enqueue_delete_message(&state.db.pool, *mid).await;
        }
    }

    // Emit MESSAGE_BULK_DELETE event
    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: "MESSAGE_BULK_DELETE".into(),
        data: serde_json::json!({
            "ids": body.messages,
            "channel_id": channel_id,
            "server_id": channel.server_id,
        }),
        server_id: channel.server_id,
        channel_id: Some(channel_id),
        user_id: Some(auth.user_id),
    });

    Ok(Json(serde_json::json!({ "deleted": deleted })))
}

// ============================================================================
// Pins
// ============================================================================

/// GET /api/v1/channels/:channel_id/pins
async fn get_pinned_messages(
    Extension(_auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
) -> NexusResult<Json<Vec<serde_json::Value>>> {
    let rows = messages::get_pinned_messages(&state.db.pool, channel_id).await?;
    let result: Vec<serde_json::Value> = rows.iter().map(|r| message_row_to_json(r, &[])).collect();
    Ok(Json(result))
}

/// PUT /api/v1/channels/:channel_id/pins/:message_id
async fn pin_message(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((channel_id, message_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<Json<serde_json::Value>> {
    let msg = messages::find_by_id(&state.db.pool, message_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Message".into(),
        })?;

    if msg.channel_id != channel_id {
        return Err(NexusError::NotFound {
            resource: "Message".into(),
        });
    }

    let channel = channels::find_by_id(&state.db.pool, channel_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Channel".into(),
        })?;

    // Check permission — for now, any member can pin in DMs, owner in servers
    if let Some(server_id) = channel.server_id {
        let server = nexus_db::repository::servers::find_by_id(&state.db.pool, server_id)
            .await?
            .ok_or(NexusError::NotFound {
                resource: "Server".into(),
            })?;
        if server.owner_id != auth.user_id {
            return Err(NexusError::MissingPermission {
                permission: "MANAGE_MESSAGES".into(),
            });
        }
    }

    let pinned = messages::pin_message(&state.db.pool, message_id).await?;
    let response = message_row_to_json(&pinned, &[]);

    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: "CHANNEL_PINS_UPDATE".into(),
        data: serde_json::json!({
            "channel_id": channel_id,
            "message": response,
        }),
        server_id: channel.server_id,
        channel_id: Some(channel_id),
        user_id: Some(auth.user_id),
    });

    Ok(Json(serde_json::json!({ "pinned": true })))
}

/// DELETE /api/v1/channels/:channel_id/pins/:message_id
async fn unpin_message(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((channel_id, message_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<Json<serde_json::Value>> {
    let msg = messages::find_by_id(&state.db.pool, message_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Message".into(),
        })?;

    if msg.channel_id != channel_id {
        return Err(NexusError::NotFound {
            resource: "Message".into(),
        });
    }

    messages::unpin_message(&state.db.pool, message_id).await?;

    let channel = channels::find_by_id(&state.db.pool, channel_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Channel".into(),
        })?;

    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: "CHANNEL_PINS_UPDATE".into(),
        data: serde_json::json!({
            "channel_id": channel_id,
            "unpinned_message_id": message_id,
        }),
        server_id: channel.server_id,
        channel_id: Some(channel_id),
        user_id: Some(auth.user_id),
    });

    Ok(Json(serde_json::json!({ "unpinned": true })))
}

// ============================================================================
// Reactions
// ============================================================================

/// PUT /api/v1/channels/:channel_id/messages/:message_id/reactions/:emoji/@me
async fn add_reaction(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((channel_id, message_id, emoji)): Path<(Uuid, Uuid, String)>,
) -> NexusResult<Json<serde_json::Value>> {
    // Verify message exists in channel
    let msg = messages::find_by_id(&state.db.pool, message_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Message".into(),
        })?;

    if msg.channel_id != channel_id {
        return Err(NexusError::NotFound {
            resource: "Message".into(),
        });
    }

    let added = reactions::add_reaction(&state.db.pool, message_id, auth.user_id, &emoji).await?;

    if added {
        let channel = channels::find_by_id(&state.db.pool, channel_id)
            .await?
            .ok_or(NexusError::NotFound {
                resource: "Channel".into(),
            })?;

        let _ = state.gateway_tx.send(GatewayEvent {
            event_type: "MESSAGE_REACTION_ADD".into(),
            data: serde_json::json!({
                "message_id": message_id,
                "channel_id": channel_id,
                "user_id": auth.user_id,
                "emoji": emoji,
            }),
            server_id: channel.server_id,
            channel_id: Some(channel_id),
            user_id: Some(auth.user_id),
        });
    }

    Ok(Json(serde_json::json!({ "added": added })))
}

/// DELETE /api/v1/channels/:channel_id/messages/:message_id/reactions/:emoji/@me
async fn remove_reaction(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((channel_id, message_id, emoji)): Path<(Uuid, Uuid, String)>,
) -> NexusResult<Json<serde_json::Value>> {
    let removed =
        reactions::remove_reaction(&state.db.pool, message_id, auth.user_id, &emoji).await?;

    if removed {
        let channel = channels::find_by_id(&state.db.pool, channel_id)
            .await?
            .ok_or(NexusError::NotFound {
                resource: "Channel".into(),
            })?;

        let _ = state.gateway_tx.send(GatewayEvent {
            event_type: "MESSAGE_REACTION_REMOVE".into(),
            data: serde_json::json!({
                "message_id": message_id,
                "channel_id": channel_id,
                "user_id": auth.user_id,
                "emoji": emoji,
            }),
            server_id: channel.server_id,
            channel_id: Some(channel_id),
            user_id: Some(auth.user_id),
        });
    }

    Ok(Json(serde_json::json!({ "removed": removed })))
}

/// GET /api/v1/channels/:channel_id/messages/:message_id/reactions/:emoji
async fn get_reactors(
    Extension(_auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((_channel_id, message_id, emoji)): Path<(Uuid, Uuid, String)>,
) -> NexusResult<Json<Vec<Uuid>>> {
    let users = reactions::get_reactors(&state.db.pool, message_id, &emoji, 100).await?;
    Ok(Json(users))
}

/// DELETE /api/v1/channels/:channel_id/messages/:message_id/reactions/:emoji — Remove all of one emoji
async fn remove_all_emoji_reactions(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((channel_id, message_id, emoji)): Path<(Uuid, Uuid, String)>,
) -> NexusResult<Json<serde_json::Value>> {
    let count = remove_all_emoji_reactions_authorized(
        &state.db.pool,
        auth.user_id,
        channel_id,
        message_id,
        &emoji,
    )
    .await?;
    Ok(Json(serde_json::json!({ "removed": count })))
}

/// DELETE /api/v1/channels/:channel_id/messages/:message_id/reactions — Remove all reactions
async fn remove_all_reactions(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((channel_id, message_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<Json<serde_json::Value>> {
    let count = remove_all_reactions_authorized(
        &state.db.pool,
        auth.user_id,
        channel_id,
        message_id,
    )
    .await?;
    Ok(Json(serde_json::json!({ "removed": count })))
}

async fn authorize_bulk_reaction_target(
    pool: &sqlx::AnyPool,
    actor_id: Uuid,
    requested_channel_id: Uuid,
    message_id: Uuid,
) -> NexusResult<()> {
    let connection = pool.acquire().await?;
    let (message_parameter, actor_parameter) = match connection.backend_name() {
        "PostgreSQL" => ("$1::uuid", "$2::uuid"),
        "SQLite" => ("$1", "$2"),
        backend => {
            return Err(sqlx::Error::Configuration(
                format!("unsupported message database backend: {backend}").into(),
            )
            .into());
        }
    };
    drop(connection);

    let query = format!(
        "SELECT CAST(m.channel_id AS TEXT), CAST(c.server_id AS TEXT), \
                CAST(s.owner_id AS TEXT), CAST(dp.user_id AS TEXT) \
         FROM messages m \
         INNER JOIN channels c ON c.id = m.channel_id \
         LEFT JOIN servers s ON s.id = c.server_id \
         LEFT JOIN dm_participants dp \
           ON dp.channel_id = m.channel_id AND dp.user_id = {actor_parameter} \
         WHERE m.id = {message_parameter}"
    );
    let target: Option<(String, Option<String>, Option<String>, Option<String>)> =
        sqlx::query_as(&query)
            .bind(message_id.to_string())
            .bind(actor_id.to_string())
            .fetch_optional(pool)
            .await?;
    let (actual_channel_id, server_id, server_owner_id, dm_participant_id) =
        target.ok_or(NexusError::NotFound {
            resource: "Message".into(),
        })?;
    let actual_channel_id = actual_channel_id.parse::<Uuid>().map_err(|error| {
        NexusError::Internal(anyhow::anyhow!("invalid target channel UUID: {error}"))
    })?;
    if actual_channel_id != requested_channel_id {
        return Err(NexusError::NotFound {
            resource: "Message".into(),
        });
    }

    if server_id.is_some() {
        let owner_id = server_owner_id
            .ok_or(NexusError::NotFound {
                resource: "Server".into(),
            })?
            .parse::<Uuid>()
            .map_err(|error| {
                NexusError::Internal(anyhow::anyhow!("invalid target server owner UUID: {error}"))
            })?;
        if owner_id != actor_id {
            return Err(NexusError::MissingPermission {
                permission: "MANAGE_MESSAGES".into(),
            });
        }
    } else if dm_participant_id.is_none() {
        return Err(NexusError::Forbidden);
    }
    Ok(())
}

async fn remove_all_emoji_reactions_authorized(
    pool: &sqlx::AnyPool,
    actor_id: Uuid,
    requested_channel_id: Uuid,
    message_id: Uuid,
    emoji: &str,
) -> NexusResult<u64> {
    authorize_bulk_reaction_target(pool, actor_id, requested_channel_id, message_id).await?;
    Ok(reactions::remove_all_reactions_for_emoji(pool, message_id, emoji).await?)
}

async fn remove_all_reactions_authorized(
    pool: &sqlx::AnyPool,
    actor_id: Uuid,
    requested_channel_id: Uuid,
    message_id: Uuid,
) -> NexusResult<u64> {
    authorize_bulk_reaction_target(pool, actor_id, requested_channel_id, message_id).await?;
    Ok(reactions::remove_all_reactions(pool, message_id).await?)
}

// ============================================================================
// Read state
// ============================================================================

/// POST /api/v1/channels/:channel_id/ack/:message_id — Acknowledge reading up to a message.
async fn ack_message(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((channel_id, message_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<Json<serde_json::Value>> {
    let rs = read_states::ack_message(&state.db.pool, auth.user_id, channel_id, message_id).await?;

    Ok(Json(serde_json::json!({
        "channel_id": rs.channel_id,
        "last_read_message_id": rs.last_read_message_id,
        "mention_count": rs.mention_count,
    })))
}

// ============================================================================
// Search
// ============================================================================

/// GET /api/v1/channels/:channel_id/search?query=...&limit=...&offset=...
async fn search_messages(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
    Query(params): Query<SearchParams>,
) -> NexusResult<Json<Vec<serde_json::Value>>> {
    // Verify access
    let channel = channels::find_by_id(&state.db.pool, channel_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Channel".into(),
        })?;

    if let Some(server_id) = channel.server_id
        && !members::is_member(&state.db.pool, auth.user_id, server_id).await? {
            return Err(NexusError::Forbidden);
        }

    let limit = params.limit.unwrap_or(25);
    let offset = params.offset.unwrap_or(0);

    let rows = messages::search_messages(
        &state.db.pool,
        Some(channel_id),
        &params.query,
        limit,
        offset,
    )
    .await?;

    let result: Vec<serde_json::Value> = rows.iter().map(|r| message_row_to_json(r, &[])).collect();
    Ok(Json(result))
}

// ============================================================================
// Helpers
// ============================================================================

/// Convert a MessageRow to a JSON response.
fn message_row_to_json(
    row: &messages::MessageRow,
    reaction_counts: &[reactions::ReactionCount],
) -> serde_json::Value {
    message_row_to_json_with_reactions(row, reaction_counts, &[])
}

/// Build the message JSON object with author_username included.
fn message_with_author_to_json(
    row: &messages::MessageWithAuthor,
    reaction_counts: &[reactions::ReactionCount],
    my_reactions: &[String],
) -> serde_json::Value {
    let reactions_json: Vec<serde_json::Value> = reaction_counts
        .iter()
        .map(|rc| {
            serde_json::json!({
                "emoji": rc.emoji,
                "count": rc.count,
                "me": my_reactions.contains(&rc.emoji),
            })
        })
        .collect();

    let reference = match (row.reference_message_id, row.reference_channel_id) {
        (Some(mid), Some(cid)) => Some(serde_json::json!({
            "message_id": mid,
            "channel_id": cid,
        })),
        _ => None,
    };

    serde_json::json!({
        "id": row.id,
        "channel_id": row.channel_id,
        "author_id": row.author_id,
        "author_username": row.author_username,
        "content": row.content,
        "message_type": row.message_type,
        "edited": row.edited,
        "edited_at": row.edited_at,
        "pinned": row.pinned,
        "embeds": row.embeds,
        "attachments": row.attachments,
        "mentions": row.mentions,
        "mention_roles": row.mention_roles,
        "mention_everyone": row.mention_everyone,
        "reference": reference,
        "thread_id": row.thread_id,
        "reactions": reactions_json,
        "created_at": row.created_at,
    })
}

fn message_row_to_json_with_reactions(
    row: &messages::MessageRow,
    reaction_counts: &[reactions::ReactionCount],
    my_reactions: &[String],
) -> serde_json::Value {
    let reactions_json: Vec<serde_json::Value> = reaction_counts
        .iter()
        .map(|rc| {
            serde_json::json!({
                "emoji": rc.emoji,
                "count": rc.count,
                "me": my_reactions.contains(&rc.emoji),
            })
        })
        .collect();

    let reference = match (row.reference_message_id, row.reference_channel_id) {
        (Some(mid), Some(cid)) => Some(serde_json::json!({
            "message_id": mid,
            "channel_id": cid,
        })),
        _ => None,
    };

    serde_json::json!({
        "id": row.id,
        "channel_id": row.channel_id,
        "author_id": row.author_id,
        "content": row.content,
        "message_type": row.message_type,
        "edited": row.edited,
        "edited_at": row.edited_at,
        "pinned": row.pinned,
        "embeds": row.embeds,
        "attachments": row.attachments,
        "mentions": row.mentions,
        "mention_roles": row.mention_roles,
        "mention_everyone": row.mention_everyone,
        "reference": reference,
        "thread_id": row.thread_id,
        "reactions": reactions_json,
        "created_at": row.created_at,
    })
}

/// Parse @<uuid> mentions from message content.
fn parse_mentions(content: &str) -> Vec<Uuid> {
    let mut mentions = Vec::new();
    for part in content.split_whitespace() {
        if let Some(id_str) = part.strip_prefix("<@").and_then(|s| s.strip_suffix('>'))
            && let Ok(id) = id_str.parse::<Uuid>()
                && !mentions.contains(&id) {
                    mentions.push(id);
                }
    }
    mentions
}

/// Spam detection: returns `true` when the same content has been sent more than
/// `max_messages` times within the last `window_secs` seconds by the same user
/// in the same channel.
///
/// Uses Redis INCR + EXPIRE.  If Redis is unavailable (lite mode or connection
/// error) the function silently returns `false` so message sending is never
/// blocked by a Redis outage.
async fn check_spam(
    redis: &Option<redis::aio::ConnectionManager>,
    user_id: Uuid,
    channel_id: Uuid,
    content: &str,
    window_secs: u64,
    max_messages: u64,
) -> bool {
    let Some(mut conn) = redis.clone() else {
        return false;
    };

    // Low-cost fingerprint \u2014 no extra dependencies required.
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    let hash = hasher.finish();

    let key = format!("msgdedup:{user_id}:{channel_id}:{hash:016x}");

    match nexus_db::redis_pool::incr_expire(&mut conn, &key, window_secs).await {
        Ok(count) => count > max_messages as i64,
        Err(_) => false, // Don't block messages on Redis failure
    }
}

async fn list_messages_from_scylla(
    state: &AppState,
    channel_id: Uuid,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<serde_json::Value>, String> {
    let session = connect_scylla(state).await?;
    let query = format!(
        "SELECT message_id, author_id, author_username, content, created_at_epoch \
         FROM {}.messages_by_channel WHERE channel_id = ? LIMIT ?",
        state.db.scylla_keyspace
    );

    let rows = session
        .query_unpaged(query, (channel_id.to_string(), limit as i32))
        .await
        .map_err(|e| e.to_string())?
        .into_rows_result()
        .map_err(|e| e.to_string())?;

    let mut scylla_rows = Vec::new();
    for row in rows
        .rows::<(String, String, String, String, i64)>()
        .map_err(|e| e.to_string())?
    {
        let (message_id, author_id, author_username, content, created_at_epoch) =
            row.map_err(|e| e.to_string())?;

        let Ok(parsed_message_id) = Uuid::parse_str(&message_id) else {
            continue;
        };
        let Ok(parsed_author_id) = Uuid::parse_str(&author_id) else {
            continue;
        };

        scylla_rows.push((
            parsed_message_id,
            parsed_author_id,
            author_username,
            content,
            created_at_epoch,
        ));
    }

    let message_ids: Vec<Uuid> = scylla_rows.iter().map(|(id, _, _, _, _)| *id).collect();
    let sql_meta_map = match messages::find_by_ids_map(&state.db.pool, &message_ids).await {
        Ok(rows) => {
            metrics::counter!(
                "nexus_scylla_read_total",
                "kind" => "sql_meta",
                "outcome" => "ok",
            )
            .increment(1);
            rows
        }
        Err(error) => {
            tracing::debug!(%channel_id, %error, "Scylla list read: SQL metadata batch lookup failed");
            metrics::counter!(
                "nexus_scylla_read_total",
                "kind" => "sql_meta",
                "outcome" => "error",
            )
            .increment(1);
            std::collections::HashMap::new()
        }
    };

    let reaction_counts_map = match reactions::get_reaction_counts_for_messages(
        &state.db.pool,
        &message_ids,
    )
    .await
    {
        Ok(rows) => {
            metrics::counter!(
                "nexus_scylla_read_total",
                "kind" => "reaction_counts",
                "outcome" => "ok",
            )
            .increment(1);
            rows
        }
        Err(error) => {
            tracing::debug!(%channel_id, %error, "Scylla list read: reaction counts batch lookup failed");
            metrics::counter!(
                "nexus_scylla_read_total",
                "kind" => "reaction_counts",
                "outcome" => "error",
            )
            .increment(1);
            std::collections::HashMap::new()
        }
    };

    let my_reactions_map = match reactions::get_user_reaction_emojis_for_messages(
        &state.db.pool,
        user_id,
        &message_ids,
    )
    .await
    {
        Ok(rows) => {
            metrics::counter!(
                "nexus_scylla_read_total",
                "kind" => "user_reactions",
                "outcome" => "ok",
            )
            .increment(1);
            rows
        }
        Err(error) => {
            tracing::debug!(%channel_id, %user_id, %error, "Scylla list read: user reactions batch lookup failed");
            metrics::counter!(
                "nexus_scylla_read_total",
                "kind" => "user_reactions",
                "outcome" => "error",
            )
            .increment(1);
            std::collections::HashMap::new()
        }
    };

    let mut result = Vec::with_capacity(scylla_rows.len());
    let mut hydrated_count: usize = 0;
    for (parsed_message_id, parsed_author_id, author_username, content, created_at_epoch) in
        scylla_rows
    {
        let reaction_counts = reaction_counts_map
            .get(&parsed_message_id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let my_reactions = my_reactions_map
            .get(&parsed_message_id)
            .map(|set| set.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();

        let sql_meta = sql_meta_map.get(&parsed_message_id);
        if sql_meta.is_some() {
            hydrated_count += 1;
        }

        let (
            message_type,
            edited,
            edited_at,
            pinned,
            embeds,
            attachments,
            mentions,
            mention_roles,
            mention_everyone,
            reference,
            thread_id,
        ) = if let Some(m) = sql_meta {
            (
                m.message_type,
                m.edited,
                m.edited_at,
                m.pinned,
                serde_json::to_value(m.embeds.clone()).unwrap_or_else(|_| serde_json::json!([])),
                serde_json::to_value(m.attachments.clone())
                    .unwrap_or_else(|_| serde_json::json!([])),
                serde_json::to_value(m.mentions.clone()).unwrap_or_else(|_| serde_json::json!([])),
                serde_json::to_value(m.mention_roles.clone())
                    .unwrap_or_else(|_| serde_json::json!([])),
                m.mention_everyone,
                match (m.reference_message_id, m.reference_channel_id) {
                    (Some(mid), Some(cid)) => Some(serde_json::json!({
                        "message_id": mid,
                        "channel_id": cid,
                    })),
                    _ => None,
                },
                m.thread_id,
            )
        } else {
            (
                0,
                false,
                None,
                false,
                serde_json::json!([]),
                serde_json::json!([]),
                serde_json::json!([]),
                serde_json::json!([]),
                false,
                None,
                None,
            )
        };

        let message_json = serde_json::json!({
            "id": parsed_message_id,
            "channel_id": channel_id,
            "author_id": parsed_author_id,
            "author_username": author_username,
            "content": content,
            "message_type": message_type,
            "edited": edited,
            "edited_at": edited_at,
            "pinned": pinned,
            "embeds": embeds,
            "attachments": attachments,
            "mentions": mentions,
            "mention_roles": mention_roles,
            "mention_everyone": mention_everyone,
            "reference": reference,
            "thread_id": thread_id,
            "reactions": reaction_counts.iter().map(|rc| {
                serde_json::json!({
                    "emoji": rc.emoji,
                    "count": rc.count,
                    "me": my_reactions.contains(&rc.emoji),
                })
            }).collect::<Vec<_>>(),
            "created_at": chrono::DateTime::from_timestamp(created_at_epoch, 0).unwrap_or_else(Utc::now),
        });

        result.push(message_json);
    }

    metrics::gauge!("nexus_scylla_metadata_hydration_ratio").set(if result.is_empty() {
        0.0
    } else {
        hydrated_count as f64 / result.len() as f64
    });

    Ok(result)
}

async fn get_message_from_scylla(
    state: &AppState,
    channel_id: Uuid,
    message_id: Uuid,
    user_id: Uuid,
) -> Result<Option<serde_json::Value>, String> {
    let session = connect_scylla(state).await?;

    let by_id_query = format!(
        "SELECT channel_id, created_at_epoch FROM {}.messages_by_id WHERE message_id = ?",
        state.db.scylla_keyspace
    );
    let id_rows = session
        .query_unpaged(by_id_query, (message_id.to_string(),))
        .await
        .map_err(|e| e.to_string())?
        .into_rows_result()
        .map_err(|e| e.to_string())?;

    let mut row_iter = id_rows.rows::<(String, i64)>().map_err(|e| e.to_string())?;
    let Some(next_row) = row_iter.next() else {
        return Ok(None);
    };
    let (row_channel_id, created_at_epoch) = next_row.map_err(|e| e.to_string())?;
    if row_channel_id != channel_id.to_string() {
        return Ok(None);
    }

    let by_channel_query = format!(
        "SELECT author_id, author_username, content \
         FROM {}.messages_by_channel WHERE channel_id = ? AND created_at_epoch = ? AND message_id = ?",
        state.db.scylla_keyspace
    );
    let channel_rows = session
        .query_unpaged(
            by_channel_query,
            (
                channel_id.to_string(),
                created_at_epoch,
                message_id.to_string(),
            ),
        )
        .await
        .map_err(|e| e.to_string())?
        .into_rows_result()
        .map_err(|e| e.to_string())?;
    let mut detail_iter = channel_rows
        .rows::<(String, String, String)>()
        .map_err(|e| e.to_string())?;
    let Some(detail_row) = detail_iter.next() else {
        return Ok(None);
    };
    let (author_id, author_username, content) = detail_row.map_err(|e| e.to_string())?;

    let parsed_author_id = Uuid::parse_str(&author_id).map_err(|e| e.to_string())?;
    let reaction_counts = match reactions::get_reaction_counts(&state.db.pool, message_id).await {
        Ok(rows) => {
            metrics::counter!(
                "nexus_scylla_read_total",
                "kind" => "reaction_counts_single",
                "outcome" => "ok",
            )
            .increment(1);
            rows
        }
        Err(error) => {
            tracing::debug!(%channel_id, %message_id, %error, "Scylla get_message: reaction count lookup failed");
            metrics::counter!(
                "nexus_scylla_read_total",
                "kind" => "reaction_counts_single",
                "outcome" => "error",
            )
            .increment(1);
            Vec::new()
        }
    };

    let my_reactions = match reactions::get_user_reaction_emojis_for_messages(
        &state.db.pool,
        user_id,
        &[message_id],
    )
    .await
    {
        Ok(map) => {
            metrics::counter!(
                "nexus_scylla_read_total",
                "kind" => "user_reactions_single",
                "outcome" => "ok",
            )
            .increment(1);
            map.get(&message_id).cloned().unwrap_or_default()
        }
        Err(error) => {
            tracing::debug!(%channel_id, %message_id, %user_id, %error, "Scylla get_message: user reaction lookup failed");
            metrics::counter!(
                "nexus_scylla_read_total",
                "kind" => "user_reactions_single",
                "outcome" => "error",
            )
            .increment(1);
            std::collections::HashSet::new()
        }
    };

    let sql_meta = match messages::find_by_id(&state.db.pool, message_id).await {
        Ok(row) => {
            metrics::counter!(
                "nexus_scylla_read_total",
                "kind" => "sql_meta_single",
                "outcome" => "ok",
            )
            .increment(1);
            row
        }
        Err(error) => {
            tracing::debug!(%channel_id, %message_id, %error, "Scylla get_message: SQL metadata lookup failed");
            metrics::counter!(
                "nexus_scylla_read_total",
                "kind" => "sql_meta_single",
                "outcome" => "error",
            )
            .increment(1);
            None
        }
    };

    let (
        message_type,
        edited,
        edited_at,
        pinned,
        embeds,
        attachments,
        mentions,
        mention_roles,
        mention_everyone,
        reference,
        thread_id,
    ) = if let Some(m) = sql_meta {
        (
            m.message_type,
            m.edited,
            m.edited_at,
            m.pinned,
            serde_json::to_value(m.embeds).unwrap_or_else(|_| serde_json::json!([])),
            serde_json::to_value(m.attachments).unwrap_or_else(|_| serde_json::json!([])),
            serde_json::to_value(m.mentions).unwrap_or_else(|_| serde_json::json!([])),
            serde_json::to_value(m.mention_roles).unwrap_or_else(|_| serde_json::json!([])),
            m.mention_everyone,
            match (m.reference_message_id, m.reference_channel_id) {
                (Some(mid), Some(cid)) => Some(serde_json::json!({
                    "message_id": mid,
                    "channel_id": cid,
                })),
                _ => None,
            },
            m.thread_id,
        )
    } else {
        (
            0,
            false,
            None,
            false,
            serde_json::json!([]),
            serde_json::json!([]),
            serde_json::json!([]),
            serde_json::json!([]),
            false,
            None,
            None,
        )
    };

    Ok(Some(serde_json::json!({
        "id": message_id,
        "channel_id": channel_id,
        "author_id": parsed_author_id,
        "author_username": author_username,
        "content": content,
        "message_type": message_type,
        "edited": edited,
        "edited_at": edited_at,
        "pinned": pinned,
        "embeds": embeds,
        "attachments": attachments,
        "mentions": mentions,
        "mention_roles": mention_roles,
        "mention_everyone": mention_everyone,
        "reference": reference,
        "thread_id": thread_id,
        "reactions": reaction_counts.iter().map(|rc| {
            serde_json::json!({
                "emoji": rc.emoji,
                "count": rc.count,
                "me": my_reactions.contains(&rc.emoji),
            })
        }).collect::<Vec<_>>(),
        "created_at": chrono::DateTime::from_timestamp(created_at_epoch, 0).unwrap_or_else(Utc::now),
    })))
}

async fn connect_scylla(state: &AppState) -> Result<Arc<scylla::Session>, String> {
    static SCYLLA_SESSION_CACHE: OnceLock<
        tokio::sync::Mutex<HashMap<String, Arc<scylla::Session>>>,
    > = OnceLock::new();

    let nodes: Vec<String> = state
        .db
        .scylla_nodes
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect();

    if nodes.is_empty() {
        return Err("no_scylla_nodes_configured".to_string());
    }

    let cache_key = format!("{}|{}", state.db.scylla_keyspace, nodes.join(","));
    let cache = SCYLLA_SESSION_CACHE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()));

    {
        let guard = cache.lock().await;
        if let Some(session) = guard.get(&cache_key) {
            return Ok(Arc::clone(session));
        }
    }

    let mut builder = SessionBuilder::new();
    for node in &nodes {
        builder = builder.known_node(node);
    }

    let session = Arc::new(builder.build().await.map_err(|e| e.to_string())?);
    {
        let mut guard = cache.lock().await;
        guard
            .entry(cache_key)
            .or_insert_with(|| Arc::clone(&session));
    }

    Ok(session)
}

fn scylla_read_strategy(state: &AppState) -> ScyllaReadStrategy {
    if !state.db.scylla_enabled {
        return ScyllaReadStrategy::Off;
    }

    match state.db.scylla_read_strategy.to_ascii_lowercase().as_str() {
        "off" => ScyllaReadStrategy::Off,
        "prefer" => ScyllaReadStrategy::Prefer,
        _ => ScyllaReadStrategy::Canary,
    }
}

// ============================================================
// Announcement channel: crosspost + follower management
// ============================================================

/// POST /api/v1/channels/:channel_id/messages/:message_id/crosspost
///
/// Publish a message from an announcement channel.  The message is
/// delivered to all channels that have followed this announcement channel
/// (via `channel_followers` table).  A message can only be published once.
async fn crosspost_message(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((channel_id, message_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<Json<serde_json::Value>> {
    // Verify the channel is an announcement channel
    let channel = channels::find_by_id(&state.db.pool, channel_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Channel".into(),
        })?;

    if !matches!(
        channel.channel_type,
        nexus_common::models::channel::ChannelType::Announcement
    ) {
        return Err(NexusError::Validation {
            message: "Channel is not an announcement channel".into(),
        });
    }

    // Only someone with SEND_MESSAGES (for the OP) or MANAGE_MESSAGES
    // may crosspost
    let server_id = channel.server_id.ok_or(NexusError::MissingPermission {
        permission: "SEND_MESSAGES".into(),
    })?;
    let server = servers::find_by_id(&state.db.pool, server_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Server".into(),
        })?;

    // Check that the caller is server owner or has SEND_MESSAGES
    if auth.user_id != server.owner_id {
        use nexus_db::repository::{members, roles};
        let member = members::find_member(&state.db.pool, auth.user_id, server_id)
            .await
            .map_err(|e| NexusError::Internal(e.into()))?
            .ok_or(NexusError::MissingPermission {
                permission: "SEND_MESSAGES or MANAGE_MESSAGES".into(),
            })?;

        let all_roles = roles::list_server_roles(&state.db.pool, server_id).await?;

        let base = all_roles
            .iter()
            .find(|r| r.is_default)
            .map(|r| nexus_common::permissions::Permissions::from_bits_truncate(r.permissions))
            .unwrap_or_else(nexus_common::permissions::Permissions::empty);

        let effective = all_roles
            .iter()
            .filter(|r| !r.is_default && member.roles.contains(&r.id))
            .map(|r| nexus_common::permissions::Permissions::from_bits_truncate(r.permissions))
            .fold(base, |acc, rp| acc | rp);

        if !effective.has(nexus_common::permissions::Permissions::SEND_MESSAGES)
            && !effective.has(nexus_common::permissions::Permissions::MANAGE_MESSAGES)
        {
            return Err(NexusError::MissingPermission {
                permission: "SEND_MESSAGES or MANAGE_MESSAGES".into(),
            });
        }
    }

    // Load the message and verify it belongs to this channel
    let msg = messages::find_by_id(&state.db.pool, message_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Message".into(),
        })?;

    if msg.channel_id != channel_id {
        return Err(NexusError::NotFound {
            resource: "Message".into(),
        });
    }

    // Mark the message as crossposted (flags bit 1)
    let current_flags = msg.flags;

    if current_flags & 1 != 0 {
        return Err(NexusError::Validation {
            message: "Message has already been published".into(),
        });
    }

    sqlx::query("UPDATE messages SET flags = flags | 1, updated_at = NOW() WHERE id = $1::uuid")
        .bind(message_id.to_string())
        .execute(&state.db.pool)
        .await?;

    // Fetch all follower channels and relay the message
    #[derive(sqlx::FromRow)]
    struct FollowerRow {
        target_channel_id: String,
    }

    let followers: Vec<FollowerRow> = sqlx::query_as(
        "SELECT target_channel_id::text FROM channel_followers \
         WHERE source_channel_id = $1::uuid",
    )
    .bind(channel_id.to_string())
    .fetch_all(&state.db.pool)
    .await
    .unwrap_or_default();

    let mut delivered_to = Vec::new();

    for follower in &followers {
        let target_channel_id: Uuid = match follower.target_channel_id.parse() {
            Ok(id) => id,
            Err(_) => continue,
        };

        // Insert a crosspost copy of the message in the target channel
        // Flag bit 2 = IS_CROSSPOST
        let relay_msg_id = snowflake::generate_id();
        let relay_result = sqlx::query(
            "INSERT INTO messages \
             (id, channel_id, author_id, content, flags, created_at, updated_at) \
             VALUES ($1::uuid, $2::uuid, $3::uuid, $4, 2, NOW(), NOW())",
        )
        .bind(relay_msg_id.to_string())
        .bind(target_channel_id.to_string())
        .bind(msg.author_id.to_string())
        .bind(&msg.content)
        .execute(&state.db.pool)
        .await;

        if relay_result.is_ok() {
            delivered_to.push(target_channel_id);

            let _ = state.gateway_tx.send(GatewayEvent {
                event_type: nexus_common::gateway_event::event_types::MESSAGE_CREATE.to_string(),
                data: serde_json::json!({
                    "id": relay_msg_id,
                    "channel_id": target_channel_id,
                    "author_id": msg.author_id,
                    "content": msg.content,
                    "flags": 2,
                    "crosspost_source": {
                        "channel_id": channel_id,
                        "message_id": message_id,
                    },
                }),
                server_id: None,
                channel_id: Some(target_channel_id),
                user_id: Some(auth.user_id),
            });
        }
    }

    Ok(Json(serde_json::json!({
        "message_id": message_id,
        "channel_id": channel_id,
        "flags": current_flags | 1,
        "delivered_to": delivered_to,
    })))
}

/// PUT /api/v1/channels/:channel_id/followers
///
/// Subscribe a target channel to this announcement channel.
/// Body: `{ "webhook_channel_id": "<uuid>" }` — the local channel that should
/// receive crossposted messages.
async fn add_channel_follower(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
    Json(body): Json<serde_json::Value>,
) -> NexusResult<Json<serde_json::Value>> {
    // Source must be an announcement channel
    let src_channel = channels::find_by_id(&state.db.pool, channel_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Channel".into(),
        })?;

    if !matches!(
        src_channel.channel_type,
        nexus_common::models::channel::ChannelType::Announcement
    ) {
        return Err(NexusError::Validation {
            message: "Can only follow announcement channels".into(),
        });
    }

    let target_channel_id: Uuid = body
        .get("webhook_channel_id")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .ok_or(NexusError::Validation {
            message: "webhook_channel_id is required".into(),
        })?;

    // Verify target channel exists
    let target_channel = channels::find_by_id(&state.db.pool, target_channel_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Target channel".into(),
        })?;

    // Caller must have MANAGE_WEBHOOKS in the target channel's server
    if let Some(target_server_id) = target_channel.server_id {
        let target_server = servers::find_by_id(&state.db.pool, target_server_id)
            .await?
            .ok_or(NexusError::NotFound {
                resource: "Server".into(),
            })?;

        if auth.user_id != target_server.owner_id {
            use nexus_db::repository::{members, roles};
            let member = members::find_member(&state.db.pool, auth.user_id, target_server_id)
                .await
                .map_err(|e| NexusError::Internal(e.into()))?
                .ok_or(NexusError::MissingPermission {
                    permission: "MANAGE_WEBHOOKS".into(),
                })?;

            let all_roles = roles::list_server_roles(&state.db.pool, target_server_id).await?;

            let base = all_roles
                .iter()
                .find(|r| r.is_default)
                .map(|r| nexus_common::permissions::Permissions::from_bits_truncate(r.permissions))
                .unwrap_or_else(nexus_common::permissions::Permissions::empty);

            let effective = all_roles
                .iter()
                .filter(|r| !r.is_default && member.roles.contains(&r.id))
                .map(|r| nexus_common::permissions::Permissions::from_bits_truncate(r.permissions))
                .fold(base, |acc, rp| acc | rp);

            if !effective.has(nexus_common::permissions::Permissions::MANAGE_WEBHOOKS) {
                return Err(NexusError::MissingPermission {
                    permission: "MANAGE_WEBHOOKS".into(),
                });
            }
        }
    }

    let follower_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO channel_followers \
         (id, source_channel_id, target_channel_id, target_guild_id) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid) \
         ON CONFLICT (source_channel_id, target_channel_id) DO NOTHING",
    )
    .bind(follower_id.to_string())
    .bind(channel_id.to_string())
    .bind(target_channel_id.to_string())
    .bind(
        target_channel
            .server_id
            .map(|u| u.to_string())
            .unwrap_or_default(),
    )
    .execute(&state.db.pool)
    .await?;

    Ok(Json(serde_json::json!({
        "source_channel_id": channel_id,
        "target_channel_id": target_channel_id,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CrossChannelReactionFixture {
        pool: sqlx::AnyPool,
        actor_id: Uuid,
        authorized_url_channel_id: Uuid,
        target_message_id: Uuid,
    }

    async fn cross_channel_reaction_fixture() -> CrossChannelReactionFixture {
        sqlx::any::install_default_drivers();
        let pool = sqlx::any::AnyPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect reaction route test database");
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .expect("enable test foreign keys");
        sqlx::migrate!("../nexus-db/migrations-lite")
            .run(&pool)
            .await
            .expect("migrate reaction route test database");

        let actor_id = Uuid::new_v4();
        let target_owner_id = Uuid::new_v4();
        let authorized_server_id = Uuid::new_v4();
        let target_server_id = Uuid::new_v4();
        let authorized_url_channel_id = Uuid::new_v4();
        let actual_target_channel_id = Uuid::new_v4();
        let target_message_id = Uuid::new_v4();

        for (id, username) in [
            (actor_id, "reaction-attacker"),
            (target_owner_id, "reaction-owner"),
        ] {
            sqlx::query("INSERT INTO users (id, username, password_hash) VALUES ($1, $2, 'test')")
                .bind(id.to_string())
                .bind(username)
                .execute(&pool)
                .await
                .expect("insert reaction route user");
        }
        for (id, name, owner_id) in [
            (authorized_server_id, "authorized-server", actor_id),
            (target_server_id, "target-server", target_owner_id),
        ] {
            sqlx::query("INSERT INTO servers (id, name, owner_id) VALUES ($1, $2, $3)")
                .bind(id.to_string())
                .bind(name)
                .bind(owner_id.to_string())
                .execute(&pool)
                .await
                .expect("insert reaction route server");
        }
        for (id, server_id, name) in [
            (
                authorized_url_channel_id,
                authorized_server_id,
                "authorized-channel",
            ),
            (actual_target_channel_id, target_server_id, "target-channel"),
        ] {
            sqlx::query(
                "INSERT INTO channels (id, server_id, channel_type, name) VALUES ($1, $2, 'text', $3)",
            )
            .bind(id.to_string())
            .bind(server_id.to_string())
            .bind(name)
            .execute(&pool)
            .await
            .expect("insert reaction route channel");
        }
        sqlx::query(
            "INSERT INTO messages (id, channel_id, author_id, content) VALUES ($1, $2, $3, 'target')",
        )
        .bind(target_message_id.to_string())
        .bind(actual_target_channel_id.to_string())
        .bind(target_owner_id.to_string())
        .execute(&pool)
        .await
        .expect("insert target message");
        for emoji in ["red", "blue"] {
            sqlx::query("INSERT INTO reactions (message_id, user_id, emoji) VALUES ($1, $2, $3)")
                .bind(target_message_id.to_string())
                .bind(target_owner_id.to_string())
                .bind(emoji)
                .execute(&pool)
                .await
                .expect("insert target reaction");
        }

        CrossChannelReactionFixture {
            pool,
            actor_id,
            authorized_url_channel_id,
            target_message_id,
        }
    }

    async fn reaction_count(pool: &sqlx::AnyPool, message_id: Uuid) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM reactions WHERE message_id = $1")
            .bind(message_id.to_string())
            .fetch_one(pool)
            .await
            .expect("count target reactions")
    }

    #[tokio::test]
    async fn remove_all_emoji_route_rejects_cross_channel_message() {
        let fixture = cross_channel_reaction_fixture().await;

        let error = remove_all_emoji_reactions_authorized(
            &fixture.pool,
            fixture.actor_id,
            fixture.authorized_url_channel_id,
            fixture.target_message_id,
            "red",
        )
        .await
        .expect_err("attacker-selected channel must not authorize another channel's message");

        assert_eq!(
            error.status_code(),
            axum::http::StatusCode::NOT_FOUND,
            "{error:?}"
        );
        assert_eq!(
            reaction_count(&fixture.pool, fixture.target_message_id).await,
            2
        );
    }

    #[tokio::test]
    async fn remove_all_reactions_route_rejects_cross_channel_message() {
        let fixture = cross_channel_reaction_fixture().await;

        let error = remove_all_reactions_authorized(
            &fixture.pool,
            fixture.actor_id,
            fixture.authorized_url_channel_id,
            fixture.target_message_id,
        )
        .await
        .expect_err("attacker-selected channel must not authorize another channel's message");

        assert_eq!(
            error.status_code(),
            axum::http::StatusCode::NOT_FOUND,
            "{error:?}"
        );
        assert_eq!(
            reaction_count(&fixture.pool, fixture.target_message_id).await,
            2
        );
    }

    // ── parse_mentions ────────────────────────────────────────────────────────

    #[test]
    fn empty_content_returns_no_mentions() {
        assert!(parse_mentions("").is_empty());
    }

    #[test]
    fn content_with_no_mentions_returns_empty() {
        assert!(parse_mentions("hello world, no mentions here").is_empty());
        assert!(parse_mentions("@plainname without angle brackets").is_empty());
    }

    #[test]
    fn single_valid_mention_is_parsed() {
        let uid = Uuid::new_v4();
        let content = format!("hey <@{uid}> how are you");
        let mentions = parse_mentions(&content);
        assert_eq!(mentions, vec![uid]);
    }

    #[test]
    fn multiple_different_mentions_are_all_parsed() {
        let u1 = Uuid::new_v4();
        let u2 = Uuid::new_v4();
        let content = format!("<@{u1}> and <@{u2}> both mentioned");
        let mentions = parse_mentions(&content);
        assert_eq!(mentions.len(), 2);
        assert!(mentions.contains(&u1));
        assert!(mentions.contains(&u2));
    }

    #[test]
    fn duplicate_mention_is_deduplicated() {
        let uid = Uuid::new_v4();
        let content = format!("<@{uid}> <@{uid}> mentioned twice");
        let mentions = parse_mentions(&content);
        assert_eq!(mentions.len(), 1, "duplicate mention must be deduplicated");
        assert_eq!(mentions[0], uid);
    }

    #[test]
    fn malformed_mention_missing_closing_angle_ignored() {
        let uid = Uuid::new_v4();
        let content = format!("<@{uid} no closing bracket");
        assert!(parse_mentions(&content).is_empty());
    }

    #[test]
    fn malformed_mention_missing_at_ignored() {
        let uid = Uuid::new_v4();
        let content = format!("<{uid}>");
        assert!(parse_mentions(&content).is_empty());
    }

    #[test]
    fn non_uuid_inside_mention_syntax_ignored() {
        assert!(parse_mentions("<@not-a-uuid>").is_empty());
        assert!(parse_mentions("<@12345>").is_empty());
        assert!(parse_mentions("<@>").is_empty());
    }

    #[test]
    fn mention_at_start_of_content_parsed() {
        let uid = Uuid::new_v4();
        let content = format!("<@{uid}>");
        let mentions = parse_mentions(&content);
        assert_eq!(mentions, vec![uid]);
    }

    #[test]
    fn mention_at_end_of_content_parsed() {
        let uid = Uuid::new_v4();
        let content = format!("hello <@{uid}>");
        let mentions = parse_mentions(&content);
        assert_eq!(mentions, vec![uid]);
    }

    #[test]
    fn mention_embedded_in_word_without_space_ignored() {
        // parse_mentions uses split_whitespace, so a mention glued to other
        // characters without spaces is treated as one token.
        // "<@uuid>foo" splits as one token, strip_prefix("<@") works but
        // strip_suffix('>') fails because the token doesn't end with '>'.
        let uid = Uuid::new_v4();
        let content = format!("<@{uid}>extra_text_no_space");
        // This should NOT extract the UUID because the token doesn't end with '>'
        assert!(
            parse_mentions(&content).is_empty(),
            "glued mention without trailing space must not parse"
        );
    }

    #[test]
    fn mixed_valid_and_invalid_mentions() {
        let uid = Uuid::new_v4();
        let content = format!("valid <@{uid}> invalid <@not-uuid> also <@12> end");
        let mentions = parse_mentions(&content);
        assert_eq!(
            mentions,
            vec![uid],
            "only valid UUID mentions should be extracted"
        );
    }
}
