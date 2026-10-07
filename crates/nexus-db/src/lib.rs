//! # nexus-db
//!
//! Database layer for Nexus.
//!
//! Supports two modes, selected automatically from the `DATABASE_URL`:
//!
//! * **Full mode** (`postgres://…`) — PostgreSQL + optional Redis + MinIO + MeiliSearch.
//! * **Lite mode** (`sqlite://…`) — embedded SQLite, no external services required.

#![allow(clippy::doc_markdown)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::needless_raw_string_hashes)]
#![allow(clippy::redundant_closure)]
#![allow(clippy::uninlined_format_args)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::default_trait_access)]
#![allow(clippy::format_push_string)]
#![allow(clippy::manual_clamp)]
#![allow(clippy::manual_let_else)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::pedantic)]
#![allow(clippy::ref_option)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::type_complexity)]
#![allow(clippy::wildcard_imports)]

pub mod any_compat;
pub mod email_crypto;
pub mod postgres;
pub mod redis_pool;
pub mod repository;
pub mod search;
pub mod select_cols;
pub mod storage;

use anyhow::Result;
use sqlx::migrate::Migrator;

const POSTGRES_MIGRATION_LOCK_ID: i64 = 569_260_301;
const LEGACY_RATCHET_MIGRATION_VERSION: i64 = 20_260_218_000_008;

/// Which backing store is in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbBackend {
    Postgres,
    Sqlite,
}

impl DbBackend {
    pub fn from_url(url: &str) -> Self {
        if url.starts_with("sqlite://") || url.starts_with("sqlite:") {
            DbBackend::Sqlite
        } else {
            DbBackend::Postgres
        }
    }
}

/// Shared database state passed through Axum extractors.
#[derive(Clone)]
pub struct Database {
    /// SQL pool — works with both Postgres and SQLite.
    pub pool: sqlx::AnyPool,
    /// Redis connection (`None` in lite mode or when `REDIS_URL` is unset).
    pub redis: Option<redis::aio::ConnectionManager>,
    /// Which backend is active.
    pub backend: DbBackend,
    /// Whether Scylla integration has been explicitly enabled by config.
    pub scylla_enabled: bool,
    /// Scylla contact points (comma-separated), used by read/write bridge workers.
    pub scylla_nodes: String,
    /// Scylla keyspace used for message tables.
    pub scylla_keyspace: String,
    /// Scylla read strategy (`off`, `canary`, `prefer`).
    pub scylla_read_strategy: String,
}

impl Database {
    /// Connect using the URL in `config.database.url`.
    pub async fn connect(config: &nexus_common::config::AppConfig) -> Result<Self> {
        // Register all built-in drivers (Postgres + SQLite).
        sqlx::any::install_default_drivers();

        let backend = DbBackend::from_url(&config.database.url);

        let pool = match backend {
            DbBackend::Postgres => {
                tracing::info!("Connecting to PostgreSQL…");
                sqlx::any::AnyPoolOptions::new()
                    .max_connections(config.database.max_connections)
                    .min_connections(config.database.min_connections)
                    .connect(&config.database.url)
                    .await?
            }
            DbBackend::Sqlite => {
                tracing::info!("Connecting to SQLite: {}", &config.database.url);
                let pool = sqlx::any::AnyPoolOptions::new()
                    .max_connections(1)
                    .min_connections(1)
                    .connect(&config.database.url)
                    .await?;

                // Applied here rather than in the migration: SQLite cannot
                // change journal mode inside a transaction, sqlx wraps every
                // migration in one, and the `Any` driver ignores the
                // `-- no-transaction` directive that used to opt out. Running
                // them on the live pool is also simply the right place —
                // foreign_keys is per-connection state, not schema.
                //
                // Safe with max_connections(1): both settings apply to the one
                // connection every query uses, and journal_mode persists in the
                // database file regardless.
                sqlx::query("PRAGMA journal_mode = WAL").execute(&pool).await?;
                sqlx::query("PRAGMA foreign_keys = ON").execute(&pool).await?;

                pool
            }
        };

        // Redis — optional in full mode, always skipped in lite mode.
        let redis = if backend == DbBackend::Postgres {
            match &config.redis.url {
                Some(url) => {
                    tracing::info!("Connecting to Redis…");
                    let client = redis::Client::open(url.as_str())?;
                    let mgr = redis::aio::ConnectionManager::new(client).await?;
                    tracing::info!("Connected to Redis");
                    Some(mgr)
                }
                None => {
                    tracing::info!("REDIS_URL not set — using in-process broadcast only");
                    None
                }
            }
        } else {
            None
        };

        if config.scylla.enabled {
            tracing::warn!(
                nodes = %config.scylla.nodes,
                keyspace = %config.scylla.keyspace,
                "Scylla is enabled in config, but message repositories still use SQL in this build"
            );
        }

        Ok(Self {
            pool,
            redis,
            backend,
            scylla_enabled: config.scylla.enabled,
            scylla_nodes: config.scylla.nodes.clone(),
            scylla_keyspace: config.scylla.keyspace.clone(),
            scylla_read_strategy: config.scylla.read_strategy.clone(),
        })
    }

