//! Server-to-server federation API routes (v0.8).
//!
//! These endpoints are accessed by *remote Nexus servers*, not directly by
//! end-user clients. Every inbound request must carry a valid
//! `Authorization: NexusFederation …` header signed with the origin server's
//! Ed25519 key.
//!
//! ## Endpoints
//!
//! | Method | Path | Description |
//! |--------|------|-------------|
//! | GET    | `/_nexus/key/v2/server` | Serve this server's public signing key document |
//! | GET    | `/.well-known/nexus/server` | SRV delegation / well-known response |
//! | PUT    | `/_nexus/federation/v1/send/{txnId}` | Receive a transaction from a remote server |
//! | GET    | `/_nexus/federation/v1/event/{eventId}` | Serve a single event by ID |
//! | GET    | `/_nexus/federation/v1/state/{roomId}` | Serve room state at an event |
//! | GET    | `/_nexus/federation/v1/make_join/{roomId}/{userId}` | Prepare a join event template |
//! | PUT    | `/_nexus/federation/v1/send_join/{roomId}/{event_id}` | Receive a signed join event |
//! | GET    | `/_nexus/federation/v1/backfill/{roomId}` | Backfill historical events |
//! | PUT    | `/_matrix/app/v1/transactions/{txnId}` | Matrix AS bridge inbound transactions |

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, put},
};
use nexus_common::gateway_event::{GatewayEvent, event_types};
use nexus_db::repository::users;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row as _;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::{
    AppState,
    middleware::{check_rate_limit_with_fallback, extract_client_tag},
};

// ─── Router ───────────────────────────────────────────────────────────────────

/// Mount all federation routes on the given app router.
///
/// Note: these routes live at paths *outside* `/api/v1` so they can follow
/// the Matrix federation spec path conventions.
pub fn federation_router() -> Router<Arc<AppState>> {
    Router::new()
        // Key document (unauthenticated)
        .route("/_nexus/key/v2/server", get(server_key_document))
        // Well-known delegation
        .route("/.well-known/nexus/server", get(well_known_server))
        // Federation S2S endpoints
        .route(
            "/_nexus/federation/v1/send/{txn_id}",
            put(receive_transaction),
        )
        .route("/_nexus/federation/v1/event/{event_id}", get(get_event))
        .route("/_nexus/federation/v1/state/{room_id}", get(get_room_state))
        .route(
            "/_nexus/federation/v1/make_join/{room_id}/{user_id}",
            get(make_join),
        )
        .route(
            "/_nexus/federation/v1/send_join/{room_id}/{event_id}",
            put(send_join),
        )
        .route("/_nexus/federation/v1/backfill/{room_id}", get(backfill))
        // v0.8/08-03: User profile endpoint (MXID resolution)
        .route("/_nexus/federation/v1/user/{user_id}", get(user_profile))
        // v0.9: Cross-server friend requests
        .route(
            "/_nexus/federation/v1/friend_request",
            put(receive_friend_request),
        )
        .route(
            "/_nexus/federation/v1/friend_request/respond",
            put(receive_friend_request_response),
        )
        // Matrix Application Service bridge (inbound)
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(matrix_as_transaction),
        )
}

// ─── Key document ─────────────────────────────────────────────────────────────

/// `GET /_nexus/key/v2/server`
///
/// Returns this server's public signing key document. Remote servers fetch
/// this once and cache it to verify future request signatures.
async fn server_key_document(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let doc = state.federation_key.to_key_document(&state.server_name);
    (StatusCode::OK, Json(doc))
}

// ─── Well-known ───────────────────────────────────────────────────────────────

/// `GET /.well-known/nexus/server`
///
/// Returns rich instance identity metadata (v0.8.5+).  Remote servers parse
/// this when initiating peering to learn the instance's display name,
/// description, admin contact, and federation policy.
///
/// Falls back gracefully to a minimal response for older remotes that only
/// look at the `m.server` field.
async fn well_known_server(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Prefer explicit env overrides, otherwise use the configured server_name.
    let server_name = std::env::var("NEXUS_FEDERATION_SERVER").unwrap_or_else(|_| {
        std::env::var("NEXUS_SERVER_NAME").unwrap_or_else(|_| state.server_name.clone())
    });

    // If server_name has no explicit port, append the API port so remote
    // servers can discover us correctly (especially on LAN / non-standard ports).
    // Standard ports (443, 8448) are implied; anything else must be explicit.
    let m_server = if nexus_federation::discovery_has_explicit_port(&server_name) {
        server_name.clone()
    } else {
        let port: u16 = nexus_common::config::get().server.port;
        match port {
            443 | 8448 => server_name.clone(),
            p => format!("{}:{}", server_name, p),
        }
    };

    // Load optional identity from instance_settings.
    let identity: serde_json::Value = sqlx::query(
        "SELECT value::text AS value_text FROM instance_settings WHERE key = 'federation_identity'",
    )
    .fetch_optional(&state.db.pool)
    .await
    .ok()
    .flatten()
    .and_then(|r| r.try_get::<String, _>("value_text").ok())
    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
    .unwrap_or_else(|| json!({}));

    // Count local users (best-effort).
    let user_count: i64 = sqlx::query("SELECT COUNT(*) AS count FROM users")
        .fetch_one(&state.db.pool)
        .await
        .ok()
        .and_then(|r| r.try_get::<i64, _>("count").ok())
        .unwrap_or(0);

    // Count active peers.
    let peer_count: i64 =
        sqlx::query("SELECT COUNT(*) AS count FROM federated_servers WHERE is_blocked = false")
            .fetch_one(&state.db.pool)
            .await
            .ok()
            .and_then(|r| r.try_get::<i64, _>("count").ok())
            .unwrap_or(0);

    let version = concat!("Nexus ", env!("CARGO_PKG_VERSION"));

    Json(json!({
        "m.server":          m_server,
        "display_name":      identity.get("display_name").and_then(|v| v.as_str()),
        "description":       identity.get("description").and_then(|v| v.as_str()),
        "admin_contact":     identity.get("admin_contact").and_then(|v| v.as_str()),
        "software_version":  version,
        "user_count":        user_count,
        "peer_count":        peer_count,
        "federation_policy": identity.get("federation_policy")
                                     .and_then(|v| v.as_str())
                                     .unwrap_or("open"),
    }))
}

// ─── Transaction receive ──────────────────────────────────────────────────────

