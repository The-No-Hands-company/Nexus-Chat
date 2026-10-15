//! Per-table SELECT column lists with Postgres-compatible type casts.
//!
//! `AnyPool` cannot decode Postgres-native types (UUID, ENUM, TIMESTAMPTZ,
//! JSONB, UUID[]).  Every column of those types must be cast to `::text` so the
//! Any driver hands them to our manual `FromRow` helpers as plain `String`s.
//!
//! # Usage
//!
//! ```rust,ignore
//! use crate::select_cols::USER_COLS;
//! let q = format!("SELECT {USER_COLS} FROM users WHERE id = $1");
//! // sqlx::query_as::<_, User>(&q).bind(id).fetch_optional(pool).await
//! ```

// ── users ─────────────────────────────────────────────────────────────────────
pub const USER_COLS: &str = "id::text AS id, username, display_name, email, password_hash, avatar, banner, \
     bio, status, presence::text AS presence, flags, totp_enabled, server_name, is_remote, \
     created_at::text AS created_at, updated_at::text AS updated_at";

// ── servers ───────────────────────────────────────────────────────────────────
pub const SERVER_COLS: &str = "id::text AS id, name, description, icon, banner, owner_id::text AS owner_id, \
     region, is_public, features::text AS features, settings::text AS settings, \
     vanity_code, member_count, max_file_size, require_2fa, spam_window_secs, spam_max_messages, \
     boost_tier, booster_count, \
     array_to_json(tags)::text AS tags, category, activity_score, featured_at::text AS featured_at, tip_jar_url, \
     created_at::text AS created_at, updated_at::text AS updated_at";

/// Same as `SERVER_COLS` but with `s.` table-alias prefix for JOIN queries.
pub const SERVER_COLS_S: &str = "s.id::text AS id, s.name, s.description, s.icon, s.banner, \
     s.owner_id::text AS owner_id, s.region, s.is_public, \
     s.features::text AS features, s.settings::text AS settings, \
     s.vanity_code, s.member_count, s.max_file_size, s.require_2fa, s.spam_window_secs, s.spam_max_messages, \
     s.boost_tier, s.booster_count, \
     array_to_json(s.tags)::text AS tags, s.category, s.activity_score, s.featured_at::text AS featured_at, s.tip_jar_url, \
     s.created_at::text AS created_at, s.updated_at::text AS updated_at";

// ── channels ──────────────────────────────────────────────────────────────────
pub const CHANNEL_COLS: &str = "id::text AS id, server_id::text AS server_id, parent_id::text AS parent_id, \
     channel_type::text AS channel_type, name, topic, position, nsfw, \
     rate_limit_per_user, bitrate, user_limit, encrypted, \
     permission_overwrites::text AS permission_overwrites, \
     last_message_id::text AS last_message_id, auto_archive_duration, \
     archived, locked, disappear_after_seconds, is_stream, \
     created_at::text AS created_at, updated_at::text AS updated_at";

/// Same as `CHANNEL_COLS` but with `c.` table-alias prefix for JOIN queries.
pub const CHANNEL_COLS_C: &str = "c.id::text AS id, c.server_id::text AS server_id, c.parent_id::text AS parent_id, \
     c.channel_type::text AS channel_type, c.name, c.topic, c.position, c.nsfw, \
     c.rate_limit_per_user, c.bitrate, c.user_limit, c.encrypted, \
     c.permission_overwrites::text AS permission_overwrites, \
     c.last_message_id::text AS last_message_id, c.auto_archive_duration, \
     c.archived, c.locked, c.disappear_after_seconds, c.is_stream, \
     c.created_at::text AS created_at, c.updated_at::text AS updated_at";

// ── members ───────────────────────────────────────────────────────────────────
/// UUID[] `roles` cast via `array_to_json` → JSON string, readable by the
/// `uuid_vec` helper.
pub const MEMBER_COLS: &str = "user_id::text AS user_id, server_id::text AS server_id, nickname, avatar, \
     COALESCE(array_to_json(roles), '[]'::json)::text AS roles, \
     muted, deafened, joined_at::text AS joined_at, \
     communication_disabled_until::text AS communication_disabled_until";

