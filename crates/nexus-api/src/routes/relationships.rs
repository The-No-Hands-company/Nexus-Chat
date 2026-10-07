//! Relationship routes — friends, pending requests, and blocks.
//!
//! Endpoints:
//!   GET    /users/@me/relationships             — list all relationships
//!   POST   /users/@me/relationships             — send friend request by username (or username@server)
//!   PATCH  /users/@me/relationships/:user_id    — accept / deny / block
//!   DELETE /users/@me/relationships/:user_id    — remove friend / cancel / unblock
//!   GET    /users/search?q=<username>           — find users by username prefix
//!
//! # Cross-server (federated) friend requests
//!
//! Use `username@serverdomain` as the target to add a user on another Nexus
//! instance.  The server will:
//!
//! 1. Resolve the profile from the remote server via
//!    `GET /_nexus/federation/v1/user/<username>`.
//! 2. Upsert a local shadow account for the remote user (preserving their UUID).
//! 3. Store a `pending` relationship row locally.
//! 4. Forward the request to the remote server via
//!    `PUT /_nexus/federation/v1/friend_request`.

use axum::http::HeaderMap;
use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    middleware,
    routing::get,
};
use chrono::Utc;
use nexus_common::{
    error::{NexusError, NexusResult},
    models::relationship::RelationshipStatus,
    snowflake,
};
use nexus_db::repository::{relationships, users};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    AppState,
    middleware::{AuthContext, check_rate_limit_with_fallback, extract_client_tag},
};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/users/@me/relationships",
            get(list_relationships).post(send_friend_request),
        )
        .route(
            "/users/@me/relationships/{user_id}",
            axum::routing::patch(update_relationship).delete(delete_relationship),
        )
        .route("/users/search", get(search_users))
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ))
}

// ── Request/Response shapes ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SendFriendRequest {
    /// Exact username of the user to add.
    username: String,
}

#[derive(Debug, Deserialize)]
struct UpdateRelationshipRequest {
    /// "accept" | "deny" | "block"
    action: String,
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    q: String,
}

#[derive(Debug, Serialize)]
struct RelationshipResponse {
    id: Uuid,
    /// Which direction: "outgoing" (I sent) or "incoming" (they sent).
    direction: String,
    status: String,
    user: UserBrief,
}