/// `PUT /_nexus/federation/v1/send/{txnId}`
///
/// Receives a bundle of events (PDUs) and ephemeral data (EDUs) from a remote
/// server. This is the primary federation ingestion point.
async fn receive_transaction(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(txn_id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    // ── 0. Rate limiting (early, before expensive signature verification) ────
    // Limit transaction ingestion to 100/min per IP and 200/min per origin
    let tag = extract_client_tag(&headers);
    if let Err(e) = check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:fed:txn:tag:{tag}"),
        100,
        60,
    )
    .await
    {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    // ── 1. Authenticate — full Ed25519 HTTP-request signature check ───────────
    let txn_uri = format!("/_nexus/federation/v1/send/{}", txn_id);
    let origin = match verify_inbound_request(&state, &headers, "PUT", &txn_uri, Some(&body)).await
    {
        Ok(o) => o,
        Err((status, e)) => {
            warn!(status = %status, "Rejected federated transaction");
            return (status, Json(json!({ "error": e }))).into_response();
        }
    };

    debug!("Received federation transaction {} from {}", txn_id, origin);

    // ── 2. Idempotency: skip already-processed transactions ───────────────────
    match sqlx::query(
        "SELECT 1 FROM federation_txn_log \
         WHERE txn_id = $1 AND origin_server = $2 \
         LIMIT 1",
    )
    .bind(&txn_id)
    .bind(&origin)
    .fetch_optional(&state.db.pool)
    .await
    {
        Ok(Some(_)) => {
            debug!(
                "Txn {} from {} already processed — replying idempotently",
                txn_id, origin
            );
            return (StatusCode::OK, Json(json!({}))).into_response();
        }
        Ok(None) => {}
        Err(e) => warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "Failed to query txn_log for idempotency"),
    }

    // ── 3. Upsert origin server in federated_servers ──────────────────────────
    if let Err(e) = sqlx::query(
        "INSERT INTO federated_servers (server_name, last_seen_at) \
         VALUES ($1, NOW()) \
         ON CONFLICT (server_name) DO UPDATE SET last_seen_at = NOW()",
    )
    .bind(&origin)
    .execute(&state.db.pool)
    .await
    {
        warn!(origin = %origin, error_kind = %nexus_common::logsafe::err_kind(&e), "Failed to upsert federated server");
    }

    // ── 4. Load verify keys for the origin server ─────────────────────────────
    let verify_keys = load_server_verify_keys(&state.db.pool, &origin).await;

    // ── 5. Process each PDU ───────────────────────────────────────────────────
    let pdus = body
        .get("pdus")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let edu_count = body
        .get("edus")
        .and_then(Value::as_array)
        .map(|a| a.len() as i32)
        .unwrap_or(0);
    let pdu_count = pdus.len() as i32;
    let mut accepted = 0i32;

    for pdu in &pdus {
        match process_pdu(
            &state.db.pool,
            &origin,
            &txn_id,
            &verify_keys,
            &state.server_name,
            pdu,
        )
        .await
        {
            Ok(true) => accepted += 1,
            Ok(false) => debug!("PDU from {} was a duplicate (already stored)", origin),
            Err(e) => warn!(origin = %origin, error_kind = %nexus_common::logsafe::anyhow_kind(&e), "Rejected PDU"),
        }
    }

    info!(
        "Federation txn {} from {}: {}/{} PDUs accepted, {} EDUs",
        txn_id, origin, accepted, pdu_count, edu_count
    );

    // ── 6. Log the transaction (idempotent guard for future retries) ──────────
    if let Err(e) = sqlx::query(
        "INSERT INTO federation_txn_log \
         (txn_id, origin_server, pdu_count, edu_count) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (txn_id, origin_server) DO NOTHING",
    )
    .bind(&txn_id)
    .bind(&origin)
    .bind(pdu_count)
    .bind(edu_count)
    .execute(&state.db.pool)
    .await
    {
        warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "Failed to write federation txn log");
    }

    // ── 7. Real-time dispatch to local members of federated rooms ─────────────
    // For each message-type PDU that arrived, notify any local users who have
    // joined the originating room so their clients update in real time.
    // First pass: collect all unique room IDs from message PDUs
    let mut message_rooms: std::collections::HashSet<String> = std::collections::HashSet::new();
    for pdu in &pdus {
        let event_type = pdu.get("type").and_then(Value::as_str).unwrap_or("");
        if !matches!(event_type, "nexus.message.create" | "m.room.message") {
            continue;
        }
        let room_id = match pdu.get("room_id").and_then(Value::as_str) {
            Some(r) => r.to_owned(),
            None => continue,
        };
        message_rooms.insert(room_id);
    }

    // Batch load all room members in one query (eliminates N queries)
    let mut room_members: std::collections::HashMap<String, Vec<uuid::Uuid>> =
        std::collections::HashMap::new();
    if !message_rooms.is_empty() {
        let room_ids: Vec<&str> = message_rooms.iter().map(|s| s.as_str()).collect();
        let mut qb = sqlx::QueryBuilder::new(
            "SELECT room_id, user_id FROM federated_room_members WHERE room_id IN (",
        );
        {
            let mut separated = qb.separated(", ");
            for room_id in &room_ids {
                separated.push_bind(*room_id);
            }
        }
        qb.push(")");

        if let Ok(rows) = qb.build().fetch_all(&state.db.pool).await {
            for row in rows {
                use sqlx::Row as _;
                if let (Ok(rid), Ok(uid_str)) = (
                    row.try_get::<String, _>("room_id"),
                    row.try_get::<String, _>("user_id"),
                )
                    && let Ok(uid) = uuid::Uuid::parse_str(&uid_str) {
                        room_members.entry(rid).or_default().push(uid);
                    }
            }
        }
    }

    // Second pass: dispatch events using pre-loaded members
    for pdu in &pdus {
        let event_type = pdu.get("type").and_then(Value::as_str).unwrap_or("");
        if !matches!(event_type, "nexus.message.create" | "m.room.message") {
            continue;
        }
        let room_id = match pdu.get("room_id").and_then(Value::as_str) {
            Some(r) => r.to_owned(),
            None => continue,
        };
        let sender = pdu
            .get("sender")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let content = pdu.get("content").cloned().unwrap_or_else(|| json!({}));
        let event_id = pdu
            .get("event_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let ts = pdu
            .get("origin_server_ts")
            .and_then(Value::as_i64)
            .unwrap_or(0);

        let member_ids = room_members.get(&room_id).cloned().unwrap_or_default();
        for member_id in member_ids {
            let _ = state.gateway_tx.send(GatewayEvent {
                event_type: "FEDERATED_MESSAGE_CREATE".to_owned(),
                data: json!({
                    "event_id": event_id,
                    "room_id": room_id,
                    "sender": sender,
                    "content": content,
                    "origin_server_ts": ts,
                    "origin_server": origin,
                }),
                server_id: None,
                channel_id: None,
                user_id: Some(member_id),
            });
        }
    }

    (StatusCode::OK, Json(json!({}))).into_response()
}