// ── roles ─────────────────────────────────────────────────────────────────────
pub const ROLE_COLS: &str = "id::text AS id, server_id::text AS server_id, name, color, hoist, icon, \
     position, permissions, mentionable, is_default, \
     created_at::text AS created_at, updated_at::text AS updated_at";

// ── invites ───────────────────────────────────────────────────────────────────
pub const INVITE_COLS: &str = "code, server_id::text AS server_id, channel_id::text AS channel_id, \
     inviter_id::text AS inviter_id, max_uses, uses, \
     expires_at::text AS expires_at, created_at::text AS created_at";

// ── messages ──────────────────────────────────────────────────────────────────
/// UUID[] columns (mentions, mention_roles) are cast via array_to_json → ::text
/// so the `get_uuid_vec` helper can parse them.
pub const MESSAGE_COLS: &str = "id::text AS id, channel_id::text AS channel_id, author_id::text AS author_id, \
     content, message_type, edited, edited_at::text AS edited_at, pinned, \
     embeds::text AS embeds, attachments::text AS attachments, \
     COALESCE(array_to_json(mentions), '[]'::json)::text AS mentions, \
     COALESCE(array_to_json(mention_roles), '[]'::json)::text AS mention_roles, \
     mention_everyone, \
     reference_message_id::text AS reference_message_id, \
     reference_channel_id::text AS reference_channel_id, \
     thread_id::text AS thread_id, \
     flags, created_at::text AS created_at, updated_at::text AS updated_at";

/// Same as `MESSAGE_COLS` but with `m.` table-alias prefix for JOIN queries.
pub const MESSAGE_COLS_M: &str = "m.id::text AS id, m.channel_id::text AS channel_id, m.author_id::text AS author_id, \
     m.content, m.message_type, m.edited, m.edited_at::text AS edited_at, m.pinned, \
     m.embeds::text AS embeds, m.attachments::text AS attachments, \
     COALESCE(array_to_json(m.mentions), '[]'::json)::text AS mentions, \
     COALESCE(array_to_json(m.mention_roles), '[]'::json)::text AS mention_roles, \
     m.mention_everyone, \
     m.reference_message_id::text AS reference_message_id, \
     m.reference_channel_id::text AS reference_channel_id, \
     m.thread_id::text AS thread_id, \
     m.flags, m.created_at::text AS created_at, m.updated_at::text AS updated_at";
// ── user_relationships ────────────────────────────────────────────────────────
pub const RELATIONSHIP_COLS: &str = "id::text AS id, requester_id::text AS requester_id, addressee_id::text AS addressee_id, \
     status::text AS status, created_at::text AS created_at, updated_at::text AS updated_at";

// ── server_emoji ──────────────────────────────────────────────────────────────
pub const EMOJI_COLS: &str = "id::text AS id, server_id::text AS server_id, creator_id::text AS creator_id, \
     name, storage_key, url, animated, managed, available, \
     created_at::text AS created_at";

// ── attachments ───────────────────────────────────────────────────────────────
pub const ATTACHMENT_COLS: &str = "id::text AS id, uploader_id::text AS uploader_id, server_id::text AS server_id, \
     channel_id::text AS channel_id, message_id::text AS message_id, \
     filename, content_type, size, storage_key, url, \
     width, height, duration_secs, spoiler, blurhash, sha256, status, \
     created_at::text AS created_at, updated_at::text AS updated_at";

/// Same as `ATTACHMENT_COLS` but with `a.` table-alias prefix for JOIN queries.
pub const ATTACHMENT_COLS_A: &str = "a.id::text AS id, a.uploader_id::text AS uploader_id, a.server_id::text AS server_id, \
     a.channel_id::text AS channel_id, a.message_id::text AS message_id, \
     a.filename, a.content_type, a.size, a.storage_key, a.url, \
     a.width, a.height, a.duration_secs, a.spoiler, a.blurhash, a.sha256, a.status, \
     a.created_at::text AS created_at, a.updated_at::text AS updated_at";

