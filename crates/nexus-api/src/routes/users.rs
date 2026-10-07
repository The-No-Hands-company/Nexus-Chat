//! User routes — profile management, user lookup, account lifecycle.

use axum::http::HeaderMap;
use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    middleware,
    routing::{get, post},
};
use nexus_common::{
    error::{NexusError, NexusResult},
    models::user::{UpdateUserRequest, UserResponse},
    validation::validate_request,
};
use nexus_db::repository::users;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    AppState,
    middleware::{AuthContext, check_rate_limit_with_fallback, extract_client_tag},
};

/// User routes (all require authentication).
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/users/@me",
            get(get_current_user)
                .patch(update_current_user)
                .delete(delete_account),
        )
        .route("/users/@me/cancel-deletion", post(cancel_account_deletion))
        .route("/users/@me/data-export", get(data_export))
        .route("/users/@me/note-to-self", get(get_note_to_self_channel))
        .route("/users/{user_id}", get(get_user))
        .route("/users/{user_id}/profile", get(get_user_profile))
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ))
}

/// GET /api/v1/users/@me — Get the authenticated user's profile.
async fn get_current_user(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> NexusResult<Json<UserResponse>> {
    let user = users::find_by_id(&state.db.pool, auth.user_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "User".into(),
        })?;

    Ok(Json(user.into()))
}

/// PATCH /api/v1/users/@me — Update the authenticated user's profile.
async fn update_current_user(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<UpdateUserRequest>,
) -> NexusResult<Json<UserResponse>> {
    validate_request(&body)?;

    // If changing username, check availability
    if let Some(ref new_username) = body.username
        && let Some(existing) = users::find_by_username(&state.db.pool, new_username).await?
            && existing.id != auth.user_id {
                return Err(NexusError::AlreadyExists {
                    resource: "Username".into(),
                });
            }

    let user = users::update_user(
        &state.db.pool,
        auth.user_id,
        body.username.as_deref(),
        body.display_name.as_deref(),
        body.bio.as_deref(),
        body.status.as_deref(),
        body.avatar.as_deref(),
    )
    .await?;

    Ok(Json(user.into()))
}

/// GET /api/v1/users/:user_id — Get a user's public profile.
async fn get_user(
    State(state): State<Arc<AppState>>,
    Path(user_id): Path<Uuid>,
) -> NexusResult<Json<UserResponse>> {
    let user = users::find_by_id(&state.db.pool, user_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "User".into(),
        })?;

    Ok(Json(user.into()))
}

/// GET /api/v1/users/:user_id/profile — Enriched profile with mutual servers/friends.
async fn get_user_profile(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(user_id): Path<Uuid>,
) -> NexusResult<Json<serde_json::Value>> {
    let user = users::find_by_id(&state.db.pool, user_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "User".into(),
        })?;

    // Servers in common — both requesting user and target user are members
    let common_rows = sqlx::query(
        "SELECT s.id::text AS id, s.name, s.icon, s.member_count \
         FROM servers s \
         JOIN members m1 ON m1.server_id = s.id AND m1.user_id = $1::uuid \
         JOIN members m2 ON m2.server_id = s.id AND m2.user_id = $2::uuid \
         ORDER BY s.name LIMIT 20",
    )
    .bind(auth.user_id.to_string())
    .bind(user_id.to_string())
    .fetch_all(&state.db.pool)
    .await?;

    use sqlx::Row;
    let servers_in_common: Vec<serde_json::Value> = common_rows
        .iter()
        .map(|row| {
            serde_json::json!({
                "id": row.try_get::<String, _>("id").unwrap_or_default(),
                "name": row.try_get::<String, _>("name").unwrap_or_default(),
                "icon": row.try_get::<Option<String>, _>("icon").unwrap_or(None),
                "member_count": row.try_get::<i64, _>("member_count").unwrap_or_default(),
            })
        })
        .collect();

    // Mutual friends — accepted friends of both the requesting user and the target user
    let mutual_rows = sqlx::query(
        "SELECT DISTINCT u.id::text AS id, u.username, u.display_name, u.avatar \
         FROM users u \
         WHERE u.id != $1::uuid AND u.id != $2::uuid \
         AND EXISTS ( \
             SELECT 1 FROM user_relationships \
             WHERE ((requester_id = $1::uuid AND addressee_id = u.id) \
                    OR (addressee_id = $1::uuid AND requester_id = u.id)) \
             AND status = 'accepted') \
         AND EXISTS ( \
             SELECT 1 FROM user_relationships \
             WHERE ((requester_id = $2::uuid AND addressee_id = u.id) \
                    OR (addressee_id = $2::uuid AND requester_id = u.id)) \
             AND status = 'accepted') \
         ORDER BY u.username LIMIT 20",
    )
    .bind(auth.user_id.to_string())
    .bind(user_id.to_string())
    .fetch_all(&state.db.pool)
    .await?;

    let mutual_friends: Vec<serde_json::Value> = mutual_rows
        .iter()
        .map(|row| {
            serde_json::json!({
                "id": row.try_get::<String, _>("id").unwrap_or_default(),
                "username": row.try_get::<String, _>("username").unwrap_or_default(),
                "display_name": row.try_get::<Option<String>, _>("display_name").unwrap_or(None),
                "avatar": row.try_get::<Option<String>, _>("avatar").unwrap_or(None),
            })
        })
        .collect();

    let resp = UserResponse::from(user);
    Ok(Json(serde_json::json!({
        "id": resp.id,
        "username": resp.username,
        "display_name": resp.display_name,
        "avatar": resp.avatar,
        "banner": resp.banner,
        "bio": resp.bio,
        "status": resp.status,
        "presence": resp.presence,
        "flags": resp.flags,
        "created_at": resp.created_at,
        "servers_in_common": servers_in_common,
        "mutual_friends": mutual_friends,
    })))
}