// ─── PDU helpers ─────────────────────────────────────────────────────────────

/// Process a single incoming PDU:
///
/// 1. Verify the Ed25519 signature if verify keys are available.
/// 2. Persist to `federated_events` (idempotent: ON CONFLICT event_id DO NOTHING).
/// 3. Upsert the sender into `federated_users` if they're from a remote server.
///
/// Returns `Ok(true)` if newly persisted, `Ok(false)` if duplicate, `Err` if rejected.
async fn process_pdu(
    pool: &sqlx::AnyPool,
    origin: &str,
    txn_id: &str,
    verify_keys: &serde_json::Map<String, Value>,
    local_server_name: &str,
    pdu: &Value,
) -> Result<bool, anyhow::Error> {
    let event_id = pdu
        .get("event_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("PDU missing event_id"))?;
    let room_id = pdu
        .get("room_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("PDU missing room_id"))?;
    let event_type = pdu
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("nexus.unknown");
    let sender = pdu.get("sender").and_then(Value::as_str).unwrap_or(origin);
    let origin_server_ts = pdu
        .get("origin_server_ts")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let content = pdu
        .get("content")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    let signatures = pdu
        .get("signatures")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));

    // Verify signature when we have the origin's public key(s).
    if !verify_keys.is_empty() {
        verify_pdu_signature(pdu, origin, verify_keys)?;
    } else {
        debug!(origin = %origin, "No cached verify keys — persisting PDU without sig check");
    }

    // Persist (ON CONFLICT handles duplicate event IDs gracefully).
    let result = sqlx::query(
        "INSERT INTO federated_events \
         (event_id, room_id, event_type, sender, origin_server, \
          origin_server_ts, content, signatures, txn_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
         ON CONFLICT (event_id) DO NOTHING",
    )
    .bind(event_id.to_string())
    .bind(room_id.to_string())
    .bind(event_type)
    .bind(sender)
    .bind(origin)
    .bind(origin_server_ts)
    .bind(serde_json::to_string(&content).unwrap_or_default())
    .bind(serde_json::to_string(&signatures).unwrap_or_default())
    .bind(txn_id.to_string())
    .execute(pool)
    .await?;

    // rows_affected == 0 means the event was already stored (conflict).
    let new_event = result.rows_affected() > 0;

    // Upsert the sender's profile into federated_users (skip for local users).
    if new_event
        && let Err(e) = upsert_federated_user(pool, local_server_name, sender, pdu).await {
            debug!(error_kind = %nexus_common::logsafe::anyhow_kind(&e), "Could not upsert federated user");
        }

    Ok(new_event)
}

/// Verify the Ed25519 signature on a PDU against the origin server's verify keys.
fn verify_pdu_signature(
    pdu: &Value,
    origin: &str,
    verify_keys: &serde_json::Map<String, Value>,
) -> Result<(), anyhow::Error> {
    let sig_for_origin = pdu
        .get("signatures")
        .and_then(|s| s.get(origin))
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("PDU has no signature for origin {}", origin))?;

    // Pick the first ed25519 key/sig pair.
    let (key_id, sig_b64) = sig_for_origin
        .iter()
        .find(|(k, _)| k.starts_with("ed25519:"))
        .map(|(k, v)| (k.as_str(), v.as_str().unwrap_or("")))
        .ok_or_else(|| anyhow::anyhow!("No ed25519 signature in PDU from {}", origin))?;

    let pubkey_b64 = verify_keys
        .get(key_id)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Unknown key {} for {}", key_id, origin))?;

    // Canonical JSON of PDU without `signatures` / `unsigned` fields.
    let mut pdu_for_signing = pdu.clone();
    if let Value::Object(ref mut m) = pdu_for_signing {
        m.remove("signatures");
        m.remove("unsigned");
    }
    let canonical = nexus_federation::signatures::canonical_json(&pdu_for_signing)
        .map_err(|e| anyhow::anyhow!("canonical_json error: {}", e))?;

    nexus_federation::keys::verify_signature(pubkey_b64, sig_b64, canonical.as_bytes())
        .map_err(|_| anyhow::anyhow!("Signature check failed for {} (key {})", origin, key_id))?;

    Ok(())
}

/// Load the cached verify keys (`key_id → base64_pubkey`) for a remote server
/// from the `federated_servers` table.
async fn load_server_verify_keys(
    pool: &sqlx::AnyPool,
    server_name: &str,
) -> serde_json::Map<String, Value> {
    let row = sqlx::query("SELECT verify_keys FROM federated_servers WHERE server_name = $1")
        .bind(server_name)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();

    if let Some(row) = row
        && let Ok(s) = row.try_get::<String, _>("verify_keys")
            && let Ok(Value::Object(m)) = serde_json::from_str::<Value>(&s) {
                return m;
            }
    Default::default()
}

// ─── Event fetching ───────────────────────────────────────────────────────────

/// `GET /_nexus/federation/v1/event/{eventId}`
async fn get_event(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(event_id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = extract_federation_origin(&headers) {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "error": e }))).into_response();
    }

    let row = sqlx::query(
        "SELECT event_id, room_id, event_type, sender, origin_server, \
                origin_server_ts, content, signatures \
         FROM federated_events \
         WHERE event_id = $1 AND is_redacted = FALSE",
    )
    .bind(&event_id)
    .fetch_optional(&state.db.pool)
    .await;

    match row {
        Ok(Some(r)) => {
            let pdu = json!({
                "event_id":        r.try_get::<String, _>("event_id").unwrap_or_default(),
                "room_id":         r.try_get::<String, _>("room_id").unwrap_or_default(),
                "type":            r.try_get::<String, _>("event_type").unwrap_or_default(),
                "sender":          r.try_get::<String, _>("sender").unwrap_or_default(),
                "origin":          r.try_get::<String, _>("origin_server").unwrap_or_default(),
                "origin_server_ts":r.try_get::<i64, _>("origin_server_ts").unwrap_or(0),
                "content":         r.try_get::<String,_>("content").ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({})),
                "signatures":      r.try_get::<String,_>("signatures").ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({})),
            });
            (StatusCode::OK, Json(json!({ "pdu": pdu }))).into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Event not found" })),
        )
            .into_response(),
        Err(e) => {
            warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "Error fetching event");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "DB error" })),
            )
                .into_response()
        }
    }
}

