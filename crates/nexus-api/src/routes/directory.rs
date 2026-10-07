//! Public server directory and federated room discovery (v0.8).
//!
//! The directory allows users to discover public servers and rooms across the
//! federated Nexus network.
//!
//! ## Endpoints
//!
//! | Method | Path | Auth | Description |
//! |--------|------|------|-------------|
//! | GET  | `/api/v1/directory/servers` | None | List all known public servers |
//! | GET  | `/api/v1/directory/rooms` | None | List all public rooms (federated) |
//! | GET  | `/api/v1/directory/rooms/search` | None | Search rooms by name/topic |
//! | POST | `/api/v1/directory/rooms/join` | Bearer | Join a federated room |
//! | GET  | `/api/v1/directory/resolve/:server_name` | None | Resolve a server's base URL |

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row as _;
use tracing::{info, warn};

use crate::AppState;

// ─── Router ───────────────────────────────────────────────────────────────────

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // Public listing (no auth)
        .route("/directory/servers", get(list_servers))
        .route("/directory/rooms", get(list_rooms))
        .route("/directory/rooms/search", get(search_rooms))
        .route("/directory/resolve/{server_name}", get(resolve_server))
        // Authenticated actions
        .route(
            "/directory/rooms/join",
            post(join_federated_room).route_layer(middleware::from_fn(
                crate::middleware::combined_auth_middleware,
            )),
        )
}

// ─── Request / response types ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct PaginationQuery {
    limit: Option<u32>,
    #[serde(rename = "since")]
    _since: Option<String>,
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
    server: Option<String>,
    limit: Option<u32>,
    #[serde(rename = "since")]
    _since: Option<String>,
}

#[derive(Deserialize)]
struct JoinRoomRequest {
    /// Fully-qualified room ID (`!id:server.tld`) or alias (`#alias:server.tld`).
    room_id: String,
}

#[derive(Serialize)]
struct ServerEntry {
    server_name: String,
    description: Option<String>,
    icon_url: Option<String>,
    public_room_count: u64,
    total_users: u64,
}

#[derive(Serialize)]
struct RoomEntry {
    room_id: String,
    name: Option<String>,
    topic: Option<String>,
    member_count: u64,
    server_name: String,
    join_rule: String,
    tags: Vec<String>,
}

#[derive(Serialize)]
struct PaginatedRooms {
    rooms: Vec<RoomEntry>,
    total_count: u64,
    next_batch: Option<String>,
}

#[derive(Serialize)]
struct PaginatedServers {
    servers: Vec<ServerEntry>,
    total_count: u64,
    next_batch: Option<String>,
}

// ─── Handlers ────────────────────────────────────────────────────────────────

/// `GET /api/v1/directory/servers`
///
/// Return all servers listed in the `directory_servers` table.
/// This includes our own server plus any federated servers that have opted in.
async fn list_servers(
    State(state): State<Arc<AppState>>,
    Query(q): Query<PaginationQuery>,
) -> Json<PaginatedServers> {
    let limit = q.limit.unwrap_or(20).min(100) as i64;

    let rows = sqlx::query(
        "SELECT server_name, description, icon_url, public_room_count, total_users \
         FROM directory_servers \
         ORDER BY server_name ASC \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(&state.db.pool)
    .await;

    let mut servers: Vec<ServerEntry> = match rows {
        Ok(rows) => rows
            .into_iter()
            .map(|r| ServerEntry {
                server_name: r.try_get("server_name").unwrap_or_default(),
                description: r.try_get("description").ok().flatten(),
                icon_url: r.try_get("icon_url").ok().flatten(),
                public_room_count: r.try_get::<i32, _>("public_room_count").unwrap_or(0) as u64,
                total_users: r.try_get::<i32, _>("total_users").unwrap_or(0) as u64,
            })
            .collect(),
        Err(e) => {
            warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "Failed to list directory servers");
            vec![]
        }
    };

    // Always include this server first (even if not yet in directory_servers).
    let this_name = state.server_name.clone();
    if !servers.iter().any(|s| s.server_name == this_name) {
        servers.insert(
            0,
            ServerEntry {
                server_name: this_name,
                description: Some("This Nexus server".into()),
                icon_url: None,
                public_room_count: 0,
                total_users: 0,
            },
        );
    }

    let total_count = servers.len() as u64;
    Json(PaginatedServers {
        servers,
        total_count,
        next_batch: None,
    })
}

