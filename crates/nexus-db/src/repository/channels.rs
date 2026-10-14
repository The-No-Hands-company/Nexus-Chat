//! Channel repository.

use nexus_common::models::channel::Channel;

use uuid::Uuid;

use crate::select_cols::{CHANNEL_COLS, CHANNEL_COLS_C};

/// Create a new channel.
pub async fn create_channel(
    pool: &sqlx::AnyPool,
    id: Uuid,
    server_id: Option<Uuid>,
    parent_id: Option<Uuid>,
    channel_type: &str,
    name: Option<&str>,
    topic: Option<&str>,
    position: i32,
) -> Result<Channel, sqlx::Error> {
    let q = format!(
        "INSERT INTO channels ( \
             id, server_id, parent_id, channel_type, name, topic, position, \
             nsfw, rate_limit_per_user, encrypted, permission_overwrites, \
             archived, locked, created_at, updated_at \
         ) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, $4::channel_type, $5, $6, $7, false, 0, false, '[]', false, false, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP) \
         RETURNING {CHANNEL_COLS}"
    );
    sqlx::query_as::<_, Channel>(&q)
        .bind(id.to_string())
        .bind(server_id.map(|u| u.to_string()))
        .bind(parent_id.map(|u| u.to_string()))
        .bind(channel_type)
        .bind(name)
        .bind(topic)
        .bind(position)
        .fetch_one(pool)
        .await
}

/// List channels in a server.
pub async fn list_server_channels(
    pool: &sqlx::AnyPool,
    server_id: Uuid,
) -> Result<Vec<Channel>, sqlx::Error> {
    let q = format!(
        "SELECT {CHANNEL_COLS} FROM channels WHERE server_id = $1::uuid ORDER BY position, created_at"
    );
    sqlx::query_as::<_, Channel>(&q)
        .bind(server_id.to_string())
        .fetch_all(pool)
        .await
}

/// Find a channel by ID.
pub async fn find_by_id(pool: &sqlx::AnyPool, id: Uuid) -> Result<Option<Channel>, sqlx::Error> {
    let q = format!("SELECT {CHANNEL_COLS} FROM channels WHERE id = $1::uuid");
    sqlx::query_as::<_, Channel>(&q)
        .bind(id.to_string())
        .fetch_optional(pool)
        .await
}

/// Update a channel.
pub async fn update_channel(
    pool: &sqlx::AnyPool,
    id: Uuid,
    name: Option<&str>,
    topic: Option<&str>,
    position: Option<i32>,
    nsfw: Option<bool>,
    rate_limit_per_user: Option<i32>,
    is_stream: Option<bool>,
) -> Result<Channel, sqlx::Error> {
    let q = format!(
        "UPDATE channels SET \
             name = COALESCE($1, name), \
             topic = COALESCE($2, topic), \
             position = COALESCE($3, position), \
             nsfw = COALESCE($4, nsfw), \
             rate_limit_per_user = COALESCE($5, rate_limit_per_user), \
             is_stream = COALESCE($7, is_stream), \
             updated_at = CURRENT_TIMESTAMP \
         WHERE id = $6::uuid \
         RETURNING {CHANNEL_COLS}"
    );
    sqlx::query_as::<_, Channel>(&q)
        .bind(name)
        .bind(topic)
        .bind(position)
        .bind(nsfw)
        .bind(rate_limit_per_user)
        .bind(id.to_string())
        .bind(is_stream)
        .fetch_one(pool)
        .await
}

/// Delete a channel.
pub async fn delete_channel(pool: &sqlx::AnyPool, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM channels WHERE id = $1::uuid")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

/// Create a DM channel between two users.
pub async fn find_or_create_dm(
    pool: &sqlx::AnyPool,
    id: Uuid,
    user1: Uuid,
    user2: Uuid,
) -> Result<Channel, sqlx::Error> {
    // Check for existing DM
    let q = format!(
        "SELECT {CHANNEL_COLS_C} FROM channels c \
         INNER JOIN dm_participants dp1 ON dp1.channel_id = c.id AND dp1.user_id = $1::uuid \
         INNER JOIN dm_participants dp2 ON dp2.channel_id = c.id AND dp2.user_id = $2::uuid \
         WHERE c.channel_type = 'dm' \
         LIMIT 1"
    );
    let existing = sqlx::query_as::<_, Channel>(&q)
        .bind(user1.to_string())
        .bind(user2.to_string())
        .fetch_optional(pool)
        .await?;

    if let Some(channel) = existing {
        return Ok(channel);
    }

    // Create new DM
    let channel = create_channel(pool, id, None, None, "dm", None, None, 0).await?;

    // Add participants
    sqlx::query("INSERT INTO dm_participants (channel_id, user_id) VALUES ($1::uuid, $2::uuid), ($3::uuid, $4::uuid)")
        .bind(channel.id.to_string())
        .bind(user1.to_string())
        .bind(channel.id.to_string())
        .bind(user2.to_string())
        .execute(pool)
        .await?;

    Ok(channel)
}

/// Return true if `user_id` is a participant in the DM/group-DM channel.
pub async fn is_dm_participant(
    pool: &sqlx::AnyPool,
    channel_id: Uuid,
    user_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT COUNT(*)::bigint FROM dm_participants \
         WHERE channel_id = $1::uuid AND user_id = $2::uuid",
    )
    .bind(channel_id.to_string())
    .bind(user_id.to_string())
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(n,)| n > 0).unwrap_or(false))
}

/// Return all participant user IDs for a DM or group DM channel.
///
/// Used by the push notification path to fan out a notification to every
/// participant when a new DM message arrives (not just @mentioned users).
pub async fn list_dm_participants(
    pool: &sqlx::AnyPool,
    channel_id: uuid::Uuid,
) -> Result<Vec<uuid::Uuid>, sqlx::Error> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT user_id::text FROM dm_participants WHERE channel_id = $1::uuid")
            .bind(channel_id.to_string())
            .fetch_all(pool)
            .await?;

    Ok(rows
        .into_iter()
        .filter_map(|(s,)| s.parse::<uuid::Uuid>().ok())
        .collect())
}
