//! PostgreSQL lifecycle coverage for reaction queries through `AnyPool`.
//!
//! Run only against a disposable database whose name includes `test` or
//! `scratch`; fixture identifiers are UUID tagged and are removed afterwards.

use nexus_db::repository::reactions;
use sqlx::AnyPool;
use uuid::Uuid;

async fn scratch_pool() -> AnyPool {
    let url = std::env::var("NEXUS_TEST_DATABASE_URL")
        .expect("set NEXUS_TEST_DATABASE_URL to a scratch PostgreSQL database");
    let database_name = url
        .split(['?', '#'])
        .next()
        .and_then(|without_query| without_query.rsplit('/').next())
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        database_name.contains("test") || database_name.contains("scratch"),
        "refusing to run: NEXUS_TEST_DATABASE_URL must name a test or scratch database"
    );

    sqlx::any::install_default_drivers();
    let pool = sqlx::any::AnyPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("scratch database connects");

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrations apply");

    pool
}

async fn insert_fixtures(
    pool: &AnyPool,
    tag: &str,
    user_id: Uuid,
    message_id: Uuid,
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
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("DELETE FROM messages WHERE id = $1::uuid")
        .bind(message_id.to_string())
        .execute(&mut *transaction)
        .await?;
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
    let pool = scratch_pool().await;
    let tag = Uuid::new_v4().simple().to_string();
    let user_id = Uuid::new_v4();
    let message_id = Uuid::new_v4();

    insert_fixtures(&pool, &tag, user_id, message_id).await?;

    let result = async {
        assert!(reactions::add_reaction(&pool, message_id, user_id, "👍").await?);
        assert!(!reactions::add_reaction(&pool, message_id, user_id, "👍").await?);
        assert_eq!(
            reactions::get_reaction_counts(&pool, message_id).await?[0].count,
            1
        );
        assert!(reactions::has_user_reacted(&pool, message_id, user_id, "👍").await?);
        assert_eq!(
            reactions::get_reactors(&pool, message_id, "👍", 10).await?,
            vec![user_id]
        );
        assert!(reactions::remove_reaction(&pool, message_id, user_id, "👍").await?);
        assert!(!reactions::has_user_reacted(&pool, message_id, user_id, "👍").await?);
        Ok::<_, sqlx::Error>(())
    }
    .await;

    remove_fixtures(&pool, &tag, user_id, message_id).await?;
    result?;
    Ok(())
}
