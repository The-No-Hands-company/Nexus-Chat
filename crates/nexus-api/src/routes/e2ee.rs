//! Encrypted DM & channel messaging routes.
//!
//! POST   /channels/:id/encrypted-messages          — Send an encrypted message
//! GET    /channels/:id/encrypted-messages          — Fetch encrypted history
//! GET    /channels/:id/encrypted-messages/:msg_id  — Single message
//! PUT    /channels/:id/e2ee                        — Enable E2EE on a channel
//! GET    /channels/:id/e2ee                        — Get channel E2EE config

use axum::http::HeaderMap;
use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    middleware,
    routing::get,
};
use nexus_common::{
    error::{NexusError, NexusResult},
    models::crypto::{
        E2eeChannel, EnableE2eeRequest, EncryptedMessage, SendEncryptedMessageRequest,
    },
};
use nexus_db::repository::keystore;
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    AppState,
    middleware::{AuthContext, check_rate_limit_with_fallback, extract_client_tag},
};
use nexus_common::gateway_event::GatewayEvent;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/channels/{channel_id}/encrypted-messages",
            get(list_encrypted_messages).post(send_encrypted_message),
        )
        .route(
            "/channels/{channel_id}/e2ee",
            get(get_e2ee_config).put(enable_e2ee),
        )
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ))
}

#[derive(Deserialize)]
struct MessagesQuery {
    before_sequence: Option<i64>,
    limit: Option<i64>,
}

// ============================================================
// GET /channels/:channel_id/encrypted-messages
// ============================================================

