//! Matrix Application Service (AS) bridge — full implementation.
//!
//! The bridge acts as a Matrix AS registered on a Matrix homeserver
//! (Synapse, Conduit, etc.). It translates between:
//!
//! - **Nexus → Matrix**: when a Nexus user posts in a bridged channel the
//!   bridge relays the message to the Matrix room via the CS API.
//! - **Matrix → Nexus**: the homeserver pushes new Matrix events to this
//!   bridge via `PUT /_matrix/app/v1/transactions/{txnId}`. The bridge
//!   converts them to Nexus messages (stored in the DB) and dispatches
//!   gateway `MESSAGE_CREATE` events so all connected clients are notified.
//!
//! # Registration
//!
//! A `registration.yaml` file (provided to the Matrix homeserver) must set:
//! - `url`: public URL of the Nexus server (e.g. `https://nexus.example.com`)
//! - `as_token`: matches `NEXUS_MATRIX_AS_TOKEN` env var
//! - `hs_token`: matches `NEXUS_MATRIX_HS_TOKEN` env var
//! - `namespaces.users`: `@matrix_.*:nexus.server` — ghost user pattern
//!
//! # Environment variables
//!
//! | Variable                  | Description                                              |
//! |---------------------------|----------------------------------------------------------|
//! | `NEXUS_MATRIX_HS_URL`     | Matrix homeserver base URL                               |
//! | `NEXUS_MATRIX_AS_TOKEN`   | Token this AS sends to the HS                            |
//! | `NEXUS_MATRIX_HS_TOKEN`   | Token the HS sends to this AS (validated on every push)  |
//! | `NEXUS_MATRIX_BOT_MXID`  | `@bot:nexus.example.com` — ghost relay user              |

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uuid::Uuid;

// ─── Types ───────────────────────────────────────────────────────────────────

/// A Matrix homeserver transaction pushed to the AS.
///
/// `PUT /_matrix/app/v1/transactions/{txnId}`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixTransaction {
    pub events: Vec<MatrixEvent>,
}

/// A stripped Matrix client event as received from the homeserver.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub room_id: String,
    pub sender: String,
    pub origin_server_ts: i64,
    pub content: serde_json::Value,
    #[serde(default)]
    pub unsigned: serde_json::Value,
    pub event_id: Option<String>,
}

/// Payload for sending a message to a Matrix room via the CS API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixMessageContent {
    #[serde(rename = "msgtype")]
    pub msgtype: String,
    pub body: String,
    /// Formatted HTML body (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub formatted_body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

// ─── Bridge configuration ─────────────────────────────────────────────────────

/// Configuration for the Matrix AS bridge.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// The Matrix homeserver base URL (e.g. `https://matrix.org`).
    pub homeserver_url: String,
    /// Application service token (sent to HS in every AS request).
    pub as_token: String,
    /// Homeserver token (sent from HS to AS, used for auth validation).
    pub hs_token: String,
    /// `@bot:server.tld` — the ghost user used to relay Nexus messages into Matrix.
    pub bot_mxid: String,
}

impl BridgeConfig {
    /// Load configuration from environment variables.
    ///
    /// Returns `None` if `NEXUS_MATRIX_HS_URL` is not set (bridge disabled).
    #[must_use] 
    pub fn from_env() -> Option<Self> {
        let homeserver_url = std::env::var("NEXUS_MATRIX_HS_URL").ok()?;
        if homeserver_url.is_empty() {
            return None;
        }
        Some(BridgeConfig {
            homeserver_url,
            as_token: std::env::var("NEXUS_MATRIX_AS_TOKEN").unwrap_or_default(),
            hs_token: std::env::var("NEXUS_MATRIX_HS_TOKEN").unwrap_or_default(),
            bot_mxid: std::env::var("NEXUS_MATRIX_BOT_MXID").unwrap_or_default(),
        })
    }
}

// ─── Bridge ──────────────────────────────────────────────────────────────────

/// Matrix Application Service bridge.
pub struct MatrixBridge {
    pub config: BridgeConfig,
    http: reqwest::Client,
}

impl MatrixBridge {
    /// Create a new bridge with the given configuration.
    ///
    /// # Panics
    /// Panics if the internal HTTP client cannot be constructed.
    #[must_use] 
    pub fn new(config: BridgeConfig) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("Nexus-Federation/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("failed to build Matrix bridge http client");
        Self { config, http }
    }

    // ── Inbound (Matrix → Nexus) ────────────────────────────────────────────

