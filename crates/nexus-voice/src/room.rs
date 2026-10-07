//! Voice room — represents a single voice channel session.
//!
//! This is the higher-level abstraction over the SFU room.
//! It manages participant metadata and integrates with the voice state manager.
//! The actual WebRTC media handling is done by the SFU engine.

use crate::state::VoiceState;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// A voice room corresponding to a voice channel.
/// Tracks participant metadata independently from the WebRTC layer.
#[derive(Clone)]
pub struct VoiceRoom {
    pub channel_id: Uuid,
    pub server_id: Option<Uuid>,
    /// Participants in this room (user_id → VoiceParticipant)
    participants: Arc<RwLock<HashMap<Uuid, VoiceParticipant>>>,
    /// When the room was created (first user joined)
    pub created_at: DateTime<Utc>,
}

/// A participant in a voice room with extended metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceParticipant {
    pub user_id: Uuid,
    pub session_id: String,
    pub self_mute: bool,
    pub self_deaf: bool,
    pub server_mute: bool,
    pub server_deaf: bool,
    pub video: bool,
    pub screen_share: bool,
    pub speaking: bool,
    /// Whether the user has noise suppression enabled (client-side nnnoiseless)
    pub noise_suppression: bool,
    /// Volume adjustment by the client (0-200%, default 100%)
    pub volume: u8,
    pub joined_at: DateTime<Utc>,
}

impl VoiceRoom {
    pub fn new(channel_id: Uuid) -> Self {
        Self {
            channel_id,
            server_id: None,
            participants: Arc::new(RwLock::new(HashMap::new())),
            created_at: Utc::now(),
        }
    }

    pub fn with_server(mut self, server_id: Uuid) -> Self {
        self.server_id = Some(server_id);
        self
    }

    /// Add a participant to the room.
    pub async fn join(&self, user_id: Uuid, session_id: String) -> VoiceParticipant {
        let participant = VoiceParticipant {
            user_id,
            session_id,
            self_mute: false,
            self_deaf: false,
            server_mute: false,
            server_deaf: false,
            video: false,
            screen_share: false,
            speaking: false,
            noise_suppression: true, // Enabled by default
            volume: 100,
            joined_at: Utc::now(),
        };

        self.participants
            .write()
            .await
            .insert(user_id, participant.clone());

        tracing::info!(
            channel = %self.channel_id,
            "User joined voice room"
        );

        participant
    }

    /// Sync participant state from VoiceState.
    pub async fn sync_from_voice_state(&self, vs: &VoiceState) {
        if let Some(participant) = self.participants.write().await.get_mut(&vs.user_id) {
            participant.self_mute = vs.self_mute;
            participant.self_deaf = vs.self_deaf;
            participant.server_mute = vs.server_mute;
            participant.server_deaf = vs.server_deaf;
            participant.video = vs.self_video;
            participant.screen_share = vs.self_stream;
            participant.speaking = vs.speaking;
        }
    }

    /// Remove a participant from the room.
    pub async fn leave(&self, user_id: Uuid) -> Option<VoiceParticipant> {
        let removed = self.participants.write().await.remove(&user_id);

        if removed.is_some() {
            tracing::info!(
                channel = %self.channel_id,
                "User left voice room"
            );
        }

        removed
    }

    /// Update a participant's state (mute, deaf, video, etc.).
    pub async fn update_state(
        &self,
        user_id: Uuid,
        self_mute: Option<bool>,
        self_deaf: Option<bool>,
        video: Option<bool>,
        screen_share: Option<bool>,
    ) {
        if let Some(participant) = self.participants.write().await.get_mut(&user_id) {
            if let Some(m) = self_mute {
                participant.self_mute = m;
            }
            if let Some(d) = self_deaf {
                participant.self_deaf = d;
            }
            if let Some(v) = video {
                participant.video = v;
            }
            if let Some(ss) = screen_share {
                participant.screen_share = ss;
            }
        }
    }

    /// Get all participants.
    pub async fn get_participants(&self) -> Vec<VoiceParticipant> {
        self.participants.read().await.values().cloned().collect()
    }

    /// Get a specific participant.
    pub async fn get_participant(&self, user_id: Uuid) -> Option<VoiceParticipant> {
        self.participants.read().await.get(&user_id).cloned()
    }

    /// Get participant count.
    pub async fn participant_count(&self) -> usize {
        self.participants.read().await.len()
    }

