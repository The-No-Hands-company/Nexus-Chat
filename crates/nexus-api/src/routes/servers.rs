//! Server (guild) routes — create, join, leave, manage.

use axum::http::HeaderMap;
use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    middleware,
    routing::{get, patch, post},
};
use nexus_common::{
    error::{NexusError, NexusResult},
    models::{
        role::{CreateRoleRequest, Role, UpdateRoleRequest},
        server::{CreateServerRequest, Invite, ServerResponse, UpdateServerRequest},
    },
    permissions::Permissions,
    snowflake,
    validation::validate_request,
};
use nexus_db::repository::{audit_log, channels, members, roles, servers, users};
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    AppState,
    middleware::{AuthContext, check_rate_limit_with_fallback, extract_client_tag},
};

/// Server routes.
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/servers", get(list_my_servers).post(create_server))
        .route(
            "/servers/{server_id}",
            get(get_server).patch(update_server).delete(delete_server),
        )
        .route("/servers/{server_id}/members", get(list_members))
        .route("/servers/{server_id}/join", post(join_server))
        .route("/servers/{server_id}/leave", post(leave_server))
        .route(
            "/servers/{server_id}/invites",
            get(list_invites_route).post(create_invite_route),
        )
        .route("/invites/{code}", get(get_invite_route))
        .route("/invites/{code}/join", post(join_via_invite_route))
        .route(
            "/servers/{server_id}/roles",
            get(list_roles_route).post(create_role_route),
        )
        .route(
            "/servers/{server_id}/roles/{role_id}",
            patch(update_role_route).delete(delete_role_route),
        )
        .route(
            "/servers/{server_id}/transfer-ownership",
            post(transfer_ownership_route),
        )
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ))
}

/// Generate a cryptographically secure random invite code.
///
/// 12 alphanumeric characters (upper+lower+digits, base-62) gives ~71 bits
/// of entropy — resistant to brute-force even without rate limiting, and
/// consistent with Slack/Discord invite code strength.
fn generate_invite_code() -> String {
    use rand::distr::{Alphanumeric, Distribution};
    Alphanumeric
        .sample_iter(rand::rng())
        .take(12)
        .map(|c| (c as char).to_uppercase().next().unwrap_or(c as char))
        .collect()
}

/// GET /api/v1/servers — List servers the authenticated user is a member of.
async fn list_my_servers(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> NexusResult<Json<Vec<ServerResponse>>> {
    let user_servers = servers::list_user_servers(&state.db.pool, auth.user_id).await?;
    Ok(Json(user_servers.into_iter().map(|s| s.into()).collect()))
}

/// POST /api/v1/servers — Create a new server.
async fn create_server(
    Extension(auth): Extension<AuthContext>,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateServerRequest>,
) -> NexusResult<Json<ServerResponse>> {
    validate_request(&body)?;

    let config = nexus_common::config::get();

    // Check server limit
    let user_servers = servers::list_user_servers(&state.db.pool, auth.user_id).await?;
    if user_servers.len() >= config.limits.max_servers_per_user as usize {
        return Err(NexusError::LimitReached {
            message: format!(
                "You can be in at most {} servers",
                config.limits.max_servers_per_user
            ),
        });
    }

    // Rate limit server creation to 3 per hour per user.
    // This is to prevent abuse and ensure that server creation is not too expensive.
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:server:create:user:{}", auth.user_id),
        3,    // 3 creates per user
        3600, // per hour
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:server:create:tag:{tag}"),
        5, // 5 per IP
        3600,
    )
    .await?;

    let server_id = snowflake::generate_id();
    let is_public = body.is_public.unwrap_or(false);
    let tags = body.tags.as_deref().unwrap_or(&[]);

    // Enforce max 5 tags, each max 32 chars
    if tags.len() > 5 {
        return Err(NexusError::Validation {
            message: "Maximum 5 tags allowed".into(),
        });
    }
    for tag in tags {
        if tag.len() > 32 || tag.is_empty() {
            return Err(NexusError::Validation {
                message: "Tags must be 1-32 characters".into(),
            });
        }
    }

    // Create the server
    let server = servers::create_server(
        &state.db.pool,
        server_id,
        &body.name,
        auth.user_id,
        is_public,
        tags,
        body.category.as_deref(),
    )
    .await?;

    // Create @everyone role with default permissions
    let everyone_role_id = snowflake::generate_id();
    roles::create_role(
        &state.db.pool,
        everyone_role_id,
        server_id,
        "@everyone",
        None,
        Permissions::default_everyone().bits(),
        0,
        true,
    )
    .await?;

    // Create default channels
    let general_id = snowflake::generate_id();
    channels::create_channel(
        &state.db.pool,
        general_id,
        Some(server_id),
        None,
        "text",
        Some("general"),
        Some("General discussion"),
        0,
    )
    .await?;

    let voice_id = snowflake::generate_id();
    channels::create_channel(
        &state.db.pool,
        voice_id,
        Some(server_id),
        None,
        "voice",
        Some("General"),
        None,
        1,
    )
    .await?;

    // Add creator as member
    members::add_member(&state.db.pool, auth.user_id, server_id).await?;

    tracing::info!(server_id = %server_id, "Server created");

    Ok(Json(server.into()))
}

