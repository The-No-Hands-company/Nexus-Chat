//! DM (Direct Message) routes — create and list DM channels.
//!
//! DMs are just channels with type "dm" or "group_dm" and dm_participants entries.
//! Messages in DMs use the same /channels/:id/messages endpoints.

use axum::http::HeaderMap;
use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    middleware,
    routing::{get, post, put},
};
use nexus_common::{
    error::{NexusError, NexusResult},
    gateway_event::GatewayEvent,
    models::channel::Channel,
    snowflake,
};
use nexus_db::{
    repository::{audit_log, channels},
    select_cols::CHANNEL_COLS_C,
};
use serde::Deserialize;
use sqlx::Row;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    AppState,
    middleware::{AuthContext, check_rate_limit_with_fallback, extract_client_tag},
};

/// DM routes — mounted under /users/@me/channels.
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/users/@me/channels", get(list_dm_channels).post(create_dm))
        .route(
            "/users/@me/channels/{channel_id}",
            get(get_dm_channel).patch(update_group_dm),
        )
        // Group DM recipient management
        .route(
            "/users/@me/channels/{channel_id}/recipients/{user_id}",
            post(add_recipient).delete(remove_recipient),
        )
        // Transfer group DM ownership
        .route(
            "/users/@me/channels/{channel_id}/owner",
            put(transfer_dm_owner),
        )
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ))
}

#[derive(Debug, Deserialize)]
struct CreateDmRequest {
    recipient_id: Option<Uuid>,
    recipient_ids: Option<Vec<Uuid>>,
    name: Option<String>,
}

