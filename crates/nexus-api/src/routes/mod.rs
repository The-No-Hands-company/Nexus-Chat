//! API route modules.

pub mod bots;
pub mod channels;
pub mod directory;
pub mod dms;
pub mod e2ee;
pub mod email_verification;
pub mod emoji;
pub mod experience;
pub mod extensibility;
pub mod federation;
pub mod files;
pub mod phantom;
// v0.8.5 Federation UX — admin management endpoints
pub mod bookmarks;
pub mod drafts;
pub mod federation_admin;
pub mod forum;
pub mod health;
pub mod keys;
pub mod messages;
pub mod metrics;
pub mod moderation;
pub mod polls;
pub mod presence;
pub mod relationships;
pub mod scheduled_messages;
pub mod search;
pub mod servers;
pub mod sessions;
pub mod slash_commands;
pub mod stages;
pub mod threads;
pub mod uploads;
pub mod users;
pub mod verification;
pub mod voice;
pub mod webhooks;
// v0.14 Platform Differentiation
pub mod events;
pub mod forward;
pub mod inline_query;
pub mod stickers;

// v0.15 Community Ecosystem
pub mod badges;
pub mod boosters;
pub mod canvas;

// v0.16 Discovery & Monetization
pub mod discovery;
pub mod monetization;

// v1.5 Collaboration & Productivity
pub mod ai_assists;
pub mod calendar;
pub mod file_versions;
pub mod tasks;

// v1.6 Multimedia & Expression
pub mod drawings;
pub mod media_gallery;
pub mod stories;
pub mod voice_notes;
pub mod voice_settings;

// v1.7 Accessibility & Inclusivity
pub mod accessibility;
pub mod translations;
pub mod voice_captions;

// v1.8 Ecosystem & Onboarding
pub mod admin_analytics;
pub mod importer;
pub mod marketplace;
pub mod onboarding;

// v1.9 Scalability & Performance Hardening
pub mod scalability;

// v2.0 AI & Intelligence Layer
pub mod ai_layer;

// v2.1 Voice & Real-Time Collaboration
pub mod voice_collab;

// v2.2 User Growth & Retention
pub mod growth;

// v2.x Sustainability & Extensibility
pub mod sustainability;

pub mod admin;
/// Web Push notification subscription management
pub mod push;

/// Instance-admin user management (list, suspend, disable)
pub mod admin_users;

/// Widget endpoints — embeddable customer chat for Nexus-Hosting sites
pub mod widget;
