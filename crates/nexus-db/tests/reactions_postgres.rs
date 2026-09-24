//! PostgreSQL lifecycle coverage for reaction queries through `AnyPool`.
//!
//! Run only against a disposable database whose name includes `test` or
//! `scratch`; fixture identifiers are UUID tagged and are removed afterwards.

use nexus_db::repository::reactions;
use sqlx::AnyPool;
use uuid::Uuid;

#[test]
fn scratch_database_url_requires_a_postgres_scheme() {
    assert!(validate_scratch_postgres_url("postgres://localhost/nexus_scratch").is_ok());
    assert!(validate_scratch_postgres_url("postgresql://localhost/nexus_test").is_ok());
    assert!(validate_scratch_postgres_url("sqlite://nexus_scratch.db").is_err());
    assert!(validate_scratch_postgres_url("mysql://localhost/nexus_scratch").is_err());
}

fn validate_scratch_postgres_url(url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let scheme = url
        .split_once(':')
        .map(|(scheme, _)| scheme.to_ascii_lowercase())
        .unwrap_or_default();
    if !matches!(scheme.as_str(), "postgres" | "postgresql") {
        return Err("NEXUS_TEST_DATABASE_URL must use postgres:// or postgresql://".into());
    }

    let database_name = url
        .split(['?', '#'])
        .next()
        .and_then(|without_query| without_query.rsplit('/').next())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !database_name.contains("test") && !database_name.contains("scratch") {
        return Err(
            "NEXUS_TEST_DATABASE_URL must name a database containing test or scratch".into(),
        );
    }

    Ok(())
}

async fn scratch_pool() -> Result<AnyPool, Box<dyn std::error::Error>> {
    let url = std::env::var("NEXUS_TEST_DATABASE_URL")
        .expect("set NEXUS_TEST_DATABASE_URL to a scratch PostgreSQL database");
    validate_scratch_postgres_url(&url)?;

    sqlx::any::install_default_drivers();
    let pool = sqlx::any::AnyPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .map_err(|error| format!("scratch database connects: {error}"))?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .map_err(|error| format!("migrations apply: {error}"))?;

    Ok(pool)
}

async fn insert_fixtures(
    pool: &AnyPool,
    tag: &str,
    user_id: Uuid,
    message_id: Uuid,
    second_message_id: Uuid,
) -> Result<(), sqlx::Error> {
    let channel_id = Uuid::new_v4();
    let mut transaction = pool.begin().await?;

    sqlx::query("INSERT INTO users (id, username, password_hash) VALUES ($1::uuid, $2, 'test')")
        .bind(user_id.to_string())
        .bind(tag)
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "INSERT INTO channels (id, channel_type, name) VALUES ($1::uuid, 'text'::channel_type, $2)",
    )
    .bind(channel_id.to_string())
    .bind(format!("reaction-test-{tag}"))
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO messages (id, channel_id, author_id, content) VALUES ($1::uuid, $2::uuid, $3::uuid, 'second reaction fixture')",
    )
    .bind(second_message_id.to_string())
    .bind(channel_id.to_string())
    .bind(user_id.to_string())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO messages (id, channel_id, author_id, content) VALUES ($1::uuid, $2::uuid, $3::uuid, 'reaction fixture')",
    )
    .bind(message_id.to_string())
    .bind(channel_id.to_string())
    .bind(user_id.to_string())
    .execute(&mut *transaction)
    .await?;

    transaction.commit().await
}

async fn remove_fixtures(
    pool: &AnyPool,
    tag: &str,
    user_id: Uuid,
    message_id: Uuid,
    second_message_id: Uuid,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    for id in [message_id, second_message_id] {
        sqlx::query("DELETE FROM messages WHERE id = $1::uuid")
            .bind(id.to_string())
            .execute(&mut *transaction)
            .await?;
    }
    sqlx::query("DELETE FROM users WHERE id = $1::uuid")
        .bind(user_id.to_string())
        .execute(&mut *transaction)
        .await?;
    sqlx::query("DELETE FROM channels WHERE name = $1")
        .bind(format!("reaction-test-{tag}"))
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await
}

