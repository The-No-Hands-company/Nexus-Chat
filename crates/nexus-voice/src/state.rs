//! Voice state management — tracks who's in which voice channel.
//!
//! This is the authoritative source of truth for voice connections:
//! - Which users are in which voice channels
//! - Mute/deaf/video/screen share state per user
//! - Server-side mute/deaf (moderation)
//!
//! State is held in-memory for speed and mirrored to Redis for
//! multi-node coordination in production.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// Voice state for a single user's voice connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceState {
    pub user_id: Uuid,
    pub channel_id: Uuid,
    pub server_id: Option<Uuid>,
    pub session_id: String,
    pub self_mute: bool,
    pub self_deaf: bool,
    pub server_mute: bool,
    pub server_deaf: bool,
    pub self_video: bool,
    pub self_stream: bool,
    pub suppress: bool,
    pub speaking: bool,
    pub connected_at: DateTime<Utc>,
}

/// Request to update voice state.
#[derive(Debug, Clone, Deserialize)]
pub struct VoiceStateUpdate {
    pub self_mute: Option<bool>,
    pub self_deaf: Option<bool>,
    pub self_video: Option<bool>,
    pub self_stream: Option<bool>,
}

/// Request from a moderator to server-mute/deaf a user.
#[derive(Debug, Clone, Deserialize)]
pub struct VoiceModAction {
    pub target_user_id: Uuid,
    pub server_mute: Option<bool>,
    pub server_deaf: Option<bool>,
}

/// Manages voice state across all channels.
///
/// Two indexes for fast lookups:
/// - `by_user`: user_id → VoiceState (quick "where is this user?")
/// - `by_channel`: channel_id → [user_id] (quick "who's in this channel?")
#[derive(Clone)]
pub struct VoiceStateManager {
    by_user: Arc<RwLock<HashMap<Uuid, VoiceState>>>,
    by_channel: Arc<RwLock<HashMap<Uuid, Vec<Uuid>>>>,
}