/// `GET /api/v1/directory/rooms`
///
/// Return all publicly joinable federated rooms — from this server and any
/// remote servers in the directory.
async fn list_rooms(
    State(state): State<Arc<AppState>>,
    Query(q): Query<PaginationQuery>,
) -> Json<PaginatedRooms> {
    let limit = q.limit.unwrap_or(20).min(100) as i64;

    let rows = sqlx::query(
        "SELECT room_id, room_name, room_topic, member_count, origin_server, join_rule \
         FROM federated_rooms \
         WHERE join_rule = 'public' \
         ORDER BY member_count DESC \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(&state.db.pool)
    .await;

    let rooms: Vec<RoomEntry> = match rows {
        Ok(rows) => rows
            .into_iter()
            .map(|r| RoomEntry {
                room_id: r.try_get("room_id").unwrap_or_default(),
                name: r.try_get("room_name").ok().flatten(),
                topic: r.try_get("room_topic").ok().flatten(),
                member_count: r.try_get::<i32, _>("member_count").unwrap_or(0) as u64,
                server_name: r.try_get("origin_server").unwrap_or_default(),
                join_rule: r.try_get("join_rule").unwrap_or_else(|_| "public".into()),
                tags: vec![],
            })
            .collect(),
        Err(e) => {
            warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "Failed to list federated rooms");
            vec![]
        }
    };

    let total_count = rooms.len() as u64;
    Json(PaginatedRooms {
        rooms,
        total_count,
        next_batch: None,
    })
}

/// `GET /api/v1/directory/rooms/search?q=<query>&server=<server>&limit=<n>`
///
/// Full-text search across public room names and topics, optionally scoped
/// to a specific server.
async fn search_rooms(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SearchQuery>,
) -> Json<PaginatedRooms> {
    let query_str = format!("%{}%", q.q.unwrap_or_default());
    let server_filter = q.server;
    let limit = q.limit.unwrap_or(20).min(100) as i64;

    let rows = if let Some(ref server) = server_filter {
        sqlx::query(
            "SELECT room_id, room_name, room_topic, member_count, origin_server, join_rule \
             FROM federated_rooms \
             WHERE join_rule = 'public' \
               AND origin_server = $1 \
               AND (room_name ILIKE $2 OR room_topic ILIKE $2) \
             ORDER BY member_count DESC \
             LIMIT $3",
        )
        .bind(server)
        .bind(&query_str)
        .bind(limit)
        .fetch_all(&state.db.pool)
        .await
    } else {
        sqlx::query(
            "SELECT room_id, room_name, room_topic, member_count, origin_server, join_rule \
             FROM federated_rooms \
             WHERE join_rule = 'public' \
               AND (room_name ILIKE $1 OR room_topic ILIKE $1) \
             ORDER BY member_count DESC \
             LIMIT $2",
        )
        .bind(&query_str)
        .bind(limit)
        .fetch_all(&state.db.pool)
        .await
    };

    let rooms: Vec<RoomEntry> = match rows {
        Ok(rows) => rows
            .into_iter()
            .map(|r| RoomEntry {
                room_id: r.try_get("room_id").unwrap_or_default(),
                name: r.try_get("room_name").ok().flatten(),
                topic: r.try_get("room_topic").ok().flatten(),
                member_count: r.try_get::<i32, _>("member_count").unwrap_or(0) as u64,
                server_name: r.try_get("origin_server").unwrap_or_default(),
                join_rule: r.try_get("join_rule").unwrap_or_else(|_| "public".into()),
                tags: vec![],
            })
            .collect(),
        Err(e) => {
            warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "Failed to search federated rooms");
            vec![]
        }
    };

    let total_count = rooms.len() as u64;
    Json(PaginatedRooms {
        rooms,
        total_count,
        next_batch: None,
    })
}

