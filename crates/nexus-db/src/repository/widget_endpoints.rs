//! Widget endpoint repository.

use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

use nexus_common::models::widget::{WidgetConfig, WidgetEndpoint, WidgetMessage};

fn row_to_endpoint(row: &sqlx::any::AnyRow) -> WidgetEndpoint {
    WidgetEndpoint {
        id: row
            .try_get::<String, _>("id")
            .unwrap_or_default()
            .parse()
            .unwrap_or(Uuid::nil()),
        owner_id: row
            .try_get::<String, _>("owner_id")
            .unwrap_or_default()
            .parse()
            .unwrap_or(Uuid::nil()),
        channel_id: row
            .try_get::<String, _>("channel_id")
            .unwrap_or_default()
            .parse()
            .unwrap_or(Uuid::nil()),
        name: row.try_get("name").unwrap_or_default(),
        widget_id: row.try_get("widget_id").unwrap_or_default(),
        allowed_origins: row.try_get("allowed_origins").unwrap_or(None),
        greeting: row.try_get("greeting").unwrap_or(None),
        active: row.try_get("active").unwrap_or(true),
        created_at: crate::any_compat::get_datetime(row, "created_at").unwrap_or_default(),
        updated_at: crate::any_compat::get_datetime(row, "updated_at").unwrap_or_default(),
    }
}

fn row_to_message(row: &sqlx::any::AnyRow) -> WidgetMessage {
    WidgetMessage {
        id: row
            .try_get::<String, _>("id")
            .unwrap_or_default()
            .parse()
            .unwrap_or(Uuid::nil()),
        widget_id: row.try_get("widget_id").unwrap_or_default(),
        visitor_id: row.try_get("visitor_id").unwrap_or_default(),
        visitor_name: row.try_get("visitor_name").unwrap_or(None),
        content: row.try_get("content").unwrap_or_default(),
        reply_message_id: row
            .try_get::<Option<String>, _>("reply_message_id")
            .unwrap_or(None)
            .and_then(|s| s.parse().ok()),
        created_at: crate::any_compat::get_datetime(row, "created_at").unwrap_or_default(),
    }
}

pub async fn create_widget(
    pool: &sqlx::AnyPool,
    owner_id: Uuid,
    channel_id: Uuid,
    name: &str,
    widget_id: &str,
    allowed_origins: Option<&str>,
    greeting: Option<&str>,
) -> Result<WidgetEndpoint> {
    let row = sqlx::query(
        r#"
        INSERT INTO widget_endpoints (owner_id, channel_id, name, widget_id, allowed_origins, greeting)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING *
        "#,
    )
    .bind(owner_id.to_string())
    .bind(channel_id.to_string())
    .bind(name)
    .bind(widget_id)
    .bind(allowed_origins)
    .bind(greeting)
    .fetch_one(pool)
    .await?;

    Ok(row_to_endpoint(&row))
}

pub async fn get_widget_by_id(
    pool: &sqlx::AnyPool,
    widget_id: &str,
) -> Result<Option<WidgetEndpoint>> {
    let row = sqlx::query("SELECT * FROM widget_endpoints WHERE widget_id = $1")
        .bind(widget_id)
        .fetch_optional(pool)
        .await?;

    Ok(row.as_ref().map(row_to_endpoint))
}

pub async fn get_widget_config(
    pool: &sqlx::AnyPool,
    widget_id: &str,
) -> Result<Option<WidgetConfig>> {
    let row = sqlx::query(
        "SELECT widget_id, name, greeting, active FROM widget_endpoints WHERE widget_id = $1",
    )
    .bind(widget_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| WidgetConfig {
        widget_id: r.try_get("widget_id").unwrap_or_default(),
        name: r.try_get("name").unwrap_or_default(),
        greeting: r.try_get("greeting").unwrap_or(None),
        active: r.try_get("active").unwrap_or(true),
    }))
}

pub async fn list_widgets_for_owner(
    pool: &sqlx::AnyPool,
    owner_id: Uuid,
) -> Result<Vec<WidgetEndpoint>> {
    let rows = sqlx::query(
        "SELECT * FROM widget_endpoints WHERE owner_id = $1 ORDER BY created_at DESC",
    )
    .bind(owner_id.to_string())
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(row_to_endpoint).collect())
}

pub async fn delete_widget(
    pool: &sqlx::AnyPool,
    widget_id: &str,
    owner_id: Uuid,
) -> Result<bool> {
    let result =
        sqlx::query("DELETE FROM widget_endpoints WHERE widget_id = $1 AND owner_id = $2")
            .bind(widget_id)
            .bind(owner_id.to_string())
            .execute(pool)
            .await?;

    Ok(result.rows_affected() > 0)
}

pub async fn insert_widget_message(
    pool: &sqlx::AnyPool,
    widget_id: &str,
    visitor_id: &str,
    visitor_name: Option<&str>,
    content: &str,
) -> Result<WidgetMessage> {
    let row = sqlx::query(
        r#"
        INSERT INTO widget_messages (widget_id, visitor_id, visitor_name, content)
        VALUES ($1, $2, $3, $4)
        RETURNING *
        "#,
    )
    .bind(widget_id)
    .bind(visitor_id)
    .bind(visitor_name)
    .bind(content)
    .fetch_one(pool)
    .await?;

    Ok(row_to_message(&row))
}

pub async fn get_widget_messages(
    pool: &sqlx::AnyPool,
    widget_id: &str,
    visitor_id: &str,
    limit: i64,
) -> Result<Vec<WidgetMessage>> {
    let rows = sqlx::query(
        r#"
        SELECT * FROM widget_messages
        WHERE widget_id = $1 AND visitor_id = $2
        ORDER BY created_at DESC
        LIMIT $3
        "#,
    )
    .bind(widget_id)
    .bind(visitor_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(row_to_message).collect())
}