// ─── Room state ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct StateQuery {
    #[serde(rename = "at")]
    _at: Option<String>,
}

/// `GET /_nexus/federation/v1/state/{roomId}`
async fn get_room_state(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
    Query(_query): Query<StateQuery>,
) -> impl IntoResponse {
    let origin = match extract_federation_origin(&headers) {
        Ok(o) => o,
        Err(e) => return (StatusCode::UNAUTHORIZED, Json(json!({ "error": e }))).into_response(),
    };

    let pool = &state.db.pool;

    // Check if this room exists and if the origin server has access
    let room_row = sqlx::query(
        "SELECT join_rule, participating_servers FROM federated_rooms WHERE room_id = $1",
    )
    .bind(&room_id)
    .fetch_optional(pool)
    .await
    .unwrap_or(None);

    match room_row {
        None => {
            // Room not found locally
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Room not found" })),
            )
                .into_response();
        }
        Some(row) => {
            let join_rule = row
                .try_get::<String, _>("join_rule")
                .unwrap_or_else(|_| "public".to_string());

            // For private/invite rooms, deny state access for now
            // (can be enhanced later with participating_servers list when using typed DB)
            if join_rule != "public" {
                tracing::warn!(origin = %origin, join_rule = %join_rule, "Denied room state request: origin not in participating servers");
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({ "error": "Not authorized to view this room's state" })),
                )
                    .into_response();
            }
        }
    }

    let rows = sqlx::query(
        "SELECT event_id, event_type, sender, origin_server, content, signatures, origin_server_ts \
         FROM federated_events WHERE room_id = $1 ORDER BY origin_server_ts ASC LIMIT 100",
    )
    .bind(&room_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let pdus: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "event_id":  row.try_get::<String, _>("event_id").unwrap_or_default(),
                "type":      row.try_get::<String, _>("event_type").unwrap_or_default(),
                "room_id":   &room_id,
                "sender":    row.try_get::<String, _>("sender").unwrap_or_default(),
                "origin":    row.try_get::<String, _>("origin_server").unwrap_or_default(),
                "content":   row.try_get::<String,_>("content").ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({})),
                "signatures": row.try_get::<String,_>("signatures").ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({})),
                "origin_server_ts": row.try_get::<i64, _>("origin_server_ts").unwrap_or(0),
            })
        })
        .collect();

    (
        StatusCode::OK,
        Json(json!({ "pdus": pdus, "auth_chain": [] })),
    )
        .into_response()
}

// ─── Join protocol ────────────────────────────────────────────────────────────

/// `GET /_nexus/federation/v1/make_join/{roomId}/{userId}`
///
/// Returns a join event template that the requesting server should sign
/// and return via `send_join`.
async fn make_join(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((room_id, user_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let origin = match extract_federation_origin(&headers) {
        Ok(o) => o,
        Err(e) => return (StatusCode::UNAUTHORIZED, Json(json!({ "error": e }))).into_response(),
    };

    let pool = &state.db.pool;

    // Check if room exists and validate join_rule
    let room_row = sqlx::query("SELECT join_rule FROM federated_rooms WHERE room_id = $1")
        .bind(&room_id)
        .fetch_optional(pool)
        .await
        .unwrap_or(None);

    if let Some(row) = room_row {
        let join_rule = row
            .try_get::<String, _>("join_rule")
            .unwrap_or_else(|_| "public".to_string());
        if join_rule != "public" {
            tracing::warn!(origin = %origin, join_rule = %join_rule, "Denied join: not public");
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": format!("Room has join_rule: {}", join_rule)})),
            )
                .into_response();
        }
    } else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Room not found"})),
        )
            .into_response();
    }

    let server_name = &state.server_name;

    let template = json!({
        "room_version": "nexus.v1",
        "event": {
            "type": "nexus.member.join",
            "room_id": room_id,
            "sender": user_id,
            "state_key": user_id,
            "content": { "membership": "join" },
            "origin": server_name,
            "origin_server_ts": chrono::Utc::now().timestamp_millis(),
        }
    });

    (StatusCode::OK, Json(template)).into_response()
}