/// `GET /api/v1/directory/resolve/:server_name`
///
/// Return the resolved federation base URL and key information for a server.
/// Useful for clients that want to verify a server before joining a room.
async fn resolve_server(
    State(state): State<Arc<AppState>>,
    Path(server_name): Path<String>,
) -> Json<Value> {
    // Check the local `federated_servers` cache first.
    let row = sqlx::query(
        "SELECT server_name, base_url, server_version, is_blocked \
         FROM federated_servers \
         WHERE server_name = $1",
    )
    .bind(&server_name)
    .fetch_optional(&state.db.pool)
    .await
    .ok()
    .flatten();

    if let Some(r) = row {
        let is_blocked: bool = r.try_get("is_blocked").unwrap_or(false);
        let base_url: Option<String> = r.try_get("base_url").ok().flatten();
        let version: Option<String> = r.try_get("server_version").ok().flatten();

        Json(json!({
            "server_name": server_name,
            "base_url": base_url.unwrap_or_else(|| format!("https://{}:8448", server_name)),
            "version": version,
            "is_blocked": is_blocked,
            "status": if is_blocked { "blocked" } else { "known" },
        }))
    } else {
        // Not in cache — return a best-effort default for the caller to verify.
        Json(json!({
            "server_name": server_name,
            "base_url": format!("https://{}:8448", server_name),
            "status": "unknown",
        }))
    }
}

/// `POST /api/v1/directory/rooms/join`
///
/// Initiate a federated join on behalf of the authenticated user.
/// If the room is on a remote server, this triggers the make_join → send_join
/// federation protocol.
async fn join_federated_room(
    State(state): State<Arc<AppState>>,
    axum::extract::Extension(auth): axum::extract::Extension<crate::middleware::AuthContext>,
    Json(body): Json<JoinRoomRequest>,
) -> (StatusCode, Json<Value>) {
    let room_id = body.room_id;
    info!("Federated join request received");

    // Parse `!channel:server_name` — extract the remote server part.
    let remote_server = match room_id.split(':').nth(1) {
        Some(s) => s.to_owned(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Invalid room_id: expected !id:server format" })),
            );
        }
    };

    // Local rooms don't need the federation join protocol.
    if remote_server == state.server_name {
        return (
            StatusCode::OK,
            Json(json!({
                "message": "Local room — joined directly",
                "room_id": room_id,
                "status": "joined",
            })),
        );
    }

    // Build the local user's MXID.
    let user_mxid = format!("@{}:{}", auth.username, state.server_name);

    // ── Step 1: make_join — get a join event template from the remote server ──
    let make_join_resp = match state
        .federation_client
        .make_join(&remote_server, &room_id, &user_mxid)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "make_join failed");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("make_join failed: {}", e) })),
            );
        }
    };

    // ── Step 2: fill in and sign the join event ───────────────────────────────
    let mut join_event = make_join_resp.event;
    if let Some(obj) = join_event.as_object_mut() {
        obj.insert("origin".to_owned(), json!(&state.server_name));
        obj.insert(
            "origin_server_ts".to_owned(),
            json!(chrono::Utc::now().timestamp_millis()),
        );
    }
    if let Err(e) =
        nexus_federation::sign_event(&state.federation_key, &state.server_name, &mut join_event)
    {
        warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "Failed to sign join event");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "Failed to sign join event" })),
        );
    }

    let event_id = nexus_federation::types::new_event_id(&state.server_name);

    // ── Step 3: send_join — submit the signed event to the remote server ──────
    match state
        .federation_client
        .send_join(&remote_server, &room_id, &event_id, &join_event)
        .await
    {
        Ok(resp) => {
            info!(state_events = resp.state.len(), "Successfully joined federated room");

            // Track local user membership so they receive real-time PDU events.
            if let Err(e) = sqlx::query(
                "INSERT INTO federated_room_members (room_id, user_id) \
                 VALUES ($1, $2) \
                 ON CONFLICT (room_id, user_id) DO NOTHING",
            )
            .bind(&room_id)
            .bind(auth.user_id.to_string())
            .execute(&state.db.pool)
            .await
            {
                warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "Failed to track federated room membership");
            }

            (
                StatusCode::OK,
                Json(json!({
                    "message": "Federated join successful",
                    "room_id": room_id,
                    "status": "joined",
                    "state_events": resp.state.len(),
                })),
            )
        }
        Err(e) => {
            warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "send_join failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("send_join failed: {}", e) })),
            )
        }
    }
}
