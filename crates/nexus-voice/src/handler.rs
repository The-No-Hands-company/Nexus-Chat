//! Voice signaling WebSocket handler.
//!
//! Separate from the main gateway, this handles the WebRTC negotiation
//! flow for voice connections:
//!
//! 1. Client connects to /voice/ws
//! 2. Authenticates with JWT token
//! 3. Sends "Join" with channel_id
//! 4. Server creates SFU peer, sends SDP answer + ICE servers
//! 5. Client and server exchange ICE candidates
//! 6. WebRTC connection established — media flows via UDP
//! 7. Client sends "Leave" or disconnects → cleanup
//!
//! This is intentionally separate from the main gateway because:
//! - Voice connections have different lifecycle (join/leave vs persistent)
//! - SDP/ICE exchange is voice-specific
//! - Allows independent scaling of voice servers

use crate::sfu::{SfuCommand, SfuManager, SfuResponse};
use crate::state::{VoiceState, VoiceStateManager, VoiceStateUpdate};
use axum::{
    Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use nexus_common::gateway_event::GatewayEvent;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

/// Voice server state — shared across all voice connections.
#[derive(Clone)]
pub struct VoiceServerState {
    pub sfu: SfuManager,
    pub voice_state: VoiceStateManager,
    /// Broadcast sender to push voice events to the main gateway.
    pub gateway_tx: broadcast::Sender<GatewayEvent>,
    pub db: nexus_db::Database,
}

/// Voice signaling messages (client ↔ server).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", content = "d")]
pub enum VoiceSignal {
    // === Client → Server ===
    /// Authenticate with JWT token.
    /// Begin a voice session. Carries no credential — see `ws_handler`.
    Identify {
        #[serde(default)]
        token: Option<String>,
    },

    /// Join a voice channel.
    Join {
        channel_id: Uuid,
        server_id: Option<Uuid>,
    },

    /// Send SDP offer to establish WebRTC connection.
    Offer { sdp: String },

    /// Send ICE candidate.
    IceCandidate {
        candidate: String,
        sdp_mid: Option<String>,
        sdp_m_line_index: Option<u32>,
    },

    /// Update self mute/deaf/video state.
    StateUpdate {
        self_mute: Option<bool>,
        self_deaf: Option<bool>,
        self_video: Option<bool>,
        self_stream: Option<bool>,
    },

    /// Set speaking state (voice activity detection result).
    Speaking { speaking: bool },

    /// Leave voice channel.
    Leave,

    // === Server → Client ===
    /// Authentication successful.
    Ready { session_id: String },

    /// Joined voice channel — here's the current state.
    Joined {
        channel_id: Uuid,
        voice_states: Vec<serde_json::Value>,
        ice_servers: Vec<IceServerConfig>,
    },

    /// SDP answer from the SFU.
    Answer { sdp: String },

    /// ICE candidate from the server.
    ServerIceCandidate {
        candidate: String,
        sdp_mid: Option<String>,
        sdp_m_line_index: Option<u32>,
    },

    /// Another user's voice state changed (joined, left, mute, etc.).
    VoiceStateUpdate { state: serde_json::Value },

    /// Speaking state changed for a user.
    SpeakingUpdate { user_id: Uuid, speaking: bool },

    /// Server requests client-side WebRTC renegotiation after track changes.
    Renegotiate { reason: String },

    /// Error occurred.
    Error { code: u32, message: String },
}

/// ICE server configuration sent to clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceServerConfig {
    pub urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

impl IceServerConfig {
    /// Default STUN servers (free, public).
    /// In production, add TURN servers for NAT traversal.
    pub fn defaults() -> Vec<Self> {
        vec![
            Self {
                urls: vec![
                    "stun:stun.l.google.com:19302".into(),
                    "stun:stun1.l.google.com:19302".into(),
                ],
                username: None,
                credential: None,
            },
            Self {
                urls: vec!["stun:stun.cloudflare.com:3478".into()],
                username: None,
                credential: None,
            },
        ]
    }
}

/// Build the voice signaling WebSocket router.
pub fn build_router(state: VoiceServerState) -> Router {
    Router::new()
        .route("/voice", get(ws_handler))
        .with_state(Arc::new(state))
}

/// Who the proxy says is on the other end of this socket.
#[derive(Clone)]
struct VoiceIdentity {
    user_id: Uuid,
    username: String,
}

/// WebSocket upgrade handler.
///
/// Authentication happens on the HTTP upgrade, where the proxy's
/// `X-Nexus-Identity` header exists — an unauthenticated client never gets a
/// socket at all.
async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<VoiceServerState>>,
) -> Response {
    let config = nexus_common::config::get();
    let claims = match nexus_common::identity::verify_header(&headers, &config.server.name).await {
        Ok(claims) => claims,
        Err(e) => {
            tracing::debug!(error_kind = %nexus_common::logsafe::err_kind(&e), "Voice: rejected upgrade, no valid identity");
            return (axum::http::StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
    };

    let user_id = match nexus_db::repository::users::provision_from_identity(
        &state.db.pool,
        &claims.sub,
        &claims.username,
        Some(claims.email.as_str()),
    )
    .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error_kind = %nexus_common::logsafe::err_kind(&e), "Voice: failed to provision user from identity");
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "provisioning failed",
            )
                .into_response();
        }
    };

    let identity = VoiceIdentity {
        user_id,
        username: claims.username,
    };
    ws.on_upgrade(move |socket| handle_voice_connection(socket, state, identity))
}