    /// Handle an inbound transaction from the Matrix homeserver.
    ///
    /// For each recognised event this method:
    /// 1. Resolves the Matrix room to a Nexus channel via the DB.
    /// 2. Finds or creates a ghost Nexus user for the Matrix sender.
    /// 3. Stores the message in the Nexus DB.
    /// 4. Returns [`BridgedEvent`] items so the caller can fire gateway events.
    ///
    /// Called from `PUT /_matrix/app/v1/transactions/{txnId}`.
    pub async fn handle_transaction(
        &self,
        pool: &sqlx::AnyPool,
        txn: MatrixTransaction,
    ) -> Vec<BridgedEvent> {
        let mut out = Vec::new();

        for ev in txn.events {
            match ev.event_type.as_str() {
                "m.room.message" => match self.handle_matrix_message(pool, &ev).await {
                    Ok(Some(bridged)) => out.push(bridged),
                    Ok(None) => {}
                    Err(e) => {
                        warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "Bridge: failed to handle m.room.message");
                    }
                },
                "m.room.member" => {
                    if let Err(e) = self.handle_matrix_member(pool, &ev).await {
                        warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "Bridge: failed to handle m.room.member");
                    }
                }
                other => {
                    debug!("Bridge: ignoring unrecognised Matrix event type: {}", other);
                }
            }
        }

        out
    }

    /// Handle a `m.room.message` event from Matrix.
    async fn handle_matrix_message(
        &self,
        pool: &sqlx::AnyPool,
        ev: &MatrixEvent,
    ) -> Result<Option<BridgedEvent>, BridgeError> {
        let body = ev
            .content
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();

        let msgtype = ev
            .content
            .get("msgtype")
            .and_then(|v| v.as_str())
            .unwrap_or("m.text");

        if !matches!(msgtype, "m.text" | "m.notice" | "m.emote") {
            debug!("Bridge: skipping msgtype={}", msgtype);
            return Ok(None);
        }

        // Resolve room → Nexus channel.
        let mapping = nexus_db::repository::matrix_bridge::get_channel_for_room(pool, &ev.room_id)
            .await
            .map_err(|e| BridgeError::Database(e.to_string()))?;

        let Some(mapping) = mapping else {
            debug!("Bridge: no channel mapped for room");
            return Ok(None);
        };

        let channel_id = mapping.channel_id;

        // Resolve MXID → ghost user.
        let display_name = ev.content.get("displayname").and_then(|v| v.as_str());
        let avatar_url = ev.content.get("avatar_url").and_then(|v| v.as_str());

        let ghost = nexus_db::repository::matrix_bridge::find_or_create_ghost(
            pool,
            &ev.sender,
            display_name,
            avatar_url,
        )
        .await
        .map_err(|e| BridgeError::Database(e.to_string()))?;

        // Store message in Nexus DB.
        let message_id = nexus_common::snowflake::generate_id();
        nexus_db::repository::messages::create_message(
            pool,
            message_id,
            channel_id,
            ghost.id,
            &body,
            0, // message_type: normal
            None,
            None,
            &[],
            &[],
            false,
        )
        .await
        .map_err(|e| BridgeError::Database(e.to_string()))?;

        info!(channel_id = %channel_id, message_id = %message_id, "Bridge: Matrix message relayed to nexus channel");

        Ok(Some(BridgedEvent::MessageCreate {
            nexus_channel_id: channel_id,
            nexus_message_id: message_id,
            matrix_room_id: ev.room_id.clone(),
            sender_mxid: ev.sender.clone(),
            sender_display_name: ghost.display_name.clone(),
            ghost_user_id: ghost.id,
            body,
            timestamp_ms: ev.origin_server_ts,
        }))
    }

    /// Handle a `m.room.member` event — find or create ghost user.
    async fn handle_matrix_member(
        &self,
        pool: &sqlx::AnyPool,
        ev: &MatrixEvent,
    ) -> Result<(), BridgeError> {
        let membership = ev
            .content
            .get("membership")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        if membership == "join" {
            let display_name = ev.content.get("displayname").and_then(|v| v.as_str());
            let avatar_url = ev
                .content
                .get("avatar_url")
                .and_then(|v| v.as_str())
                .filter(|u| u.starts_with("mxc://"));

            nexus_db::repository::matrix_bridge::find_or_create_ghost(
                pool,
                &ev.sender,
                display_name,
                avatar_url,
            )
            .await
            .map_err(|e| BridgeError::Database(e.to_string()))?;

            info!("Bridge: ghost user updated/created");
        } else {
            debug!(
                "Bridge: m.room.member membership={} (no action)",
                membership
            );
        }

        Ok(())
    }

    // ── Outbound (Nexus → Matrix) ───────────────────────────────────────────

    /// Relay a Nexus message to the Matrix room mapped to `channel_id`.
    ///
    /// Looks up the room mapping in the DB; if no mapping exists, returns
    /// `Err(BridgeError::RoomNotFound)` which the caller should silently
    /// ignore for non-bridged channels.
    ///
    /// # Errors
    /// Returns an error if database lookups fail, no room mapping exists, or
    /// the outbound Matrix request fails.
    pub async fn relay_to_matrix(
        &self,
        pool: &sqlx::AnyPool,
        channel_id: Uuid,
        message_id: Uuid,
        display_name: &str,
        body: &str,
    ) -> Result<(), BridgeError> {
        let mapping = nexus_db::repository::matrix_bridge::get_room_for_channel(pool, channel_id)
            .await
            .map_err(|e| BridgeError::Database(e.to_string()))?
            .ok_or_else(|| BridgeError::RoomNotFound(channel_id.to_string()))?;

        // Use per-room token if configured, fall back to global token.
        let token = if mapping.as_token.is_empty() {
            &self.config.as_token
        } else {
            &mapping.as_token
        };

        // Stable txnId: Nexus message UUID (idempotent on retry).
        self.send_to_matrix(
            &mapping.matrix_room_id,
            token,
            &message_id.simple().to_string(),
            display_name,
            body,
        )
        .await
    }

    /// Low-level: PUT a message to a Matrix room via the CS API.
    ///
    /// # Errors
    /// Returns an error if the HTTP request fails or the homeserver returns a
    /// non-success response.
    pub async fn send_to_matrix(
        &self,
        room_id: &str,
        token: &str,
        txn_id: &str,
        display_name: &str,
        body: &str,
    ) -> Result<(), BridgeError> {
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            self.config.homeserver_url,
            urlencoded(room_id),
            txn_id
        );

        let content = MatrixMessageContent {
            msgtype: "m.text".to_owned(),
            body: format!("{display_name}: {body}"),
            formatted_body: Some(format!(
                "<b>{}</b>: {}",
                html_escape(display_name),
                html_escape(body)
            )),
            format: Some("org.matrix.custom.html".to_owned()),
        };

        let resp = self
            .http
            .put(&url)
            .header("Authorization", format!("Bearer {token}"))
            .json(&content)
            .send()
            .await
            .map_err(|e| BridgeError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(BridgeError::HomeserverError(status.as_u16(), body));
        }

        info!("Bridge: relayed message to Matrix room");
        Ok(())
    }

    // ── Room alias management ───────────────────────────────────────────────

    /// Create a room alias on the Matrix homeserver for a Nexus channel.
    ///
    /// # Errors
    /// Returns an error if the HTTP request fails or the homeserver returns a
    /// non-success response.
    pub async fn create_room_alias(&self, room_id: &str, alias: &str) -> Result<(), BridgeError> {
        let url = format!(
            "{}/_matrix/client/v3/directory/room/{}",
            self.config.homeserver_url,
            urlencoded(alias)
        );

        let resp = self
            .http
            .put(&url)
            .header("Authorization", format!("Bearer {}", self.config.as_token))
            .json(&serde_json::json!({ "room_id": room_id }))
            .send()
            .await
            .map_err(|e| BridgeError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(BridgeError::HomeserverError(status.as_u16(), body));
        }

        info!("Bridge: created room alias");
        Ok(())
    }

    /// Resolve a room alias on the homeserver → room ID.
    ///
    /// # Errors
    /// Returns an error if the HTTP request fails, the homeserver returns a
    /// non-success response, or the JSON payload is invalid.
    pub async fn resolve_room_alias(&self, alias: &str) -> Result<String, BridgeError> {
        let url = format!(
            "{}/_matrix/client/v3/directory/room/{}",
            self.config.homeserver_url,
            urlencoded(alias)
        );

        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.config.as_token))
            .send()
            .await
            .map_err(|e| BridgeError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(BridgeError::HomeserverError(status.as_u16(), body));
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| BridgeError::Http(e.to_string()))?;

        json["room_id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| BridgeError::Http("missing room_id in alias response".into()))
    }
}