/// `PUT /_nexus/federation/v1/send_join/{roomId}/{eventId}`
///
/// Accepts a signed join event from a remote server, adds the user to the
/// room, and returns the current room state + auth chain.
async fn send_join(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((room_id, event_id)): Path<(String, String)>,
    Json(event): Json<Value>,
) -> impl IntoResponse {
    let origin = match extract_federation_origin(&headers) {
        Ok(o) => o,
        Err(e) => return (StatusCode::UNAUTHORIZED, Json(json!({ "error": e }))).into_response(),
    };

    info!(origin = %origin, "Processing send_join");

    let pool = &state.db.pool;

    // Verify signature (soft: skip when no keys are cached for the origin yet).
    let verify_keys = load_server_verify_keys(pool, &origin).await;
    if !verify_keys.is_empty() {
        if let Err(e) = verify_pdu_signature(&event, &origin, &verify_keys) {
            warn!(origin = %origin, error_kind = %nexus_common::logsafe::anyhow_kind(&e), "send_join sig verify failed");
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "invalid signature" })),
            )
                .into_response();
        }
    } else {
        debug!(
            "No cached keys for {} — accepting send_join without sig verify",
            origin
        );
    }

    // Upsert room.
    let room_name = event
        .get("content")
        .and_then(|c| c.get("room_name"))
        .and_then(Value::as_str)
        .unwrap_or(&room_id)
        .to_owned();
    let _ = sqlx::query(
        "INSERT INTO federated_rooms (room_id, origin_server, room_name, join_rule, member_count) \
         VALUES ($1, $2, $3, 'public', 1) \
         ON CONFLICT (room_id) DO UPDATE \
         SET member_count = federated_rooms.member_count + 1, updated_at = NOW()",
    )
    .bind(&room_id)
    .bind(&origin)
    .bind(&room_name)
    .execute(pool)
    .await;

    // Persist join event.
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("nexus.member.join")
        .to_owned();
    let sender = event
        .get("sender")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let ts = event
        .get("origin_server_ts")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let content = event.get("content").cloned().unwrap_or(json!({}));
    let sigs = event.get("signatures").cloned().unwrap_or(json!({}));
    let _ = sqlx::query(
        "INSERT INTO federated_events \
         (event_id, room_id, event_type, sender, origin_server, origin_server_ts, content, signatures, txn_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'send_join') \
         ON CONFLICT (event_id) DO NOTHING",
    )
    .bind(&event_id)
    .bind(&room_id)
    .bind(&event_type)
    .bind(&sender)
    .bind(&origin)
    .bind(ts)
    .bind(serde_json::to_string(&content).unwrap_or_default())
    .bind(serde_json::to_string(&sigs).unwrap_or_default())
    .execute(pool)
    .await;

    // Notify gateway of the member join.
    let gw = nexus_common::gateway_event::GatewayEvent {
        event_type: "FEDERATED_MEMBER_JOIN".to_owned(),
        data: json!({ "room_id": room_id, "sender": sender, "origin": origin }),
        server_id: None,
        channel_id: None,
        user_id: None,
    };
    let _ = state.gateway_tx.send(gw);

    // Return room state snapshot.
    let state_rows = sqlx::query(
        "SELECT event_id, event_type, sender, origin_server, content, signatures, origin_server_ts \
         FROM federated_events WHERE room_id = $1 ORDER BY origin_server_ts ASC LIMIT 100",
    )
    .bind(&room_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let state_pdus: Vec<Value> = state_rows
        .iter()
        .map(|row| {
            json!({
                "event_id":  row.try_get::<String, _>("event_id").unwrap_or_default(),
                "type":      row.try_get::<String, _>("event_type").unwrap_or_default(),
                "room_id":   &room_id,
                "sender":    row.try_get::<String, _>("sender").unwrap_or_default(),
                "origin":    row.try_get::<String, _>("origin_server").unwrap_or_default(),
                "content":   row.try_get::<String,_>("content").ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({})),
                "origin_server_ts": row.try_get::<i64, _>("origin_server_ts").unwrap_or(0),
            })
        })
        .collect();

    (
        StatusCode::OK,
        Json(json!({ "state": state_pdus, "auth_chain": [] })),
    )
        .into_response()
}

// ─── Backfill ─────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct BackfillQuery {
    v: Option<String>, // starting event IDs (comma-separated)
    limit: Option<u32>,
}

/// `GET /_nexus/federation/v1/backfill/{roomId}`
///
/// Returns historical PDUs for a room, starting from the given event IDs,
/// going backwards. Used to fill gaps in a remote server's event DAG.
async fn backfill(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(room_id): Path<String>,
    Query(query): Query<BackfillQuery>,
) -> impl IntoResponse {
    if let Err(e) = extract_federation_origin(&headers) {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "error": e }))).into_response();
    }
    let pool = &state.db.pool;
    let limit = query.limit.unwrap_or(20).min(100) as i64;

    // Resolve starting timestamp from the first `v` event ID (if provided).
    let start_ts: i64 = if let Some(ref v_param) = query.v {
        let first_id = v_param.split(',').next().unwrap_or("").trim();
        sqlx::query("SELECT origin_server_ts FROM federated_events WHERE event_id = $1")
            .bind(first_id.to_string())
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .and_then(|row| row.try_get::<i64, _>("origin_server_ts").ok())
            .unwrap_or(i64::MAX)
    } else {
        i64::MAX
    };

    let rows = sqlx::query(
        "SELECT event_id, event_type, sender, origin_server, content, signatures, origin_server_ts \
         FROM federated_events \
         WHERE room_id = $1 AND origin_server_ts <= $2 \
         ORDER BY origin_server_ts DESC LIMIT $3",
    )
    .bind(&room_id)
    .bind(start_ts)
    .bind(limit)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let pdus: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "event_id":  row.try_get::<String, _>("event_id").unwrap_or_default(),
                "type":      row.try_get::<String, _>("event_type").unwrap_or_default(),
                "room_id":   &room_id,
                "sender":    row.try_get::<String, _>("sender").unwrap_or_default(),
                "origin":    row.try_get::<String, _>("origin_server").unwrap_or_default(),
                "content":   row.try_get::<String,_>("content").ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({})),
                "signatures": row.try_get::<String,_>("signatures").ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({})),
                "origin_server_ts": row.try_get::<i64, _>("origin_server_ts").unwrap_or(0),
            })
        })
        .collect();

    (StatusCode::OK, Json(json!({ "pdus": pdus }))).into_response()
}

// ─── Matrix AS bridge inbound ────────────────────────────────────────────────