// ── Account lifecycle (09.7-04) ───────────────────────────────────────────────

/// Body of a delete request.
///
/// Empty: the password confirmation this used to carry cannot be satisfied by
/// an ecosystem-provisioned account, whose stored hash is deliberately
/// unparseable. Kept as a type so the endpoint still takes a JSON body and
/// clients need no change.
#[derive(Deserialize)]
struct DeleteAccountBody {}

/// DELETE /api/v1/users/@me
///
/// Request account deletion.  Sets a 30-day grace period (`scheduled_deletion_at`).
/// The account remains accessible during this window.  A background purge job
/// executes the final deletion after 30 days.
///
/// This is the canonical GDPR "right to erasure" flow.
async fn delete_account(
    Extension(auth_ctx): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(_body): Json<DeleteAccountBody>,
) -> NexusResult<StatusCode> {
    // Rate limit account deletion attempts (3 per hour per user)
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:user:{}:delete_account", auth_ctx.user_id),
        3,
        3600,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:delete_account:tag:{tag}"),
        10,
        3600,
    )
    .await?;

    let user = users::find_by_id(&state.db.pool, auth_ctx.user_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "User".into(),
        })?;

    // No password confirmation. Accounts are provisioned from the ecosystem and
    // carry a deliberately unparseable hash, so this check could only ever fail
    // — it would make the endpoint unusable rather than safe. The protections
    // that remain are real: the rate limits above, and the fact that deletion
    // is scheduled 30 days out and reversible through
    // POST /users/@me/cancel-deletion.

    // Schedule deletion 30 days from now
    sqlx::query(
        "UPDATE users SET scheduled_deletion_at = NOW() + INTERVAL '30 days', \
         updated_at = NOW() WHERE id = $1::uuid",
    )
    .bind(user.id.to_string())
    .execute(&state.db.pool)
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    tracing::info!(user_id = %user.id, "Account deletion scheduled (30-day grace period)");
    Ok(StatusCode::ACCEPTED)
}

#[derive(Serialize)]
struct CancelDeletionResponse {
    scheduled_deletion_at: Option<String>,
}

/// POST /api/v1/users/@me/cancel-deletion
///
/// Cancels a pending account deletion request.
async fn cancel_account_deletion(
    Extension(auth_ctx): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> NexusResult<Json<CancelDeletionResponse>> {
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:user:{}:cancel_delete_account", auth_ctx.user_id),
        5,
        3600,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:cancel_delete_account:tag:{tag}"),
        15,
        3600,
    )
    .await?;

    let canceled = users::cancel_scheduled_deletion(&state.db.pool, auth_ctx.user_id).await?;
    if !canceled {
        return Err(NexusError::NotFound {
            resource: "ScheduledDeletion".into(),
        });
    }

    let user = users::find_by_id(&state.db.pool, auth_ctx.user_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "User".into(),
        })?;

    tracing::info!(user_id = %user.id, "Account deletion cancelled");

    Ok(Json(CancelDeletionResponse {
        scheduled_deletion_at: None,
    }))
}

// ── GDPR data export (09.7-04) ───────────────────────────────────────────────