// ── bot_applications ──────────────────────────────────────────────────────────
pub const BOT_COLS: &str = "id::text AS id, owner_id::text AS owner_id, name, description, avatar, \
     token_hash, public_key, redirect_uris::text AS redirect_uris, permissions, \
     verified, is_public, interactions_endpoint_url, flags, \
     created_at::text AS created_at, updated_at::text AS updated_at";

// ── bot_server_installs ───────────────────────────────────────────────────────
pub const BOT_INSTALL_COLS: &str = "id::text AS id, bot_id::text AS bot_id, server_id::text AS server_id, \
     installed_by::text AS installed_by, scopes::text AS scopes, permissions, \
     installed_at::text AS installed_at";

// ── threads ───────────────────────────────────────────────────────────────────
/// Columns for the `threads` table (no prefix).  Used in RETURNING clauses
/// where `parent_channel_id` is appended as a separate expression.
pub const THREAD_COLS: &str = "channel_id::text AS channel_id, parent_message_id::text AS parent_message_id, \
     owner_id::text AS owner_id, title, message_count, member_count, \
     auto_archive_minutes, archived, archived_at::text AS archived_at, locked, \
     COALESCE(array_to_json(tags), '[]'::json)::text AS tags, \
     created_at::text AS created_at, updated_at::text AS updated_at";

/// Same as `THREAD_COLS` but with `t.` table-alias prefix for JOIN queries.
pub const THREAD_COLS_T: &str = "t.channel_id::text AS channel_id, t.parent_message_id::text AS parent_message_id, \
     t.owner_id::text AS owner_id, t.title, t.message_count, t.member_count, \
     t.auto_archive_minutes, t.archived, t.archived_at::text AS archived_at, t.locked, \
     COALESCE(array_to_json(t.tags), '[]'::json)::text AS tags, \
     t.created_at::text AS created_at, t.updated_at::text AS updated_at";

// ── tasks ─────────────────────────────────────────────────────────────────────
pub const TASK_COLS: &str = "id::text AS id, server_id::text AS server_id, channel_id::text AS channel_id, \
     creator_id::text AS creator_id, assignee_id::text AS assignee_id, \
     title, description, status, priority, \
     due_at::text AS due_at, completed_at::text AS completed_at, \
     position, created_at::text AS created_at, updated_at::text AS updated_at";

// ── calendar_events ───────────────────────────────────────────────────────────
pub const CALENDAR_EVENT_COLS: &str = "id::text AS id, server_id::text AS server_id, channel_id::text AS channel_id, \
     creator_id::text AS creator_id, title, description, location, \
     starts_at::text AS starts_at, ends_at::text AS ends_at, all_day, rrule, color, \
     created_at::text AS created_at, updated_at::text AS updated_at";

// ── file_versions ─────────────────────────────────────────────────────────────
pub const FILE_VERSION_COLS: &str = "id::text AS id, attachment_id::text AS attachment_id, uploader_id::text AS uploader_id, \
     version_number, filename, content_type, size, storage_key, sha256, comment, \
     created_at::text AS created_at";

// ═══════════════════════════════════════════════════════════════════════════════
//  Phase 17 — Multimedia & Expression
// ═══════════════════════════════════════════════════════════════════════════════

// ── voice_notes ───────────────────────────────────────────────────────────────
pub const VOICE_NOTE_COLS: &str = "id::text AS id, channel_id::text AS channel_id, author_id::text AS author_id, \
     storage_key, filename, content_type, size, duration_ms, waveform, transcript, \
     message_id::text AS message_id, created_at::text AS created_at";

// ── video_notes ───────────────────────────────────────────────────────────────
pub const VIDEO_NOTE_COLS: &str = "id::text AS id, channel_id::text AS channel_id, author_id::text AS author_id, \
     storage_key, filename, content_type, size, duration_ms, width, height, \
     thumbnail_key, transcript, message_id::text AS message_id, \
     created_at::text AS created_at";

// ── stories ───────────────────────────────────────────────────────────────────
pub const STORY_COLS: &str = "id::text AS id, author_id::text AS author_id, media_type, storage_key, \
     text_content, text_style::text AS text_style, \
     expires_at::text AS expires_at, visibility, created_at::text AS created_at";