// ─── Bridged event ────────────────────────────────────────────────────────────

/// A normalised event produced by the bridge for Nexus to consume.
#[derive(Debug, Clone)]
pub enum BridgedEvent {
    MessageCreate {
        /// Nexus channel that the message lives in.
        nexus_channel_id: Uuid,
        /// Nexus message ID (snowflake) just created in the DB.
        nexus_message_id: Uuid,
        /// Originating Matrix room.
        matrix_room_id: String,
        /// Full MXID of the sender (`@user:server`).
        sender_mxid: String,
        /// Display name of the sender (from ghost user table).
        sender_display_name: Option<String>,
        /// Ghost Nexus user ID that authored the DB message.
        ghost_user_id: Uuid,
        /// Plain-text body.
        body: String,
        /// Matrix `origin_server_ts`.
        timestamp_ms: i64,
    },
    MemberJoin {
        matrix_room_id: String,
        mxid: String,
        display_name: Option<String>,
    },
    MemberLeave {
        matrix_room_id: String,
        mxid: String,
    },
}

// ─── Bridge error ─────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("Matrix homeserver returned {0}: {1}")]
    HomeserverError(u16, String),
    #[error("Room not found for channel '{0}'")]
    RoomNotFound(String),
    #[error("Database error: {0}")]
    Database(String),
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn urlencoded(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Convert a Matrix MXID to a stable Nexus username for ghost users.
///
/// e.g. `@alice:matrix.org` → `matrix_alice_matrix.org`
#[must_use] 
pub fn mxid_to_username(mxid: &str) -> String {
    // Strip leading '@'
    let without_at = mxid.trim_start_matches('@');
    // Replace ':' (only the first occurrence — server separator) with '_'
    let username = without_at.replacen(':', "_", 1);
    format!("matrix_{username}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── mxid_to_username ──────────────────────────────────────────────────────

    #[test]
    fn converts_standard_mxid_to_username() {
        assert_eq!(
            mxid_to_username("@alice:matrix.org"),
            "matrix_alice_matrix.org"
        );
    }

    #[test]
    fn handles_mxid_without_leading_at() {
        // trim_start_matches('@') is idempotent
        assert_eq!(
            mxid_to_username("alice:matrix.org"),
            "matrix_alice_matrix.org"
        );
    }

    #[test]
    fn replaces_only_first_colon() {
        // Colons after the first (e.g. port numbers) should stay
        assert_eq!(
            mxid_to_username("@bot:server.tld:8448"),
            "matrix_bot_server.tld:8448"
        );
    }

    #[test]
    fn preserves_dots_in_server_name() {
        // replace('.', '.') is a no-op — dots stay
        let result = mxid_to_username("@user:my.nexus.server");
        assert!(result.contains('.'), "dots should be preserved");
    }

    #[test]
    fn always_prefixed_with_matrix_underscore() {
        let result = mxid_to_username("@anyone:anywhere");
        assert!(result.starts_with("matrix_"));
    }

    // ── html_escape ───────────────────────────────────────────────────────────

    #[test]
    fn ampersand_is_escaped() {
        assert_eq!(html_escape("A & B"), "A &amp; B");
    }

    #[test]
    fn angle_brackets_are_escaped() {
        assert_eq!(html_escape("<script>"), "&lt;script&gt;");
    }

    #[test]
    fn double_quote_is_escaped() {
        assert_eq!(html_escape("say \"hi\""), "say &quot;hi&quot;");
    }

    #[test]
    fn plain_text_unchanged() {
        assert_eq!(html_escape("hello world"), "hello world");
    }

    #[test]
    fn multiple_special_chars_all_escaped() {
        let result = html_escape("<b>bold & \"quoted\"</b>");
        assert_eq!(result, "&lt;b&gt;bold &amp; &quot;quoted&quot;&lt;/b&gt;");
    }

    #[test]
    fn empty_string_stays_empty() {
        assert_eq!(html_escape(""), "");
    }

    // ── urlencoded ────────────────────────────────────────────────────────────

    #[test]
    fn plain_ascii_is_unchanged() {
        assert_eq!(urlencoded("hello"), "hello");
    }

    #[test]
    fn spaces_become_plus_or_percent20() {
        // form_urlencoded encodes spaces as '+'
        let r = urlencoded("hello world");
        assert!(
            r.contains('+') || r.contains("%20"),
            "space must be encoded"
        );
    }

    #[test]
    fn special_chars_are_percent_encoded() {
        let r = urlencoded("foo@bar.com");
        // '@' must be encoded
        assert!(!r.contains('@'), "@ must be percent-encoded, got: {r}");
    }

    #[test]
    fn empty_string_stays_empty_urlencoded() {
        assert_eq!(urlencoded(""), "");
    }
}
