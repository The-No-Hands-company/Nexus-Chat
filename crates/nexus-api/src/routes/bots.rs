//! Bot API routes — create and manage bot applications, install bots to servers.
//!
//! Bot tokens use the format "Bot <token>" in the Authorization header (same
//! as Discord's bot token scheme). This file handles the developer-facing REST
//! API for managing bot applications. Bot authentication is handled by
//! `bot_auth_middleware` in `middleware.rs`.

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    middleware,
    routing::{delete, get, post},
};
use nexus_common::{
    error::{NexusError, NexusResult},
    models::bot::{BotApplication, BotServerInstall, BotToken, CreateBotRequest, UpdateBotRequest},
    snowflake,
};
use nexus_db::repository::bots;
use rand::Rng;
use rand::distr::Alphanumeric;
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    AppState,
    middleware::{AuthContext, check_rate_limit_with_fallback, extract_client_tag},
};

/// Bot application routes.
pub fn router() -> Router<Arc<AppState>> {
    // Public routes — no auth required
    let public = Router::new().route("/bots/{bot_id}", get(get_public_bot_info));

    // Authenticated routes — require user or bot auth
    let authenticated = Router::new()
        // Developer portal
        .route(
            "/applications",
            post(create_application).get(list_applications),
        )
        .route(
            "/applications/{app_id}",
            get(get_application)
                .patch(update_application)
                .delete(delete_application),
        )
        .route("/applications/{app_id}/token/reset", post(reset_token))
        // Server bot integrations
        .route(
            "/servers/{server_id}/integrations",
            get(list_server_bots).post(install_bot),
        )
        .route(
            "/servers/{server_id}/integrations/{bot_id}",
            delete(uninstall_bot),
        )
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ));

    Router::new().merge(public).merge(authenticated)
}

// ============================================================================
// Helpers
// ============================================================================

/// Generate a cryptographically random bot token (48 URL-safe chars).
fn generate_bot_token() -> String {
    rand::rng()
        .sample_iter(Alphanumeric)
        .take(48)
        .map(char::from)
        .collect()
}

/// Hash a bot token using SHA-256 (stored in the DB).
fn hash_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Generate a real Ed25519 keypair and return the public key as hex.
///
/// The signing keypair is generated fresh for each bot application at creation
/// time.  The public key is stored so that consumers (bots, webhook receivers)
/// can verify HMAC/Ed25519 signatures from Nexus.  The private key is NOT
/// stored server-side — the server signs with its own internal key; this
/// public key is exposed in the developer portal for client-side verification.
fn generate_public_key() -> String {
    use ed25519_dalek::SigningKey;
    use rand_core::OsRng;
    let signing_key = SigningKey::generate(&mut OsRng);
    hex::encode(signing_key.verifying_key().as_bytes())
}

// ============================================================================
// Developer Portal Endpoints
// ============================================================================

/// GET /api/v1/applications — List all bot applications owned by the current user.
async fn list_applications(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> NexusResult<Json<Vec<BotApplication>>> {
    let bots = bots::get_bots_by_owner(&state.db.pool, auth.user_id).await?;
    Ok(Json(bots))
}

/// GET /api/v1/applications/{app_id} — Get a specific bot application.
async fn get_application(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<Uuid>,
) -> NexusResult<Json<BotApplication>> {
    let bot = bots::get_bot(&state.db.pool, app_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "application".to_string(),
        })?;

    if bot.owner_id != auth.user_id {
        return Err(NexusError::Forbidden);
    }

    Ok(Json(bot))
}

/// POST /api/v1/applications — Create a new bot application.
async fn create_application(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CreateBotRequest>,
) -> NexusResult<Json<(BotApplication, BotToken)>> {
    // ── Input validation ───────────────────────────────────────────────────
    if body.name.len() > 32 || body.name.is_empty() {
        return Err(NexusError::Validation {
            message: "Bot name must be 1-32 characters".into(),
        });
    }
    if let Some(ref desc) = body.description
        && desc.len() > 120 {
            return Err(NexusError::Validation {
                message: "Bot description must be 120 characters or fewer".into(),
            });
        }

    // Rate limiting: 5 bot apps per hour per user
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:bot:create:{}", auth.user_id),
        5,
        3600,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:bot:create:tag:{tag}"),
        10,
        3600,
    )
    .await?;
    let app_id = snowflake::generate_id();
    let raw_token = generate_bot_token();
    let token_hash = hash_token(&raw_token);
    let public_key = generate_public_key();

    let bot = bots::create_bot(
        &state.db.pool,
        app_id,
        auth.user_id,
        &body.name,
        body.description.as_deref(),
        &token_hash,
        &public_key,
        body.is_public.unwrap_or(false),
        &body.redirect_uris.unwrap_or_default(),
        body.interactions_endpoint_url.as_deref(),
    )
    .await?;

    // Token is only returned once on creation
    Ok(Json((
        bot,
        BotToken {
            token: format!("Bot {raw_token}"),
        },
    )))
}

