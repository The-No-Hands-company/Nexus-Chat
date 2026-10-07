//! Session management — persists and queries refresh tokens.
//!
//! Maps 1:1 with the `refresh_tokens` table.  Each row represents one
//! active session (device/browser).  Tokens are stored by their JTI (JWT ID,
//! a random UUID), allowing revocation without storing the raw token.

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

/// Lightweight session view returned to the client.
#[derive(Debug, Serialize)]
pub struct SessionRow {
    pub id: String,
    pub device_info: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

/// Persist a new session when a user logs in or registers.
#[allow(clippy::too_many_arguments)]
pub async fn create_session(
    pool: &sqlx::AnyPool,
    session_id: Uuid,
    user_id: Uuid,
    // The raw refresh token is NOT stored — only its JTI (session_id) is used for lookups.
    device_info: Option<&str>,
    expires_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO refresh_tokens \
         (id, user_id, token_hash, device_info, expires_at, last_seen_at, created_at) \
         VALUES ($1::uuid, $2::uuid, $3, $4, $5::timestamptz, NOW(), NOW())",
    )
    .bind(session_id.to_string())
    .bind(user_id.to_string())
    // token_hash is no longer used for lookup — we use the JTI (session_id).
    // Store the session UUID itself to satisfy the NOT NULL + UNIQUE constraint.
    .bind(session_id.to_string())
    .bind(device_info)
    .bind(expires_at.to_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}

/// List all non-expired sessions for a user, sorted newest-first.
pub async fn list_sessions(
    pool: &sqlx::AnyPool,
    user_id: Uuid,
) -> Result<Vec<SessionRow>, sqlx::Error> {
    let rows: Vec<(String, Option<String>, String, String)> = sqlx::query_as(
        "SELECT id::text, device_info, \
                    to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'), \
                    to_char(last_seen_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') \
             FROM refresh_tokens \
             WHERE user_id = $1::uuid AND expires_at > NOW() \
             ORDER BY last_seen_at DESC",
    )
    .bind(user_id.to_string())
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(
            |(id, device_info, created_str, last_seen_str)| {
                let created_at = created_str
                    .parse::<DateTime<Utc>>()
                    .unwrap_or(DateTime::<Utc>::MIN_UTC);
                let last_seen_at = last_seen_str
                    .parse::<DateTime<Utc>>()
                    .unwrap_or(DateTime::<Utc>::MIN_UTC);
                Ok(SessionRow {
                    id,
                    device_info,
                    created_at,
                    last_seen_at,
                })
            },
        )
        .collect()
}

/// Touch the `last_seen_at` timestamp for a session when the token is used.
pub async fn touch_session(pool: &sqlx::AnyPool, session_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE refresh_tokens SET last_seen_at = NOW() WHERE id = $1::uuid")
        .bind(session_id.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

/// Revoke a specific session.
///
/// Verifies the session belongs to `user_id` to prevent cross-user revocation.
pub async fn revoke_session(
    pool: &sqlx::AnyPool,
    user_id: Uuid,
    session_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let result =
        sqlx::query("DELETE FROM refresh_tokens WHERE id = $1::uuid AND user_id = $2::uuid")
            .bind(session_id.to_string())
            .bind(user_id.to_string())
            .execute(pool)
            .await?;
    Ok(result.rows_affected() > 0)
}

/// Revoke all sessions for a user except the current one (identified by JTI).
pub async fn revoke_all_except(
    pool: &sqlx::AnyPool,
    user_id: Uuid,
    current_session_id: Uuid,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM refresh_tokens \
         WHERE user_id = $1::uuid AND id != $2::uuid",
    )
    .bind(user_id.to_string())
    .bind(current_session_id.to_string())
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Delete all expired sessions (background maintenance, called by cleanup job).
pub async fn purge_expired(pool: &sqlx::AnyPool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM refresh_tokens WHERE expires_at <= NOW()")
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Check whether a session still exists and has not been revoked.
///
/// Used by auth middleware to validate that a JWT's `jti` claim corresponds to
/// an active session row — ensuring that logout (which deletes the row) actually
/// invalidates the token before its cryptographic expiry.
///
/// The result is intentionally NOT cached at this layer; callers use Redis for
/// short-lived caching. No longer read on the request path: user sessions
/// live in Nexus-Auth now and identity tokens expire in ~120 seconds.
pub async fn session_exists(
    pool: &sqlx::AnyPool,
    session_id: Uuid,
    user_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query(
        "SELECT 1 FROM refresh_tokens \
         WHERE id = $1::uuid AND user_id = $2::uuid AND expires_at > NOW()",
    )
    .bind(session_id.to_string())
    .bind(user_id.to_string())
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}