#[derive(Debug, Serialize)]
struct UserBrief {
    id: Uuid,
    username: String,
    display_name: Option<String>,
    avatar: Option<String>,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parse a `username@server.tld` string into `(username, server_name)`.
///
/// Supports both `alice@nexus.other.tld` and `@alice@nexus.other.tld`.
/// Returns `None` for plain local usernames.
fn parse_federated_username(input: &str) -> Option<(&str, &str)> {
    // Strip leading `@` if MXID-style (`@alice@server` or `@alice:server` — we only handle `@`-sep)
    let stripped = input.strip_prefix('@').unwrap_or(input);
    // Find the LAST `@` to split username from server.
    if let Some(pos) = stripped.rfind('@') {
        let username = &stripped[..pos];
        let server = &stripped[pos + 1..];
        if !username.is_empty() && server.contains('.') {
            return Some((username, server));
        }
    }
    None
}

/// Build the display username for a user — `username@server_name` for remote users,
/// plain `username` for local ones.
fn qualified_username(username: &str, server_name: Option<&str>) -> String {
    match server_name {
        Some(s) => format!("{}@{}", username, s),
        None => username.to_owned(),
    }
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /api/v1/users/@me/relationships
async fn list_relationships(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> NexusResult<Json<Vec<RelationshipResponse>>> {
    let rows = relationships::list_for_user(&state.db.pool, auth.user_id).await?;

    let other_ids: Vec<Uuid> = rows
        .iter()
        .map(|rel| {
            if rel.requester_id == auth.user_id {
                rel.addressee_id
            } else {
                rel.requester_id
            }
        })
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let user_map = users::find_by_ids_map(&state.db.pool, &other_ids).await?;

    let mut result = Vec::with_capacity(rows.len());
    for rel in rows {
        let other_id = if rel.requester_id == auth.user_id {
            rel.addressee_id
        } else {
            rel.requester_id
        };
        let direction = if rel.requester_id == auth.user_id {
            "outgoing".to_string()
        } else {
            "incoming".to_string()
        };

        let status_str = match rel.status {
            RelationshipStatus::Pending => "pending",
            RelationshipStatus::Accepted => "accepted",
            RelationshipStatus::Blocked => "blocked",
        };

        if let Some(other) = user_map.get(&other_id) {
            result.push(RelationshipResponse {
                id: other.id,
                direction,
                status: status_str.to_string(),
                user: UserBrief {
                    id: other.id,
                    username: qualified_username(&other.username, other.server_name.as_deref()),
                    display_name: other.display_name.clone(),
                    avatar: other.avatar.clone(),
                },
            });
        }
    }

    Ok(Json(result))
}

/// POST /api/v1/users/@me/relationships — send a friend request by username.
///
/// Accepts both `username` (local) and `username@server.tld` (federated).
async fn send_friend_request(
    Extension(auth): Extension<AuthContext>,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(body): Json<SendFriendRequest>,
) -> NexusResult<Json<RelationshipResponse>> {
    // Rate limiting: 20 friend requests per hour per user, 40 per IP
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:friendreq:user:{}", auth.user_id),
        20,
        3600,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:friendreq:tag:{tag}"),
        40,
        3600,
    )
    .await?;

    // Detect federated username (`user@server.tld`)
    if let Some((target_username, target_server)) = parse_federated_username(&body.username) {
        return send_federated_friend_request(&auth, &state, target_username, target_server).await;
    }

    // ── Local path ────────────────────────────────────────────────────────────
    let target = users::find_by_username(&state.db.pool, &body.username)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "User".into(),
        })?;

    if target.id == auth.user_id {
        return Err(NexusError::Validation {
            message: "Cannot add yourself as a friend".into(),
        });
    }

    if let Some(_existing) =
        relationships::find_between(&state.db.pool, auth.user_id, target.id).await?
    {
        return Err(NexusError::Validation {
            message: "A relationship with this user already exists".into(),
        });
    }

    let rel_id = snowflake::generate_id();
    let _rel =
        relationships::create(&state.db.pool, rel_id, auth.user_id, target.id, "pending").await?;

    Ok(Json(RelationshipResponse {
        id: target.id,
        direction: "outgoing".to_string(),
        status: "pending".to_string(),
        user: UserBrief {
            id: target.id,
            username: target.username.clone(),
            display_name: target.display_name.clone(),
            avatar: target.avatar.clone(),
        },
    }))
}