// ── drawings ──────────────────────────────────────────────────────────────────
pub const DRAWING_COLS: &str = "id::text AS id, channel_id::text AS channel_id, author_id::text AS author_id, \
     drawing_data::text AS drawing_data, width, height, preview_key, \
     message_id::text AS message_id, is_whiteboard, \
     created_at::text AS created_at, updated_at::text AS updated_at";

// ── voice_music_queue ─────────────────────────────────────────────────────────
pub const VOICE_MUSIC_QUEUE_COLS: &str = "id::text AS id, channel_id::text AS channel_id, added_by::text AS added_by, \
     title, source_url, duration_ms, position, status, \
     created_at::text AS created_at";

// ── media_gallery_filters ─────────────────────────────────────────────────────
pub const MEDIA_GALLERY_FILTER_COLS: &str = "id::text AS id, user_id::text AS user_id, server_id::text AS server_id, \
     channel_id::text AS channel_id, name, \
     array_to_json(media_types)::text AS media_types, \
     date_from::text AS date_from, date_to::text AS date_to, \
     created_at::text AS created_at";

// ── Phase 18: Accessibility ───────────────────────────────────────────────────

pub const USER_ACCESSIBILITY_COLS: &str = "user_id::text AS user_id, \
     screen_reader_mode, announce_messages, announce_reactions, announce_typing, keyboard_shortcuts, \
     high_contrast_mode, reduced_motion, font_family, custom_font_name, color_blind_mode, \
     preferred_language, auto_translate, rtl_override, \
     captions_enabled, caption_font_size, caption_position, tts_enabled, tts_rate, tts_voice, \
     created_at::text AS created_at, updated_at::text AS updated_at";

pub const VOICE_CAPTION_COLS: &str = "id::text AS id, channel_id::text AS channel_id, speaker_id::text AS speaker_id, \
     text, language, is_final, \
     started_at::text AS started_at, ended_at::text AS ended_at, \
     created_at::text AS created_at";

pub const MESSAGE_TTS_REQUEST_COLS: &str = "id::text AS id, user_id::text AS user_id, message_id::text AS message_id, \
     channel_id::text AS channel_id, status, \
     created_at::text AS created_at";

pub const MESSAGE_TRANSLATION_COLS: &str = "id::text AS id, message_id::text AS message_id, \
     source_language, target_language, translated_text, \
     created_at::text AS created_at";

// ── Phase 19: Ecosystem & Onboarding ──────────────────────────────────────────

pub const IMPORT_JOB_COLS: &str = "id::text AS id, server_id::text AS server_id, user_id::text AS user_id, \
     source_platform, status, total_items, imported_items, error_log, metadata, \
     created_at::text AS created_at, updated_at::text AS updated_at";

pub const BULK_INVITATION_COLS: &str = "id::text AS id, server_id::text AS server_id, inviter_id::text AS inviter_id, \
     emails, status, sent_count, total_count, invite_code, \
     created_at::text AS created_at";

pub const SERVER_TEMPLATE_COLS: &str = "id::text AS id, name, description, category, icon_url, \
     channels, roles, settings, is_builtin, \
     creator_id::text AS creator_id, usage_count, \
     created_at::text AS created_at";

pub const ONBOARDING_PROGRESS_COLS: &str = "user_id::text AS user_id, completed_steps, dismissed, \
     created_at::text AS created_at, updated_at::text AS updated_at";

pub const SERVER_ANALYTICS_SNAPSHOT_COLS: &str = "id::text AS id, server_id::text AS server_id, \
     period_date::text AS period_date, messages_count, active_members, \
     new_members, left_members, voice_minutes, \
     reports_resolved, bans_issued, filters_triggered, \
     created_at::text AS created_at";

pub const MARKETPLACE_PLUGIN_COLS: &str = "id::text AS id, name, slug, description, author_id::text AS author_id, \
     version, manifest_url, icon_url, source_url, \
     signature, signing_key_id, category, tags, \
     downloads, avg_rating, rating_count, \
     is_verified, is_published, \
     created_at::text AS created_at, updated_at::text AS updated_at";