    /// Probe Redis connectivity.
    ///
    /// Returns `None` when Redis is not configured (lite mode / no `REDIS_URL`).
    /// Returns `Some(true)` on a successful PING, `Some(false)` on any error.
    pub async fn health_redis(&self) -> Option<bool> {
        let conn = self.redis.as_ref()?;
        let mut c = conn.clone();
        let ok: bool = redis::cmd("PING")
            .query_async::<String>(&mut c)
            .await
            .map(|s| s == "PONG")
            .unwrap_or(false);
        Some(ok)
    }

    /// Run migrations appropriate for the active backend.
    pub async fn migrate(&self) -> Result<()> {
        tracing::info!("Running database migrations…");
        match self.backend {
            DbBackend::Postgres => {
                self.migrate_postgres().await?;
            }
            DbBackend::Sqlite => {
                sqlx::migrate!("./migrations-lite").run(&self.pool).await?;
            }
        }
        tracing::info!("Migrations complete");
        Ok(())
    }

    async fn migrate_postgres(&self) -> Result<()> {
        static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

        // Guard migrations with a process-shared advisory lock so multiple nodes
        // starting together do not race inserts into `_sqlx_migrations`.
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(POSTGRES_MIGRATION_LOCK_ID)
            .execute(&self.pool)
            .await?;

        let migrate_result: Result<()> = async {
            self.reconcile_legacy_migration_checksum(&MIGRATOR).await?;
            MIGRATOR.run(&self.pool).await?;
            Ok(())
        }
        .await;

        if let Err(err) = sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(POSTGRES_MIGRATION_LOCK_ID)
            .execute(&self.pool)
            .await
        {
            tracing::warn!(error_kind = %nexus_common::logsafe::err_kind(&err), "failed to release postgres migration advisory lock");
        }

        migrate_result?;
        Ok(())
    }

    async fn reconcile_legacy_migration_checksum(&self, migrator: &Migrator) -> Result<()> {
        let Some(expected_checksum) = migrator
            .iter()
            .find(|migration| migration.version == LEGACY_RATCHET_MIGRATION_VERSION)
            .map(|migration| migration.checksum.to_vec())
        else {
            return Ok(());
        };

        // On a fresh database the `_sqlx_migrations` table does not exist yet;
        // `MIGRATOR.run()` creates it afterwards. Nothing to reconcile there.
        let table_exists = sqlx::query_scalar::<_, bool>(
            // Note the spaces before each '\': a Rust line continuation eats the
            // newline AND the next line's indentation, so without them this
            // collapses to "...information_schema.tablesWHERE table_schema...".
            "SELECT EXISTS ( \
                SELECT 1 FROM information_schema.tables \
                WHERE table_schema = ANY (current_schemas(false)) \
                  AND table_name = '_sqlx_migrations' \
            )",
        )
        .fetch_one(&self.pool)
        .await?;
        if !table_exists {
            return Ok(());
        }

        let applied = sqlx::query_as::<_, (bool, Vec<u8>)>(
            "SELECT success, checksum FROM _sqlx_migrations WHERE version = $1",
        )
        .bind(LEGACY_RATCHET_MIGRATION_VERSION)
        .fetch_optional(&self.pool)
        .await?;

        let Some((success, stored_checksum)) = applied else {
            return Ok(());
        };

        if !success || stored_checksum == expected_checksum {
            return Ok(());
        }

        let has_ratchet_column = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS ( \
                SELECT 1 FROM information_schema.columns \
                WHERE table_name = 'encrypted_messages' \
                  AND column_name = 'sender_ratchet_step' \
            )",
        )
        .fetch_one(&self.pool)
        .await?;

        let has_ratchet_index = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS ( \
                SELECT 1 FROM pg_indexes \
                WHERE schemaname = ANY (current_schemas(false)) \
                  AND indexname = 'idx_enc_msg_ratchet' \
            )",
        )
        .fetch_one(&self.pool)
        .await?;

        if !(has_ratchet_column && has_ratchet_index) {
            tracing::warn!(
                version = LEGACY_RATCHET_MIGRATION_VERSION,
                "legacy migration checksum differs and schema markers are incomplete; leaving row untouched"
            );
            return Ok(());
        }

        sqlx::query("UPDATE _sqlx_migrations SET checksum = $1 WHERE version = $2")
            .bind(expected_checksum)
            .bind(LEGACY_RATCHET_MIGRATION_VERSION)
            .execute(&self.pool)
            .await?;

        tracing::warn!(
            version = LEGACY_RATCHET_MIGRATION_VERSION,
            "reconciled legacy migration checksum to unblock startup on upgraded deployments"
        );

        Ok(())
    }
}