/// Inner federated path for `send_friend_request`.
async fn send_federated_friend_request(
    auth: &AuthContext,
    state: &Arc<AppState>,
    target_username: &str,
    target_server: &str,
) -> NexusResult<Json<RelationshipResponse>> {
    // Load our own profile so we can send our metadata to the remote server.
    let me = users::find_by_id(&state.db.pool, auth.user_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "User".into(),
        })?;

    // Resolve the remote user's public profile via the S2S federation API.
    let profile = state
        .federation_client
        .get_user_profile(target_server, target_username)
        .await
        .map_err(|e| {
            NexusError::Internal(anyhow::anyhow!(
                "Could not reach remote server {}: {}",
                target_server,
                e
            ))
        })?;

    // The remote server must return `"id"` (UUID) for us to link the relationship.
    let remote_id_str =
        profile
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| NexusError::Validation {
                message:
                    "Remote server did not return a user ID — it may be running an older version"
                        .into(),
            })?;

    let remote_id = Uuid::parse_str(remote_id_str).map_err(|_| NexusError::Validation {
        message: "Remote server returned an invalid user ID".into(),
    })?;

    let remote_username = profile
        .get("username")
        .and_then(|v| v.as_str())
        .unwrap_or(target_username);
    let remote_display_name = profile.get("displayname").and_then(|v| v.as_str());
    let remote_avatar = profile.get("avatar_url").and_then(|v| v.as_str());

    if remote_id == auth.user_id {
        return Err(NexusError::Validation {
            message: "Cannot add yourself as a friend".into(),
        });
    }

    // Upsert a local shadow account for the remote user.
    let shadow = users::upsert_remote_user(
        &state.db.pool,
        remote_id,
        remote_username,
        target_server,
        remote_display_name,
        remote_avatar,
    )
    .await?;

    // Guard against duplicate relationships.
    if relationships::find_between(&state.db.pool, auth.user_id, shadow.id)
        .await?
        .is_some()
    {
        return Err(NexusError::Validation {
            message: "A relationship with this user already exists".into(),
        });
    }

    // Create the local pending relationship.
    let rel_id = snowflake::generate_id();
    relationships::create(&state.db.pool, rel_id, auth.user_id, shadow.id, "pending").await?;

    // Deliver the request to the remote server.
    let fed_req = nexus_federation::FederatedFriendRequest {
        requester_id: auth.user_id.to_string(),
        requester_username: me.username.clone(),
        requester_display_name: me.display_name.clone(),
        requester_avatar: me.avatar.clone(),
        origin_server: state.server_name.clone(),
        target_username: remote_username.to_owned(),
        timestamp: Utc::now(),
    };

    if let Err(e) = state
        .federation_client
        .send_friend_request_to_remote(target_server, &fed_req)
        .await
    {
        tracing::warn!(
            "Failed to deliver federated friend request to {}: {}. \
             Relationship stored locally — will sync on next contact.",
            target_server,
            e
        );
    }

    let username_qualified = qualified_username(remote_username, Some(target_server));
    Ok(Json(RelationshipResponse {
        id: shadow.id,
        direction: "outgoing".to_string(),
        status: "pending".to_string(),
        user: UserBrief {
            id: shadow.id,
            username: username_qualified,
            display_name: shadow.display_name,
            avatar: shadow.avatar,
        },
    }))
}

/// PATCH /api/v1/users/@me/relationships/:user_id — accept / deny / block.
async fn update_relationship(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(other_user_id): Path<Uuid>,
    Json(body): Json<UpdateRelationshipRequest>,
) -> NexusResult<Json<RelationshipResponse>> {
    let rel = relationships::find_between(&state.db.pool, auth.user_id, other_user_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Relationship".into(),
        })?;

    let new_status = match body.action.as_str() {
        "accept" => {
            // Only the addressee can accept.
            if rel.addressee_id != auth.user_id {
                return Err(NexusError::Forbidden);
            }
            "accepted"
        }
        "deny" => {
            // Only the addressee can deny.
            if rel.addressee_id != auth.user_id {
                return Err(NexusError::Forbidden);
            }
            // Delete rather than update status.
            relationships::delete(&state.db.pool, rel.id).await?;
            let other = users::find_by_id(&state.db.pool, other_user_id)
                .await?
                .ok_or(NexusError::NotFound {
                    resource: "User".into(),
                })?;

            // Forward denial to remote server if the requester is federated.
            maybe_forward_friend_response(&auth, &state, &rel, &other, "denied").await;

            return Ok(Json(RelationshipResponse {
                id: other.id,
                direction: "incoming".to_string(),
                status: "denied".to_string(),
                user: UserBrief {
                    id: other.id,
                    username: qualified_username(&other.username, other.server_name.as_deref()),
                    display_name: other.display_name,
                    avatar: other.avatar,
                },
            }));
        }
        "block" => "blocked",
        _ => {
            return Err(NexusError::Validation {
                message: "action must be 'accept', 'deny', or 'block'".into(),
            });
        }
    };

    let updated = relationships::update_status(&state.db.pool, rel.id, new_status).await?;

    let other = users::find_by_id(&state.db.pool, other_user_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "User".into(),
        })?;

    // Forward acceptance to remote server if the requester is federated.
    if new_status == "accepted" {
        maybe_forward_friend_response(&auth, &state, &rel, &other, "accepted").await;
    }

    let direction = if updated.requester_id == auth.user_id {
        "outgoing"
    } else {
        "incoming"
    };

    Ok(Json(RelationshipResponse {
        id: other.id,
        direction: direction.to_string(),
        status: new_status.to_string(),
        user: UserBrief {
            id: other.id,
            username: qualified_username(&other.username, other.server_name.as_deref()),
            display_name: other.display_name,
            avatar: other.avatar,
        },
    }))
}

