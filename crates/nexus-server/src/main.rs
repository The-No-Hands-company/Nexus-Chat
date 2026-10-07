//! # Nexus Server
//!
//! Main binary that orchestrates all Nexus services:
//! - REST API (HTTP)
//! - WebSocket Gateway (real-time events)
//! - Voice Server (WebRTC SFU + signaling)
//!
//! All services can run in a single process (simple deployment)
//! or be split into separate processes (horizontal scaling).
//!
//! ## Lite mode
//!
//! Run `nexus serve --lite` for a zero-dependency, single-binary deployment:
//! - SQLite database (`nexus.db` in the current directory)
//! - Local filesystem uploads (`./data/uploads/`)
//! - No Docker, no MinIO, no MeiliSearch required.

#![allow(clippy::pedantic)]

use clap::{Parser, Subcommand};
use metrics_exporter_prometheus::PrometheusBuilder;
use nexus_api::{AppState, build_router};
use nexus_common::gateway_event::GatewayEvent;
use nexus_db::{
    Database,
    search::SearchClient,
    storage::{StorageClient, StorageConfig as DbStorageConfig},
};
use nexus_federation::{FederationClient, KeyManager};
use nexus_gateway::GatewayState;
use nexus_voice::VoiceServer;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::broadcast;

mod import_worker;
mod nexus_cloud;
mod scylla_sink;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "nexus",
    about = "Privacy-first, community-owned chat server",
    version = env!("CARGO_PKG_VERSION"),
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the Nexus server.
    ///
    /// Example:
    ///   nexus serve --lite
    ///   # then add to .env:
    ///   # NEXUS__PUSH__VAPID_PRIVATE_KEY=<output>
    #[command(name = "serve")]
    Serve {
        /// Lite mode: single binary, SQLite database, local file storage.
        /// No Docker or external services required.
        #[arg(long, env = "NEXUS_LITE", default_value_t = false)]
        lite: bool,

        /// HTTP API port (default: 8080).
        #[arg(long, env = "PORT", default_value_t = 8080)]
        port: u16,

        /// WebSocket gateway port (default: 8081).
        #[arg(long, env = "GATEWAY_PORT", default_value_t = 8081)]
        gateway_port: u16,

        /// Voice signaling port (default: 8082).
        #[arg(long, env = "VOICE_PORT", default_value_t = 8082)]
        voice_port: u16,
    },

    /// Generate a VAPID key pair for Web Push notifications.
    ///
    /// Outputs the private key (base64url). Set it as NEXUS__PUSH__VAPID_PRIVATE_KEY.
    /// The matching public key is served at GET /push/vapid-public-key.
    ///
    /// Example:
    ///   nexus gen-vapid-key
    ///   # then add to .env:
    ///   # NEXUS__PUSH__VAPID_PRIVATE_KEY=<output>
    #[command(name = "gen-vapid-key")]
    GenVapidKey,
}

// ── Entry point ───────────────────────────────────────────────────────────────

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Serve {
            lite,
            port,
            gateway_port,
            voice_port,
        } => run_server(lite, port, gateway_port, voice_port).await,

        Command::GenVapidKey => {
            let private_key = nexus_api::push_sender::PushSender::generate_private_key();
            let subject = "mailto:admin@your-domain.com";
            let sender =
                nexus_api::push_sender::PushSender::from_private_key(&private_key, subject)
                    .expect("freshly generated key must be valid");

            println!("# Add to your .env file:");
            println!("NEXUS__PUSH__VAPID_PRIVATE_KEY={private_key}");
            println!();
            println!("# Public key (served at GET /push/vapid-public-key):");
            println!("# {}", sender.public_key_b64);
            Ok(())
        }
    }
}

// ── Server startup ────────────────────────────────────────────────────────────