/// GET /api/v1/servers/:server_id
async fn get_server(
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
) -> NexusResult<Json<ServerResponse>> {
    let server = servers::find_by_id(&state.db.pool, server_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Server".into(),
        })?;

    Ok(Json(server.into()))
}

/// PATCH /api/v1/servers/:server_id
async fn update_server(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
    Json(body): Json<UpdateServerRequest>,
) -> NexusResult<Json<ServerResponse>> {
    validate_request(&body)?;

    let server = servers::find_by_id(&state.db.pool, server_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Server".into(),
        })?;

    // Only owner or admin can update
    if server.owner_id != auth.user_id {
        return Err(NexusError::Forbidden);
    }

    // Enforce max 5 tags, each max 32 chars
    if let Some(ref tags) = body.tags {
        if tags.len() > 5 {
            return Err(NexusError::Validation {
                message: "Maximum 5 tags allowed".into(),
            });
        }
        for tag in tags {
            if tag.len() > 32 || tag.is_empty() {
                return Err(NexusError::Validation {
                    message: "Tags must be 1-32 characters".into(),
                });
            }
        }
    }

    let updated = servers::update_server(
        &state.db.pool,
        server_id,
        body.name.as_deref(),
        body.description.as_deref(),
        body.is_public,
        body.require_2fa,
        body.spam_window_secs,
        body.spam_max_messages,
        body.tags.as_deref(),
        body.category.as_deref(),
        body.tip_jar_url.as_deref(),
    )
    .await?;

    let _ = audit_log::write_entry(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        Some(auth.user_id),
        "SERVER_UPDATE",
        Some("server"),
        None,
        &serde_json::json!({ "name": body.name, "is_public": body.is_public, "require_2fa": body.require_2fa }),
        None,
    )
    .await;

    Ok(Json(updated.into()))
}

/// DELETE /api/v1/servers/:server_id
async fn delete_server(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
) -> NexusResult<Json<serde_json::Value>> {
    let server = servers::find_by_id(&state.db.pool, server_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Server".into(),
        })?;

    if server.owner_id != auth.user_id {
        return Err(NexusError::Forbidden);
    }

    servers::delete_server(&state.db.pool, server_id).await?;

    let _ = audit_log::write_entry(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        Some(auth.user_id),
        "SERVER_DELETE",
        Some("server"),
        None,
        &serde_json::json!({ "name": server.name }),
        None,
    )
    .await;

    tracing::info!(server_id = %server_id, "Server deleted");

    Ok(Json(serde_json::json!({ "deleted": true })))
}

/// GET /api/v1/servers/:server_id/members
async fn list_members(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
) -> NexusResult<Json<serde_json::Value>> {
    if !members::is_member(&state.db.pool, auth.user_id, server_id).await? {
        return Err(NexusError::Forbidden);
    }

    let rows = sqlx::query(
        "SELECT m.user_id::text AS user_id, m.nickname, \
         COALESCE(array_to_json(m.roles), '[]'::json)::text AS roles, \
         m.muted, m.deafened, m.joined_at::text AS joined_at, \
         u.username, u.display_name, u.avatar, u.presence::text AS presence \
         FROM members m \
         JOIN users u ON u.id = m.user_id \
         WHERE m.server_id = $1::uuid \
         ORDER BY u.username \
         LIMIT 500",
    )
    .bind(server_id.to_string())
    .fetch_all(&state.db.pool)
    .await?;

    use sqlx::Row;
    let members_json: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            serde_json::json!({
                "user_id": row.try_get::<String, _>("user_id").unwrap_or_default(),
                "nickname": row.try_get::<Option<String>, _>("nickname").unwrap_or(None),
                "roles": row.try_get::<String, _>("roles").unwrap_or_else(|_| "[]".into()),
                "muted": row.try_get::<bool, _>("muted").unwrap_or_default(),
                "deafened": row.try_get::<bool, _>("deafened").unwrap_or_default(),
                "joined_at": row.try_get::<String, _>("joined_at").unwrap_or_default(),
                "username": row.try_get::<String, _>("username").unwrap_or_default(),
                "display_name": row.try_get::<Option<String>, _>("display_name").unwrap_or(None),
                "avatar": row.try_get::<Option<String>, _>("avatar").unwrap_or(None),
                "presence": row.try_get::<String, _>("presence").unwrap_or_else(|_| "offline".into()),
            })
        })
        .collect();

    Ok(Json(serde_json::json!(members_json)))
}