pub const PLUGIN_REVIEW_COLS: &str = "id::text AS id, plugin_id::text AS plugin_id, user_id::text AS user_id, \
     rating, title, body, \
     created_at::text AS created_at, updated_at::text AS updated_at";

pub const PLUGIN_INSTALL_COLS: &str = "id::text AS id, plugin_id::text AS plugin_id, server_id::text AS server_id, \
     installed_by::text AS installed_by, version, is_enabled, \
     created_at::text AS created_at";

// ── v1.9 Scalability & Performance Hardening ──────────────────────────────

pub const SCALING_CONFIG_COLS: &str = "id::text AS id, instance_id, region, shard_strategy, redis_mode, \
     gateway_weight, max_connections, metadata, is_active, \
     created_at::text AS created_at, updated_at::text AS updated_at";

pub const SFU_NODE_COLS: &str = "id::text AS id, instance_id, region, hostname, port, capacity, current_load, \
     status, metadata, last_heartbeat::text AS last_heartbeat, \
     created_at::text AS created_at";

pub const FEDERATION_EVENT_BATCH_COLS: &str = "id::text AS id, target_instance, events, event_count, status, retry_count, \
     created_at::text AS created_at, sent_at::text AS sent_at";

pub const FEDERATION_ROUTE_COLS: &str = "id::text AS id, source_instance, target_instance, latency_ms, is_websocket, \
     priority, status, last_probed::text AS last_probed, \
     created_at::text AS created_at";

pub const FEDERATION_DEDUP_COLS: &str =
    "event_id, source_instance, received_at::text AS received_at";

pub const VOICE_QUALITY_LOG_COLS: &str = "id::text AS id, channel_id::text AS channel_id, user_id::text AS user_id, \
     sfu_node_id::text AS sfu_node_id, bitrate, packet_loss, jitter_ms, latency_ms, \
     fec_enabled, quality_score, created_at::text AS created_at";

pub const MEMBER_PRUNE_RULE_COLS: &str = "id::text AS id, server_id::text AS server_id, inactivity_days, grace_period_days, \
     exclude_roles, notify_before, is_enabled, last_run_at::text AS last_run_at, \
     pruned_count, created_at::text AS created_at, updated_at::text AS updated_at";

pub const SLOW_MODE_OVERRIDE_COLS: &str = "id::text AS id, channel_id::text AS channel_id, role_id::text AS role_id, \
     cooldown_secs, escalation_mult, created_at::text AS created_at";

pub const SCALING_METRIC_COLS: &str = "id::text AS id, instance_id, metric_name, metric_value, unit, tags, \
     recorded_at::text AS recorded_at";

pub const UPGRADE_RECORD_COLS: &str = "id::text AS id, from_version, to_version, status, \
     started_at::text AS started_at, completed_at::text AS completed_at, \
     notes, metadata";

// ── v2.0 AI & Intelligence Layer ──────────────────────────────────────────

pub const SEARCH_EMBEDDING_COLS: &str = "id::text AS id, message_id::text AS message_id, channel_id::text AS channel_id, \
     embedding, model_name, model_version, created_at::text AS created_at";

pub const SEARCH_QUERY_COLS: &str = "id::text AS id, user_id::text AS user_id, raw_query, parsed_filters, \
     result_count, latency_ms, created_at::text AS created_at";

pub const AI_SUGGESTION_COLS: &str = "id::text AS id, user_id::text AS user_id, channel_id::text AS channel_id, \
     suggestion_type, content, context_ids, model_name, accepted, \
     created_at::text AS created_at";

pub const THREAD_SUMMARY_COLS: &str = "id::text AS id, thread_id::text AS thread_id, channel_id::text AS channel_id, \
     summary, message_count, model_name, model_version, \
     created_at::text AS created_at, updated_at::text AS updated_at";

pub const TOXICITY_SCORE_COLS: &str = "id::text AS id, message_id::text AS message_id, server_id::text AS server_id, \
     score, categories, model_name, flagged, reviewed, \
     created_at::text AS created_at";