/// GET /api/v1/users/@me/channels — List all DM channels for the current user.
async fn list_dm_channels(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> NexusResult<Json<Vec<serde_json::Value>>> {
    let q = format!(
        "SELECT {CHANNEL_COLS_C} FROM channels c \
         INNER JOIN dm_participants dp ON dp.channel_id = c.id \
         WHERE dp.user_id = $1::uuid AND c.channel_type IN ('dm', 'group_dm') \
         ORDER BY c.updated_at DESC"
    );
    let dms = sqlx::query_as::<_, Channel>(&q)
        .bind(auth.user_id.to_string())
        .fetch_all(&state.db.pool)
        .await?;

    let dm_ids: Vec<Uuid> = dms.iter().map(|dm| dm.id).collect();
    let mut participants_by_channel: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    let mut unique_user_ids: HashSet<Uuid> = HashSet::new();

    if !dm_ids.is_empty() {
        let mut qb = sqlx::QueryBuilder::new(
            "SELECT channel_id::text AS channel_id, user_id::text AS user_id FROM dm_participants WHERE channel_id IN (",
        );
        {
            let mut separated = qb.separated(", ");
            for channel_id in &dm_ids {
                separated.push_bind(channel_id.to_string());
            }
        }
        qb.push(")");

        let rows = qb
            .build()
            .fetch_all(&state.db.pool)
            .await
            .map_err(NexusError::Database)?;

        for row in rows {
            let channel_id = match row.try_get::<String, _>("channel_id") {
                Ok(v) => match v.parse::<Uuid>() {
                    Ok(id) => id,
                    Err(_) => continue,
                },
                Err(_) => continue,
            };
            let user_id = match row.try_get::<String, _>("user_id") {
                Ok(v) => match v.parse::<Uuid>() {
                    Ok(id) => id,
                    Err(_) => continue,
                },
                Err(_) => continue,
            };

            participants_by_channel
                .entry(channel_id)
                .or_default()
                .push(user_id);
            if user_id != auth.user_id {
                unique_user_ids.insert(user_id);
            }
        }
    }

    let user_map = nexus_db::repository::users::find_by_ids_map(
        &state.db.pool,
        &unique_user_ids.into_iter().collect::<Vec<_>>(),
    )
    .await?;

    let mut results = Vec::with_capacity(dms.len());
    for dm in &dms {
        let users = participants_by_channel
            .get(&dm.id)
            .map(|participant_ids| {
                participant_ids
                    .iter()
                    .filter(|uid| **uid != auth.user_id)
                    .filter_map(|uid| user_map.get(uid))
                    .map(|user| {
                        serde_json::json!({
                            "id": user.id,
                            "username": user.username,
                            "display_name": user.display_name,
                            "avatar": user.avatar,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        results.push(serde_json::json!({
            "id": dm.id,
            "channel_type": dm.channel_type,
            "name": dm.name,
            "last_message_id": dm.last_message_id,
            "recipients": users,
            "updated_at": dm.updated_at,
        }));
    }

    Ok(Json(results))
}

/// POST /api/v1/users/@me/channels — Create a DM channel (or return existing).
async fn create_dm(
    headers: HeaderMap,
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateDmRequest>,
) -> NexusResult<Json<serde_json::Value>> {
    // ── Rate limiting: 10 DM creates per user per minute ────────────
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:dm:create:{}", auth.user_id),
        10,
        60,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:dm:create:tag:{tag}"),
        20,
        60,
    )
    .await?;

    if let Some(recipient_id) = body.recipient_id {
        if recipient_id == auth.user_id {
            return Err(NexusError::Validation {
                message: "Cannot DM yourself".into(),
            });
        }

        let recipient = nexus_db::repository::users::find_by_id(&state.db.pool, recipient_id)
            .await?
            .ok_or(NexusError::NotFound {
                resource: "User".into(),
            })?;

        let dm_id = snowflake::generate_id();
        let dm =
            channels::find_or_create_dm(&state.db.pool, dm_id, auth.user_id, recipient_id).await?;

        // Audit log
        let _ = audit_log::write_entry(
            &state.db.pool,
            snowflake::generate_id(),
            dm.id,
            Some(auth.user_id),
            "DM_CREATE",
            Some("channel"),
            Some(dm.id),
            &serde_json::json!({"recipient_id": recipient_id, "recipient_username": recipient.username}),
            None,
        ).await;

        Ok(Json(serde_json::json!({
            "id": dm.id,
            "channel_type": "dm",
            "recipients": [{
                "id": recipient.id,
                "username": recipient.username,
                "display_name": recipient.display_name,
                "avatar": recipient.avatar,
            }],
            "last_message_id": dm.last_message_id,
        })))
    } else if let Some(recipient_ids) = body.recipient_ids {
        let unique_recipient_ids: Vec<Uuid> = recipient_ids
            .iter()
            .copied()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        if unique_recipient_ids.len() != recipient_ids.len() {
            return Err(NexusError::Validation {
                message: "recipient_ids must not contain duplicates".into(),
            });
        }

        if unique_recipient_ids.len() < 2 {
            return Err(NexusError::Validation {
                message: "Group DM requires at least 2 other users".into(),
            });
        }
        if unique_recipient_ids.len() > 9 {
            return Err(NexusError::Validation {
                message: "Group DM can have at most 10 participants".into(),
            });
        }
        if unique_recipient_ids.contains(&auth.user_id) {
            return Err(NexusError::Validation {
                message: "Do not include yourself in recipient_ids".into(),
            });
        }

        let mut all_participants = unique_recipient_ids.clone();
        all_participants.push(auth.user_id);

        let participants_map =
            nexus_db::repository::users::find_by_ids_map(&state.db.pool, &all_participants).await?;
        if participants_map.len() != all_participants.len() {
            return Err(NexusError::NotFound {
                resource: "One or more users".into(),
            });
        }

        let channel_id = snowflake::generate_id();
        let name = body.name.as_deref();

        let channel = channels::create_channel(
            &state.db.pool,
            channel_id,
            None,
            None,
            "group_dm",
            name,
            None,
            0,
        )
        .await?;

        let mut qb =
            sqlx::QueryBuilder::new("INSERT INTO dm_participants (channel_id, user_id) VALUES ");
        {
            let mut separated = qb.separated(", ");
            for uid in &all_participants {
                separated
                    .push("(")
                    .push_bind(channel_id.to_string())
                    .push(", ")
                    .push_bind(uid.to_string())
                    .push(")");
            }
        }
        qb.push(" ON CONFLICT DO NOTHING");
        qb.build().execute(&state.db.pool).await?;

        let users = all_participants
            .iter()
            .filter_map(|uid| participants_map.get(uid))
            .map(|user| {
                serde_json::json!({
                    "id": user.id,
                    "username": user.username,
                    "display_name": user.display_name,
                    "avatar": user.avatar,
                })
            })
            .collect::<Vec<_>>();

        Ok(Json(serde_json::json!({
            "id": channel.id,
            "channel_type": "group_dm",
            "name": channel.name,
            "recipients": users,
            "last_message_id": channel.last_message_id,
        })))
    } else {
        Err(NexusError::Validation {
            message: "Must provide recipient_id (1:1 DM) or recipient_ids (group DM)".into(),
        })
    }
}

/// GET /api/v1/users/@me/channels/:channel_id — Get a specific DM channel.
async fn get_dm_channel(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
) -> NexusResult<Json<serde_json::Value>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT channel_id::text FROM dm_participants \
         WHERE channel_id = $1::uuid AND user_id = $2::uuid LIMIT 1",
    )
    .bind(channel_id.to_string())
    .bind(auth.user_id.to_string())
    .fetch_optional(&state.db.pool)
    .await?;

    if row.is_none() {
        return Err(NexusError::NotFound {
            resource: "Channel".into(),
        });
    }

    let channel = channels::find_by_id(&state.db.pool, channel_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Channel".into(),
        })?;

    let participants: Vec<(String,)> =
        sqlx::query_as("SELECT user_id::text FROM dm_participants WHERE channel_id = $1::uuid")
            .bind(channel_id.to_string())
            .fetch_all(&state.db.pool)
            .await?;

    let other_ids: Vec<Uuid> = participants
        .iter()
        .filter_map(|(uid_str,)| uid_str.parse::<Uuid>().ok())
        .filter(|uid| *uid != auth.user_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let users_map =
        nexus_db::repository::users::find_by_ids_map(&state.db.pool, &other_ids).await?;
    let users = other_ids
        .iter()
        .filter_map(|uid| users_map.get(uid))
        .map(|user| {
            serde_json::json!({
                "id": user.id,
                "username": user.username,
                "display_name": user.display_name,
                "avatar": user.avatar,
            })
        })
        .collect::<Vec<_>>();

    Ok(Json(serde_json::json!({
        "id": channel.id,
        "channel_type": channel.channel_type,
        "name": channel.name,
        "recipients": users,
        "last_message_id": channel.last_message_id,
        "updated_at": channel.updated_at,
    })))
}

// ============================================================
// Group DM management — name, icon, recipients, ownership
// ============================================================

#[derive(Debug, Deserialize)]
struct UpdateGroupDmRequest {
    name: Option<String>,
    icon: Option<String>, // base64-encoded image or null to clear
}

#[derive(Debug, Deserialize)]
struct TransferOwnerRequest {
    user_id: Uuid,
}

/// Ensure the channel is a group DM and the caller is a participant.
async fn require_group_dm_participant(
    state: &AppState,
    channel_id: Uuid,
    user_id: Uuid,
) -> NexusResult<()> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT c.id::text FROM channels c \
         INNER JOIN dm_participants dp ON dp.channel_id = c.id \
         WHERE c.id = $1::uuid AND dp.user_id = $2::uuid \
           AND c.channel_type = 'group_dm'",
    )
    .bind(channel_id.to_string())
    .bind(user_id.to_string())
    .fetch_optional(&state.db.pool)
    .await?;

    row.ok_or(NexusError::NotFound {
        resource: "Channel".into(),
    })?;
    Ok(())
}

/// Get the current owner of a group DM channel (owner_id column, nullable).
/// Falls back to the first participant if no owner is set.
async fn get_dm_owner(state: &AppState, channel_id: Uuid) -> Option<Uuid> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT owner_id::text FROM channels WHERE id = $1::uuid")
            .bind(channel_id.to_string())
            .fetch_optional(&state.db.pool)
            .await
            .ok()
            .flatten();

    row.and_then(|(s,)| s).and_then(|s| s.parse().ok())
}

/// PATCH /api/v1/users/@me/channels/:channel_id — Update group DM name and/or icon.
async fn update_group_dm(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
    Json(body): Json<UpdateGroupDmRequest>,
) -> NexusResult<Json<serde_json::Value>> {
    require_group_dm_participant(&state, channel_id, auth.user_id).await?;

    if let Some(ref name) = body.name
        && name.len() > 100 {
            return Err(NexusError::Validation {
                message: "Group DM name must be at most 100 characters".into(),
            });
        }

    // Update name if provided
    if let Some(ref name) = body.name {
        sqlx::query("UPDATE channels SET name = $1, updated_at = NOW() WHERE id = $2::uuid")
            .bind(name.as_str())
            .bind(channel_id.to_string())
            .execute(&state.db.pool)
            .await?;
    }

    // Update icon if provided (null string clears the icon)
    if let Some(ref icon) = body.icon {
        let icon_val: Option<&str> = if icon.is_empty() {
            None
        } else {
            Some(icon.as_str())
        };
        sqlx::query("UPDATE channels SET icon = $1, updated_at = NOW() WHERE id = $2::uuid")
            .bind(icon_val)
            .bind(channel_id.to_string())
            .execute(&state.db.pool)
            .await?;
    }

    // Fetch the updated channel row
    let row: Option<(String, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT id::text, name, icon FROM channels WHERE id = $1::uuid")
            .bind(channel_id.to_string())
            .fetch_optional(&state.db.pool)
            .await?;

    let (id_str, name, icon) = row.ok_or(NexusError::NotFound {
        resource: "Channel".into(),
    })?;

    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: nexus_common::gateway_event::event_types::CHANNEL_UPDATE.to_string(),
        data: serde_json::json!({ "id": id_str, "name": name, "icon": icon }),
        server_id: None,
        channel_id: Some(channel_id),
        user_id: Some(auth.user_id),
    });

    Ok(Json(serde_json::json!({
        "id": id_str,
        "channel_type": "group_dm",
        "name": name,
        "icon": icon,
    })))
}

