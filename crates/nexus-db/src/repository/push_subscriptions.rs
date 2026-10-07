//! Push subscription repository — store and retrieve Web Push subscriber records.

use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct PushSubscription {
    pub id: Uuid,
    pub user_id: Uuid,
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// Upsert a push subscription (insert or replace if endpoint already exists).
pub async fn upsert(
    pool: &sqlx::AnyPool,
    user_id: Uuid,
    endpoint: &str,
    p256dh: &str,
    auth: &str,
) -> Result<PushSubscription, sqlx::Error> {
    // Use INSERT … ON CONFLICT to handle re-subscription with the same endpoint.
    let row = sqlx::query_as::<_, PushSubscriptionRow>(
        "INSERT INTO push_subscriptions (id, user_id, endpoint, p256dh, auth) \
         VALUES ($1::uuid, $2::uuid, $3, $4, $5) \
         ON CONFLICT (endpoint) DO UPDATE SET \
           user_id    = EXCLUDED.user_id, \
           p256dh     = EXCLUDED.p256dh, \
           auth       = EXCLUDED.auth \
         RETURNING id::text, user_id::text, endpoint, p256dh, auth, \
                   created_at::text, last_used_at::text",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(user_id.to_string())
    .bind(endpoint)
    .bind(p256dh)
    .bind(auth)
    .fetch_one(pool)
    .await?;

    Ok(row.into())
}

/// Delete a subscription by endpoint (when the browser unsubscribes).
pub async fn delete_by_endpoint(
    pool: &sqlx::AnyPool,
    user_id: Uuid,
    endpoint: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM push_subscriptions \
         WHERE user_id = $1::uuid AND endpoint = $2",
    )
    .bind(user_id.to_string())
    .bind(endpoint)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// Delete all subscriptions for a user (e.g. account deletion, logout-all).
pub async fn delete_all_for_user(pool: &sqlx::AnyPool, user_id: Uuid) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM push_subscriptions WHERE user_id = $1::uuid")
        .bind(user_id.to_string())
        .execute(pool)
        .await?;

    Ok(result.rows_affected())
}

/// Get all subscriptions for a user (to fan-out a notification).
pub async fn list_for_user(
    pool: &sqlx::AnyPool,
    user_id: Uuid,
) -> Result<Vec<PushSubscription>, sqlx::Error> {
    let rows = sqlx::query_as::<_, PushSubscriptionRow>(
        "SELECT id::text, user_id::text, endpoint, p256dh, auth, \
                created_at::text, last_used_at::text \
         FROM push_subscriptions \
         WHERE user_id = $1::uuid \
         ORDER BY created_at DESC",
    )
    .bind(user_id.to_string())
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(Into::into).collect())
}

/// Mark a subscription as successfully delivered to (updates last_used_at).
pub async fn touch(pool: &sqlx::AnyPool, endpoint: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE push_subscriptions SET last_used_at = NOW() WHERE endpoint = $1")
        .bind(endpoint)
        .execute(pool)
        .await?;
    Ok(())
}

/// Remove subscriptions whose endpoints have returned a 410 Gone response
/// (the browser has unsubscribed server-side).
pub async fn delete_gone(pool: &sqlx::AnyPool, endpoints: &[String]) -> Result<u64, sqlx::Error> {
    if endpoints.is_empty() {
        return Ok(0);
    }
    let mut qb = sqlx::QueryBuilder::new("DELETE FROM push_subscriptions WHERE endpoint IN (");
    let mut sep = qb.separated(", ");
    for ep in endpoints {
        sep.push_bind(ep);
    }
    qb.push(")");
    let result = qb.build().execute(pool).await?;
    Ok(result.rows_affected())
}

// ── Internal row type ─────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct PushSubscriptionRow {
    id: String,
    user_id: String,
    endpoint: String,
    p256dh: String,
    auth: String,
    created_at: String,
    last_used_at: Option<String>,
}

impl From<PushSubscriptionRow> for PushSubscription {
    fn from(r: PushSubscriptionRow) -> Self {
        let parse_dt = |s: &str| {
            s.parse::<DateTime<Utc>>()
                .or_else(|_| {
                    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                        .map(|dt| dt.and_utc())
                })
                .unwrap_or_else(|_| Utc::now())
        };

        Self {
            id: r.id.parse().unwrap_or_else(|_| Uuid::new_v4()),
            user_id: r.user_id.parse().unwrap_or_else(|_| Uuid::new_v4()),
            endpoint: r.endpoint,
            p256dh: r.p256dh,
            auth: r.auth,
            created_at: parse_dt(&r.created_at),
            last_used_at: r.last_used_at.as_deref().map(parse_dt),
        }
    }
}