#[tokio::test]
#[ignore = "needs a scratch Postgres in NEXUS_TEST_DATABASE_URL"]
async fn reaction_lifecycle_is_portable_through_any_pool() -> Result<(), Box<dyn std::error::Error>>
{
    let pool = scratch_pool().await?;
    let tag = Uuid::new_v4().simple().to_string();
    let user_id = Uuid::new_v4();
    let message_id = Uuid::new_v4();
    let second_message_id = Uuid::new_v4();

    insert_fixtures(&pool, &tag, user_id, message_id, second_message_id)
        .await
        .map_err(|error| format!("reaction fixture insert: {error}"))?;

    let lifecycle_result = async {
        if !reactions::add_reaction(&pool, message_id, user_id, "👍")
            .await
            .map_err(|error| format!("first reaction insert: {error}"))?
        {
            return Err("first reaction insert was not applied".into());
        }
        if reactions::add_reaction(&pool, message_id, user_id, "👍")
            .await
            .map_err(|error| format!("duplicate reaction insert: {error}"))?
        {
            return Err("duplicate reaction insert was applied".into());
        }
        let counts = reactions::get_reaction_counts(&pool, message_id)
            .await
            .map_err(|error| format!("single reaction counts: {error}"))?;
        if counts.first().map(|count| count.count) != Some(1) {
            return Err(format!("expected one reaction count, got {counts:?}").into());
        }
        if !reactions::has_user_reacted(&pool, message_id, user_id, "👍")
            .await
            .map_err(|error| format!("single user reaction lookup: {error}"))?
        {
            return Err("reaction was not reported after insertion".into());
        }
        if reactions::get_reactors(&pool, message_id, "👍", 10)
            .await
            .map_err(|error| format!("reactor listing: {error}"))?
            != vec![user_id]
        {
            return Err("unexpected reaction users".into());
        }
        if !reactions::add_reaction(&pool, message_id, user_id, "🎉")
            .await
            .map_err(|error| format!("second emoji reaction insert: {error}"))?
            || !reactions::add_reaction(&pool, second_message_id, user_id, "👍")
                .await
                .map_err(|error| format!("second message reaction insert: {error}"))?
        {
            return Err("batch reaction fixtures were not applied".into());
        }

        let batch_counts =
            reactions::get_reaction_counts_for_messages(&pool, &[message_id, second_message_id])
                .await
                .map_err(|error| format!("batch reaction counts: {error}"))?;
        if batch_counts.get(&message_id).map(Vec::len) != Some(2)
            || batch_counts
                .get(&second_message_id)
                .and_then(|counts| counts.first())
                .map(|count| count.count)
                != Some(1)
        {
            return Err(format!("unexpected batch reaction counts: {batch_counts:?}").into());
        }

        let batch_user_reactions = reactions::get_user_reaction_emojis_for_messages(
            &pool,
            user_id,
            &[message_id, second_message_id],
        )
        .await
        .map_err(|error| format!("batch user reaction lookup: {error}"))?;
        if !batch_user_reactions
            .get(&message_id)
            .is_some_and(|emojis| emojis.contains("👍") && emojis.contains("🎉"))
            || !batch_user_reactions
                .get(&second_message_id)
                .is_some_and(|emojis| emojis.contains("👍"))
        {
            return Err(
                format!("unexpected batch user reaction lookup: {batch_user_reactions:?}").into(),
            );
        }

        if reactions::remove_all_reactions_for_emoji(&pool, message_id, "👍")
            .await
            .map_err(|error| format!("emoji moderation delete: {error}"))?
            != 1
        {
            return Err("emoji moderation delete removed the wrong row count".into());
        }
        if reactions::remove_all_reactions(&pool, message_id)
            .await
            .map_err(|error| format!("message moderation delete: {error}"))?
            != 1
        {
            return Err("message moderation delete removed the wrong row count".into());
        }
        if !reactions::remove_reaction(&pool, second_message_id, user_id, "👍")
            .await
            .map_err(|error| format!("single reaction removal: {error}"))?
        {
            return Err("single reaction removal was not applied".into());
        }
        if reactions::has_user_reacted(&pool, second_message_id, user_id, "👍")
            .await
            .map_err(|error| format!("post-removal reaction lookup: {error}"))?
        {
            return Err("reaction remained after single removal".into());
        }
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;

    let cleanup_result = remove_fixtures(&pool, &tag, user_id, message_id, second_message_id).await;
    match (lifecycle_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(lifecycle_error), Ok(())) => Err(lifecycle_error),
        (Ok(()), Err(cleanup_error)) => {
            Err(format!("reaction fixture cleanup failed: {cleanup_error}").into())
        }
        (Err(lifecycle_error), Err(cleanup_error)) => Err(format!(
            "reaction lifecycle failed: {lifecycle_error}; fixture cleanup also failed: {cleanup_error}"
        )
        .into()),
    }
}
