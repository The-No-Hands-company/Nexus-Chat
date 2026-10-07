//! # nexus-common
//!
//! Shared types, configuration, error handling, and utilities used across all Nexus crates.
//! This is the foundation layer — no business logic, just primitives and contracts.

/// Manual `sqlx::FromRow<'_, AnyRow>` impls for all model types (AnyPool compat).
pub mod any_row;
pub mod config;
pub mod crypto;
pub mod error;
pub mod identity;
pub mod logsafe;
pub mod gateway_event;
pub mod models;
pub mod module_gating;
pub mod permissions;
pub mod security_scanning;
pub mod snowflake;
pub mod validation;