pub const RAID_DETECTION_COLS: &str = "id::text AS id, server_id::text AS server_id, detection_type, severity, \
     details, auto_actions, resolved, detected_at::text AS detected_at, \
     resolved_at::text AS resolved_at";

pub const VOICE_TRANSCRIPT_COLS: &str = "id::text AS id, channel_id::text AS channel_id, session_id::text AS session_id, \
     speaker_id::text AS speaker_id, segment_start, segment_end, text, language, \
     confidence, created_at::text AS created_at";

pub const VOICE_COMMAND_COLS: &str = "id::text AS id, user_id::text AS user_id, channel_id::text AS channel_id, \
     command_text, action, confidence, executed, created_at::text AS created_at";

pub const AI_CONSENT_COLS: &str = "user_id::text AS user_id, server_id::text AS server_id, feature, enabled, \
     updated_at::text AS updated_at";

pub const AI_AUDIT_ENTRY_COLS: &str = "id::text AS id, server_id::text AS server_id, feature, action, \
     actor_id::text AS actor_id, details, model_name, model_version, \
     created_at::text AS created_at";

// ── v2.1 Voice & Real-Time Collaboration ──────────────────────────────────

pub const VIDEO_LAYOUT_COLS: &str = "id::text AS id, channel_id::text AS channel_id, user_id::text AS user_id, \
     layout_type, pinned_users, custom_positions, pip_enabled, \
     created_at::text AS created_at, updated_at::text AS updated_at";

pub const VIRTUAL_BACKGROUND_COLS: &str = "id::text AS id, user_id::text AS user_id, name, bg_type, image_url, \
     is_default, created_at::text AS created_at";

pub const LIVE_STREAM_COLS: &str = "id::text AS id, channel_id::text AS channel_id, streamer_id::text AS streamer_id, \
     title, status, viewer_count, max_viewers, is_e2ee, recording_url, hls_url, \
     started_at::text AS started_at, ended_at::text AS ended_at";

pub const STREAM_VIEWER_COLS: &str = "stream_id::text AS stream_id, user_id::text AS user_id, \
     joined_at::text AS joined_at, left_at::text AS left_at";

pub const BREAKOUT_ROOM_COLS: &str = "id::text AS id, parent_channel::text AS parent_channel, name, capacity, status, \
     created_by::text AS created_by, created_at::text AS created_at, \
     closed_at::text AS closed_at";

pub const COLLAB_SESSION_COLS: &str = "id::text AS id, channel_id::text AS channel_id, session_type, \
     document_id::text AS document_id, participants, is_active, \
     created_at::text AS created_at, ended_at::text AS ended_at";

pub const SPATIAL_AUDIO_CONFIG_COLS: &str = "id::text AS id, channel_id::text AS channel_id, preset, \
     room_width, room_depth, room_height, positions, hrtf_enabled, ambisonics_order, \
     created_at::text AS created_at, updated_at::text AS updated_at";

pub const VOICE_PRESET_COLS: &str = "id::text AS id, name, description, target_latency_ms, jitter_buffer_ms, \
     fec_level, dtx_enabled, normalization, is_builtin, \
     created_at::text AS created_at";

// ── v2.2 User Growth & Retention ──────────────────────────────────────────

pub const SERVER_RECOMMENDATION_COLS: &str = "id::text AS id, user_id::text AS user_id, server_id::text AS server_id, \
     score, reason, dismissed, joined, created_at::text AS created_at";

pub const ONBOARDING_FLOW_COLS: &str = "id::text AS id, server_id::text AS server_id, steps, adaptive, skip_completed, \
     is_active, created_at::text AS created_at, updated_at::text AS updated_at";

pub const DEVICE_SESSION_COLS: &str = "id::text AS id, user_id::text AS user_id, device_id, device_type, \
     last_channel_id::text AS last_channel_id, scroll_position, is_active, \
     last_seen_at::text AS last_seen_at, created_at::text AS created_at";

pub const CLIPBOARD_SYNC_COLS: &str = "id::text AS id, user_id::text AS user_id, source_device, content_type, \
     encrypted_data, expires_at::text AS expires_at, created_at::text AS created_at";