/// PATCH /api/v1/applications/{app_id} — Update a bot application.
async fn update_application(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<Uuid>,
    Json(body): Json<UpdateBotRequest>,
) -> NexusResult<Json<BotApplication>> {
    // Verify ownership
    let existing = bots::get_bot(&state.db.pool, app_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "application".to_string(),
        })?;
    if existing.owner_id != auth.user_id {
        return Err(NexusError::Forbidden);
    }

    let updated = bots::update_bot(
        &state.db.pool,
        app_id,
        body.name.as_deref(),
        body.description.as_deref(),
        body.avatar.as_deref(),
        body.is_public,
        body.redirect_uris.as_deref(),
        body.interactions_endpoint_url.as_deref(),
    )
    .await?
    .ok_or(NexusError::NotFound {
        resource: "application".to_string(),
    })?;

    Ok(Json(updated))
}

/// DELETE /api/v1/applications/{app_id} — Delete a bot application.
async fn delete_application(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<Uuid>,
) -> NexusResult<axum::http::StatusCode> {
    // Rate limiting: 10 deletions per user per minute
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:bot:delete:{}", auth.user_id),
        10,
        60,
    )
    .await?;

    let existing = bots::get_bot(&state.db.pool, app_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "application".to_string(),
        })?;
    if existing.owner_id != auth.user_id {
        return Err(NexusError::Forbidden);
    }

    bots::delete_bot(&state.db.pool, app_id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// POST /api/v1/applications/{app_id}/token/reset — Regenerate the bot token.
async fn reset_token(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<Uuid>,
) -> NexusResult<Json<BotToken>> {
    // Rate limiting: 3 token resets per user per hour
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:bot:token_reset:{}", auth.user_id),
        3,
        3600,
    )
    .await?;

    let existing = bots::get_bot(&state.db.pool, app_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "application".to_string(),
        })?;
    if existing.owner_id != auth.user_id {
        return Err(NexusError::Forbidden);
    }

    let raw_token = generate_bot_token();
    let token_hash = hash_token(&raw_token);
    bots::update_bot_token(&state.db.pool, app_id, &token_hash).await?;

    Ok(Json(BotToken {
        token: format!("Bot {raw_token}"),
    }))
}

// ============================================================================
// Server Integration Endpoints
// ============================================================================

/// GET /api/v1/servers/{server_id}/integrations — List bots in a server.
async fn list_server_bots(
    Extension(_auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<Uuid>,
) -> NexusResult<Json<Vec<BotServerInstall>>> {
    let installs = bots::get_server_bots(&state.db.pool, server_id).await?;
    Ok(Json(installs))
}

/// POST /api/v1/servers/{server_id}/integrations — Install a bot.
#[derive(serde::Deserialize)]
struct InstallBotBody {
    bot_id: Uuid,
    permissions: Option<i64>,
    scopes: Option<Vec<String>>,
}

async fn install_bot(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(server_id): Path<Uuid>,
    Json(body): Json<InstallBotBody>,
) -> NexusResult<Json<BotServerInstall>> {
    // Rate limiting: 10 bot installs per hour per user
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:bot:install:{}", auth.user_id),
        10,
        3600,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:bot:install:tag:{tag}"),
        20,
        3600,
    )
    .await?;
    // Verify bot exists
    let _bot = bots::get_bot(&state.db.pool, body.bot_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "bot".to_string(),
        })?;

    let scopes = body.scopes.unwrap_or_else(|| vec!["bot".to_string()]);
    let permissions = body.permissions.unwrap_or(0);

    let install = bots::install_bot_to_server(
        &state.db.pool,
        body.bot_id,
        server_id,
        auth.user_id,
        &scopes,
        permissions,
    )
    .await?;

    Ok(Json(install))
}

/// DELETE /api/v1/servers/{server_id}/integrations/{bot_id} — Uninstall a bot.
async fn uninstall_bot(
    Extension(_auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((server_id, bot_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<axum::http::StatusCode> {
    bots::uninstall_bot_from_server(&state.db.pool, bot_id, server_id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ============================================================================
// Public Endpoints (no auth required)
// ============================================================================

/// Public view of a bot — only non-sensitive fields returned, only for public bots.
#[derive(serde::Serialize)]
pub struct PublicBotInfo {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub avatar: Option<String>,
    pub verified: bool,
}

/// GET /api/v1/bots/{bot_id} — Fetch public info for a bot application.
/// Returns the bot name / description / avatar so server admins can verify
/// the bot they are about to install.  Only works for bots with `is_public = true`.
async fn get_public_bot_info(
    State(state): State<Arc<AppState>>,
    Path(bot_id): Path<Uuid>,
) -> NexusResult<Json<PublicBotInfo>> {
    let bot = bots::get_bot(&state.db.pool, bot_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "bot".to_string(),
        })?;

    if !bot.is_public {
        return Err(NexusError::NotFound {
            resource: "bot".to_string(),
        });
    }

    Ok(Json(PublicBotInfo {
        id: bot.id,
        name: bot.name,
        description: bot.description,
        avatar: bot.avatar,
        verified: bot.verified,
    }))
}