/// POST /api/v1/servers/:server_id/join
async fn join_server(
    Extension(auth): Extension<AuthContext>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
) -> NexusResult<Json<serde_json::Value>> {
    // Rate limiting: 10 joins per hour per user, 20 per IP
    // Protects against rapid join/leave cycles and spam joins
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:join:user:{}", auth.user_id),
        10,
        3600,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:join:tag:{tag}"),
        20,
        3600,
    )
    .await?;

    // Check server exists and is public (or user has invite)
    let server = servers::find_by_id(&state.db.pool, server_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Server".into(),
        })?;

    if !server.is_public {
        return Err(NexusError::Forbidden);
    }

    // If the server requires 2FA, the joining user must have TOTP enabled.
    if server.require_2fa {
        let user = users::find_by_id(&state.db.pool, auth.user_id)
            .await?
            .ok_or(NexusError::Unauthorized)?;
        if !user.totp_enabled {
            return Err(NexusError::Validation {
                message: "This server requires two-factor authentication (TOTP) to join.".into(),
            });
        }
    }

    // Check if already a member
    if members::is_member(&state.db.pool, auth.user_id, server_id).await? {
        return Err(NexusError::AlreadyExists {
            resource: "Membership".into(),
        });
    }

    members::add_member(&state.db.pool, auth.user_id, server_id).await?;
    servers::increment_member_count(&state.db.pool, server_id).await?;

    Ok(Json(serde_json::json!({ "joined": true })))
}

/// POST /api/v1/servers/:server_id/leave
async fn leave_server(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
) -> NexusResult<Json<serde_json::Value>> {
    let server = servers::find_by_id(&state.db.pool, server_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Server".into(),
        })?;

    // Owner can't leave (must transfer or delete)
    if server.owner_id == auth.user_id {
        return Err(NexusError::Validation {
            message: "Server owner cannot leave. Transfer ownership or delete the server.".into(),
        });
    }

    members::remove_member(&state.db.pool, auth.user_id, server_id).await?;
    servers::decrement_member_count(&state.db.pool, server_id).await?;

    Ok(Json(serde_json::json!({ "left": true })))
}
/// GET /api/v1/servers/:server_id/invites — List active invites for a server.
async fn list_invites_route(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
) -> NexusResult<Json<Vec<Invite>>> {
    let _server = servers::find_by_id(&state.db.pool, server_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Server".into(),
        })?;
    let is_member = members::is_member(&state.db.pool, auth.user_id, server_id).await?;
    if !is_member {
        return Err(NexusError::Forbidden);
    }
    let invites = servers::list_server_invites(&state.db.pool, server_id).await?;
    Ok(Json(invites))
}

/// POST /api/v1/servers/:server_id/invites
async fn create_invite_route(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
    Json(body): Json<nexus_common::models::server::CreateInviteRequest>,
) -> NexusResult<Json<serde_json::Value>> {
    // Must be a member to create an invite
    if !members::is_member(&state.db.pool, auth.user_id, server_id).await? {
        return Err(NexusError::Forbidden);
    }

    let code = generate_invite_code();
    let expires_at = body
        .max_age_secs
        .filter(|&s| s > 0)
        .map(|s| chrono::Utc::now() + chrono::Duration::seconds(s as i64));
    let max_uses = body.max_uses.filter(|&u| u > 0);

    let invite = servers::create_invite(
        &state.db.pool,
        &code,
        server_id,
        None,
        auth.user_id,
        max_uses,
        expires_at,
    )
    .await?;

    let _ = audit_log::write_entry(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        Some(auth.user_id),
        "INVITE_CREATE",
        Some("invite"),
        None,
        &serde_json::json!({ "code": invite.code }),
        None,
    )
    .await;

    Ok(Json(serde_json::json!({ "code": invite.code })))
}