pub const USER_XP_COLS: &str = "user_id::text AS user_id, server_id::text AS server_id, xp, level, \
     last_xp_at::text AS last_xp_at";

pub const GAMIFICATION_CONFIG_COLS: &str = "server_id::text AS server_id, enabled, xp_per_message, xp_per_reaction, \
     xp_per_voice_min, level_formula, streak_enabled, \
     created_at::text AS created_at, updated_at::text AS updated_at";

pub const ACHIEVEMENT_COLS: &str = "id::text AS id, server_id::text AS server_id, name, description, icon_url, \
     criteria, reward_xp, created_at::text AS created_at";

pub const USER_ACHIEVEMENT_COLS: &str = "user_id::text AS user_id, achievement_id::text AS achievement_id, \
     earned_at::text AS earned_at";

pub const ACTIVITY_STREAK_COLS: &str = "user_id::text AS user_id, server_id::text AS server_id, \
     current_streak, longest_streak, last_active_date::text AS last_active_date";

pub const SYNC_CURSOR_COLS: &str = "user_id::text AS user_id, device_id, channel_id::text AS channel_id, \
     last_message_id::text AS last_message_id, last_synced_at::text AS last_synced_at";

pub const OFFLINE_QUEUE_ITEM_COLS: &str = "id::text AS id, user_id::text AS user_id, device_id, action_type, payload, \
     status, conflict_data, created_at::text AS created_at, synced_at::text AS synced_at";

// ── v2.x Sustainability & Extensibility ───────────────────────────────────

pub const PROTOCOL_VERSION_COLS: &str = "id::text AS id, protocol, version, status, capabilities, \
     deprecation_date::text AS deprecation_date, sunset_date::text AS sunset_date, \
     migration_guide, created_at::text AS created_at";

pub const CAPABILITY_NEGOTIATION_COLS: &str = "id::text AS id, local_instance, remote_instance, local_caps, remote_caps, \
     agreed_caps, protocol_version, negotiated_at::text AS negotiated_at";

pub const GOVERNANCE_POLL_COLS: &str = "id::text AS id, server_id::text AS server_id, title, description, poll_type, \
     options, min_participation, allow_multiple, anonymous, status, \
     created_by::text AS created_by, opens_at::text AS opens_at, \
     closes_at::text AS closes_at, created_at::text AS created_at";

pub const POLL_VOTE_COLS: &str = "poll_id::text AS poll_id, user_id::text AS user_id, option_index, \
     voted_at::text AS voted_at";

pub const GOVERNANCE_PROPOSAL_COLS: &str = "id::text AS id, server_id::text AS server_id, title, body, status, \
     author_id::text AS author_id, discussion_channel::text AS discussion_channel, \
     poll_id::text AS poll_id, created_at::text AS created_at, \
     updated_at::text AS updated_at";

pub const CONTRIBUTOR_BADGE_COLS: &str = "id::text AS id, user_id::text AS user_id, badge_type, source, verified, \
     metadata, awarded_at::text AS awarded_at";

pub const SECURITY_AUDIT_COLS: &str = "id::text AS id, audit_type, status, findings, severity_summary, auditor, \
     started_at::text AS started_at, completed_at::text AS completed_at, \
     created_at::text AS created_at";

pub const VULNERABILITY_RECORD_COLS: &str = "id::text AS id, audit_id::text AS audit_id, cve_id, package_name, severity, \
     description, remediation, status, discovered_at::text AS discovered_at, \
     resolved_at::text AS resolved_at";

pub const TUTORIAL_PROGRESS_COLS: &str = "user_id::text AS user_id, tutorial_id, completed_steps, completed, \
     started_at::text AS started_at, completed_at::text AS completed_at";

pub const MIGRATION_GUIDE_COLS: &str = "id::text AS id, from_platform, title, content, version, is_published, \
     author_id::text AS author_id, created_at::text AS created_at, \
     updated_at::text AS updated_at";

// ── phantom identities (public keys only; Privacy Part 3) ────────────────────
pub const PHANTOM_IDENTITY_COLS: &str = "user_id::text AS user_id, did, kem_public, signing_public, \
     created_at::text AS created_at";
