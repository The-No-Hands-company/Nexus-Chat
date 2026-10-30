//! Core domain models shared across all Nexus services.
//!
//! These are the "truth" types — what the database stores and the API serializes.
//! Each model uses Snowflake IDs (like Discord) for globally unique, time-sortable identifiers.

pub mod bot;
pub mod channel;
pub mod crypto;
pub mod member;
pub mod message;
pub mod plugin;
pub mod relationship;
pub mod rich;
pub mod role;
pub mod server;
pub mod slash_command;
pub mod user;
pub mod webhook;

// v1.5 Collaboration & Productivity
pub mod collaboration;

// v1.6 Multimedia & Expression
pub mod multimedia;

// v1.7 Accessibility & Inclusivity
pub mod accessibility;

// v1.8 Ecosystem & Onboarding
pub mod ecosystem;

/// Re-export all model types for convenience.
pub use bot::*;
pub use channel::*;
pub use crypto::*;
pub use member::*;
pub use message::*;
pub use plugin::*;
pub use relationship::*;
pub use rich::*;
pub use role::*;
pub use server::*;
pub use slash_command::*;
pub use user::*;
pub use webhook::*;

// v1.5 Collaboration & Productivity
pub use collaboration::*;

// v1.6 Multimedia & Expression
pub use multimedia::*;

// v1.7 Accessibility & Inclusivity
pub use accessibility::*;

// v1.8 Ecosystem & Onboarding
pub use ecosystem::*;

// v1.9 Scalability & Performance Hardening
pub mod scalability;
pub use scalability::*;

// v2.0 AI & Intelligence Layer
pub mod ai_intelligence;
pub use ai_intelligence::*;

// v2.1 Voice & Real-Time Collaboration
pub mod voice_collab;
pub use voice_collab::*;

// v2.2 User Growth & Retention
pub mod growth;
pub use growth::*;

// v2.x Sustainability & Extensibility
pub mod sustainability;
pub use sustainability::*;

// v3.0 Phantom Protocol — Post-Quantum E2EE
pub mod phantom;
pub use phantom::*;

// Widget endpoints — embeddable customer chat
pub mod widget;
pub use widget::*;