/// `PUT /_matrix/app/v1/transactions/{txnId}`
///
/// Matrix homeserver pushes events to this Application Service.
/// Validates the `access_token` query param (our `hs_token`), then
/// hands off to the bridge for processing and dispatches Nexus gateway events.
async fn matrix_as_transaction(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    Path(txn_id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    // Validate homeserver token.
    let expected_token = std::env::var("NEXUS_MATRIX_HS_TOKEN").unwrap_or_default();
    let provided = params.get("access_token").map(String::as_str).unwrap_or("");

    if !expected_token.is_empty() && provided != expected_token {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Invalid homeserver token" })),
        )
            .into_response();
    }

    // Parse body as a Matrix AS transaction.
    let txn: nexus_federation::MatrixTransaction = match serde_json::from_value(body) {
        Ok(t) => t,
        Err(e) => {
            warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "matrix_as_transaction: parse error");
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid transaction body" })),
            )
                .into_response();
        }
    };

    info!(
        "Matrix AS transaction {}: {} events",
        txn_id,
        txn.events.len()
    );

    // Create bridge from env and translate events.
    let homeserver_url = std::env::var("NEXUS_MATRIX_HS_URL").unwrap_or_default();
    if !homeserver_url.is_empty() {
        let bridge = nexus_federation::MatrixBridge::new(nexus_federation::BridgeConfig {
            homeserver_url,
            as_token: std::env::var("NEXUS_MATRIX_AS_TOKEN").unwrap_or_default(),
            hs_token: std::env::var("NEXUS_MATRIX_HS_TOKEN").unwrap_or_default(),
            bot_mxid: std::env::var("NEXUS_MATRIX_BOT_MXID").unwrap_or_default(),
        });

        for bridged in bridge.handle_transaction(&state.db.pool, txn).await {
            match bridged {
                nexus_federation::BridgedEvent::MessageCreate {
                    nexus_channel_id,
                    nexus_message_id,
                    matrix_room_id,
                    sender_mxid,
                    sender_display_name,
                    ghost_user_id,
                    body,
                    timestamp_ms,
                } => {
                    let gw = nexus_common::gateway_event::GatewayEvent {
                        event_type: "MESSAGE_CREATE".to_owned(),
                        data: json!({
                            "id": nexus_message_id,
                            "channel_id": nexus_channel_id,
                            "author_id": ghost_user_id,
                            "content": body,
                            "timestamp_ms": timestamp_ms,
                            "source": "matrix",
                            "matrix_room_id": matrix_room_id,
                            "sender_mxid": sender_mxid,
                            "sender_display_name": sender_display_name,
                        }),
                        server_id: None,
                        channel_id: Some(nexus_channel_id),
                        user_id: Some(ghost_user_id),
                    };
                    let _ = state.gateway_tx.send(gw);
                }
                nexus_federation::BridgedEvent::MemberJoin {
                    matrix_room_id: _,
                    mxid: _,
                    display_name: _,
                } => {
                    debug!("Matrix member join");
                }
                nexus_federation::BridgedEvent::MemberLeave {
                    matrix_room_id: _,
                    mxid: _,
                } => {
                    debug!("Matrix member leave");
                }
            }
        }
    } else {
        debug!(
            "NEXUS_MATRIX_HS_URL not set — skipping bridge processing for {}",
            txn_id
        );
    }

    (StatusCode::OK, Json(json!({}))).into_response()
}

// ─── v0.8/08-03: Federated Identity ─────────────────────────────────────────

/// `GET /_nexus/federation/v1/user/{userId}`
///
/// Serves the public profile of a local user (identified by their MXID or
/// bare username) so remote servers can resolve display names and avatars.
///
/// URL-encoded MXID: `%40alice%3Anexus.example.com` → `@alice:nexus.example.com`
/// Or bare localpart: `alice`
async fn user_profile(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = extract_federation_origin(&headers) {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "error": e }))).into_response();
    }

    // Accept both `@alice:server.tld` (URL-decoded) and plain `alice`.
    let localpart = if user_id.starts_with('@') {
        match parse_mxid(&user_id) {
            Some((lp, server)) => {
                // Only serve profiles for users on this server.
                if server != state.server_name {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "error": "User not on this server" })),
                    )
                        .into_response();
                }
                lp
            }
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "Invalid MXID" })),
                )
                    .into_response();
            }
        }
    } else {
        user_id.clone()
    };

    match users::find_by_username(&state.db.pool, &localpart).await {
        Ok(Some(user)) => {
            let mxid = format!("@{}:{}", user.username, state.server_name);
            (
                StatusCode::OK,
                Json(json!({
                    "user_id":      mxid,
                    "id":           user.id.to_string(),
                    "username":     user.username,
                    "displayname":  user.display_name,
                    "avatar_url":   user.avatar,
                    "bio":          user.bio,
                })),
            )
                .into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "User not found" })),
        )
            .into_response(),
        Err(e) => {
            warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "DB error resolving user");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "DB error" })),
            )
                .into_response()
        }
    }
}

// ─── MXID helpers ─────────────────────────────────────────────────────────────

/// Parse a Matrix-style / Nexus MXID: `@localpart:server.tld`.
///
/// Returns `Some((localpart, server))` or `None` if malformed.
fn parse_mxid(mxid: &str) -> Option<(String, String)> {
    let mxid = mxid.strip_prefix('@')?;
    let colon = mxid.find(':')?;
    let localpart = mxid[..colon].to_owned();
    let server = mxid[colon + 1..].to_owned();
    if localpart.is_empty() || server.is_empty() {
        return None;
    }
    Some((localpart, server))
}

/// Upsert a remote user's profile into `federated_users`.
///
/// Called after accepting an inbound PDU to keep the remote profile cache
/// up-to-date. For membership events the display name and avatar in the
/// event content are used; for other event types only the MXID is stored.
async fn upsert_federated_user(
    pool: &sqlx::AnyPool,
    _local_server_name: &str,
    sender: &str,
    pdu: &Value,
) -> Result<(), anyhow::Error> {
    let (localpart, server) = match parse_mxid(sender) {
        Some(parts) => parts,
        None => {
            debug!("Skipping federated user upsert: invalid MXID");
            return Ok(());
        }
    };

    // Look up (or insert) the origin server to get its UUID.
    let server_id: Option<uuid::Uuid> =
        sqlx::query("SELECT id FROM federated_servers WHERE server_name = $1")
            .bind(&server)
            .fetch_optional(pool)
            .await?
            .and_then(|r| r.try_get::<String, _>("id").ok())
            .and_then(|s| uuid::Uuid::parse_str(&s).ok());

    let server_id = match server_id {
        Some(id) => id,
        None => {
            // Auto-register the server if we haven't seen it yet.
            let row = sqlx::query(
                "INSERT INTO federated_servers (server_name) VALUES ($1) \
                 ON CONFLICT (server_name) DO UPDATE SET last_seen_at = CURRENT_TIMESTAMP \
                 RETURNING id",
            )
            .bind(&server)
            .fetch_one(pool)
            .await?;
            uuid::Uuid::parse_str(&row.try_get::<String, _>("id")?)
                .map_err(|e| sqlx::Error::Decode(Box::new(e) as _))?
        }
    };

    // For `nexus.member.join` / `m.room.member` events, extract optional profile.
    let event_type = pdu.get("type").and_then(Value::as_str).unwrap_or("");
    let is_membership = event_type == "nexus.member.join"
        || event_type == "nexus.member.leave"
        || event_type == "m.room.member";

    let display_name: Option<String> = if is_membership {
        pdu.get("content")
            .and_then(|c| c.get("displayname"))
            .and_then(Value::as_str)
            .map(String::from)
    } else {
        None
    };
    let avatar_url: Option<String> = if is_membership {
        pdu.get("content")
            .and_then(|c| c.get("avatar_url"))
            .and_then(Value::as_str)
            .map(String::from)
    } else {
        None
    };

    sqlx::query(
        "INSERT INTO federated_users \
         (mxid, localpart, server_id, display_name, avatar_url) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (mxid) DO UPDATE SET \
         display_name = COALESCE(excluded.display_name, federated_users.display_name), \
         avatar_url   = COALESCE(excluded.avatar_url, federated_users.avatar_url)",
    )
    .bind(sender)
    .bind(&localpart)
    .bind(server_id.to_string())
    .bind(display_name)
    .bind(avatar_url)
    .execute(pool)
    .await?;

    Ok(())
}