/// Handle a single voice signaling WebSocket connection.
async fn handle_voice_connection(
    socket: WebSocket,
    state: Arc<VoiceServerState>,
    identity: VoiceIdentity,
) {
    let (mut sender, mut receiver) = socket.split();

    let session_id = Uuid::new_v4().to_string();
    let mut authenticated = false;
    let mut user_id: Option<Uuid> = None;
    let mut current_channel: Option<Uuid> = None;
    let mut peer_id: Option<Uuid> = None;

    tracing::debug!(session = %session_id, "Voice WebSocket connected");

    // Voice clients must authenticate within 10 seconds of connecting.
    const IDENTIFY_TIMEOUT_SECS: u64 = 10;
    const MAX_VOICE_FRAME_BYTES: usize = 32 * 1024; // 32 KiB — ample for any signaling op
    let identify_deadline =
        tokio::time::Instant::now() + tokio::time::Duration::from_secs(IDENTIFY_TIMEOUT_SECS);

    // Receive loop
    while let Some(Ok(msg)) = receiver.next().await {
        if !authenticated && tokio::time::Instant::now() > identify_deadline {
            tracing::warn!(session = %session_id, "Voice: Identify timeout — closing connection");
            break;
        }
        match msg {
            Message::Text(text) => {
                if text.len() > MAX_VOICE_FRAME_BYTES {
                    tracing::warn!(
                        session = %session_id,
                        bytes = text.len(),
                        "Voice: oversized signaling frame rejected"
                    );
                    continue;
                }
                let signal = match serde_json::from_str::<VoiceSignal>(&text) {
                    Ok(s) => s,
                    Err(e) => {
                        let err = VoiceSignal::Error {
                            code: 4000,
                            message: format!("Invalid message: {e}"),
                        };
                        let _ = sender
                            .send(Message::Text(
                                serde_json::to_string(&err).unwrap_or_default().into(),
                            ))
                            .await;
                        continue;
                    }
                };

                match signal {
                    VoiceSignal::Identify { token: _ } => {
                        // Established on the upgrade request; anything the
                        // client sends here is not a credential.
                        authenticated = true;
                        user_id = Some(identity.user_id);

                        let ready = VoiceSignal::Ready {
                            session_id: session_id.clone(),
                        };
                        send_signal(&mut sender, &ready).await;

                        tracing::info!(
                            session = %session_id,
                            "Voice client authenticated"
                        );
                    }

                    VoiceSignal::Join {
                        channel_id,
                        server_id,
                    } => {
                        if !authenticated {
                            send_error(&mut sender, 4003, "Not authenticated").await;
                            continue;
                        }
                        let uid = user_id.unwrap();

                        // If already in a channel, leave first
                        if let Some(old_channel) = current_channel.take() {
                            leave_channel(&state, uid, old_channel, peer_id.take()).await;
                        }

                        // Join voice state
                        let (voice_state, _old_channel) = state
                            .voice_state
                            .join(uid, channel_id, server_id, session_id.clone())
                            .await;

                        current_channel = Some(channel_id);

                        // Get current participants
                        let members = state.voice_state.get_channel_members(channel_id).await;
                        let voice_states: Vec<serde_json::Value> = members
                            .iter()
                            .map(|s| serde_json::to_value(s).unwrap_or_default())
                            .collect();

                        // Send Joined response
                        let joined = VoiceSignal::Joined {
                            channel_id,
                            voice_states,
                            ice_servers: IceServerConfig::defaults(),
                        };
                        send_signal(&mut sender, &joined).await;

                        // Broadcast VOICE_STATE_UPDATE to gateway
                        broadcast_voice_state(&state, &voice_state);

                        tracing::info!(
                            session = %session_id,
                            channel = %channel_id,
                            "User joined voice channel"
                        );
                    }

                    VoiceSignal::Offer { sdp } => {
                        if !authenticated || current_channel.is_none() {
                            send_error(&mut sender, 4003, "Not in a voice channel").await;
                            continue;
                        }
                        let uid = user_id.unwrap();
                        let channel_id = current_channel.unwrap();

                        // Renegotiation path: if this session already has a peer,
                        // remove it first so we can recreate a fresh SFU peer with
                        // the new SDP offer.
                        if let Some(existing_peer) = peer_id {
                            let room_tx = state.sfu.get_or_create_room(channel_id).await;
                            let _ = room_tx
                                .send(SfuCommand::RemovePeer {
                                    peer_id: existing_peer,
                                })
                                .await;
                            peer_id = None;
                        }

                        // Create a peer in the SFU room
                        let new_peer_id = Uuid::new_v4();
                        let room_tx = state.sfu.get_or_create_room(channel_id).await;

                        let (reply_tx, mut reply_rx) = mpsc::channel(1);
                        let cmd = SfuCommand::AddPeer {
                            peer_id: new_peer_id,
                            user_id: uid,
                            offer_sdp: sdp,
                            reply: reply_tx,
                        };

                        if room_tx.send(cmd).await.is_err() {
                            send_error(&mut sender, 5000, "SFU room unavailable").await;
                            continue;
                        }

                        // Wait for SDP answer
                        match reply_rx.recv().await {
                            Some(SfuResponse::Answer { sdp }) => {
                                peer_id = Some(new_peer_id);
                                let answer = VoiceSignal::Answer { sdp };
                                send_signal(&mut sender, &answer).await;

                                tracing::info!(
                                    session = %session_id,
                                    peer = %new_peer_id,
                                    "SDP answer sent to client"
                                );
                            }
                            Some(SfuResponse::Error(e)) => {
                                send_error(&mut sender, 5001, &e).await;
                            }
                            _ => {
                                send_error(&mut sender, 5002, "No response from SFU").await;
                            }
                        }
                    }

                    VoiceSignal::IceCandidate {
                        candidate,
                        sdp_mid: _,
                        sdp_m_line_index: _,
                    } => {
                        if let Some(pid) = peer_id
                            && let Some(channel_id) = current_channel
                        {
                            let room_tx = state.sfu.get_or_create_room(channel_id).await;
                            let _ = room_tx
                                .send(SfuCommand::IceCandidate {
                                    peer_id: pid,
                                    candidate,
                                })
                                .await;
                        }
                    }

                    VoiceSignal::StateUpdate {
                        self_mute,
                        self_deaf,
                        self_video,
                        self_stream,
                    } => {
                        if let Some(uid) = user_id {
                            let update = VoiceStateUpdate {
                                self_mute,
                                self_deaf,
                                self_video,
                                self_stream,
                            };
                            if let Some(new_state) =
                                state.voice_state.update_self_state(uid, &update).await
                            {
                                broadcast_voice_state(&state, &new_state);

                                // Inform SFU about local media toggles so room stats and
                                // forwarding policy can adapt.
                                if let (Some(pid), Some(channel_id)) = (peer_id, current_channel) {
                                    let room_tx = state.sfu.get_or_create_room(channel_id).await;
                                    let _ = room_tx
                                        .send(SfuCommand::UpdateMedia {
                                            peer_id: pid,
                                            audio_enabled: self_mute.map(|m| !m),
                                            video_enabled: self_video,
                                        })
                                        .await;
                                }

                                // Video/stream toggles generally require renegotiation on
                                // browser/mobile clients to refresh transceivers.
                                if self_video.is_some() || self_stream.is_some() {
                                    send_signal(
                                        &mut sender,
                                        &VoiceSignal::Renegotiate {
                                            reason: "media_state_changed".into(),
                                        },
                                    )
                                    .await;
                                }
                            }
                        }
                    }

                    VoiceSignal::Speaking { speaking } => {
                        if let Some(uid) = user_id
                            && let Some(new_state) =
                                state.voice_state.set_speaking(uid, speaking).await
                        {
                            // Broadcast speaking event to the channel
                            if let Some(channel_id) = current_channel {
                                let _ = state.gateway_tx.send(GatewayEvent {
                                    event_type: "VOICE_SPEAKING".into(),
                                    data: serde_json::json!({
                                        "user_id": uid,
                                        "channel_id": channel_id,
                                        "speaking": speaking,
                                    }),
                                    server_id: new_state.server_id,
                                    channel_id: Some(channel_id),
                                    user_id: Some(uid),
                                });
                            }
                        }
                    }

                    VoiceSignal::Leave => {
                        if let Some(uid) = user_id
                            && let Some(channel_id) = current_channel.take()
                        {
                            leave_channel(&state, uid, channel_id, peer_id.take()).await;

                            tracing::info!(
                                session = %session_id,
                                channel = %channel_id,
                                "User left voice channel"
                            );
                        }
                    }

                    // Server → Client messages should not be received from client
                    _ => {
                        send_error(&mut sender, 4000, "Invalid opcode").await;
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    // Cleanup on disconnect
    if let Some(uid) = user_id
        && let Some(channel_id) = current_channel
    {
        leave_channel(&state, uid, channel_id, peer_id).await;
    }

    tracing::info!(session = %session_id, "Voice WebSocket disconnected");
}

/// Leave a voice channel — remove from state, SFU, and broadcast.
async fn leave_channel(
    state: &VoiceServerState,
    user_id: Uuid,
    channel_id: Uuid,
    peer_id: Option<Uuid>,
) {
    // Remove from voice state
    let old_state = state.voice_state.get_user_state(user_id).await;
    state.voice_state.leave(user_id).await;

    // Remove from SFU
    if let Some(pid) = peer_id {
        let room_tx = state.sfu.get_or_create_room(channel_id).await;
        let _ = room_tx.send(SfuCommand::RemovePeer { peer_id: pid }).await;
    }

    // Broadcast leave event
    if let Some(vs) = old_state {
        let _ = state.gateway_tx.send(GatewayEvent {
            event_type: "VOICE_STATE_UPDATE".into(),
            data: serde_json::json!({
                "user_id": user_id,
                "channel_id": null,
                "server_id": vs.server_id,
                "session_id": vs.session_id,
                // null channel_id means the user left voice
            }),
            server_id: vs.server_id,
            channel_id: Some(channel_id),
            user_id: Some(user_id),
        });
    }
}

/// Broadcast a voice state update through the gateway.
fn broadcast_voice_state(state: &VoiceServerState, voice_state: &VoiceState) {
    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: "VOICE_STATE_UPDATE".into(),
        data: serde_json::to_value(voice_state).unwrap_or_default(),
        server_id: voice_state.server_id,
        channel_id: Some(voice_state.channel_id),
        user_id: Some(voice_state.user_id),
    });
}

/// Send a voice signal to the client.
async fn send_signal(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    signal: &VoiceSignal,
) {
    if let Ok(json) = serde_json::to_string(signal) {
        let _ = sender.send(Message::Text(json.into())).await;
    }
}

/// Send an error to the client.
async fn send_error(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    code: u32,
    message: &str,
) {
    let err = VoiceSignal::Error {
        code,
        message: message.to_string(),
    };
    send_signal(sender, &err).await;
}