/// GET /api/v1/invites/:code — public, no auth required
async fn get_invite_route(
    State(state): State<Arc<AppState>>,
    Path(code): Path<String>,
) -> NexusResult<Json<serde_json::Value>> {
    let invite =
        servers::find_invite(&state.db.pool, &code)
            .await?
            .ok_or(NexusError::NotFound {
                resource: "Invite".into(),
            })?;

    if let Some(exp) = invite.expires_at
        && exp < chrono::Utc::now() {
            return Err(NexusError::NotFound {
                resource: "Invite".into(),
            });
        }
    if let Some(max) = invite.max_uses
        && invite.uses >= max {
            return Err(NexusError::NotFound {
                resource: "Invite".into(),
            });
        }

    let server = servers::find_by_id(&state.db.pool, invite.server_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Server".into(),
        })?;

    Ok(Json(serde_json::json!({
        "code": invite.code,
        "server": { "id": server.id, "name": server.name, "member_count": server.member_count },
        "uses": invite.uses,
        "max_uses": invite.max_uses,
    })))
}

/// POST /api/v1/invites/:code/join
async fn join_via_invite_route(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(code): Path<String>,
) -> NexusResult<Json<serde_json::Value>> {
    // Rate limit invite joins to prevent brute-force enumeration of invite codes.
    // Even with 12-char codes (~71 bit entropy) an attacker sending millions of
    // requests could eventually find a valid code without this guard.
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:invite:user:{}", auth.user_id),
        10,  // 10 join attempts per user
        300, // per 5 minutes
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:invite:tag:{tag}"),
        20, // 20 per IP
        300,
    )
    .await?;
    let invite =
        servers::find_invite(&state.db.pool, &code)
            .await?
            .ok_or(NexusError::NotFound {
                resource: "Invite".into(),
            })?;

    if let Some(exp) = invite.expires_at
        && exp < chrono::Utc::now() {
            return Err(NexusError::Validation {
                message: "Invite has expired.".into(),
            });
        }
    if let Some(max) = invite.max_uses
        && invite.uses >= max {
            return Err(NexusError::Validation {
                message: "Invite has reached its maximum uses.".into(),
            });
        }

    let server_id = invite.server_id;

    // Already a member — just return the server info
    if members::is_member(&state.db.pool, auth.user_id, server_id).await? {
        let server = servers::find_by_id(&state.db.pool, server_id)
            .await?
            .ok_or(NexusError::NotFound {
                resource: "Server".into(),
            })?;
        return Ok(Json(serde_json::json!({
            "server": { "id": server.id, "name": server.name }
        })));
    }

    // If the server requires 2FA, the joining user must have TOTP enabled.
    let server = servers::find_by_id(&state.db.pool, server_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Server".into(),
        })?;
    if server.require_2fa {
        let user = users::find_by_id(&state.db.pool, auth.user_id)
            .await?
            .ok_or(NexusError::Unauthorized)?;
        if !user.totp_enabled {
            return Err(NexusError::Validation {
                message: "This server requires two-factor authentication (TOTP) to join.".into(),
            });
        }
    }

    members::add_member(&state.db.pool, auth.user_id, server_id).await?;
    servers::use_invite(&state.db.pool, &code).await?;
    servers::increment_member_count(&state.db.pool, server_id).await?;

    let server = servers::find_by_id(&state.db.pool, server_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Server".into(),
        })?;

    Ok(Json(serde_json::json!({
        "server": { "id": server.id, "name": server.name }
    })))
}

// ─── Role management ────────────────────────────────────────────────────────

/// GET /api/v1/servers/:server_id/roles — List all roles for a server.
async fn list_roles_route(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
) -> NexusResult<Json<Vec<Role>>> {
    if !members::is_member(&state.db.pool, auth.user_id, server_id).await? {
        return Err(NexusError::Forbidden);
    }

    let server_roles = roles::list_server_roles(&state.db.pool, server_id).await?;
    Ok(Json(server_roles))
}

/// POST /api/v1/servers/:server_id/roles — Create a new role.
async fn create_role_route(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
    Json(body): Json<CreateRoleRequest>,
) -> NexusResult<Json<Role>> {
    validate_request(&body)?;

    let server = servers::find_by_id(&state.db.pool, server_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Server".into(),
        })?;

    // Only the server owner can manage roles
    if server.owner_id != auth.user_id {
        return Err(NexusError::Forbidden);
    }

    let role_id = snowflake::generate_id();
    let permissions = body.permissions.unwrap_or(0);
    let position = body.position.unwrap_or(1);

    let role = roles::create_role(
        &state.db.pool,
        role_id,
        server_id,
        &body.name,
        body.color,
        permissions,
        position,
        false,
    )
    .await?;

    tracing::info!(role_id = %role_id, server_id = %server_id, "Role created");

    let _ = audit_log::write_entry(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        Some(auth.user_id),
        "ROLE_CREATE",
        Some("role"),
        Some(role_id),
        &serde_json::json!({ "name": body.name, "permissions": permissions }),
        None,
    )
    .await;

    Ok(Json(role))
}