/// POST /api/v1/users/@me/channels/:channel_id/recipients/:user_id
/// Add a user to a group DM (owner only, max 10 total).
async fn add_recipient(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((channel_id, target_user_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<Json<serde_json::Value>> {
    require_group_dm_participant(&state, channel_id, auth.user_id).await?;

    // Only the owner may add recipients
    let owner = get_dm_owner(&state, channel_id).await;
    if owner.map(|o| o != auth.user_id).unwrap_or(false) {
        return Err(NexusError::MissingPermission {
            permission: "group_dm_owner".into(),
        });
    }

    // Check participant count (max 10)
    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM dm_participants WHERE channel_id = $1::uuid")
            .bind(channel_id.to_string())
            .fetch_one(&state.db.pool)
            .await?;

    if count.0 >= 10 {
        return Err(NexusError::Validation {
            message: "Group DM can have at most 10 participants".into(),
        });
    }

    // Verify target user exists
    nexus_db::repository::users::find_by_id(&state.db.pool, target_user_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "User".into(),
        })?;

    sqlx::query(
        "INSERT INTO dm_participants (channel_id, user_id) \
         VALUES ($1::uuid, $2::uuid) ON CONFLICT DO NOTHING",
    )
    .bind(channel_id.to_string())
    .bind(target_user_id.to_string())
    .execute(&state.db.pool)
    .await?;

    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: nexus_common::gateway_event::event_types::CHANNEL_RECIPIENT_ADD.to_string(),
        data: serde_json::json!({ "channel_id": channel_id, "user_id": target_user_id }),
        server_id: None,
        channel_id: Some(channel_id),
        user_id: Some(target_user_id),
    });

    Ok(Json(
        serde_json::json!({ "channel_id": channel_id, "user_id": target_user_id }),
    ))
}