async fn run_server(
    lite: bool,
    port: u16,
    gateway_port: u16,
    voice_port: u16,
) -> anyhow::Result<()> {
    // ── Lite-mode environment bootstrap ──────────────────────────────────────
    // Inject sensible defaults so the server works out-of-the-box without any
    // env vars or config files.  Values are passed directly to the config
    // builder via `init_with_overrides` so we never need to mutate the process
    // environment (avoids `unsafe { std::env::set_var(...) }`).
    let config = if lite {
        let sqlite_url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "sqlite://nexus.db?mode=rwc".to_string());
        let jwt_secret = if std::env::var("JWT_SECRET").is_ok()
            || std::env::var("NEXUS__AUTH__JWT_SECRET").is_ok()
        {
            std::env::var("JWT_SECRET")
                .or_else(|_| std::env::var("NEXUS__AUTH__JWT_SECRET"))
                .unwrap()
        } else {
            generate_or_load_lite_secret("nexus.toml")?
        };

        nexus_common::config::init_with_overrides(&[
            ("database.url", sqlite_url),
            ("auth.jwt_secret", jwt_secret),
        ])?
    } else {
        nexus_common::config::init()?
    };

    // ── Tracing ───────────────────────────────────────────────────────────────
    // Format selection:
    //   NEXUS_LOG_FORMAT=json  → structured JSON (default in non-lite/production)
    //   NEXUS_LOG_FORMAT=pretty → human-readable (default in lite / dev mode)
    let log_format = std::env::var("NEXUS_LOG_FORMAT")
        .unwrap_or_else(|_| if lite { "pretty".into() } else { "json".into() });

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "nexus=info,tower_http=info,axum=info".into());

    if log_format == "json" {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(env_filter)
            // Standard JSON fields: timestamp, level, target, fields, span
            .with_current_span(true)
            .with_span_list(false)
            .with_file(false)
            .with_line_number(false)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_target(!lite) // less noisy in lite mode
            .with_thread_ids(false)
            .init();
    }

    // ── Prometheus metrics recorder ───────────────────────────────────────────
    // Install the global metrics recorder; the handle is stored in AppState so
    // the /metrics endpoint can call handle.render() to produce scrape output.
    let prometheus_handle = PrometheusBuilder::new()
        .install_recorder()
        .map_err(|e| anyhow::anyhow!("failed to install Prometheus recorder: {e}"))?;

    if lite {
        tracing::info!(
            version = env!("CARGO_PKG_VERSION"),
            mode = "lite",
            "nexus starting — no Docker required, data stored locally"
        );
    } else {
        tracing::info!(
            version = env!("CARGO_PKG_VERSION"),
            mode = "full",
            "nexus starting — privacy-first, community-owned"
        );
    }

    // ── Startup security validation ───────────────────────────────────────────
    // Fail fast on insecure configurations that would silently weaken security.
    {
        let jwt_len = config.auth.jwt_secret.len();
        if jwt_len < 32 {
            tracing::error!(
                length = jwt_len,
                required = 32,
                "JWT secret is too short — tokens can be brute-forced. \
                 Generate a secure secret: openssl rand -hex 32"
            );
            anyhow::bail!(
                "JWT secret must be at least 32 bytes ({jwt_len} given). \
                 Set NEXUS__AUTH__JWT_SECRET or JWT_SECRET to a cryptographically \
                 random value: `openssl rand -hex 32`"
            );
        }
        // The literals below are the placeholders this check REJECTS, not
        // credentials — hence the allowlist pragmas for the secret scanner.
        if config.auth.jwt_secret == "change-me" // pragma: allowlist secret
            || config.auth.jwt_secret == "secret" // pragma: allowlist secret
            || config.auth.jwt_secret == "nexus" // pragma: allowlist secret
            || config.auth.jwt_secret.starts_with("dev-") // pragma: allowlist secret
        {
            let preview = &config.auth.jwt_secret[..config.auth.jwt_secret.len().min(8)];
            anyhow::bail!(
                "JWT secret looks like a placeholder value ('{preview}...'). Set a random secret: `openssl rand -hex 32`" // pragma: allowlist secret
            );
        }
        tracing::info!(length = jwt_len, "JWT secret validated");
    }

    // ── Database ──────────────────────────────────────────────────────────────
    let db = Database::connect(config).await?;
    db.migrate().await?;
    tracing::info!(subsystem = "database", "database ready");

    // ── Event bus ─────────────────────────────────────────────────────────────
    let (gateway_tx, _) = broadcast::channel::<GatewayEvent>(10_000);

    // ── Voice Server ──────────────────────────────────────────────────────────
    let local_ip: std::net::IpAddr = "127.0.0.1".parse()?;
    let voice_server = VoiceServer::new(db.clone(), gateway_tx.clone(), local_ip);
    let voice_state = voice_server.state.voice_state.clone();

    // ── Storage ───────────────────────────────────────────────────────────────
    let public_base =
        std::env::var("NEXUS_PUBLIC_URL").unwrap_or_else(|_| format!("http://127.0.0.1:{port}"));

    let storage = if lite || config.storage.endpoint.is_empty() {
        let data_dir = &config.storage.data_dir;
        tracing::info!(subsystem = "storage", backend = "local", path = %data_dir, "local file storage ready");
        StorageClient::new_local(data_dir, format!("{public_base}/files"))?
    } else {
        let s = StorageClient::new(&DbStorageConfig {
            endpoint: config.storage.endpoint.clone(),
            access_key: config.storage.access_key.clone(),
            secret_key: config.storage.secret_key.clone(),
            bucket: config.storage.bucket.clone(),
            region: config.storage.region.clone(),
            public_url: None,
        })?;
        s.ensure_bucket().await?;
        tracing::info!(subsystem = "storage", backend = "s3", bucket = %config.storage.bucket, "object storage ready");
        s
    };

    // ── Search ────────────────────────────────────────────────────────────────
    // MeiliSearch when it is configured; otherwise the embedded Tantivy index.
    //
    // Full mode used to demand MeiliSearch whenever a URL was set and disable
    // search entirely when it was not — so a self-hosted node without Meili got
    // no search at all, even though the binary already ships an embedded engine
    // that lite mode uses happily. Falling back to Tantivy keeps search working
    // with no extra service to run, and clearing NEXUS__SEARCH__URL is now the
    // way to opt out of MeiliSearch rather than the way to lose search.
    let search = if !config.search.url.is_empty() {
        let s = SearchClient::new(&config.search.url, &config.search.api_key);
        s.bootstrap_indexes().await?;
        tracing::info!(subsystem = "search", backend = "meilisearch", url = %config.search.url, "search ready");
        s
    } else {
        let data_dir = &config.storage.data_dir;
        tracing::info!(
            subsystem = "search",
            backend = "tantivy",
            mode = "embedded",
            lite,
            "search ready"
        );
        SearchClient::new_tantivy(data_dir)?
    };

    // ── Federation ────────────────────────────────────────────────────────────
    let federation_key = KeyManager::new(db.pool.clone()).load_or_generate().await?;
    tracing::info!(subsystem = "federation", key_id = %federation_key.key_id, "federation signing key ready");
    let federation_client = Arc::new(FederationClient::new(
        &config.server.name,
        federation_key.clone(),
    ));

    // ── REST API ──────────────────────────────────────────────────────────────
    let email_service = nexus_api::email::EmailService::new(config.email.clone());
    if email_service.is_enabled() {
        tracing::info!( "email delivery enabled (Resend)");
    } else {
        tracing::info!("email delivery disabled (no NEXUS__EMAIL__API_KEY)");
    }

    // ── Web Push (VAPID) ──────────────────────────────────────────────────────
    // Load the VAPID private key from NEXUS__PUSH__VAPID_PRIVATE_KEY.
    // The PushSender is stored in AppState so key material is parsed once at
    // startup and reused across all requests — never re-parsed per message.
    // Generate a key with: cargo run --bin nexus -- gen-vapid-key
    let (push_sender, vapid_public_key) = match std::env::var("NEXUS__PUSH__VAPID_PRIVATE_KEY")
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(priv_key_b64) => {
            let subject = format!("mailto:admin@{}", config.server.name);
            match nexus_api::push_sender::PushSender::from_private_key(&priv_key_b64, &subject) {
                Ok(sender) => {
                    tracing::info!(
                        subsystem = "push",
                        pub_key_prefix = %sender.public_key_b64.chars().take(12).collect::<String>(),
                        "Web Push (VAPID) enabled"
                    );
                    let pub_key = sender.public_key_b64.clone();
                    (Some(sender), Some(pub_key))
                }
                Err(e) => {
                    tracing::warn!(subsystem = "push", error_kind = %nexus_common::logsafe::anyhow_kind(&e), "Invalid VAPID key — push disabled");
                    (None, None)
                }
            }
        }
        None => {
            tracing::info!(
                subsystem = "push",
                "Web Push disabled (NEXUS__PUSH__VAPID_PRIVATE_KEY not set). \
                 Generate with: cargo run --bin nexus -- gen-vapid-key"
            );
            (None, None)
        }
    };

    let api_state = AppState {
        db: db.clone(),
        gateway_tx: gateway_tx.clone(),
        voice_state: voice_state.clone(),
        storage,
        search,
        server_name: config.server.name.clone(),
        federation_key,
        federation_client,
        started_at: std::time::Instant::now(),
        prometheus: prometheus_handle,
        email: email_service,
        push: push_sender,
        vapid_public_key,
    };
    let search_for_workers = api_state.search.clone();
    let api_router = build_router(api_state);
    // Loopback by default: the ecosystem proxy is the only intended client, and
    // the login gate lives there. Hardcoding 0.0.0.0 let anyone on the LAN reach
    // the API, gateway and voice ports directly and skip authentication — a gate
    // is only a gate if it is the sole way in. Override with
    // NEXUS__SERVER__HOST=0.0.0.0 when something off-box genuinely must connect.
    //
    // An unparseable value falls back to loopback rather than propagating the
    // error: failing closed on a bind address means a typo costs you a local-only
    // server, not a publicly listening one.
    let host: std::net::IpAddr = config.server.host.parse().unwrap_or_else(|_| {
        tracing::warn!(
            host = %config.server.host,
            "invalid NEXUS__SERVER__HOST — falling back to 127.0.0.1"
        );
        std::net::IpAddr::from([127, 0, 0, 1])
    });
    let api_addr = SocketAddr::new(host, port);
    let gateway_addr = SocketAddr::new(host, gateway_port);
    let voice_addr = SocketAddr::new(host, voice_port);

    // ── WebSocket Gateway ─────────────────────────────────────────────────────
    let gateway_state = GatewayState::with_broadcast(db.clone(), gateway_tx.clone());
    let gateway_router = nexus_gateway::build_router(gateway_state);

    // ── Voice Signaling ───────────────────────────────────────────────────────
    let voice_router = voice_server.build_router();

    if lite {
        tracing::info!(
            api = format!("http://127.0.0.1:{port}"),
            gateway = format!("ws://127.0.0.1:{gateway_port}"),
            voice = format!("ws://127.0.0.1:{voice_port}"),
            mode = "lite",
            "nexus ready"
        );
    } else {
        tracing::info!(
            api = %api_addr,
            gateway = %gateway_addr,
            voice = %voice_addr,
            mode = "full",
            "nexus ready"
        );
    }

    // ── Graceful shutdown ─────────────────────────────────────────────────────
    // A broadcast channel fans out the shutdown signal to all three servers.
    // When SIGTERM or SIGINT is received we stop accepting new connections and
    // wait for Axum to drain in-flight requests before exiting.
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
    let shutdown_tx = Arc::new(shutdown_tx);

    {
        // OS signal handler — runs in a background task.
        let tx = shutdown_tx.clone();
        tokio::spawn(async move {
            let ctrl_c = tokio::signal::ctrl_c();

            #[cfg(unix)]
            let sigterm = async {
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("failed to install SIGTERM handler")
                    .recv()
                    .await;
            };
            #[cfg(not(unix))]
            let sigterm = std::future::pending::<()>();

            tokio::select! {
                _ = ctrl_c => {
                    tracing::info!(signal = "SIGINT", "shutdown signal received — draining connections");
                }
                _ = sigterm => {
                    tracing::info!(signal = "SIGTERM", "shutdown signal received — draining connections");
                }
            }

            // Broadcast to all servers; ignore error if all receivers already dropped.
            let _ = tx.send(());
        });
    }

    // ── Nexus Cloud registration (non-blocking best-effort) ─────────────────
    if let (Ok(cloud_url), Ok(public_url)) = (
        std::env::var("NEXUS_CLOUD_URL"),
        std::env::var("PUBLIC_URL"),
    ) {
        // NEXUS_CLOUD_UPSTREAM_URL overrides PUBLIC_URL for Cloud portal iframe embedding
        // (e.g. point at the web frontend on 5174 rather than the raw API on 8080)
        let upstream_url =
            std::env::var("NEXUS_CLOUD_UPSTREAM_URL").unwrap_or_else(|_| public_url.clone());
        let api_key = std::env::var("NEXUS_CLOUD_API_KEY").unwrap_or_default();
        {
            let cu = cloud_url.clone();
            let pu = upstream_url.clone();
            let ak = api_key.clone();
            tokio::spawn(async move { nexus_cloud::register_with_cloud(&pu, &cu, &ak).await });
        }
        {
            let mut hb_rx = shutdown_tx.subscribe();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            nexus_cloud::send_heartbeat(&cloud_url, &api_key, &upstream_url).await;
                        }
                        _ = hb_rx.recv() => break,
                    }
                }
            });
        }
    }

    // ── Import worker (Phase 19-01) ─────────────────────────────────────────
    // Claims pending import jobs and processes metadata payloads into channels
    // and messages in the background.
    {
        let import_rx = shutdown_tx.subscribe();
        import_worker::spawn_import_worker(db.pool.clone(), import_rx.resubscribe());
    }

    // ── Scylla outbox worker (dual-write bridge) ───────────────────────────
    // Drains message mutations queued for Scylla synchronization.
    if db.scylla_enabled {
        let scylla_cfg = config.scylla.clone();
        let pool = db.pool.clone();
        let mut scylla_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut sink: Option<scylla_sink::ScyllaSink> = None;
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if sink.is_none() {
                            match scylla_sink::ScyllaSink::connect(&scylla_cfg).await {
                                Ok(s) => {
                                    tracing::info!(
                                        nodes = %scylla_cfg.nodes,
                                        keyspace = %scylla_cfg.keyspace,
                                        subsystem = "scylla_outbox",
                                        "connected Scylla sink"
                                    );
                                    sink = Some(s);
                                }
                                Err(e) => {
                                    tracing::warn!(error_kind = %nexus_common::logsafe::anyhow_kind(&e), subsystem = "scylla_outbox", "failed to connect Scylla sink");
                                    continue;
                                }
                            }
                        }

                        let jobs = match nexus_db::repository::scylla_outbox::claim_pending_jobs(&pool, 100).await {
                            Ok(j) => j,
                            Err(e) => {
                                tracing::warn!(error_kind = %nexus_common::logsafe::err_kind(&e), subsystem = "scylla_outbox", "failed to claim outbox jobs");
                                continue;
                            }
                        };

                        if !jobs.is_empty() {
                            let reclaimed = jobs.iter().filter(|j| j.attempt_count > 1).count();
                            tracing::debug!(
                                count = jobs.len(),
                                reclaimed,
                                subsystem = "scylla_outbox",
                                "claimed outbox jobs"
                            );
                        }

                        for job in jobs {
                            let process_result: Result<(), String> = match job.operation.as_str() {
                                "upsert" => {
                                    let Some(payload) = &job.payload else {
                                        continue;
                                    };
                                    let doc = match serde_json::from_value::<nexus_db::search::MessageDocument>(payload.clone()) {
                                        Ok(d) => d,
                                        Err(e) => {
                                            let err = format!("invalid upsert payload: {e}");
                                            let _ = nexus_db::repository::scylla_outbox::mark_job_failed(&pool, job.id, &err).await;
                                            continue;
                                        }
                                    };

                                    match sink.as_ref().unwrap().upsert_message(&doc).await {
                                        Ok(()) => Ok(()),
                                        Err(e) => Err(e.to_string()),
                                    }
                                }
                                "delete" => match sink.as_ref().unwrap().delete_message(job.message_id, job.payload.as_ref()).await {
                                    Ok(()) => Ok(()),
                                    Err(e) => Err(e.to_string()),
                                },
                                other => Err(format!("unknown outbox operation: {other}")),
                            };

                            match process_result {
                                Ok(()) => {
                                    if let Err(e) = nexus_db::repository::scylla_outbox::mark_job_completed(&pool, job.id).await {
                                        tracing::warn!(error_kind = %nexus_common::logsafe::err_kind(&e), job_id = job.id, subsystem = "scylla_outbox", "failed to mark outbox job completed");
                                    }
                                }
                                Err(err) => {
                                    if let Err(e) = nexus_db::repository::scylla_outbox::mark_job_failed(&pool, job.id, &err).await {
                                        tracing::warn!(error_kind = %nexus_common::logsafe::err_kind(&e), job_id = job.id, subsystem = "scylla_outbox", "failed to mark outbox job failed");
                                    } else {
                                        tracing::warn!(
                                            job_id = job.id,
                                            message_id = %job.message_id,
                                            attempts = job.attempt_count,
                                            subsystem = "scylla_outbox",
                                            "outbox job processing failed"
                                        );
                                    }
                                }
                            }
                        }
                    }
                    _ = scylla_rx.recv() => {
                        tracing::debug!(subsystem = "scylla_outbox", "outbox worker shutting down");
                        break;
                    }
                }
            }
        });
    }

    // ── Moderation background sweep ───────────────────────────────────────────
    // Periodically lift expired bans and timeouts so users are not kept in a
    // timed-out state past the expiry they were given.  Runs every 5 minutes;
    // shuts down cleanly on the same signal as the HTTP servers.
    {
        let pool = db.pool.clone();
        let mut sweep_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5 * 60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        match nexus_db::repository::moderation::purge_expired_timeouts(&pool).await {
                            Ok(n) if n > 0 => tracing::info!(
                                count = n, subsystem = "moderation", "lifted expired timeouts"
                            ),
                            Err(e) => tracing::warn!(
                                error_kind = %nexus_common::logsafe::err_kind(&e), subsystem = "moderation", "failed to purge expired timeouts"
                            ),
                            _ => {}
                        }
                        match nexus_db::repository::moderation::purge_expired_bans(&pool).await {
                            Ok(n) if n > 0 => tracing::info!(
                                count = n, subsystem = "moderation", "purged expired bans"
                            ),
                            Err(e) => tracing::warn!(
                                error_kind = %nexus_common::logsafe::err_kind(&e), subsystem = "moderation", "failed to purge expired bans"
                            ),
                            _ => {}
                        }
                    }
                    _ = sweep_rx.recv() => {
                        tracing::debug!(subsystem = "moderation", "sweep task shutting down");
                        break;
                    }
                }
            }
        });
    }

    // ── Account deletion purge worker ────────────────────────────────────────
    // Applies due scheduled account deletions after the 30-day grace period.
    {
        let pool = db.pool.clone();
        let mut deletion_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60 * 60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        match nexus_db::repository::users::purge_scheduled_deletions(&pool).await {
                            Ok(n) if n > 0 => tracing::info!(
                                count = n,
                                subsystem = "account_deletion",
                                "purged due scheduled account deletions"
                            ),
                            Err(e) => tracing::warn!(
                                error_kind = %nexus_common::logsafe::err_kind(&e),
                                subsystem = "account_deletion",
                                "failed to purge scheduled account deletions"
                            ),
                            _ => {}
                        }
                    }
                    _ = deletion_rx.recv() => {
                        tracing::debug!(subsystem = "account_deletion", "purge worker shutting down");
                        break;
                    }
                }
            }
        });
    }

    // ── Engagement background tasks (Phase 13) ────────────────────────────────
    // Runs every minute:
    //   • Auto-end polls that have passed their `ends_at`
    //   • Dispatch pending scheduled messages at their `scheduled_at` time
    //   • Purge expired (disappearing) messages
    //   • Clear expired custom statuses + emit PRESENCE_UPDATE
    {
        let pool = db.pool.clone();
        let gw_tx = gateway_tx.clone();
        let search = search_for_workers.clone();
        let scylla_enabled = db.scylla_enabled;
        let mut engage_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let _ = search.process_sync_queue(&pool).await;

                        // 1. Auto-end polls
                        #[derive(sqlx::FromRow)]
                        struct PollEndRow { id: String, channel_id: String }
                        let expired_polls: Vec<PollEndRow> = sqlx::query_as(
                            "UPDATE polls SET status = 'ended', updated_at = NOW() \
                             WHERE status = 'open' AND ends_at IS NOT NULL AND ends_at <= NOW() \
                             RETURNING id::text AS id, channel_id::text AS channel_id"
                        )
                        .fetch_all(&pool)
                        .await
                        .unwrap_or_default();
                        for p in &expired_polls {
                            let _ = gw_tx.send(nexus_common::gateway_event::GatewayEvent {
                                event_type: nexus_common::gateway_event::event_types::POLL_ENDED.into(),
                                data: serde_json::json!({
                                    "poll_id": p.id,
                                    "channel_id": p.channel_id,
                                }),
                                server_id: None, channel_id: None, user_id: None,
                            });
                        }
                        if !expired_polls.is_empty() {
                            tracing::info!(count = expired_polls.len(), subsystem = "polls", "auto-ended expired polls");
                        }

                        // 2. Dispatch scheduled messages
                        #[derive(sqlx::FromRow)]
                        struct SmsRow {
                            id: String,
                            channel_id: String,
                            server_id: Option<String>,
                            author_id: String,
                            author_username: String,
                            content: String,
                        }
                        let due: Vec<SmsRow> = sqlx::query_as(
                            "UPDATE scheduled_messages \
                             SET status = 'sent', updated_at = NOW() \
                             WHERE status = 'pending' AND scheduled_at <= NOW() \
                             RETURNING id::text AS id, channel_id::text AS channel_id, \
                                       (SELECT server_id::text FROM channels c WHERE c.id = scheduled_messages.channel_id) AS server_id, \
                                       author_id::text AS author_id, \
                                       COALESCE((SELECT username FROM users u WHERE u.id = scheduled_messages.author_id), '') AS author_username, \
                                       content"
                        )
                        .fetch_all(&pool)
                        .await
                        .unwrap_or_default();
                        for sm in &due {
                            // Insert the actual message
                            let msg_id = nexus_common::snowflake::generate_id();
                            let insert_res = sqlx::query(
                                "INSERT INTO messages \
                                 (id, channel_id, author_id, content, message_type, \
                                  edited, pinned, embeds, attachments, \
                                  mentions, mention_roles, mention_everyone, flags, \
                                  created_at, updated_at) \
                                 VALUES ($1::uuid, $2::uuid, $3::uuid, $4, 0, \
                                         false, false, '[]'::jsonb, '[]'::jsonb, \
                                         '{}'::uuid[], '{}'::uuid[], false, 0, \
                                         NOW(), NOW())"
                            )
                            .bind(msg_id.to_string())
                            .bind(&sm.channel_id)
                            .bind(&sm.author_id)
                            .bind(&sm.content)
                            .execute(&pool)
                            .await;
                            if let Err(e) = insert_res {
                                tracing::warn!(error_kind = %nexus_common::logsafe::err_kind(&e), sm_id = %sm.id, "Failed to dispatch scheduled message");
                                continue;
                            }

                            let doc = nexus_db::search::MessageDocument {
                                id: msg_id.to_string(),
                                channel_id: sm.channel_id.clone(),
                                server_id: sm.server_id.clone(),
                                author_id: sm.author_id.clone(),
                                author_username: sm.author_username.clone(),
                                content: sm.content.clone(),
                                has_attachments: false,
                                has_embeds: false,
                                created_at: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs() as i64)
                                    .unwrap_or_default(),
                            };

                            if let Err(e) = search.sync_message_index(&pool, msg_id, doc.clone()).await {
                                tracing::warn!(error_kind = %nexus_common::logsafe::anyhow_kind(&e), sm_id = %sm.id, "failed to index dispatched scheduled message");
                            }

                            if scylla_enabled {
                                let _ = nexus_db::repository::scylla_outbox::enqueue_upsert_message(&pool, &doc).await;
                            }

                            let server_uuid = sm.server_id.as_ref().and_then(|s| s.parse::<uuid::Uuid>().ok());
                            let channel_uuid = sm.channel_id.parse::<uuid::Uuid>().ok();
                            let author_uuid = sm.author_id.parse::<uuid::Uuid>().ok();
                            let _ = gw_tx.send(nexus_common::gateway_event::GatewayEvent {
                                event_type: nexus_common::gateway_event::event_types::MESSAGE_CREATE.into(),
                                data: serde_json::json!({
                                    "id": msg_id.to_string(),
                                    "channel_id": sm.channel_id,
                                    "author_id": sm.author_id,
                                    "author_username": sm.author_username,
                                    "content": sm.content,
                                    "scheduled_message_id": sm.id,
                                }),
                                server_id: server_uuid,
                                channel_id: channel_uuid,
                                user_id: author_uuid,
                            });
                        }
                        if !due.is_empty() {
                            tracing::info!(count = due.len(), subsystem = "scheduled_messages", "dispatched scheduled messages");
                        }

                        // 3. Purge expired (disappearing) messages
                        #[derive(sqlx::FromRow)]
                        struct ExpiredMessageRow {
                            id: String,
                            channel_id: String,
                        }
                        let deleted: Result<Vec<ExpiredMessageRow>, _> = sqlx::query_as(
                            "DELETE FROM messages \
                             WHERE expires_at IS NOT NULL AND expires_at <= NOW() \
                             RETURNING id::text AS id, channel_id::text AS channel_id"
                        )
                        .fetch_all(&pool)
                        .await;
                        match deleted {
                            Ok(rows) if !rows.is_empty() => {
                                let mut search_deleted = 0usize;
                                let mut scylla_enqueued = 0usize;
                                let mut gateway_emitted = 0usize;
                                for row in &rows {
                                    let Ok(message_id) = row.id.parse::<uuid::Uuid>() else {
                                        continue;
                                    };
                                    let Ok(channel_id) = row.channel_id.parse::<uuid::Uuid>() else {
                                        continue;
                                    };

                                    if search.sync_message_delete(&pool, message_id).await.is_ok() {
                                        search_deleted += 1;
                                    }
                                    if scylla_enabled
                                        && nexus_db::repository::scylla_outbox::enqueue_delete_message(&pool, message_id).await.is_ok()
                                    {
                                        scylla_enqueued += 1;
                                    }

                                    let _ = gw_tx.send(nexus_common::gateway_event::GatewayEvent {
                                        event_type: nexus_common::gateway_event::event_types::MESSAGE_DELETE.into(),
                                        data: serde_json::json!({
                                            "id": row.id,
                                            "channel_id": row.channel_id,
                                        }),
                                        server_id: None,
                                        channel_id: Some(channel_id),
                                        user_id: None,
                                    });
                                    gateway_emitted += 1;
                                }
                                tracing::info!(
                                    count = rows.len(),
                                    search_deleted,
                                    scylla_enqueued,
                                    gateway_emitted,
                                    subsystem = "disappearing_messages",
                                    "purged expired messages"
                                );
                            }
                            Ok(_) => {}
                            Err(e) => tracing::warn!(
                                error_kind = %nexus_common::logsafe::err_kind(&e), subsystem = "disappearing_messages",
                                "failed to purge expired messages"
                            ),
                        }

                        // 4. Clear expired custom statuses
                        #[derive(sqlx::FromRow)]
                        struct ExpiredStatusRow { id: String }
                        let expired_statuses: Vec<ExpiredStatusRow> = sqlx::query_as(
                            "UPDATE users \
                             SET status = NULL, custom_status_expires_at = NULL, updated_at = NOW() \
                             WHERE custom_status_expires_at IS NOT NULL AND custom_status_expires_at <= NOW() \
                             RETURNING id::text AS id"
                        )
                        .fetch_all(&pool)
                        .await
                        .unwrap_or_default();
                        for u in &expired_statuses {
                            let _ = gw_tx.send(nexus_common::gateway_event::GatewayEvent {
                                event_type: nexus_common::gateway_event::event_types::PRESENCE_UPDATE.into(),
                                data: serde_json::json!({
                                    "user_id": u.id,
                                    "status": null,
                                    "custom_status_expires_at": null,
                                }),
                                server_id: None, channel_id: None, user_id: None,
                            });
                        }
                        if !expired_statuses.is_empty() {
                            tracing::info!(
                                count = expired_statuses.len(), subsystem = "status_expiry",
                                "cleared expired custom statuses"
                            );
                        }

                        // 5. Activate server events whose starts_at has arrived (v0.14)
                        #[derive(sqlx::FromRow)]
                        struct StartedEventRow {
                            id: String,
                            server_id: String,
                            title: String,
                        }
                        let started_events: Vec<StartedEventRow> = sqlx::query_as(
                            "UPDATE server_events \
                             SET status = 'active', updated_at = NOW() \
                             WHERE status = 'scheduled' AND starts_at <= NOW() \
                             RETURNING id::text AS id, server_id::text AS server_id, title"
                        )
                        .fetch_all(&pool)
                        .await
                        .unwrap_or_default();
                        for ev in &started_events {
                            let server_uuid = ev.server_id.parse::<uuid::Uuid>().ok();
                            let _ = gw_tx.send(nexus_common::gateway_event::GatewayEvent {
                                event_type: nexus_common::gateway_event::event_types::GUILD_SCHEDULED_EVENT_START.into(),
                                data: serde_json::json!({
                                    "event_id": ev.id,
                                    "server_id": ev.server_id,
                                    "title": ev.title,
                                }),
                                server_id: server_uuid,
                                channel_id: None,
                                user_id: None,
                            });
                        }
                        if !started_events.is_empty() {
                            tracing::info!(
                                count = started_events.len(), subsystem = "server_events",
                                "activated scheduled server events"
                            );
                        }
                    }
                    _ = engage_rx.recv() => {
                        tracing::debug!(subsystem = "engagement", "engagement task shutting down");
                        break;
                    }
                }
            }
        });
    }

    // Pre-subscribe each server before spawning so they receive the signal even
    // if it fires before the async block starts.
    let api_shutdown_rx = shutdown_tx.subscribe();
    let gateway_shutdown_rx = shutdown_tx.subscribe();
    let voice_shutdown_rx = shutdown_tx.subscribe();

    tokio::try_join!(
        async {
            let listener = tokio::net::TcpListener::bind(api_addr).await?;
            let mut rx = api_shutdown_rx;
            axum::serve(listener, api_router)
                .with_graceful_shutdown(async move {
                    let _ = rx.recv().await;
                })
                .await?;
            tracing::info!(server = "api", "server drained and stopped");
            Ok::<_, anyhow::Error>(())
        },
        async {
            let listener = tokio::net::TcpListener::bind(gateway_addr).await?;
            let mut rx = gateway_shutdown_rx;
            axum::serve(listener, gateway_router)
                .with_graceful_shutdown(async move {
                    let _ = rx.recv().await;
                })
                .await?;
            tracing::info!(server = "gateway", "server drained and stopped");
            Ok::<_, anyhow::Error>(())
        },
        async {
            let listener = tokio::net::TcpListener::bind(voice_addr).await?;
            let mut rx = voice_shutdown_rx;
            axum::serve(listener, voice_router)
                .with_graceful_shutdown(async move {
                    let _ = rx.recv().await;
                })
                .await?;
            tracing::info!(server = "voice", "server drained and stopped");
            Ok::<_, anyhow::Error>(())
        },
    )?;

    tracing::info!("nexus shut down cleanly");

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Load the JWT secret from `nexus.toml`, or generate and persist a new one.
/// The file is a minimal TOML with a single `jwt_secret` key so it survives
/// across restarts without any additional config.
fn generate_or_load_lite_secret(path: &str) -> anyhow::Result<String> {
    use std::io::{Read, Write};

    let key = "jwt_secret";

    // Try to read existing secret
    if let Ok(mut f) = std::fs::File::open(path) {
        let mut contents = String::new();
        f.read_to_string(&mut contents)?;
        for line in contents.lines() {
            if let Some(rest) = line.strip_prefix("jwt_secret = \"")
                && let Some(secret) = rest.strip_suffix('"')
                && !secret.is_empty()
            {
                return Ok(secret.to_string());
            }
        }
    }

    // Generate a new 64-byte hex secret
    use rand::RngCore;
    let mut bytes = [0u8; 64];
    rand::rng().fill_bytes(&mut bytes);
    let secret = hex::encode(bytes);

    // Persist — append/create
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "\n{key} = \"{secret}\"")?;

    tracing::info!("🔐 Generated JWT secret → {path}  (keep this file safe)");
    Ok(secret)
}
