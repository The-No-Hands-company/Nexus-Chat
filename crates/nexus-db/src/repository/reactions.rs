//! Reactions repository — add/remove emoji reactions on messages.

use chrono::{DateTime, Utc};
use sqlx::Row;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReactionDialect {
    Postgres,
    Sqlite,
}

impl ReactionDialect {
    async fn detect(pool: &sqlx::AnyPool) -> Result<Self, sqlx::Error> {
        let connection = pool.acquire().await?;
        match connection.backend_name() {
            "PostgreSQL" => Ok(Self::Postgres),
            "SQLite" => Ok(Self::Sqlite),
            backend => Err(sqlx::Error::Configuration(
                format!("unsupported reaction database backend: {backend}").into(),
            )),
        }
    }

    fn uuid_parameter(self, placeholder: &str) -> String {
        match self {
            Self::Postgres => format!("{placeholder}::uuid"),
            Self::Sqlite => placeholder.to_owned(),
        }
    }

    fn uuid_predicate(self, column: &str, placeholder: &str) -> String {
        format!("{column} = {}", self.uuid_parameter(placeholder))
    }

    fn uuid_parameter_list(self, first_index: usize, count: usize) -> String {
        (first_index..first_index + count)
            .map(|index| self.uuid_parameter(&format!("${index}")))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// A reaction row from the database.
#[derive(Debug)]
pub struct ReactionRow {
    pub message_id: Uuid,
    pub user_id: Uuid,
    pub emoji: String,
    pub created_at: DateTime<Utc>,
}

impl<'r> sqlx::FromRow<'r, sqlx::any::AnyRow> for ReactionRow {
    fn from_row(row: &'r sqlx::any::AnyRow) -> Result<Self, sqlx::Error> {
        use crate::any_compat::*;
        Ok(ReactionRow {
            message_id: get_uuid(row, "message_id")?,
            user_id: get_uuid(row, "user_id")?,
            emoji: row.try_get("emoji")?,
            created_at: get_datetime(row, "created_at")?,
        })
    }
}

/// Aggregated reaction count for a specific emoji on a message.
#[derive(Debug, sqlx::FromRow)]
pub struct ReactionCount {
    pub emoji: String,
    pub count: i64,
}

/// Add a reaction to a message. Returns true if newly added, false if already exists.
pub async fn add_reaction(
    pool: &sqlx::AnyPool,
    message_id: Uuid,
    user_id: Uuid,
    emoji: &str,
) -> Result<bool, sqlx::Error> {
    let dialect = ReactionDialect::detect(pool).await?;
    let query = format!(
        "INSERT INTO reactions (message_id, user_id, emoji, created_at) \
         VALUES ({}, {}, $3, CURRENT_TIMESTAMP) \
         ON CONFLICT (message_id, user_id, emoji) DO NOTHING",
        dialect.uuid_parameter("$1"),
        dialect.uuid_parameter("$2")
    );
    let result = sqlx::query(&query)
        .bind(message_id.to_string())
        .bind(user_id.to_string())
        .bind(emoji)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Remove a reaction from a message.
pub async fn remove_reaction(
    pool: &sqlx::AnyPool,
    message_id: Uuid,
    user_id: Uuid,
    emoji: &str,
) -> Result<bool, sqlx::Error> {
    let dialect = ReactionDialect::detect(pool).await?;
    let query = format!(
        "DELETE FROM reactions WHERE {} AND {} AND emoji = $3",
        dialect.uuid_predicate("message_id", "$1"),
        dialect.uuid_predicate("user_id", "$2")
    );
    let result = sqlx::query(&query)
        .bind(message_id.to_string())
        .bind(user_id.to_string())
        .bind(emoji)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Remove all reactions of a specific emoji from a message (moderation).
pub async fn remove_all_reactions_for_emoji(
    pool: &sqlx::AnyPool,
    message_id: Uuid,
    emoji: &str,
) -> Result<u64, sqlx::Error> {
    let dialect = ReactionDialect::detect(pool).await?;
    let query = format!(
        "DELETE FROM reactions WHERE {} AND emoji = $2",
        dialect.uuid_predicate("message_id", "$1")
    );
    let result = sqlx::query(&query)
        .bind(message_id.to_string())
        .bind(emoji)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Remove ALL reactions from a message.
pub async fn remove_all_reactions(
    pool: &sqlx::AnyPool,
    message_id: Uuid,
) -> Result<u64, sqlx::Error> {
    let dialect = ReactionDialect::detect(pool).await?;
    let query = format!(
        "DELETE FROM reactions WHERE {}",
        dialect.uuid_predicate("message_id", "$1")
    );
    let result = sqlx::query(&query)
        .bind(message_id.to_string())
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Get reaction counts for a message, grouped by emoji.
pub async fn get_reaction_counts(
    pool: &sqlx::AnyPool,
    message_id: Uuid,
) -> Result<Vec<ReactionCount>, sqlx::Error> {
    let dialect = ReactionDialect::detect(pool).await?;
    let query = format!(
        "SELECT emoji, COUNT(*) as count \
         FROM reactions WHERE {} \
         GROUP BY emoji ORDER BY MIN(created_at) ASC",
        dialect.uuid_predicate("message_id", "$1")
    );
    sqlx::query_as::<_, ReactionCount>(&query)
        .bind(message_id.to_string())
        .fetch_all(pool)
        .await
}

/// Batch-get reaction counts for many messages.
pub async fn get_reaction_counts_for_messages(
    pool: &sqlx::AnyPool,
    message_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<ReactionCount>>, sqlx::Error> {
    if message_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let dialect = ReactionDialect::detect(pool).await?;
    let id_strings: Vec<String> = message_ids.iter().map(|id| id.to_string()).collect();
    let placeholders = dialect.uuid_parameter_list(1, id_strings.len());
    let query = format!(
        "SELECT CAST(message_id AS TEXT) AS message_id, emoji, COUNT(*) as count \
         FROM reactions WHERE message_id IN ({placeholders}) \
         GROUP BY message_id, emoji ORDER BY MIN(created_at) ASC"
    );
    let mut statement = sqlx::query_as::<_, (String, String, i64)>(&query);
    for id in id_strings {
        statement = statement.bind(id);
    }
    let rows = statement.fetch_all(pool).await?;

    let mut out: HashMap<Uuid, Vec<ReactionCount>> = HashMap::new();
    for (message_id, emoji, count) in rows {
        if let Ok(mid) = message_id.parse::<Uuid>() {
            out.entry(mid)
                .or_default()
                .push(ReactionCount { emoji, count });
        }
    }
    Ok(out)
}

/// Batch-get which emojis a user has reacted with per message.
pub async fn get_user_reaction_emojis_for_messages(
    pool: &sqlx::AnyPool,
    user_id: Uuid,
    message_ids: &[Uuid],
) -> Result<HashMap<Uuid, HashSet<String>>, sqlx::Error> {
    if message_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let dialect = ReactionDialect::detect(pool).await?;
    let id_strings: Vec<String> = message_ids.iter().map(|id| id.to_string()).collect();
    let message_placeholders = dialect.uuid_parameter_list(2, id_strings.len());
    let query = format!(
        "SELECT CAST(message_id AS TEXT) AS message_id, emoji \
         FROM reactions WHERE {} AND message_id IN ({message_placeholders})",
        dialect.uuid_predicate("user_id", "$1")
    );
    let mut statement = sqlx::query_as::<_, (String, String)>(&query).bind(user_id.to_string());
    for id in id_strings {
        statement = statement.bind(id);
    }
    let rows = statement.fetch_all(pool).await?;

    let mut out: HashMap<Uuid, HashSet<String>> = HashMap::new();
    for (message_id, emoji) in rows {
        if let Ok(mid) = message_id.parse::<Uuid>() {
            out.entry(mid).or_default().insert(emoji);
        }
    }
    Ok(out)
}

/// Check if a specific user has reacted with a specific emoji.
pub async fn has_user_reacted(
    pool: &sqlx::AnyPool,
    message_id: Uuid,
    user_id: Uuid,
    emoji: &str,
) -> Result<bool, sqlx::Error> {
    let dialect = ReactionDialect::detect(pool).await?;
    let query = format!(
        "SELECT CASE WHEN EXISTS(SELECT 1 FROM reactions WHERE {} \
         AND {} AND emoji = $3) THEN 1 ELSE 0 END AS ex",
        dialect.uuid_predicate("message_id", "$1"),
        dialect.uuid_predicate("user_id", "$2")
    );
    let row: (i64,) = sqlx::query_as(&query)
        .bind(message_id.to_string())
        .bind(user_id.to_string())
        .bind(emoji)
        .fetch_one(pool)
        .await?;
    Ok(row.0 != 0)
}

/// Get users who reacted with a specific emoji on a message.
pub async fn get_reactors(
    pool: &sqlx::AnyPool,
    message_id: Uuid,
    emoji: &str,
    limit: i64,
) -> Result<Vec<Uuid>, sqlx::Error> {
    let dialect = ReactionDialect::detect(pool).await?;
    let query = format!(
        "SELECT CAST(user_id AS TEXT) AS user_id FROM reactions \
         WHERE {} AND emoji = $2 \
         ORDER BY created_at ASC LIMIT $3",
        dialect.uuid_predicate("message_id", "$1")
    );
    let rows: Vec<(String,)> = sqlx::query_as(&query)
        .bind(message_id.to_string())
        .bind(emoji)
        .bind(limit.min(100))
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().filter_map(|r| r.0.parse().ok()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_uuid_predicate_casts_the_bound_parameter_not_the_indexed_column() {
        let predicate = ReactionDialect::Postgres.uuid_predicate("message_id", "$1");

        assert_eq!(predicate, "message_id = $1::uuid");
        assert!(!predicate.contains("CAST(message_id"));
    }

    #[test]
    fn sqlite_uuid_predicate_keeps_the_text_column_and_parameter_directly_comparable() {
        assert_eq!(
            ReactionDialect::Sqlite.uuid_predicate("message_id", "$1"),
            "message_id = $1"
        );
    }

    #[test]
    fn postgres_batch_parameters_cast_each_bound_value() {
        assert_eq!(
            ReactionDialect::Postgres.uuid_parameter_list(2, 3),
            "$2::uuid, $3::uuid, $4::uuid"
        );
    }

    #[test]
    fn sqlite_batch_parameters_remain_numbered_and_uncast() {
        assert_eq!(
            ReactionDialect::Sqlite.uuid_parameter_list(2, 3),
            "$2, $3, $4"
        );
    }

    #[tokio::test]
    async fn sqlite_executes_single_batch_lookup_and_moderation_queries() {
        sqlx::any::install_default_drivers();
        let pool = sqlx::any::AnyPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect SQLite reaction regression database");
        sqlx::query(
            "CREATE TABLE reactions (\
                message_id TEXT NOT NULL, user_id TEXT NOT NULL, emoji TEXT NOT NULL, \
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, \
                UNIQUE(message_id, user_id, emoji)\
             )",
        )
        .execute(&pool)
        .await
        .expect("create SQLite reaction table");

        let user_id = Uuid::new_v4();
        let first_message_id = Uuid::new_v4();
        let second_message_id = Uuid::new_v4();
        assert!(add_reaction(&pool, first_message_id, user_id, "red").await.unwrap());
        assert!(add_reaction(&pool, first_message_id, user_id, "blue").await.unwrap());
        assert!(add_reaction(&pool, second_message_id, user_id, "red").await.unwrap());

        let counts = get_reaction_counts_for_messages(
            &pool,
            &[first_message_id, second_message_id],
        )
        .await
        .unwrap();
        assert_eq!(counts.get(&first_message_id).map(Vec::len), Some(2));
        assert_eq!(counts.get(&second_message_id).map(Vec::len), Some(1));

        let user_reactions = get_user_reaction_emojis_for_messages(
            &pool,
            user_id,
            &[first_message_id, second_message_id],
        )
        .await
        .unwrap();
        assert!(user_reactions[&first_message_id].contains("red"));
        assert!(user_reactions[&first_message_id].contains("blue"));
        assert!(user_reactions[&second_message_id].contains("red"));

        assert_eq!(
            remove_all_reactions_for_emoji(&pool, first_message_id, "red")
                .await
                .unwrap(),
            1
        );
        assert_eq!(remove_all_reactions(&pool, first_message_id).await.unwrap(), 1);
        assert!(remove_reaction(&pool, second_message_id, user_id, "red")
            .await
            .unwrap());
    }
}