/// If the other party in a relationship is a remote (federated) user and they
/// were the *requester*, forward the accept/deny response to their home server.
///
/// Errors are logged but not propagated — the local DB is already updated.
async fn maybe_forward_friend_response(
    auth: &AuthContext,
    state: &Arc<AppState>,
    rel: &nexus_common::models::relationship::Relationship,
    other_user: &nexus_common::models::user::User,
    action: &str,
) {
    // Only forward if the other user is a remote shadow account AND was the requester.
    if !other_user.is_remote || rel.requester_id != other_user.id {
        return;
    }
    let Some(ref server_name) = other_user.server_name else {
        return;
    };

    // Load our own profile for the response metadata.
    let me = match users::find_by_id(&state.db.pool, auth.user_id).await {
        Ok(Some(u)) => u,
        _ => return,
    };

    let fed_resp = nexus_federation::FederatedFriendResponse {
        requester_id: other_user.id.to_string(),
        responder_id: auth.user_id.to_string(),
        responder_username: me.username.clone(),
        responder_display_name: me.display_name.clone(),
        responder_avatar: me.avatar.clone(),
        origin_server: state.server_name.clone(),
        action: action.to_owned(),
        timestamp: Utc::now(),
    };

    if let Err(e) = state
        .federation_client
        .respond_to_remote_friend_request(server_name, &fed_resp)
        .await
    {
        tracing::warn!(
            "Failed to forward friend {} response to {}: {}",
            action,
            server_name,
            e
        );
    }
}

/// DELETE /api/v1/users/@me/relationships/:user_id — remove friend / cancel / unblock.
async fn delete_relationship(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(other_user_id): Path<Uuid>,
) -> NexusResult<axum::http::StatusCode> {
    let rel = relationships::find_between(&state.db.pool, auth.user_id, other_user_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Relationship".into(),
        })?;

    relationships::delete(&state.db.pool, rel.id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// GET /api/v1/users/search?q=<prefix> — search users by username prefix.
async fn search_users(
    Extension(auth): Extension<AuthContext>,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Query(params): Query<SearchQuery>,
) -> NexusResult<Json<Vec<UserBrief>>> {
    // Rate limiting: 60 searches per minute per user
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:search:user:{}", auth.user_id),
        60,
        60,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:search:tag:{tag}"),
        120,
        60,
    )
    .await?;

    if params.q.len() < 2 {
        return Err(NexusError::Validation {
            message: "Search query must be at least 2 characters".into(),
        });
    }

    let q = format!(
        "SELECT {cols} FROM users \
         WHERE LOWER(username) LIKE LOWER($1) AND id != $2::uuid \
         ORDER BY username \
         LIMIT 20",
        cols = nexus_db::select_cols::USER_COLS
    );
    let like_pattern = format!("{}%", params.q.to_lowercase());
    let found = sqlx::query_as::<_, nexus_common::models::user::User>(&q)
        .bind(like_pattern)
        .bind(auth.user_id.to_string())
        .fetch_all(&state.db.pool)
        .await?;

    let briefs: Vec<UserBrief> = found
        .into_iter()
        .map(|u| UserBrief {
            id: u.id,
            username: u.username,
            display_name: u.display_name,
            avatar: u.avatar,
        })
        .collect();

    Ok(Json(briefs))
}