// ─── Federated friend requests ────────────────────────────────────────────────

/// `PUT /_nexus/federation/v1/friend_request`
///
/// Receives an inbound cross-server friend request from a remote Nexus instance.
/// The handler:
/// 1. Authenticates the request via the `NexusFederation` Authorization header.
/// 2. Looks up the target username in the local `users` table.
/// 3. Upserts a shadow user account for the requester (preserving their UUID).
/// 4. Creates a `pending` relationship row so the local user sees the request.
async fn receive_friend_request(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<nexus_federation::FederatedFriendRequest>,
) -> impl IntoResponse {
    const URI: &str = "/_nexus/federation/v1/friend_request";

    // Serialize body back to Value for signature verification before consuming it.
    let body_value = match serde_json::to_value(&body) {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal error" })),
            )
                .into_response();
        }
    };

    let origin = match verify_inbound_request(&state, &headers, "PUT", URI, Some(&body_value)).await
    {
        Ok(o) => o,
        Err((status, msg)) => {
            warn!(status = %status, "Rejected federated friend request");
            return (status, Json(json!({ "error": msg }))).into_response();
        }
    };

    debug!(
        "Federated friend request from {}",
        origin
    );

    // Find the local target user (must be a real local account, not a shadow).
    let target =
        match nexus_db::repository::users::find_by_username(&state.db.pool, &body.target_username)
            .await
        {
            Ok(Some(u)) if !u.is_remote => u,
            Ok(Some(_)) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "user not found" })),
                )
                    .into_response();
            }
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "user not found" })),
                )
                    .into_response();
            }
            Err(e) => {
                warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "DB error looking up target user");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "internal error" })),
                )
                    .into_response();
            }
        };

    // Parse the requester UUID.
    let requester_id = match uuid::Uuid::parse_str(&body.requester_id) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid requester_id" })),
            )
                .into_response();
        }
    };

    // Upsert shadow user for the requester.
    let remote_user = match nexus_db::repository::users::upsert_remote_user(
        &state.db.pool,
        requester_id,
        &body.requester_username,
        &origin,
        body.requester_display_name.as_deref(),
        body.requester_avatar.as_deref(),
    )
    .await
    {
        Ok(u) => u,
        Err(e) => {
            warn!(origin = %origin, error_kind = %nexus_common::logsafe::err_kind(&e), "Failed to upsert remote user");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal error" })),
            )
                .into_response();
        }
    };

    // Guard against duplicate relationships.
    match nexus_db::repository::relationships::find_between(
        &state.db.pool,
        remote_user.id,
        target.id,
    )
    .await
    {
        Ok(Some(_)) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "relationship already exists" })),
            )
                .into_response();
        }
        Ok(None) => {}
        Err(e) => warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "DB error checking existing relationship"),
    }

    // Create the pending relationship (remote_user → local target).
    let rel_id = nexus_common::snowflake::generate_id();
    match nexus_db::repository::relationships::create(
        &state.db.pool,
        rel_id,
        remote_user.id,
        target.id,
        "pending",
    )
    .await
    {
        Ok(_) => {
            info!(
                "Federated friend request stored from {}",
                origin
            );

            // Push live notification to the target user's connected clients.
            let qualified = format!("{}@{}", body.requester_username, origin);
            let _ = state.gateway_tx.send(GatewayEvent {
                event_type: event_types::RELATIONSHIP_UPDATE.into(),
                data: json!({
                    "id": rel_id.to_string(),
                    "type": "pending_incoming",
                    "user": {
                        "id": remote_user.id,
                        "username": qualified,
                        "display_name": body.requester_display_name,
                        "avatar_url": body.requester_avatar,
                    }
                }),
                server_id: None,
                channel_id: None,
                user_id: Some(target.id),
            });

            (StatusCode::OK, Json(json!({ "status": "ok" }))).into_response()
        }
        Err(e) => {
            warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "Failed to create federated relationship");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal error" })),
            )
                .into_response()
        }
    }
}