    /// Check if room is empty.
    pub async fn is_empty(&self) -> bool {
        self.participants.read().await.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel() -> Uuid {
        Uuid::new_v4()
    }
    fn user() -> Uuid {
        Uuid::new_v4()
    }
    fn session() -> String {
        Uuid::new_v4().to_string()
    }

    // ── VoiceRoom construction ────────────────────────────────────────────────

    #[test]
    fn new_room_has_correct_channel_id() {
        let cid = channel();
        let room = VoiceRoom::new(cid);
        assert_eq!(room.channel_id, cid);
    }

    #[test]
    fn new_room_has_no_server_id() {
        let room = VoiceRoom::new(channel());
        assert!(room.server_id.is_none());
    }

    #[test]
    fn with_server_sets_server_id() {
        let sid = Uuid::new_v4();
        let room = VoiceRoom::new(channel()).with_server(sid);
        assert_eq!(room.server_id, Some(sid));
    }

    // ── join / leave / count ──────────────────────────────────────────────────

    #[tokio::test]
    async fn join_adds_participant_with_defaults() {
        let room = VoiceRoom::new(channel());
        let uid = user();
        let p = room.join(uid, session()).await;

        assert_eq!(p.user_id, uid);
        assert!(!p.self_mute);
        assert!(!p.self_deaf);
        assert!(!p.server_mute);
        assert!(!p.video);
        assert!(p.noise_suppression, "noise suppression on by default");
        assert_eq!(p.volume, 100);
    }

    #[tokio::test]
    async fn participant_count_increments_on_join() {
        let room = VoiceRoom::new(channel());
        assert_eq!(room.participant_count().await, 0);
        room.join(user(), session()).await;
        assert_eq!(room.participant_count().await, 1);
        room.join(user(), session()).await;
        assert_eq!(room.participant_count().await, 2);
    }

    #[tokio::test]
    async fn leave_removes_participant_and_returns_it() {
        let room = VoiceRoom::new(channel());
        let uid = user();
        room.join(uid, session()).await;

        let removed = room.leave(uid).await;
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().user_id, uid);
        assert_eq!(room.participant_count().await, 0);
    }

    #[tokio::test]
    async fn leave_nonexistent_user_returns_none() {
        let room = VoiceRoom::new(channel());
        let result = room.leave(user()).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn is_empty_true_when_no_participants() {
        let room = VoiceRoom::new(channel());
        assert!(room.is_empty().await);
        let uid = user();
        room.join(uid, session()).await;
        assert!(!room.is_empty().await);
        room.leave(uid).await;
        assert!(room.is_empty().await);
    }

    // ── get_participant / get_participants ────────────────────────────────────

    #[tokio::test]
    async fn get_participant_returns_correct_user() {
        let room = VoiceRoom::new(channel());
        let uid = user();
        room.join(uid, session()).await;

        let p = room.get_participant(uid).await.unwrap();
        assert_eq!(p.user_id, uid);
    }

    #[tokio::test]
    async fn get_participant_absent_returns_none() {
        let room = VoiceRoom::new(channel());
        assert!(room.get_participant(user()).await.is_none());
    }

    #[tokio::test]
    async fn get_participants_returns_all() {
        let room = VoiceRoom::new(channel());
        let uid1 = user();
        let uid2 = user();
        room.join(uid1, session()).await;
        room.join(uid2, session()).await;

        let all = room.get_participants().await;
        assert_eq!(all.len(), 2);
        let ids: Vec<Uuid> = all.iter().map(|p| p.user_id).collect();
        assert!(ids.contains(&uid1));
        assert!(ids.contains(&uid2));
    }

    // ── update_state ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn update_state_applies_mute() {
        let room = VoiceRoom::new(channel());
        let uid = user();
        room.join(uid, session()).await;

        room.update_state(uid, Some(true), None, None, None).await;
        let p = room.get_participant(uid).await.unwrap();
        assert!(p.self_mute);
        assert!(!p.self_deaf); // unchanged
    }

    #[tokio::test]
    async fn update_state_applies_video_and_screen_share() {
        let room = VoiceRoom::new(channel());
        let uid = user();
        room.join(uid, session()).await;

        room.update_state(uid, None, None, Some(true), Some(true))
            .await;
        let p = room.get_participant(uid).await.unwrap();
        assert!(p.video);
        assert!(p.screen_share);
    }

    #[tokio::test]
    async fn update_state_no_op_for_unknown_user() {
        let room = VoiceRoom::new(channel());
        // Should not panic for a user who isn't in the room
        room.update_state(user(), Some(true), Some(true), Some(true), Some(true))
            .await;
    }

    // ── sync_from_voice_state ─────────────────────────────────────────────────

    #[tokio::test]
    async fn sync_from_voice_state_mirrors_fields() {
        use crate::state::VoiceState;

        let room = VoiceRoom::new(channel());
        let uid = user();
        room.join(uid, session()).await;

        let vs = VoiceState {
            user_id: uid,
            channel_id: room.channel_id,
            server_id: None,
            session_id: session(),
            self_mute: true,
            self_deaf: true,
            server_mute: true,
            server_deaf: false,
            self_video: true,
            self_stream: true,
            suppress: false,
            speaking: true,
            connected_at: chrono::Utc::now(),
        };

        room.sync_from_voice_state(&vs).await;

        let p = room.get_participant(uid).await.unwrap();
        assert!(p.self_mute);
        assert!(p.self_deaf);
        assert!(p.server_mute);
        assert!(p.video);
        assert!(p.screen_share);
        assert!(p.speaking);
    }
}
