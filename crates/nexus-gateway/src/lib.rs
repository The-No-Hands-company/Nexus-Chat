//! # nexus-gateway
//!
//! Real-time WebSocket gateway for Nexus. Handles:
//! - Client connections with authentication
//! - Event dispatch (messages, presence, typing, etc.)
//! - Heartbeat/keepalive
//! - Session resume on reconnect
//!
//! Protocol inspired by Discord's Gateway but cleaner:
//! - Opcodes are named, not numbered
//! - Events are typed and documented
//! - No hidden rate limits

#![allow(clippy::pedantic)]

pub mod events;
pub mod session;

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
use nexus_db::repository::{bots, channels, members, read_states, servers};
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use session::SessionManager;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{Mutex, RwLock, broadcast};

/// Gateway state.
#[derive(Clone)]
pub struct GatewayState {
    /// Local broadcast channel for dispatching events to all connected clients.
    /// Multi-node fanout is bridged via Redis pub/sub when configured.
    pub broadcast: broadcast::Sender<GatewayEvent>,
    pub db: nexus_db::Database,
    pub sessions: Arc<SessionManager>,
    pub instance_id: String,
}

impl GatewayState {
    pub fn new(db: nexus_db::Database) -> Self {
        let (broadcast, _) = broadcast::channel(10_000);
        Self {
            broadcast,
            db,
            sessions: Arc::new(SessionManager::new()),
            instance_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    /// Create a GatewayState using an externally-created broadcast sender.
    /// This allows the API server to share the same broadcast channel.
    pub fn with_broadcast(
        db: nexus_db::Database,
        broadcast: broadcast::Sender<GatewayEvent>,
    ) -> Self {
        Self {
            broadcast,
            db,
            sessions: Arc::new(SessionManager::new()),
            instance_id: uuid::Uuid::new_v4().to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RedisGatewayEnvelope {
    origin: String,
    hash: String,
    event: GatewayEvent,
}

/// Gateway opcodes — what the client and server send to each other.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", content = "d")]
pub enum GatewayMessage {
    /// Client → Server: begin a user session.
    ///
    /// Carries no credential. The user was already authenticated by the
    /// ecosystem proxy, which put a signed identity token on the upgrade
    /// request; this server verified it before the socket existed. `token` is
    /// accepted and ignored so older clients still parse.
    Identify {
        #[serde(default)]
        token: Option<String>,
    },

    /// Client → Server: Bot-specific identify.
    ///
    /// The `token` field must be the full `"Bot <raw-token>"` string (same
    /// format as the REST `Authorization` header).  `intents` is a bitfield
    /// declaring which event categories the bot wants to receive; passing
    /// `0xFFFFFFFF` (all bits set) subscribes to every intent.
    BotIdentify { token: String, intents: u32 },

    /// Server → Client: Connection accepted, here's your session info
    Ready {
        session_id: String,
        user: serde_json::Value,
        servers: Vec<serde_json::Value>,
    },

    /// Bidirectional: Keepalive ping/pong
    Heartbeat { timestamp: i64 },

    /// Server → Client: Heartbeat acknowledged
    HeartbeatAck { timestamp: i64 },

    /// Client → Server: Resume a disconnected session
    Resume {
        session_id: String,
        token: String,
        sequence: u64,
    },

    /// Server → Client: An event occurred
    Dispatch {
        event: String,
        data: serde_json::Value,
        sequence: u64,
    },

    /// Server → Client: Reconnect requested (server restarting, etc.)
    Reconnect,

    /// Server → Client: Session invalidated, must re-identify
    InvalidSession,

    /// Client → Server: Request presence update
    PresenceUpdate {
        status: String,
        custom_status: Option<String>,
    },

    /// Client → Server: Typing indicator
    TypingStart { channel_id: String },

    /// Client → Server: Join voice channel
    VoiceStateUpdate {
        server_id: Option<String>,
        channel_id: Option<String>,
        self_mute: bool,
        self_deaf: bool,
    },
}

// GatewayEvent is imported at the top of the file — re-export it here
// so consumers (nexus-server) can use `nexus_gateway::GatewayEvent`

/// Build the gateway WebSocket router.
pub fn build_router(state: GatewayState) -> Router {
    setup_redis_fanout_bridge(&state);
    Router::new()
        .route("/gateway", get(ws_handler))
        .with_state(Arc::new(state))
}

fn setup_redis_fanout_bridge(state: &GatewayState) {
    let redis_url = std::env::var("NEXUS__REDIS__URL")
        .ok()
        .or_else(|| std::env::var("REDIS_URL").ok());
    let Some(redis_url) = redis_url else {
        tracing::info!("Gateway Redis fanout disabled (no REDIS_URL configured)");
        return;
    };

    let channel = std::env::var("NEXUS_GATEWAY_REDIS_CHANNEL")
        .unwrap_or_else(|_| "nexus:gateway:events".to_string());
    let origin = state.instance_id.clone();
    let broadcast = state.broadcast.clone();
    let seen_hashes: Arc<Mutex<HashMap<String, std::time::Instant>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Subscriber: Redis -> local broadcast
    {
        let redis_url = redis_url.clone();
        let channel = channel.clone();
        let origin = origin.clone();
        let seen = seen_hashes.clone();
        let broadcast = broadcast.clone();

        tokio::spawn(async move {
            let client = match redis::Client::open(redis_url.as_str()) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "gateway redis fanout: invalid redis url");
                    return;
                }
            };

            loop {
                match client.get_async_pubsub().await {
                    Ok(mut pubsub) => {
                        if let Err(e) = pubsub.subscribe(&channel).await {
                            tracing::warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "gateway redis fanout: subscribe failed");
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            continue;
                        }

                        tracing::info!(channel = %channel, "gateway redis fanout subscriber ready");
                        let mut stream = pubsub.on_message();
                        let mut recv_count: u64 = 0;

                        while let Some(msg) = stream.next().await {
                            let payload: Result<String, _> = msg.get_payload();
                            let Ok(payload) = payload else { continue };
                            let Ok(envelope) =
                                serde_json::from_str::<RedisGatewayEnvelope>(&payload)
                            else {
                                continue;
                            };
                            if envelope.origin == origin {
                                continue;
                            }

                            {
                                let mut guard = seen.lock().await;
                                guard.insert(envelope.hash.clone(), std::time::Instant::now());
                                let now = std::time::Instant::now();
                                guard.retain(|_, ts| {
                                    now.duration_since(*ts) < Duration::from_secs(120)
                                });
                            }

                            let _ = broadcast.send(envelope.event);
                            recv_count += 1;
                            if recv_count.is_multiple_of(500) {
                                tracing::debug!(count = recv_count, channel = %channel, "gateway redis fanout subscriber processed events");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "gateway redis fanout: pubsub connect failed");
                    }
                }

                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        });
    }

    // Publisher: local broadcast -> Redis
    {
        let redis_url = redis_url.clone();
        let channel = channel.clone();
        let origin = origin.clone();
        let seen = seen_hashes.clone();
        let mut rx = broadcast.subscribe();

        tokio::spawn(async move {
            let client = match redis::Client::open(redis_url.as_str()) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "gateway redis fanout publisher: invalid redis url");
                    return;
                }
            };

            loop {
                let evt = match rx.recv().await {
                    Ok(e) => e,
                    Err(_) => continue,
                };

                let hash = {
                    let json = match serde_json::to_string(&evt) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    use sha2::{Digest, Sha256};
                    let mut hasher = Sha256::new();
                    hasher.update(json.as_bytes());
                    format!("{:x}", hasher.finalize())
                };

                {
                    let mut guard = seen.lock().await;
                    if guard.remove(&hash).is_some() {
                        continue;
                    }
                    let now = std::time::Instant::now();
                    guard.retain(|_, ts| now.duration_since(*ts) < Duration::from_secs(120));
                }

                let envelope = RedisGatewayEnvelope {
                    origin: origin.clone(),
                    hash,
                    event: evt,
                };

                let payload = match serde_json::to_string(&envelope) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                match client.get_multiplexed_async_connection().await {
                    Ok(mut conn) => {
                        let publish_res: redis::RedisResult<i64> =
                            conn.publish(&channel, payload).await;
                        if let Err(e) = publish_res {
                            tracing::warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "gateway redis fanout: publish failed");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "gateway redis fanout: publisher connect failed");
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        });
    }
}

/// Who the proxy says is on the other end of this socket.
#[derive(Clone)]
struct GatewayIdentity {
    user_id: uuid::Uuid,
    username: String,
}

/// WebSocket upgrade handler.
///
/// Authentication happens here, on the HTTP upgrade, not inside the socket.
/// That is the only point where the proxy's `X-Nexus-Identity` header exists —
/// and doing it here means an unauthenticated client never gets a WebSocket at
/// all, rather than one it can hold open while trying things.
async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Response {
    let config = nexus_common::config::get();
    let claims = match nexus_common::identity::verify_header(&headers, &config.server.name).await {
        Ok(claims) => claims,
        Err(e) => {
            tracing::debug!(error_kind = %nexus_common::logsafe::err_kind(&e), "Gateway: rejected upgrade, no valid identity");
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
            tracing::error!(error_kind = %nexus_common::logsafe::err_kind(&e), "Gateway: failed to provision user from identity");
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "provisioning failed",
            )
                .into_response();
        }
    };

    let identity = GatewayIdentity {
        user_id,
        username: claims.username,
    };
    ws.on_upgrade(move |socket| handle_connection(socket, state, identity))
}

/// Handle a single WebSocket connection.
async fn handle_connection(socket: WebSocket, state: Arc<GatewayState>, identity: GatewayIdentity) {
    let (mut sender, mut receiver) = socket.split();

    let session_id = uuid::Uuid::new_v4().to_string();

    // Direct-send channel: receive loop → sender task (for Ready, HeartbeatAck, etc.)
    let (direct_tx, mut direct_rx) = tokio::sync::mpsc::channel::<serde_json::Value>(64);

    // Shared mutable state accessed by both the sender task and the receive loop
    let subscribed: Arc<RwLock<Vec<uuid::Uuid>>> = Arc::new(RwLock::new(Vec::new()));
    let authed_user_id: Arc<RwLock<Option<uuid::Uuid>>> = Arc::new(RwLock::new(None));

    // Subscribe to broadcast BEFORE spawning tasks so we don't miss events
    let mut broadcast_rx = state.broadcast.subscribe();

    // Send Hello immediately to prompt the client to Identify
    let hello = serde_json::json!({"op": "Hello", "d": {"heartbeat_interval": 45000}});
    if sender
        .send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
        .await
        .is_err()
    {
        return;
    }

    // ── Sender task ──────────────────────────────────────────────────────────
    // Merges broadcast events (filtered to this user's servers) and direct
    // messages (Ready, HeartbeatAck) onto the single WebSocket sender.
    let subscribed_clone = subscribed.clone();
    let uid_clone = authed_user_id.clone();

    let send_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                Ok(event) = broadcast_rx.recv() => {
                    // Only forward events after the client has identified
                    let uid = *uid_clone.read().await;
                    let Some(uid) = uid else { continue };

                    let subs = subscribed_clone.read().await;
                    let forward = match event.server_id {
                        Some(sid) => subs.contains(&sid),
                        None => {
                            // DM / targeted events — forward if addressed to this user
                            event.user_id == Some(uid)
                        }
                    };
                    drop(subs);

                    if !forward { continue; }

                    let wire = serde_json::json!({
                        "op": "Dispatch",
                        "d": {
                            "event": event.event_type,
                            "data": event.data,
                        }
                    });
                    if sender
                        .send(Message::Text(serde_json::to_string(&wire).unwrap().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Some(direct) = direct_rx.recv() => {
                    if sender
                        .send(Message::Text(serde_json::to_string(&direct).unwrap().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                else => break,
            }
        }
    });

    // ── Receive loop ─────────────────────────────────────────────────────────
    let mut authenticated = false;
    let mut user_id: Option<uuid::Uuid> = None;

    // Clients must send Identify within 10 seconds of receiving Hello.
    // Unauthenticated connections that stall indefinitely would accumulate,
    // exhausting file descriptors and memory.
    const IDENTIFY_TIMEOUT_SECS: u64 = 10;
    const MAX_FRAME_BYTES: usize = 64 * 1024; // 64 KiB — enough for any valid op

    let identify_deadline =
        tokio::time::Instant::now() + tokio::time::Duration::from_secs(IDENTIFY_TIMEOUT_SECS);

    while let Some(Ok(msg)) = receiver.next().await {
        // Enforce Identify timeout: drop unauthenticated connections that
        // haven't sent Identify within the deadline.
        if !authenticated && tokio::time::Instant::now() > identify_deadline {
            tracing::warn!(session = %session_id, "Gateway: Identify timeout — closing unauthenticated connection");
            break;
        }

        match msg {
            Message::Text(text) => {
                // Reject oversized frames before deserialization to prevent
                // memory exhaustion and ReDoS on malicious JSON.
                if text.len() > MAX_FRAME_BYTES {
                    tracing::warn!(
                        session = %session_id,
                        bytes = text.len(),
                        "Gateway: oversized frame rejected"
                    );
                    continue;
                }
                let Ok(gateway_msg) = serde_json::from_str::<GatewayMessage>(&text) else {
                    continue;
                };
                match gateway_msg {
                    GatewayMessage::Identify { token: _ } => {
                        // The identity was established on the upgrade request.
                        // Whatever the client sent here is not a credential and
                        // is deliberately not consulted.
                        {
                            {
                                let uid = identity.user_id;
                                let username = identity.username.clone();

                                // Reject suspended or disabled accounts before doing anything else.
                                let account_ok = match nexus_db::repository::users::find_by_id(
                                    &state.db.pool,
                                    uid,
                                )
                                .await
                                {
                                    Ok(Some(user)) => {
                                        let flags = user.flags;
                                        let suspended =
                                            nexus_common::models::user::user_flags::SUSPENDED;
                                        let disabled =
                                            nexus_common::models::user::user_flags::DISABLED;
                                        (flags & suspended) == 0 && (flags & disabled) == 0
                                    }
                                    // User not found or DB error — reject to be safe.
                                    _ => false,
                                };

                                if !account_ok {
                                    tracing::warn!(
                                        session = %session_id,
                                        "Gateway IDENTIFY rejected: account suspended or disabled"
                                    );
                                    let _ = direct_tx
                                        .send(serde_json::json!({
                                            "op": "InvalidSession",
                                            "d": { "reason": "Account suspended" },
                                        }))
                                        .await;
                                    break;
                                }

                                authenticated = true;
                                user_id = Some(uid);

                                // Update shared uid so sender task can start forwarding
                                *authed_user_id.write().await = Some(uid);

                                // Build READY payload (servers + channels + read states)
                                let ready_data =
                                    build_ready_payload(&state, uid, &session_id, &username)
                                        .await;

                                // Populate subscribed server list BEFORE sender task
                                // processes any further broadcast events
                                let server_ids: Vec<uuid::Uuid> = ready_data["servers"]
                                    .as_array()
                                    .unwrap_or(&vec![])
                                    .iter()
                                    .filter_map(|s| s["id"].as_str()?.parse().ok())
                                    .collect();

                                *subscribed.write().await = server_ids.clone();

                                state
                                    .sessions
                                    .register(session_id.clone(), uid, server_ids)
                                    .await;

                                // Mark user online now that the session is registered
                                let _ = nexus_db::repository::users::update_presence(
                                    &state.db.pool,
                                    uid,
                                    "online",
                                )
                                .await;
                                let _ = state.broadcast.send(GatewayEvent {
                                    event_type: "PRESENCE_UPDATE".into(),
                                    data: serde_json::json!({
                                        "user_id": uid,
                                        "status": "online",
                                    }),
                                    server_id: None,
                                    channel_id: None,
                                    user_id: Some(uid),
                                });

                                // Send READY directly (not via broadcast)
                                let _ = direct_tx
                                    .send(serde_json::json!({
                                        "op": "Ready",
                                        "d": ready_data,
                                    }))
                                    .await;

                                tracing::info!(
                                    session = %session_id,
                                    "Gateway READY sent"
                                );
                            }
                        }
                    }

                    // ── Bot-specific identify ─────────────────────────────
                    GatewayMessage::BotIdentify { token, intents } => {
                        let token_hash = {
                            use sha2::{Digest, Sha256};
                            // Accept "Bot <raw>" or bare raw token
                            let raw = token.strip_prefix("Bot ").unwrap_or(&token);
                            let mut h = Sha256::new();
                            h.update(raw.as_bytes());
                            format!("{:x}", h.finalize())
                        };

                        match bots::get_bot_by_token_hash(&state.db.pool, &token_hash).await {
                            Ok(Some(bot)) => {
                                let bot_id = bot.id;
                                authenticated = true;
                                user_id = Some(bot_id);
                                *authed_user_id.write().await = Some(bot_id);

                                // Collect all servers this bot is installed in
                                let bot_installs = bots::get_bot_servers(&state.db.pool, bot_id)
                                    .await
                                    .unwrap_or_default();

                                let server_ids: Vec<uuid::Uuid> =
                                    bot_installs.iter().map(|i| i.server_id).collect();

                                // Resolve server metadata for the READY payload
                                let mut server_payloads: Vec<serde_json::Value> = Vec::new();
                                for sid in &server_ids {
                                    if let Ok(Some(srv)) =
                                        servers::find_by_id(&state.db.pool, *sid).await
                                    {
                                        let chans =
                                            channels::list_server_channels(&state.db.pool, *sid)
                                                .await
                                                .unwrap_or_default();

                                        server_payloads.push(serde_json::json!({
                                            "id": srv.id,
                                            "name": srv.name,
                                            "icon": srv.icon,
                                            "owner_id": srv.owner_id,
                                            "member_count": srv.member_count,
                                            "channels": chans.iter().map(|c| serde_json::json!({
                                                "id": c.id,
                                                "name": c.name,
                                                "channel_type": c.channel_type,
                                                "position": c.position,
                                                "parent_id": c.parent_id,
                                                "last_message_id": c.last_message_id,
                                                "topic": c.topic,
                                                "nsfw": c.nsfw,
                                            })).collect::<Vec<_>>(),
                                        }));
                                    }
                                }

                                *subscribed.write().await = server_ids.clone();

                                state
                                    .sessions
                                    .register(session_id.clone(), bot_id, server_ids)
                                    .await;

                                let ready = serde_json::json!({
                                    "op": "Ready",
                                    "d": {
                                        "session_id": session_id,
                                        "type": "bot",
                                        "intents": intents,
                                        "application": {
                                            "id": bot.id,
                                            "name": bot.name,
                                            "description": bot.description,
                                            "avatar": bot.avatar,
                                            "is_public": bot.is_public,
                                        },
                                        "servers": server_payloads,
                                    }
                                });

                                let _ = direct_tx.send(ready).await;

                                tracing::info!(session = %session_id, "Bot gateway READY sent");
                            }
                            _ => {
                                let _ = direct_tx
                                    .send(serde_json::json!({
                                        "op": "InvalidSession",
                                        "d": null,
                                    }))
                                    .await;
                            }
                        }
                    }

                    GatewayMessage::Heartbeat { .. } => {
                        let _ = direct_tx
                            .send(serde_json::json!({
                                "op": "HeartbeatAck",
                                "d": { "timestamp": chrono::Utc::now().timestamp_millis() },
                            }))
                            .await;
                    }

                    GatewayMessage::TypingStart { channel_id } => {
                        if authenticated {
                            let _ = state.broadcast.send(GatewayEvent {
                                event_type: "TYPING_START".into(),
                                data: serde_json::json!({
                                    "channel_id": channel_id,
                                    "user_id": user_id,
                                    "timestamp": chrono::Utc::now().timestamp(),
                                }),
                                server_id: None,
                                channel_id: channel_id.parse().ok(),
                                user_id,
                            });
                        }
                    }

                    GatewayMessage::PresenceUpdate {
                        status,
                        custom_status,
                    } => {
                        if let Some(uid) = user_id {
                            let _ = nexus_db::repository::users::update_presence(
                                &state.db.pool,
                                uid,
                                &status,
                            )
                            .await;
                            let _ = state.broadcast.send(GatewayEvent {
                                event_type: "PRESENCE_UPDATE".into(),
                                data: serde_json::json!({
                                    "user_id": uid,
                                    "status": status,
                                    "custom_status": custom_status,
                                }),
                                server_id: None,
                                channel_id: None,
                                user_id: Some(uid),
                            });
                        }
                    }

                    GatewayMessage::VoiceStateUpdate {
                        server_id: vs_server_id,
                        channel_id: vs_channel_id,
                        self_mute,
                        self_deaf,
                    } => {
                        if let Some(uid) = user_id {
                            let server_uuid = vs_server_id
                                .as_ref()
                                .and_then(|s| s.parse::<uuid::Uuid>().ok());
                            let channel_uuid = vs_channel_id
                                .as_ref()
                                .and_then(|c| c.parse::<uuid::Uuid>().ok());
                            let _ = state.broadcast.send(GatewayEvent {
                                event_type: "VOICE_STATE_UPDATE".into(),
                                data: serde_json::json!({
                                    "user_id": uid,
                                    "server_id": vs_server_id,
                                    "channel_id": vs_channel_id,
                                    "self_mute": self_mute,
                                    "self_deaf": self_deaf,
                                }),
                                server_id: server_uuid,
                                channel_id: channel_uuid,
                                user_id: Some(uid),
                            });
                        }
                    }

                    _ => {}
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    // ── Cleanup ───────────────────────────────────────────────────────────────
    state.sessions.remove(&session_id).await;
    if let Some(uid) = user_id
        && !state.sessions.is_online(uid).await
    {
        let _ =
            nexus_db::repository::users::update_presence(&state.db.pool, uid, "offline").await;
        let _ = state.broadcast.send(GatewayEvent {
            event_type: "PRESENCE_UPDATE".into(),
            data: serde_json::json!({"user_id": uid, "status": "offline"}),
            server_id: None,
            channel_id: None,
            user_id: Some(uid),
        });
    }

    send_task.abort();
    tracing::info!(session = %session_id, "Client disconnected from gateway");
}
/// Build the READY payload for a newly authenticated user.
/// Contains: user profile, server list with channels, read states.
async fn build_ready_payload(
    state: &GatewayState,
    uid: uuid::Uuid,
    session_id: &str,
    _username: &str,
) -> serde_json::Value {
    // Fetch user profile
    let user = nexus_db::repository::users::find_by_id(&state.db.pool, uid)
        .await
        .ok()
        .flatten();

    // Fetch user's servers
    let user_servers = servers::list_user_servers(&state.db.pool, uid)
        .await
        .unwrap_or_default();

    // For each server, fetch channels
    let mut server_payloads = Vec::new();
    for server in &user_servers {
        let server_channels = channels::list_server_channels(&state.db.pool, server.id)
            .await
            .unwrap_or_default();

        let member = members::find_member(&state.db.pool, uid, server.id)
            .await
            .ok()
            .flatten();

        server_payloads.push(serde_json::json!({
            "id": server.id,
            "name": server.name,
            "icon": server.icon,
            "owner_id": server.owner_id,
            "member_count": server.member_count,
            "channels": server_channels.iter().map(|c| serde_json::json!({
                "id": c.id,
                "name": c.name,
                "channel_type": c.channel_type,
                "position": c.position,
                "parent_id": c.parent_id,
                "last_message_id": c.last_message_id,
                "topic": c.topic,
                "nsfw": c.nsfw,
            })).collect::<Vec<_>>(),
            "member": member.map(|m| serde_json::json!({
                "nickname": m.nickname,
                "roles": m.roles,
                "joined_at": m.joined_at,
            })),
        }));
    }

    // Fetch DM channels
    let dm_channels = sqlx::query_as::<_, nexus_common::models::channel::Channel>(
        r#"
        SELECT c.* FROM channels c
        INNER JOIN dm_participants dp ON dp.channel_id = c.id
        WHERE dp.user_id = ? AND c.channel_type IN ('dm', 'group_dm')
        ORDER BY c.updated_at DESC
        "#,
    )
    .bind(uid.to_string())
    .fetch_all(&state.db.pool)
    .await
    .unwrap_or_default();

    // Fetch read states
    let user_read_states = read_states::get_all_read_states(&state.db.pool, uid)
        .await
        .unwrap_or_default();

    serde_json::json!({
        "session_id": session_id,
        "user": user.map(|u| serde_json::json!({
            "id": u.id,
            "username": u.username,
            "display_name": u.display_name,
            "avatar": u.avatar,
            "bio": u.bio,
            "status": u.status,
            "presence": u.presence,
            "flags": u.flags,
        })),
        "servers": server_payloads,
        "dm_channels": dm_channels.iter().map(|c| serde_json::json!({
            "id": c.id,
            "channel_type": c.channel_type,
            "last_message_id": c.last_message_id,
        })).collect::<Vec<_>>(),
        "read_states": user_read_states.iter().map(|rs| serde_json::json!({
            "channel_id": rs.channel_id,
            "last_read_message_id": rs.last_read_message_id,
            "mention_count": rs.mention_count,
        })).collect::<Vec<_>>(),
    })
}