/// DELETE /api/v1/users/@me/channels/:channel_id/recipients/:user_id
/// Remove a recipient from a group DM (self-leave, or owner removing another).
async fn remove_recipient(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((channel_id, target_user_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<Json<serde_json::Value>> {
    require_group_dm_participant(&state, channel_id, auth.user_id).await?;

    // Allow self-leave always. Owner may remove others. Non-owner may only remove themselves.
    let is_self = target_user_id == auth.user_id;
    if !is_self {
        let owner = get_dm_owner(&state, channel_id).await;
        if owner.map(|o| o != auth.user_id).unwrap_or(true) {
            return Err(NexusError::MissingPermission {
                permission: "group_dm_owner".into(),
            });
        }
    }

    sqlx::query("DELETE FROM dm_participants WHERE channel_id = $1::uuid AND user_id = $2::uuid")
        .bind(channel_id.to_string())
        .bind(target_user_id.to_string())
        .execute(&state.db.pool)
        .await?;

    let _ = state.gateway_tx.send(GatewayEvent {
        event_type: nexus_common::gateway_event::event_types::CHANNEL_RECIPIENT_REMOVE.to_string(),
        data: serde_json::json!({ "channel_id": channel_id, "user_id": target_user_id }),
        server_id: None,
        channel_id: Some(channel_id),
        user_id: Some(target_user_id),
    });

    Ok(Json(
        serde_json::json!({ "channel_id": channel_id, "user_id": target_user_id, "removed": true }),
    ))
}

/// PUT /api/v1/users/@me/channels/:channel_id/owner
/// Transfer group DM ownership to another participant (current owner only).
async fn transfer_dm_owner(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<Uuid>,
    Json(body): Json<TransferOwnerRequest>,
) -> NexusResult<Json<serde_json::Value>> {
    require_group_dm_participant(&state, channel_id, auth.user_id).await?;

    // Only current owner may transfer
    let owner = get_dm_owner(&state, channel_id).await;
    // If owner is not set, the caller implicitly becomes the owner by being first
    if owner.map(|o| o != auth.user_id).unwrap_or(false) {
        return Err(NexusError::MissingPermission {
            permission: "group_dm_owner".into(),
        });
    }

    // New owner must be a participant
    let is_participant: Option<(String,)> = sqlx::query_as(
        "SELECT channel_id::text FROM dm_participants \
         WHERE channel_id = $1::uuid AND user_id = $2::uuid",
    )
    .bind(channel_id.to_string())
    .bind(body.user_id.to_string())
    .fetch_optional(&state.db.pool)
    .await?;

    if is_participant.is_none() {
        return Err(NexusError::Validation {
            message: "New owner must be a participant of this group DM".into(),
        });
    }

    sqlx::query("UPDATE channels SET owner_id = $1::uuid, updated_at = NOW() WHERE id = $2::uuid")
        .bind(body.user_id.to_string())
        .bind(channel_id.to_string())
        .execute(&state.db.pool)
        .await?;

    Ok(Json(serde_json::json!({
        "channel_id": channel_id,
        "owner_id": body.user_id,
    })))
}