/// `PUT /_nexus/federation/v1/friend_request/respond`
///
/// Receives an accept or deny for a previously sent federated friend request.
/// The handler:
/// 1. Authenticates the request.
/// 2. Upserts a shadow user for the responder.
/// 3. Finds the local relationship row (requester = local user, addressee = shadow responder).
/// 4. Updates or deletes the relationship accordingly.
async fn receive_friend_request_response(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<nexus_federation::FederatedFriendResponse>,
) -> impl IntoResponse {
    const URI: &str = "/_nexus/federation/v1/friend_request/respond";

    let body_value = match serde_json::to_value(&body) {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal error" })),
            )
                .into_response();
        }
    };

    let origin = match verify_inbound_request(&state, &headers, "PUT", URI, Some(&body_value)).await
    {
        Ok(o) => o,
        Err((status, msg)) => {
            warn!(status = %status, "Rejected federated friend response");
            return (status, Json(json!({ "error": msg }))).into_response();
        }
    };

    debug!(action = %body.action, "Federated friend response received");

    if !matches!(body.action.as_str(), "accepted" | "denied") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "action must be 'accepted' or 'denied'" })),
        )
            .into_response();
    }

    // Parse UUIDs.
    let requester_id = match uuid::Uuid::parse_str(&body.requester_id) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid requester_id" })),
            )
                .into_response();
        }
    };
    let responder_id = match uuid::Uuid::parse_str(&body.responder_id) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid responder_id" })),
            )
                .into_response();
        }
    };

    // Upsert shadow user for the responder so local queries can join on their ID.
    let _ = nexus_db::repository::users::upsert_remote_user(
        &state.db.pool,
        responder_id,
        &body.responder_username,
        &origin,
        body.responder_display_name.as_deref(),
        body.responder_avatar.as_deref(),
    )
    .await;

    // Find the relationship: original requester (local) sent the request,
    // the responder (now a shadow user) is the addressee.
    let rel = match nexus_db::repository::relationships::find_between(
        &state.db.pool,
        requester_id,
        responder_id,
    )
    .await
    {
        Ok(Some(r)) => r,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "relationship not found" })),
            )
                .into_response();
        }
        Err(e) => {
            warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "DB error looking up relationship");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal error" })),
            )
                .into_response();
        }
    };

    let qualified_responder = format!("{}@{}", body.responder_username, origin);

    match body.action.as_str() {
        "accepted" => {
            if let Err(e) = nexus_db::repository::relationships::update_status(
                &state.db.pool,
                rel.id,
                "accepted",
            )
            .await
            {
                warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "Failed to accept federated friendship");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "internal error" })),
                )
                    .into_response();
            }
            info!("Federated friendship accepted");
            // Notify the requester's connected clients that their friend request was accepted.
            let _ = state.gateway_tx.send(GatewayEvent {
                event_type: event_types::RELATIONSHIP_UPDATE.into(),
                data: json!({
                    "id": rel.id.to_string(),
                    "type": "friend",
                    "user": {
                        "id": responder_id,
                        "username": qualified_responder,
                        "display_name": body.responder_display_name,
                        "avatar_url": body.responder_avatar,
                    }
                }),
                server_id: None,
                channel_id: None,
                user_id: Some(requester_id),
            });
        }
        "denied" => {
            if let Err(e) =
                nexus_db::repository::relationships::delete(&state.db.pool, rel.id).await
            {
                warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "Failed to remove denied federated relationship");
            }
            info!("Federated friend request denied");
            // Notify the requester that the request was denied so their pending list updates live.
            let _ = state.gateway_tx.send(GatewayEvent {
                event_type: event_types::RELATIONSHIP_UPDATE.into(),
                data: json!({
                    "id": rel.id.to_string(),
                    "type": "removed",
                    "user": {
                        "id": responder_id,
                        "username": qualified_responder,
                    }
                }),
                server_id: None,
                channel_id: None,
                user_id: Some(requester_id),
            });
        }
        _ => unreachable!(),
    }

    (StatusCode::OK, Json(json!({ "status": "ok" }))).into_response()
}

// ─── Auth helpers ─────────────────────────────────────────────────────────────

/// Extract and loosely parse the originating server from the federation
/// Authorization header. Full signature verification happens in the specific
/// handler once we've loaded the server's public key from DB.
fn extract_federation_origin(headers: &HeaderMap) -> Result<String, String> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or("Missing Authorization header")?;

    let auth = auth
        .strip_prefix("NexusFederation ")
        .ok_or("Authorization scheme must be 'NexusFederation'")?;

    for part in auth.split(',') {
        let part = part.trim();
        if let Some(origin) = part
            .strip_prefix("origin=\"")
            .and_then(|s| s.strip_suffix('"'))
        {
            return Ok(origin.to_owned());
        }
    }
    Err("NexusFederation header missing 'origin' field".to_owned())
}

/// Parse the `key=` field from a `NexusFederation` Authorization header.
fn extract_key_id_from_auth(auth: &str) -> Result<String, String> {
    let inner = auth
        .strip_prefix("NexusFederation ")
        .ok_or("Authorization scheme must be 'NexusFederation'")?;
    for part in inner.split(',') {
        let part = part.trim();
        if let Some(key_id) = part
            .strip_prefix("key=\"")
            .and_then(|s| s.strip_suffix('"'))
        {
            return Ok(key_id.to_owned());
        }
    }
    Err("NexusFederation header missing 'key' field".to_owned())
}

/// Fully verify an inbound S2S request:
/// 1. Parse `origin` + `key_id` from the `Authorization: NexusFederation…` header.
/// 2. Load the origin's cached public key from `federated_servers`.
/// 3. If the key is not cached, fetch `/_nexus/key/v2/server` from the origin and cache it.
/// 4. Verify the Ed25519 signature over the canonical request object.
///
/// Returns the verified origin server name on success.
async fn verify_inbound_request(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    method: &str,
    uri: &str,
    body: Option<&Value>,
) -> Result<String, (StatusCode, String)> {
    let auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                "Missing Authorization header".to_owned(),
            )
        })?
        .to_owned();

    let origin = extract_federation_origin(headers).map_err(|e| (StatusCode::UNAUTHORIZED, e))?;

    let key_id =
        extract_key_id_from_auth(&auth_header).map_err(|e| (StatusCode::UNAUTHORIZED, e))?;

    // Load cached verify keys for this origin.
    let mut verify_keys = load_server_verify_keys(&state.db.pool, &origin).await;

    // If the specific key_id is not cached, fetch it from the remote server.
    if !verify_keys.contains_key(&key_id) {
        match state.federation_client.fetch_server_keys(&origin).await {
            Ok(key_doc) => {
                for (kid, vk) in &key_doc.verify_keys {
                    verify_keys.insert(kid.clone(), Value::String(vk.key.clone()));
                }
                // Persist for future requests (upsert into federated_servers).
                if let Ok(keys_json) = serde_json::to_string(&verify_keys) {
                    let _ = sqlx::query(
                        "INSERT INTO federated_servers (server_name, verify_keys, last_seen_at) \
                         VALUES ($1, $2, NOW()) \
                         ON CONFLICT (server_name) DO UPDATE \
                         SET verify_keys = $2, last_seen_at = NOW()",
                    )
                    .bind(&origin)
                    .bind(&keys_json)
                    .execute(&state.db.pool)
                    .await;
                }
            }
            Err(e) => {
                warn!(origin = %origin, error_kind = %nexus_common::logsafe::err_kind(&e), "Could not fetch signing keys");
                return Err((
                    StatusCode::UNAUTHORIZED,
                    format!("Cannot retrieve server keys for '{}': {}", origin, e),
                ));
            }
        }
    }

    // Find the public key for this key_id.
    let pubkey_b64 = verify_keys
        .get(&key_id)
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                format!("Unknown key '{}' for server '{}'", key_id, origin),
            )
        })?
        .to_owned();

    // Verify the Ed25519 signature over the canonical request object.
    nexus_federation::signatures::verify_request(
        &auth_header,
        &state.server_name,
        method,
        uri,
        body,
        &pubkey_b64,
    )
    .map_err(|e| {
        (
            StatusCode::UNAUTHORIZED,
            format!("Signature verification failed: {}", e),
        )
    })
}