async fn list_encrypted_messages(
    Extension(_auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
    Query(params): Query<MessagesQuery>,
) -> NexusResult<Json<Vec<EncryptedMessage>>> {
    let limit = params.limit.unwrap_or(50).min(100);
    let msgs = keystore::list_encrypted_messages(
        &state.db.pool,
        channel_id,
        params.before_sequence,
        limit,
    )
    .await
    .map_err(NexusError::Internal)?;
    Ok(Json(msgs))
}

// ============================================================
// POST /channels/:channel_id/encrypted-messages
// ============================================================

async fn send_encrypted_message(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(channel_id): Path<Uuid>,
    Json(body): Json<SendEncryptedMessageRequest>,
) -> NexusResult<Json<EncryptedMessage>> {
    // ── Rate limiting: 20 encrypted messages per user per minute ───────────
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:e2ee:user:{}", auth.user_id),
        20, // 20 messages
        60, // per minute
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:e2ee:tag:{tag}"),
        50, // 50 per IP
        60,
    )
    .await?;

    // Verify ciphertext_map is a JSON object
    if !body.ciphertext_map.is_object() {
        return Err(NexusError::Validation {
            message: "ciphertext_map must be a JSON object".into(),
        });
    }

    let recipient_map = body
        .ciphertext_map
        .as_object()
        .ok_or(NexusError::Validation {
            message: "ciphertext_map must be a JSON object".into(),
        })?;

    // Require at least one recipient
    if recipient_map.is_empty() {
        return Err(NexusError::Validation {
            message: "ciphertext_map must contain at least one recipient".into(),
        });
    }

    // Keep request size bounded to avoid abuse with huge per-recipient maps.
    if recipient_map.len() > 4096 {
        return Err(NexusError::Validation {
            message: "ciphertext_map exceeds recipient limit (max 4096)".into(),
        });
    }

    // The sender's device_id must be among the keys (they encrypt for themselves too)
    // We derive sender_device_id from a request header or from the first key that
    // matches auth.user_id's devices. For simplicity, clients pass it as a query param
    // or we require it embedded. Here we accept it from the ciphertext_map structure —
    // the sender MUST include their own device in the map.
    // We pick the first device registered to this user as the "sender device".
    let devices = nexus_db::repository::keystore::list_devices(&state.db.pool, auth.user_id)
        .await
        .map_err(NexusError::Internal)?;

    let sender_device = devices.into_iter().next().ok_or(NexusError::Validation {
        message: "No device registered for sender. Register a device before sending E2EE messages."
            .into(),
    })?;

    // Ensure sender includes their own device in recipient map so local replay
    // and multi-device sync keep a canonical encrypted copy.
    if !recipient_map.contains_key(&sender_device.id.to_string()) {
        return Err(NexusError::Validation {
            message: "ciphertext_map must include sender's device_id as a recipient".into(),
        });
    }

    // Validate recipient IDs + envelope shape before persisting.
    for (device_id_str, envelope) in recipient_map {
        let recipient_device_id =
            Uuid::parse_str(device_id_str).map_err(|_| NexusError::Validation {
                message: format!("Invalid recipient device UUID: {device_id_str}"),
            })?;

        let Some(envelope_obj) = envelope.as_object() else {
            return Err(NexusError::Validation {
                message: format!(
                    "ciphertext envelope for device {device_id_str} must be an object"
                ),
            });
        };

        let msg_type =
            envelope_obj
                .get("type")
                .and_then(|v| v.as_u64())
                .ok_or(NexusError::Validation {
                    message: format!(
                        "ciphertext envelope for device {device_id_str} must include numeric 'type'"
                    ),
                })?;

        if msg_type != 1 && msg_type != 2 {
            return Err(NexusError::Validation {
                message: format!(
                    "ciphertext envelope type for device {device_id_str} must be 1 or 2"
                ),
            });
        }

        let body_b64 =
            envelope_obj
                .get("body")
                .and_then(|v| v.as_str())
                .ok_or(NexusError::Validation {
                    message: format!(
                        "ciphertext envelope for device {device_id_str} must include string 'body'"
                    ),
                })?;

        if body_b64.is_empty() {
            return Err(NexusError::Validation {
                message: format!("ciphertext body for device {device_id_str} cannot be empty"),
            });
        }

        if body_b64.len() > 1_000_000 {
            return Err(NexusError::Validation {
                message: format!("ciphertext body for device {device_id_str} exceeds max length"),
            });
        }

        let device_exists = keystore::find_device(&state.db.pool, recipient_device_id)
            .await
            .map_err(NexusError::Internal)?
            .is_some();

        if !device_exists {
            return Err(NexusError::Validation {
                message: format!("Unknown recipient device_id: {device_id_str}"),
            });
        }
    }

    let msg = keystore::store_encrypted_message(
        &state.db.pool,
        channel_id,
        auth.user_id,
        sender_device.id,
        &body.ciphertext_map,
        body.attachment_meta.as_ref(),
        body.client_ts,
        body.sender_ratchet_step,
    )
    .await
    .map_err(NexusError::Internal)?;

    // Broadcast to gateway (clients receive the ciphertext_map and decrypt locally)
    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: "ENCRYPTED_MESSAGE_CREATE".into(),
        data: serde_json::json!({
            "id": msg.id,
            "channel_id": msg.channel_id,
            "sender_id": msg.sender_id,
            "sender_device_id": msg.sender_device_id,
            "ciphertext_map": msg.ciphertext_map,
            "attachment_meta": msg.attachment_meta,
            "sequence": msg.sequence,
            "client_ts": msg.client_ts,
            "created_at": msg.created_at,
        }),
        server_id: None,
        channel_id: Some(channel_id),
        user_id: Some(auth.user_id),
    });

    // ── Ratchet step advancement ───────────────────────────────────────────────
    //
    // For each recipient device in the ciphertext map, advance the server-side
    // ratchet_step counter.  This counter is a server-maintained monotonic
    // integer that mirrors the client's Double Ratchet step; it is not the
    // ratchet state itself (which remains opaque on the client side).
    //
    // Signal Protocol message types:
    //   type 1 = PreKeySignalMessage  — X3DH session establishment.
    //            The server does NOT create a session row here.  A session row
    //            with the encrypted ratchet state blob will be created by the
    //            client via PUT /devices/:id/sessions/:remote after completing
    //            the X3DH calculation locally.
    //
    //   type 2 = SignalMessage        — in-session ratchet step.
    //            Increment the session's ratchet_step counter so recipients can
    //            detect gaps (out-of-order or dropped messages).  If no session
    //            row exists we log a warning (client sent without establishing a
    //            session first) but still store the message so it can be re-read
    //            if the client recovers.
    if let Some(map) = body.ciphertext_map.as_object() {
        for (device_id_str, envelope) in map {
            if let Ok(recipient_device_id) = Uuid::parse_str(device_id_str) {
                let msg_type = envelope.get("type").and_then(|v| v.as_u64()).unwrap_or(2);

                if msg_type == 2 {
                    // Validate a session exists before incrementing.
                    let exists = keystore::session_exists(
                        &state.db.pool,
                        sender_device.id,
                        recipient_device_id,
                    )
                    .await
                    .unwrap_or(false);

                    if exists {
                        let _ = keystore::increment_ratchet_step(
                            &state.db.pool,
                            sender_device.id,
                            recipient_device_id,
                        )
                        .await;
                    } else {
                        tracing::warn!(
                            sender_device = %sender_device.id,
                            recipient_device = %recipient_device_id,
                            "SignalMessage sent without an established session — \
                             client should call PUT /sessions before sending type=2 messages"
                        );
                    }
                }
                // type=1 (PreKeySignalMessage): no server-side session action.
                // The client will PUT the encrypted session state blob once X3DH
                // completes, establishing the session row.
            }
        }
    }

    Ok(Json(msg))
}

// ============================================================
// GET /channels/:channel_id/e2ee
// ============================================================

async fn get_e2ee_config(
    Extension(_auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
) -> NexusResult<Json<Option<E2eeChannel>>> {
    let config = keystore::get_e2ee_channel(&state.db.pool, channel_id)
        .await
        .map_err(NexusError::Internal)?;
    Ok(Json(config))
}

// ============================================================
// PUT /channels/:channel_id/e2ee — Enable E2EE
// ============================================================

async fn enable_e2ee(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
    Json(body): Json<EnableE2eeRequest>,
) -> NexusResult<Json<E2eeChannel>> {
    let rotation_secs = body.rotation_interval_secs.unwrap_or(604_800); // 7 days

    if rotation_secs < 3600 {
        return Err(NexusError::Validation {
            message: "rotation_interval_secs must be at least 3600 (1 hour)".into(),
        });
    }

    let config =
        keystore::enable_e2ee_channel(&state.db.pool, channel_id, auth.user_id, rotation_secs)
            .await
            .map_err(NexusError::Internal)?;

    // Notify channel members that E2EE is now active
    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: "CHANNEL_E2EE_ENABLED".into(),
        data: serde_json::json!({
            "channel_id": channel_id,
            "enabled_by": auth.user_id,
            "rotation_interval_secs": config.rotation_interval_secs,
        }),
        server_id: None,
        channel_id: Some(channel_id),
        user_id: Some(auth.user_id),
    });

    Ok(Json(config))
}