impl VoiceStateManager {
    pub fn new() -> Self {
        Self {
            by_user: Arc::new(RwLock::new(HashMap::new())),
            by_channel: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// User joins a voice channel. If already in another channel, leaves it first.
    /// Returns (new_state, Option<old_channel_id>).
    pub async fn join(
        &self,
        user_id: Uuid,
        channel_id: Uuid,
        server_id: Option<Uuid>,
        session_id: String,
    ) -> (VoiceState, Option<Uuid>) {
        let old_channel = self.leave(user_id).await;

        let state = VoiceState {
            user_id,
            channel_id,
            server_id,
            session_id,
            self_mute: false,
            self_deaf: false,
            server_mute: false,
            server_deaf: false,
            self_video: false,
            self_stream: false,
            suppress: false,
            speaking: false,
            connected_at: Utc::now(),
        };

        // Add to user index
        self.by_user.write().await.insert(user_id, state.clone());

        // Add to channel index
        self.by_channel
            .write()
            .await
            .entry(channel_id)
            .or_default()
            .push(user_id);

        tracing::info!(
            channel = %channel_id,
            server = ?server_id,
            "User joined voice channel"
        );
        metrics::gauge!("nexus_voice_users_active").increment(1.0);

        (state, old_channel)
    }

    /// User leaves their current voice channel. Returns the channel they left.
    pub async fn leave(&self, user_id: Uuid) -> Option<Uuid> {
        let state = self.by_user.write().await.remove(&user_id);

        if let Some(ref s) = state {
            // Remove from channel index
            let mut channels = self.by_channel.write().await;
            if let Some(members) = channels.get_mut(&s.channel_id) {
                members.retain(|u| *u != user_id);
                if members.is_empty() {
                    channels.remove(&s.channel_id);
                }
            }

            tracing::info!(
                channel = %s.channel_id,
                "User left voice channel"
            );
            metrics::gauge!("nexus_voice_users_active").decrement(1.0);
        }

        state.map(|s| s.channel_id)
    }

    /// Update a user's self-mute/deaf/video/stream state.
    pub async fn update_self_state(
        &self,
        user_id: Uuid,
        update: &VoiceStateUpdate,
    ) -> Option<VoiceState> {
        let mut users = self.by_user.write().await;
        if let Some(state) = users.get_mut(&user_id) {
            if let Some(m) = update.self_mute {
                state.self_mute = m;
            }
            if let Some(d) = update.self_deaf {
                state.self_deaf = d;
                // If undeafening, also unmute? Discord does this — we don't force it.
            }
            if let Some(v) = update.self_video {
                state.self_video = v;
            }
            if let Some(s) = update.self_stream {
                state.self_stream = s;
            }
            Some(state.clone())
        } else {
            None
        }
    }

    /// Moderator action: server-mute or server-deaf a user.
    pub async fn apply_mod_action(&self, action: &VoiceModAction) -> Option<VoiceState> {
        let mut users = self.by_user.write().await;
        if let Some(state) = users.get_mut(&action.target_user_id) {
            if let Some(m) = action.server_mute {
                state.server_mute = m;
            }
            if let Some(d) = action.server_deaf {
                state.server_deaf = d;
            }
            Some(state.clone())
        } else {
            None
        }
    }

    /// Update speaking state (from voice activity detection).
    pub async fn set_speaking(&self, user_id: Uuid, speaking: bool) -> Option<VoiceState> {
        let mut users = self.by_user.write().await;
        if let Some(state) = users.get_mut(&user_id) {
            state.speaking = speaking;
            Some(state.clone())
        } else {
            None
        }
    }

    /// Get a user's current voice state.
    pub async fn get_user_state(&self, user_id: Uuid) -> Option<VoiceState> {
        self.by_user.read().await.get(&user_id).cloned()
    }

    /// Get all users in a voice channel.
    pub async fn get_channel_members(&self, channel_id: Uuid) -> Vec<VoiceState> {
        let channels = self.by_channel.read().await;
        let users = self.by_user.read().await;

        channels
            .get(&channel_id)
            .map(|member_ids| {
                member_ids
                    .iter()
                    .filter_map(|uid| users.get(uid).cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get the count of users in a voice channel.
    pub async fn get_channel_count(&self, channel_id: Uuid) -> usize {
        self.by_channel
            .read()
            .await
            .get(&channel_id)
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// Check if a user is in any voice channel.
    pub async fn is_in_voice(&self, user_id: Uuid) -> bool {
        self.by_user.read().await.contains_key(&user_id)
    }

    /// Disconnect all users from a channel (e.g., channel deleted).
    pub async fn disconnect_channel(&self, channel_id: Uuid) -> Vec<VoiceState> {
        let member_ids = self
            .by_channel
            .write()
            .await
            .remove(&channel_id)
            .unwrap_or_default();

        let mut disconnected = Vec::new();
        let mut users = self.by_user.write().await;
        for uid in member_ids {
            if let Some(state) = users.remove(&uid) {
                disconnected.push(state);
            }
        }

        if !disconnected.is_empty() {
            tracing::info!(
                channel = %channel_id,
                count = disconnected.len(),
                "Disconnected all users from voice channel"
            );
        }

        disconnected
    }

    /// Get global voice stats.
    pub async fn stats(&self) -> VoiceGlobalStats {
        let users = self.by_user.read().await;
        let channels = self.by_channel.read().await;

        VoiceGlobalStats {
            active_channels: channels.len(),
            total_connections: users.len(),
            streaming_count: users.values().filter(|s| s.self_stream).count(),
            video_count: users.values().filter(|s| s.self_video).count(),
        }
    }
}

impl Default for VoiceStateManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Global voice statistics.
#[derive(Debug, Serialize)]
pub struct VoiceGlobalStats {
    pub active_channels: usize,
    pub total_connections: usize,
    pub streaming_count: usize,
    pub video_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid() -> Uuid {
        Uuid::new_v4()
    }
    fn cid() -> Uuid {
        Uuid::new_v4()
    }
    fn sid() -> String {
        Uuid::new_v4().to_string()
    }

    // ── join ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn join_creates_state_with_defaults() {
        let mgr = VoiceStateManager::new();
        let user = uid();
        let chan = cid();

        let (state, old) = mgr.join(user, chan, None, sid()).await;

        assert_eq!(state.user_id, user);
        assert_eq!(state.channel_id, chan);
        assert!(old.is_none(), "first join has no previous channel");
        assert!(!state.self_mute);
        assert!(!state.self_deaf);
        assert!(!state.server_mute);
        assert!(!state.speaking);
    }

    #[tokio::test]
    async fn join_returns_old_channel_on_channel_switch() {
        let mgr = VoiceStateManager::new();
        let user = uid();
        let chan1 = cid();
        let chan2 = cid();

        mgr.join(user, chan1, None, sid()).await;
        let (_, old) = mgr.join(user, chan2, None, sid()).await;

        assert_eq!(old, Some(chan1), "second join must report previous channel");
    }

    #[tokio::test]
    async fn join_moves_user_out_of_previous_channel() {
        let mgr = VoiceStateManager::new();
        let user = uid();
        let chan1 = cid();
        let chan2 = cid();

        mgr.join(user, chan1, None, sid()).await;
        mgr.join(user, chan2, None, sid()).await;

        assert_eq!(
            mgr.get_channel_count(chan1).await,
            0,
            "old channel must be empty"
        );
        assert_eq!(
            mgr.get_channel_count(chan2).await,
            1,
            "new channel has the user"
        );
    }

    // ── leave ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn leave_returns_channel_id() {
        let mgr = VoiceStateManager::new();
        let user = uid();
        let chan = cid();

        mgr.join(user, chan, None, sid()).await;
        let left = mgr.leave(user).await;

        assert_eq!(left, Some(chan));
    }

    #[tokio::test]
    async fn leave_removes_from_channel_index() {
        let mgr = VoiceStateManager::new();
        let user = uid();
        let chan = cid();

        mgr.join(user, chan, None, sid()).await;
        mgr.leave(user).await;

        assert_eq!(mgr.get_channel_count(chan).await, 0);
        assert!(!mgr.is_in_voice(user).await);
    }

    #[tokio::test]
    async fn leave_for_absent_user_returns_none() {
        let mgr = VoiceStateManager::new();
        assert!(mgr.leave(uid()).await.is_none());
    }

    // ── is_in_voice / get_user_state ─────────────────────────────────────────

    #[tokio::test]
    async fn is_in_voice_true_after_join() {
        let mgr = VoiceStateManager::new();
        let user = uid();
        assert!(!mgr.is_in_voice(user).await);
        mgr.join(user, cid(), None, sid()).await;
        assert!(mgr.is_in_voice(user).await);
    }

    #[tokio::test]
    async fn get_user_state_returns_current_state() {
        let mgr = VoiceStateManager::new();
        let user = uid();
        let chan = cid();
        mgr.join(user, chan, None, sid()).await;

        let state = mgr.get_user_state(user).await.unwrap();
        assert_eq!(state.channel_id, chan);
    }

    // ── get_channel_members / get_channel_count ───────────────────────────────

    #[tokio::test]
    async fn channel_members_includes_all_joined_users() {
        let mgr = VoiceStateManager::new();
        let chan = cid();
        let u1 = uid();
        let u2 = uid();

        mgr.join(u1, chan, None, sid()).await;
        mgr.join(u2, chan, None, sid()).await;

        let members = mgr.get_channel_members(chan).await;
        assert_eq!(members.len(), 2);
        let ids: Vec<Uuid> = members.iter().map(|s| s.user_id).collect();
        assert!(ids.contains(&u1));
        assert!(ids.contains(&u2));
    }

    #[tokio::test]
    async fn channel_count_zero_for_empty_channel() {
        let mgr = VoiceStateManager::new();
        assert_eq!(mgr.get_channel_count(cid()).await, 0);
    }

    // ── update_self_state ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn update_self_state_applies_mute() {
        let mgr = VoiceStateManager::new();
        let user = uid();
        mgr.join(user, cid(), None, sid()).await;

        let update = VoiceStateUpdate {
            self_mute: Some(true),
            self_deaf: None,
            self_video: None,
            self_stream: None,
        };
        let updated = mgr.update_self_state(user, &update).await.unwrap();
        assert!(updated.self_mute);
        assert!(!updated.self_deaf); // unchanged
    }

    #[tokio::test]
    async fn update_self_state_returns_none_for_absent_user() {
        let mgr = VoiceStateManager::new();
        let update = VoiceStateUpdate {
            self_mute: Some(true),
            self_deaf: None,
            self_video: None,
            self_stream: None,
        };
        assert!(mgr.update_self_state(uid(), &update).await.is_none());
    }

    // ── apply_mod_action ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn apply_mod_action_server_mutes_user() {
        let mgr = VoiceStateManager::new();
        let user = uid();
        mgr.join(user, cid(), None, sid()).await;

        let action = VoiceModAction {
            target_user_id: user,
            server_mute: Some(true),
            server_deaf: None,
        };
        let updated = mgr.apply_mod_action(&action).await.unwrap();
        assert!(updated.server_mute);
        assert!(!updated.server_deaf); // unchanged
    }

    #[tokio::test]
    async fn apply_mod_action_on_absent_user_returns_none() {
        let mgr = VoiceStateManager::new();
        let action = VoiceModAction {
            target_user_id: uid(),
            server_mute: Some(true),
            server_deaf: None,
        };
        assert!(mgr.apply_mod_action(&action).await.is_none());
    }

    // ── set_speaking ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn set_speaking_updates_flag() {
        let mgr = VoiceStateManager::new();
        let user = uid();
        mgr.join(user, cid(), None, sid()).await;

        mgr.set_speaking(user, true).await;
        assert!(mgr.get_user_state(user).await.unwrap().speaking);

        mgr.set_speaking(user, false).await;
        assert!(!mgr.get_user_state(user).await.unwrap().speaking);
    }

    // ── disconnect_channel ────────────────────────────────────────────────────

    #[tokio::test]
    async fn disconnect_channel_removes_all_members() {
        let mgr = VoiceStateManager::new();
        let chan = cid();
        let u1 = uid();
        let u2 = uid();

        mgr.join(u1, chan, None, sid()).await;
        mgr.join(u2, chan, None, sid()).await;

        let evicted = mgr.disconnect_channel(chan).await;
        assert_eq!(evicted.len(), 2);
        assert_eq!(mgr.get_channel_count(chan).await, 0);
        assert!(!mgr.is_in_voice(u1).await);
        assert!(!mgr.is_in_voice(u2).await);
    }

    #[tokio::test]
    async fn disconnect_empty_channel_returns_empty_vec() {
        let mgr = VoiceStateManager::new();
        let evicted = mgr.disconnect_channel(cid()).await;
        assert!(evicted.is_empty());
    }

    // ── stats ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn stats_reflect_current_connections() {
        let mgr = VoiceStateManager::new();
        let chan = cid();
        let u1 = uid();
        let u2 = uid();

        mgr.join(u1, chan, None, sid()).await;
        mgr.join(u2, chan, None, sid()).await;

        // Set u1 streaming and u2 on video
        let s1 = VoiceStateUpdate {
            self_stream: Some(true),
            self_mute: None,
            self_deaf: None,
            self_video: None,
        };
        let s2 = VoiceStateUpdate {
            self_video: Some(true),
            self_mute: None,
            self_deaf: None,
            self_stream: None,
        };
        mgr.update_self_state(u1, &s1).await;
        mgr.update_self_state(u2, &s2).await;

        let stats = mgr.stats().await;
        assert_eq!(stats.total_connections, 2);
        assert_eq!(stats.active_channels, 1);
        assert_eq!(stats.streaming_count, 1);
        assert_eq!(stats.video_count, 1);
    }
}