/// GET /api/v1/users/@me/data-export
///
/// Generate a JSON archive of the authenticated user's data (GDPR Art. 20).
///
/// Includes:
///   - Account profile
///   - Servers the user belongs to
///   - Last 1 000 messages authored by the user
///
/// Note: attachment binaries are NOT included — only metadata + CDN URLs.
async fn data_export(
    Extension(auth_ctx): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> NexusResult<Json<serde_json::Value>> {
    use sqlx::Row;

    let user = users::find_by_id(&state.db.pool, auth_ctx.user_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "User".into(),
        })?;

    // Servers the user is a member of
    let server_rows = sqlx::query(
        "SELECT s.id::text AS id, s.name, s.icon, m.joined_at::text AS joined_at \
         FROM servers s \
         JOIN members m ON m.server_id = s.id AND m.user_id = $1::uuid \
         ORDER BY m.joined_at ASC",
    )
    .bind(user.id.to_string())
    .fetch_all(&state.db.pool)
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    let servers: Vec<serde_json::Value> = server_rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.try_get::<String, _>("id").unwrap_or_default(),
                "name": r.try_get::<String, _>("name").unwrap_or_default(),
                "icon": r.try_get::<Option<String>, _>("icon").unwrap_or(None),
                "joined_at": r.try_get::<String, _>("joined_at").unwrap_or_default(),
            })
        })
        .collect();

    // Last 1 000 messages authored by the user
    let msg_rows = sqlx::query(
        "SELECT id::text AS id, channel_id::text AS channel_id, content, \
                created_at::text AS created_at \
         FROM messages \
         WHERE author_id = $1::uuid \
         ORDER BY created_at DESC LIMIT 1000",
    )
    .bind(user.id.to_string())
    .fetch_all(&state.db.pool)
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    let messages: Vec<serde_json::Value> = msg_rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.try_get::<String, _>("id").unwrap_or_default(),
                "channel_id": r.try_get::<String, _>("channel_id").unwrap_or_default(),
                "content": r.try_get::<Option<String>, _>("content").unwrap_or(None),
                "created_at": r.try_get::<String, _>("created_at").unwrap_or_default(),
            })
        })
        .collect();

    tracing::info!(user_id = %user.id, "Data export generated");

    let user_resp = UserResponse::from(user.clone());
    Ok(Json(serde_json::json!({
        "export_version": "1.0",
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "user": {
            "id": user_resp.id,
            "username": user_resp.username,
            "display_name": user_resp.display_name,
            "email": user.email,
            "bio": user_resp.bio,
            "avatar": user_resp.avatar,
            "banner": user_resp.banner,
            "created_at": user_resp.created_at,
        },
        "servers": servers,
        "messages": {
            "count": messages.len(),
            "note": "Last 1000 messages. Contact support for a full archive.",
            "data": messages,
        },
    })))
}

// ── Note-to-Self ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct NoteToSelfResponse {
    channel_id: String,
}

/// GET /api/v1/users/@me/note-to-self
///
/// Returns the channel ID of the authenticated user's note-to-self channel
/// (a private DM the user has with themselves, shown as "Saved Notes" in the
/// client — similar to Telegram's Saved Messages feature).
///
/// The channel is created automatically on account registration.  If for some
/// reason it doesn't exist yet this handler creates it on the fly.
async fn get_note_to_self_channel(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> NexusResult<Json<NoteToSelfResponse>> {
    // Look up existing association
    let existing: Option<String> =
        sqlx::query_scalar("SELECT channel_id::text FROM note_to_self_channels WHERE user_id = $1")
            .bind(auth.user_id.to_string())
            .fetch_optional(&state.db.pool)
            .await
            .map_err(|e| NexusError::Internal(e.into()))?;

    if let Some(channel_id) = existing {
        return Ok(Json(NoteToSelfResponse { channel_id }));
    }

    // Not found — create channel + association on-the-fly (idempotent via ON CONFLICT)
    let note_channel_id = nexus_common::snowflake::generate_id();
    sqlx::query(
        "INSERT INTO channels (id, channel_type, created_at, updated_at) \
         VALUES ($1::uuid, 'dm', NOW(), NOW()) \
         ON CONFLICT DO NOTHING",
    )
    .bind(note_channel_id.to_string())
    .execute(&state.db.pool)
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    sqlx::query(
        "INSERT INTO note_to_self_channels (user_id, channel_id) \
         VALUES ($1::uuid, $2::uuid) \
         ON CONFLICT DO NOTHING",
    )
    .bind(auth.user_id.to_string())
    .bind(note_channel_id.to_string())
    .execute(&state.db.pool)
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(NoteToSelfResponse {
        channel_id: note_channel_id.to_string(),
    }))
}