/// PATCH /api/v1/servers/:server_id/roles/:role_id — Update a role.
async fn update_role_route(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((server_id, role_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<UpdateRoleRequest>,
) -> NexusResult<Json<Role>> {
    validate_request(&body)?;

    let server = servers::find_by_id(&state.db.pool, server_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Server".into(),
        })?;

    if server.owner_id != auth.user_id {
        return Err(NexusError::Forbidden);
    }

    let role = roles::find_by_id(&state.db.pool, role_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Role".into(),
        })?;

    // Ensure the role belongs to this server
    if role.server_id != server_id {
        return Err(NexusError::NotFound {
            resource: "Role".into(),
        });
    }

    let updated = roles::update_role(
        &state.db.pool,
        role_id,
        body.name.as_deref(),
        body.color,
        body.permissions,
        body.position,
        body.hoist,
        body.mentionable,
    )
    .await?;

    let _ = audit_log::write_entry(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        Some(auth.user_id),
        "ROLE_UPDATE",
        Some("role"),
        Some(role_id),
        &serde_json::json!({ "name": body.name, "permissions": body.permissions }),
        None,
    )
    .await;

    Ok(Json(updated))
}

/// DELETE /api/v1/servers/:server_id/roles/:role_id — Delete a role.
async fn delete_role_route(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((server_id, role_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<StatusCode> {
    let server = servers::find_by_id(&state.db.pool, server_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Server".into(),
        })?;

    if server.owner_id != auth.user_id {
        return Err(NexusError::Forbidden);
    }

    let role = roles::find_by_id(&state.db.pool, role_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Role".into(),
        })?;

    if role.server_id != server_id {
        return Err(NexusError::NotFound {
            resource: "Role".into(),
        });
    }

    if role.is_default {
        return Err(NexusError::Validation {
            message: "Cannot delete the @everyone role.".into(),
        });
    }

    roles::delete_role(&state.db.pool, role_id).await?;

    let _ = audit_log::write_entry(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        Some(auth.user_id),
        "ROLE_DELETE",
        Some("role"),
        Some(role_id),
        &serde_json::json!({ "name": role.name }),
        None,
    )
    .await;

    tracing::info!(role_id = %role_id, server_id = %server_id, "Role deleted");

    Ok(StatusCode::NO_CONTENT)
}

// ── Ownership transfer ────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct TransferOwnershipRequest {
    new_owner_id: Uuid,
}

/// POST /api/v1/servers/:server_id/transfer-ownership
///
/// Transfer the server ownership to another existing member.  The caller must
/// be the current owner.  If the server has `require_2fa` enabled the new
/// owner must also have TOTP configured.
async fn transfer_ownership_route(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
    Json(body): Json<TransferOwnershipRequest>,
) -> NexusResult<Json<ServerResponse>> {
    let server = servers::find_by_id(&state.db.pool, server_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Server".into(),
        })?;

    // Only the current owner may transfer.
    if server.owner_id != auth.user_id {
        return Err(NexusError::Forbidden);
    }

    // Cannot transfer to yourself.
    if body.new_owner_id == auth.user_id {
        return Err(NexusError::Validation {
            message: "New owner must be a different user.".into(),
        });
    }

    // New owner must already be a member of the server.
    if !members::is_member(&state.db.pool, body.new_owner_id, server_id).await? {
        return Err(NexusError::Validation {
            message: "The new owner must already be a member of this server.".into(),
        });
    }

    // If the server requires 2FA, enforce it on the incoming owner too.
    if server.require_2fa {
        let new_owner = users::find_by_id(&state.db.pool, body.new_owner_id)
            .await?
            .ok_or(NexusError::NotFound {
                resource: "User".into(),
            })?;
        if !new_owner.totp_enabled {
            return Err(NexusError::Validation {
                message: "The new owner must have two-factor authentication enabled.".into(),
            });
        }
    }

    let updated = servers::transfer_ownership(&state.db.pool, server_id, body.new_owner_id).await?;

    let _ = audit_log::write_entry(
        &state.db.pool,
        snowflake::generate_id(),
        server_id,
        Some(auth.user_id),
        "SERVER_TRANSFER_OWNERSHIP",
        Some("server"),
        None,
        &serde_json::json!({
            "old_owner_id": auth.user_id,
            "new_owner_id": body.new_owner_id,
        }),
        None,
    )
    .await;

    tracing::info!(
        server_id = %server_id,
        "Server ownership transferred"
    );

    Ok(Json(updated.into()))
}
