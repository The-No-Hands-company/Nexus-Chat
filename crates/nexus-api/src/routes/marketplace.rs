//! Plugin marketplace routes — Phase 19-04.
//!
//! GET    /marketplace/plugins                     — Browse / search plugins
//! GET    /marketplace/plugins/:slug               — Get plugin by slug
//! POST   /marketplace/plugins                     — Publish a new plugin
//! POST   /marketplace/plugins/:id/reviews         — Submit / update review
//! GET    /marketplace/plugins/:id/reviews         — List reviews for a plugin
//! DELETE /marketplace/plugins/:id/reviews         — Delete own review
//! POST   /servers/:server_id/plugin-installs      — Install a plugin
//! GET    /servers/:server_id/plugin-installs      — List installed plugins
//! PATCH  /servers/:server_id/plugin-installs/:plugin_id — Toggle enable/disable
//! DELETE /servers/:server_id/plugin-installs/:plugin_id — Uninstall

use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    middleware,
    routing::{get, patch, post},
};
use nexus_common::error::{NexusError, NexusResult};
use nexus_common::models::Member;
use nexus_common::models::ecosystem::{
    MarketplaceMonetization, MarketplacePlugin, PluginInstall, PluginReview, ReviewStatus,
    TrustTier,
};
use nexus_common::security_scanning::{MockMalwareScanner, SecurityScanner};
use nexus_db::repository::{marketplace, members};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::{AppState, middleware::AuthContext};

// ── Router ────────────────────────────────────────────────────────────────────

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // Marketplace browse / publish
        .route(
            "/marketplace/plugins",
            get(search_plugins).post(publish_plugin),
        )
        .route("/marketplace/plugins/{slug}", get(get_plugin_by_slug))
        // Reviews (user ratings)
        .route(
            "/marketplace/plugins/{plugin_id}/reviews",
            get(list_reviews).post(submit_review).delete(delete_review),
        )
        // Store governance (admin endpoints)
        .route(
            "/marketplace/plugins/{plugin_id}/submit-review",
            post(submit_plugin_for_review),
        )
        .route(
            "/marketplace/plugins/{plugin_id}/purchase-intent",
            post(create_purchase_intent),
        )
        .route("/marketplace/admin/review-queue", get(get_review_queue))
        .route(
            "/marketplace/admin/plugins/{plugin_id}/approve",
            post(approve_plugin),
        )
        .route(
            "/marketplace/admin/plugins/{plugin_id}/reject",
            post(reject_plugin),
        )
        .route(
            "/marketplace/admin/plugins/{plugin_id}/quarantine",
            post(quarantine_plugin),
        )
        .route(
            "/marketplace/admin/plugins/{plugin_id}/security-scan",
            post(trigger_security_scan),
        )
        .route(
            "/marketplace/admin/plugins/{plugin_id}/takedown",
            post(request_takedown_admin),
        )
        .route(
            "/marketplace/admin/takedowns/{takedown_id}/review",
            post(review_takedown),
        )
        .route(
            "/marketplace/admin/takedowns/{takedown_id}/reinstate",
            post(reinstate_plugin_admin),
        )
        // Creator vetting
        .route(
            "/marketplace/creator/vetting",
            get(get_creator_vetting_status).post(apply_for_creator),
        )
        .route(
            "/marketplace/creator/vetting/materials",
            patch(update_creator_verification_materials),
        )
        .route(
            "/marketplace/creator/plugins/{plugin_id}/monetization",
            get(get_creator_monetization).post(upsert_creator_monetization),
        )
        .route(
            "/marketplace/admin/creators/vetting-queue",
            get(get_vetting_queue),
        )
        .route(
            "/marketplace/admin/creators/{user_id}/approve",
            post(approve_creator),
        )
        .route(
            "/marketplace/admin/creators/{user_id}/reject",
            post(reject_creator),
        )
        .route(
            "/marketplace/admin/dashboard/stats",
            get(get_dashboard_stats),
        )
        // Server installs
        .route(
            "/servers/{server_id}/plugin-installs",
            post(install_plugin).get(list_installs),
        )
        .route(
            "/servers/{server_id}/plugin-installs/{plugin_id}",
            axum::routing::patch(toggle_install).delete(uninstall_plugin),
        )
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ))
}

// ── Request / Query Types ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SearchQuery {
    q: Option<String>,
    category: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct PublishPluginRequest {
    name: String,
    slug: String,
    description: Option<String>,
    version: String,
    manifest_url: String,
    icon_url: Option<String>,
    source_url: Option<String>,
    signature: Option<String>,
    signing_key_id: Option<String>,
    category: String,
    tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct SubmitReviewRequest {
    rating: i16,
    title: Option<String>,
    body: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InstallRequest {
    plugin_id: Uuid,
    version: String,
}

#[derive(Debug, Deserialize)]
struct ToggleRequest {
    enabled: bool,
}

#[derive(Debug, Deserialize)]
struct ApprovePluginRequest {
    trust_tier: Option<String>, // "reviewed" or "verified", default "reviewed"
    notes: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RejectPluginRequest {
    reason: String,
    notes: Option<String>,
}

#[derive(Debug, Deserialize)]
struct QuarantinePluginRequest {
    reason: String,
    notes: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TakedownRequest {
    reason: String, // 'copyright', 'malware', 'abuse', 'spam', 'tos_violation'
    description: String,
    evidence_urls: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct ReviewTakedownRequest {
    status: String, // 'pending', 'quarantined', 'reviewed', 'reinstated', 'permanent_takedown'
    notes: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApplyForCreatorRequest {
    rights_attestation: String,
}

#[derive(Debug, Deserialize)]
struct UpdateCreatorVerificationMaterialsRequest {
    domain: Option<String>,
    signing_key_fingerprint: Option<String>,
    signing_key_type: Option<String>, // "pgp", "ssh", or "minisign"
}

#[derive(Debug, Deserialize)]
struct UpsertMonetizationRequest {
    price_cents: Option<i32>,
    payment_link: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApproveCreatorRequest {
    identity_level: String, // "email_verified", "domain_verified", "signature_verified"
}

#[derive(Debug, Deserialize)]
struct RejectCreatorRequest {
    reason: String,
}

#[derive(Debug, Serialize)]
struct CreatorVettingStatus {
    id: Uuid,
    identity_level: String,
    status: String,
    applied_at: chrono::DateTime<chrono::Utc>,
    approved_at: Option<chrono::DateTime<chrono::Utc>>,
    can_publish: bool,
}

#[derive(Debug, Serialize)]
struct DashboardStats {
    review_queue_count: i64,
    vetting_queue_count: i64,
    todays_approvals: i64,
    todays_rejections: i64,
    pending_takedowns: i64,
}

#[derive(Debug, Serialize)]
struct SecurityScanResponse {
    scan_id: String,
    plugin_id: Uuid,
    passed: bool,
    threat_level: String,
    issues_count: usize,
    scanner: String,
}

#[derive(Debug, Serialize)]
struct PurchaseIntentResponse {
    plugin_id: Uuid,
    is_monetized: bool,
    price_cents: Option<i32>,
    currency: Option<String>,
    payment_link: Option<String>,
    purchase_count: i64,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn search_plugins(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SearchQuery>,
) -> NexusResult<Json<Vec<MarketplacePlugin>>> {
    let limit = q.limit.unwrap_or(20).min(100);
    let offset = q.offset.unwrap_or(0);

    let plugins = marketplace::search_plugins(
        &state.db.pool,
        q.q.as_deref(),
        q.category.as_deref(),
        limit,
        offset,
    )
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(plugins))
}

async fn get_plugin_by_slug(
    State(state): State<Arc<AppState>>,
    Path(slug): Path<String>,
) -> NexusResult<Json<MarketplacePlugin>> {
    let plugin = marketplace::get_plugin_by_slug(&state.db.pool, &slug)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .ok_or_else(|| NexusError::NotFound {
            resource: "marketplace_plugin".into(),
        })?;

    Ok(Json(plugin))
}

async fn publish_plugin(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<PublishPluginRequest>,
) -> NexusResult<Json<MarketplacePlugin>> {
    if body.name.is_empty() || body.slug.is_empty() {
        return Err(NexusError::Validation {
            message: "name and slug are required".into(),
        });
    }

    // Validate slug format (lowercase alphanumeric + hyphens)
    if !body
        .slug
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(NexusError::Validation {
            message: "slug must contain only lowercase letters, digits, and hyphens".into(),
        });
    }

    let id = Uuid::new_v4();
    let tags = serde_json::to_value(body.tags.unwrap_or_default())
        .unwrap_or(serde_json::Value::Array(vec![]));

    let plugin = marketplace::create_plugin(
        &state.db.pool,
        id,
        &body.name,
        &body.slug,
        body.description.as_deref(),
        Some(ctx.user_id),
        &body.version,
        &body.manifest_url,
        body.icon_url.as_deref(),
        body.source_url.as_deref(),
        body.signature.as_deref(),
        body.signing_key_id.as_deref(),
        &body.category,
        &tags,
    )
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(plugin))
}

// ── Reviews ───────────────────────────────────────────────────────────────────

async fn submit_review(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(plugin_id): Path<Uuid>,
    Json(body): Json<SubmitReviewRequest>,
) -> NexusResult<Json<PluginReview>> {
    if !(1..=5).contains(&body.rating) {
        return Err(NexusError::Validation {
            message: "rating must be between 1 and 5".into(),
        });
    }

    let id = Uuid::new_v4();
    let review = marketplace::create_review(
        &state.db.pool,
        id,
        plugin_id,
        ctx.user_id,
        body.rating,
        body.title.as_deref(),
        body.body.as_deref(),
    )
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    // Update plugin's avg rating
    let _ = marketplace::update_plugin_rating(&state.db.pool, plugin_id).await;

    Ok(Json(review))
}

async fn list_reviews(
    State(state): State<Arc<AppState>>,
    Path(plugin_id): Path<Uuid>,
) -> NexusResult<Json<Vec<PluginReview>>> {
    let reviews = marketplace::list_reviews(&state.db.pool, plugin_id, 100)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(reviews))
}

async fn delete_review(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(plugin_id): Path<Uuid>,
) -> NexusResult<Json<serde_json::Value>> {
    let deleted = marketplace::delete_review(&state.db.pool, plugin_id, ctx.user_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    if !deleted {
        return Err(NexusError::NotFound {
            resource: "plugin_review".into(),
        });
    }

    // Update avg rating
    let _ = marketplace::update_plugin_rating(&state.db.pool, plugin_id).await;

    Ok(Json(serde_json::json!({ "deleted": true })))
}

// ── Server Installs ───────────────────────────────────────────────────────────

async fn install_plugin(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(server_id): Path<Uuid>,
    Json(body): Json<InstallRequest>,
) -> NexusResult<Json<PluginInstall>> {
    let _member: Option<Member> = members::find_member(&state.db.pool, ctx.user_id, server_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;
    if _member.is_none() {
        return Err(NexusError::Forbidden);
    }

    // Verify plugin exists
    let _plugin = marketplace::get_plugin(&state.db.pool, body.plugin_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .ok_or_else(|| NexusError::NotFound {
            resource: "marketplace_plugin".into(),
        })?;

    let id = Uuid::new_v4();
    let install = marketplace::install_plugin(
        &state.db.pool,
        id,
        body.plugin_id,
        server_id,
        ctx.user_id,
        &body.version,
    )
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    // Increment download counter
    let _ = marketplace::increment_downloads(&state.db.pool, body.plugin_id).await;

    Ok(Json(install))
}

async fn list_installs(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(server_id): Path<Uuid>,
) -> NexusResult<Json<Vec<PluginInstall>>> {
    let _member: Option<Member> = members::find_member(&state.db.pool, ctx.user_id, server_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;
    if _member.is_none() {
        return Err(NexusError::Forbidden);
    }

    let installs = marketplace::list_server_installs(&state.db.pool, server_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(installs))
}

async fn toggle_install(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path((server_id, plugin_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<ToggleRequest>,
) -> NexusResult<Json<PluginInstall>> {
    let _member: Option<Member> = members::find_member(&state.db.pool, ctx.user_id, server_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;
    if _member.is_none() {
        return Err(NexusError::Forbidden);
    }

    let install =
        marketplace::toggle_plugin_install(&state.db.pool, plugin_id, server_id, body.enabled)
            .await
            .map_err(|e| NexusError::Internal(e.into()))?
            .ok_or_else(|| NexusError::NotFound {
                resource: "plugin_install".into(),
            })?;

    Ok(Json(install))
}

async fn uninstall_plugin(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path((server_id, plugin_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<Json<serde_json::Value>> {
    let _member: Option<Member> = members::find_member(&state.db.pool, ctx.user_id, server_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;
    if _member.is_none() {
        return Err(NexusError::Forbidden);
    }

    let deleted = marketplace::uninstall_plugin(&state.db.pool, plugin_id, server_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    if !deleted {
        return Err(NexusError::NotFound {
            resource: "plugin_install".into(),
        });
    }

    Ok(Json(serde_json::json!({ "deleted": true })))
}

// ── Store Governance Handlers ─────────────────────────────────────────────────────

async fn submit_plugin_for_review(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(plugin_id): Path<Uuid>,
) -> NexusResult<Json<serde_json::Value>> {
    // Verify user is the plugin author
    let plugin = marketplace::get_plugin(&state.db.pool, plugin_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .ok_or_else(|| NexusError::NotFound {
            resource: "marketplace_plugin".into(),
        })?;

    if plugin.author_id != Some(ctx.user_id) {
        return Err(NexusError::Forbidden);
    }

    // Submit for initial scanning
    marketplace::submit_for_review(&state.db.pool, plugin_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    // Kick off asynchronous scanning immediately to reduce time-to-review.
    let pool = state.db.pool.clone();
    let manifest_url = plugin.manifest_url.clone();
    let source_url = plugin.source_url.clone();
    let actor_id = ctx.user_id;

    tokio::spawn(async move {
        if let Err(error) = run_security_scan_pipeline(
            &pool,
            plugin_id,
            actor_id,
            &manifest_url,
            source_url.as_deref(),
            true,
        )
        .await
        {
            tracing::warn!(%plugin_id, "background security scan pipeline failed");
        }
    });

    Ok(Json(serde_json::json!({ "submitted": true })))
}

async fn get_review_queue(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Query(limit): Query<Option<i64>>,
) -> NexusResult<Json<serde_json::Value>> {
    // Load user flags and check permissions
    ctx.load_flags(&state.db.pool)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    if !ctx.can_review_plugins().await {
        return Err(NexusError::Forbidden);
    }

    let items = marketplace::get_review_queue(&state.db.pool, limit.unwrap_or(20), 0)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(serde_json::json!({
        "queue": items,
        "count": items.len()
    })))
}

async fn approve_plugin(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(plugin_id): Path<Uuid>,
    Json(body): Json<ApprovePluginRequest>,
) -> NexusResult<Json<serde_json::Value>> {
    // Load user flags and check permissions
    ctx.load_flags(&state.db.pool)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    if !ctx.can_review_plugins().await {
        return Err(NexusError::Forbidden);
    }

    let tier = match body.trust_tier.as_deref().unwrap_or("reviewed") {
        "verified" => TrustTier::Verified,
        _ => TrustTier::Reviewed,
    };

    marketplace::update_review_status(
        &state.db.pool,
        plugin_id,
        ReviewStatus::Approved,
        ctx.user_id,
        None,
        body.notes.as_deref(),
    )
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    marketplace::update_trust_tier(&state.db.pool, plugin_id, tier)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    marketplace::log_review(
        &state.db.pool,
        Uuid::new_v4(),
        plugin_id,
        Some(ctx.user_id),
        "review",
        "approved",
        "approved",
        None,
        body.notes.as_deref(),
    )
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(serde_json::json!({ "approved": true })))
}

async fn reject_plugin(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(plugin_id): Path<Uuid>,
    Json(body): Json<RejectPluginRequest>,
) -> NexusResult<Json<serde_json::Value>> {
    // Load user flags and check permissions
    ctx.load_flags(&state.db.pool)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    if !ctx.can_review_plugins().await {
        return Err(NexusError::Forbidden);
    }

    marketplace::update_review_status(
        &state.db.pool,
        plugin_id,
        ReviewStatus::Rejected,
        ctx.user_id,
        Some(&body.reason),
        body.notes.as_deref(),
    )
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    marketplace::log_review(
        &state.db.pool,
        Uuid::new_v4(),
        plugin_id,
        Some(ctx.user_id),
        "review",
        "rejected",
        "rejected",
        Some(&body.reason),
        body.notes.as_deref(),
    )
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(serde_json::json!({ "rejected": true })))
}

async fn quarantine_plugin(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(plugin_id): Path<Uuid>,
    Json(body): Json<QuarantinePluginRequest>,
) -> NexusResult<Json<serde_json::Value>> {
    // Load user flags and check permissions
    ctx.load_flags(&state.db.pool)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    if !ctx.can_handle_takedowns().await {
        return Err(NexusError::Forbidden);
    }

    marketplace::update_review_status(
        &state.db.pool,
        plugin_id,
        ReviewStatus::Quarantined,
        ctx.user_id,
        Some(&body.reason),
        body.notes.as_deref(),
    )
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    marketplace::log_review(
        &state.db.pool,
        Uuid::new_v4(),
        plugin_id,
        Some(ctx.user_id),
        "review",
        "quarantined",
        "quarantined",
        Some(&body.reason),
        body.notes.as_deref(),
    )
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(serde_json::json!({ "quarantined": true })))
}

async fn request_takedown_admin(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(plugin_id): Path<Uuid>,
    Json(body): Json<TakedownRequest>,
) -> NexusResult<Json<serde_json::Value>> {
    // Load user flags and check permissions
    ctx.load_flags(&state.db.pool)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    if !ctx.can_handle_takedowns().await {
        return Err(NexusError::Forbidden);
    }

    let takedown_id = Uuid::new_v4();
    let evidence_json = body
        .evidence_urls
        .as_ref()
        .map(|urls| serde_json::to_value(urls).unwrap_or(serde_json::Value::Array(vec![])));

    marketplace::request_takedown(
        &state.db.pool,
        takedown_id,
        plugin_id,
        Some(ctx.user_id),
        &body.reason,
        &body.description,
        evidence_json.as_ref(),
    )
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(serde_json::json!({
        "takedown_id": takedown_id,
        "plugin_id": plugin_id
    })))
}

async fn review_takedown(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(takedown_id): Path<Uuid>,
    Json(body): Json<ReviewTakedownRequest>,
) -> NexusResult<Json<serde_json::Value>> {
    // Load user flags and check permissions
    ctx.load_flags(&state.db.pool)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    if !ctx.can_handle_takedowns().await {
        return Err(NexusError::Forbidden);
    }

    marketplace::review_takedown(
        &state.db.pool,
        takedown_id,
        ctx.user_id,
        &body.status,
        body.notes.as_deref(),
    )
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(serde_json::json!({ "reviewed": true })))
}

async fn reinstate_plugin_admin(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(takedown_id): Path<Uuid>,
) -> NexusResult<Json<serde_json::Value>> {
    // Load user flags and check permissions
    ctx.load_flags(&state.db.pool)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    if !ctx.can_handle_takedowns().await {
        return Err(NexusError::Forbidden);
    }

    marketplace::reinstate_plugin(&state.db.pool, takedown_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(serde_json::json!({ "reinstated": true })))
}

// ── Creator Vetting Handlers ──────────────────────────────────────────────────────

async fn get_creator_vetting_status(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
) -> NexusResult<Json<CreatorVettingStatus>> {
    let vetting = marketplace::get_creator_vetting(&state.db.pool, ctx.user_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .ok_or_else(|| NexusError::NotFound {
            resource: "creator_vetting".into(),
        })?;

    let can_publish = vetting.status == "approved";

    Ok(Json(CreatorVettingStatus {
        id: vetting.id,
        identity_level: vetting.identity_level.to_string(),
        status: vetting.status,
        applied_at: vetting.created_at,
        approved_at: vetting.approved_at,
        can_publish,
    }))
}

async fn apply_for_creator(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<ApplyForCreatorRequest>,
) -> NexusResult<Json<serde_json::Value>> {
    // Create or get existing vetting record
    let vetting = match marketplace::get_creator_vetting(&state.db.pool, ctx.user_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
    {
        Some(v) => v,
        None => marketplace::create_creator_vetting(&state.db.pool, Uuid::new_v4(), ctx.user_id)
            .await
            .map_err(|e| NexusError::Internal(e.into()))?,
    };

    // Verify not already approved
    if vetting.status == "approved" {
        return Err(NexusError::Validation {
            message: "Creator account already approved".into(),
        });
    }

    marketplace::upsert_creator_attestation(&state.db.pool, ctx.user_id, &body.rights_attestation)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(serde_json::json!({
        "vetting_id": vetting.id,
        "status": "pending",
        "message": "Application submitted for review"
    })))
}

async fn update_creator_verification_materials(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<UpdateCreatorVerificationMaterialsRequest>,
) -> NexusResult<Json<serde_json::Value>> {
    // Ensure vetting record exists first.
    if marketplace::get_creator_vetting(&state.db.pool, ctx.user_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .is_none()
    {
        let _ = marketplace::create_creator_vetting(&state.db.pool, Uuid::new_v4(), ctx.user_id)
            .await
            .map_err(|e| NexusError::Internal(e.into()))?;
    }

    if let Some(domain) = body.domain.as_deref() {
        let trimmed = domain.trim();
        if trimmed.is_empty() || trimmed.len() > 255 || trimmed.contains(' ') {
            return Err(NexusError::Validation {
                message: "domain must be a non-empty host/domain string".into(),
            });
        }
    }

    if let Some(key_type) = body.signing_key_type.as_deref() {
        match key_type {
            "pgp" | "ssh" | "minisign" => {}
            _ => {
                return Err(NexusError::Validation {
                    message: "signing_key_type must be one of: pgp, ssh, minisign".into(),
                });
            }
        }
    }

    if body.signing_key_fingerprint.is_some() && body.signing_key_type.is_none() {
        return Err(NexusError::Validation {
            message: "signing_key_type is required when signing_key_fingerprint is provided".into(),
        });
    }

    marketplace::update_creator_verification_materials(
        &state.db.pool,
        ctx.user_id,
        body.domain.map(|s| s.trim().to_string()),
        body.signing_key_fingerprint.map(|s| s.trim().to_string()),
        body.signing_key_type,
    )
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(serde_json::json!({
        "updated": true,
        "status": "pending"
    })))
}

async fn get_vetting_queue(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Query(limit): Query<Option<i64>>,
) -> NexusResult<Json<serde_json::Value>> {
    // Load flags and check permissions
    ctx.load_flags(&state.db.pool)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    if !ctx.is_marketplace_admin().await {
        return Err(NexusError::Forbidden);
    }

    let items =
        marketplace::get_creator_vetting_queue(&state.db.pool, "pending", limit.unwrap_or(20), 0)
            .await
            .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(serde_json::json!({
        "queue": items,
        "count": items.len()
    })))
}

async fn approve_creator(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(user_id): Path<Uuid>,
    Json(body): Json<ApproveCreatorRequest>,
) -> NexusResult<Json<serde_json::Value>> {
    // Load flags and check permissions
    ctx.load_flags(&state.db.pool)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    if !ctx.is_marketplace_admin().await {
        return Err(NexusError::Forbidden);
    }

    let identity_level = match body.identity_level.as_str() {
        "email_verified" | "domain_verified" | "signature_verified" => body.identity_level.as_str(),
        _ => {
            return Err(NexusError::Validation {
                message: "identity_level must be one of: email_verified, domain_verified, signature_verified".into(),
            });
        }
    };

    // Update identity level
    marketplace::update_creator_identity_level(&state.db.pool, user_id, identity_level)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    // Approve vetting
    marketplace::approve_creator_vetting(&state.db.pool, user_id, ctx.user_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(serde_json::json!({ "approved": true })))
}

async fn reject_creator(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(user_id): Path<Uuid>,
    Json(body): Json<RejectCreatorRequest>,
) -> NexusResult<Json<serde_json::Value>> {
    // Load flags and check permissions
    ctx.load_flags(&state.db.pool)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    if !ctx.is_marketplace_admin().await {
        return Err(NexusError::Forbidden);
    }

    marketplace::reject_creator_vetting(&state.db.pool, user_id, &body.reason)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(serde_json::json!({ "rejected": true })))
}

async fn get_dashboard_stats(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
) -> NexusResult<Json<DashboardStats>> {
    // Load flags and check permissions
    ctx.load_flags(&state.db.pool)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    if !ctx.is_marketplace_admin().await {
        return Err(NexusError::Forbidden);
    }

    // Query review queue count
    let review_queue: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM marketplace_plugins WHERE review_status IN ('submitted', 'scanning', 'review')"
    )
    .fetch_one(&state.db.pool)
    .await
    .unwrap_or(0);

    // Query vetting queue count
    let vetting_queue: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM creator_vetting WHERE status = 'pending'")
            .fetch_one(&state.db.pool)
            .await
            .unwrap_or(0);

    // Query today's approvals
    let todays_approvals: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM marketplace_reviews WHERE action = 'approved' AND created_at > NOW() - INTERVAL '24 hours'"
    )
    .fetch_one(&state.db.pool)
    .await
    .unwrap_or(0);

    // Query today's rejections
    let todays_rejections: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM marketplace_reviews WHERE action = 'rejected' AND created_at > NOW() - INTERVAL '24 hours'"
    )
    .fetch_one(&state.db.pool)
    .await
    .unwrap_or(0);

    // Query pending takedowns
    let pending_takedowns: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM marketplace_takedowns WHERE quarantine_status = 'pending'",
    )
    .fetch_one(&state.db.pool)
    .await
    .unwrap_or(0);

    Ok(Json(DashboardStats {
        review_queue_count: review_queue,
        vetting_queue_count: vetting_queue,
        todays_approvals,
        todays_rejections,
        pending_takedowns,
    }))
}

async fn get_creator_monetization(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(plugin_id): Path<Uuid>,
) -> NexusResult<Json<MarketplaceMonetization>> {
    let plugin = marketplace::get_plugin(&state.db.pool, plugin_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .ok_or_else(|| NexusError::NotFound {
            resource: "marketplace_plugin".into(),
        })?;

    if plugin.author_id != Some(ctx.user_id) {
        return Err(NexusError::Forbidden);
    }

    let monetization = marketplace::get_monetization_by_plugin(&state.db.pool, plugin_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .ok_or_else(|| NexusError::NotFound {
            resource: "marketplace_monetization".into(),
        })?;

    Ok(Json(monetization))
}

async fn upsert_creator_monetization(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(plugin_id): Path<Uuid>,
    Json(body): Json<UpsertMonetizationRequest>,
) -> NexusResult<Json<MarketplaceMonetization>> {
    let plugin = marketplace::get_plugin(&state.db.pool, plugin_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .ok_or_else(|| NexusError::NotFound {
            resource: "marketplace_plugin".into(),
        })?;

    if plugin.author_id != Some(ctx.user_id) {
        return Err(NexusError::Forbidden);
    }

    if let Some(price) = body.price_cents
        && price < 0 {
            return Err(NexusError::Validation {
                message: "price_cents must be >= 0".into(),
            });
        }

    if body.price_cents.unwrap_or(0) > 0 {
        let has_payment_link = body
            .payment_link
            .as_ref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);

        if !has_payment_link {
            return Err(NexusError::Validation {
                message: "payment_link is required for paid plugins".into(),
            });
        }
    }

    if let Some(link) = body.payment_link.as_deref() {
        let is_http = link.starts_with("http://") || link.starts_with("https://");
        if !is_http {
            return Err(NexusError::Validation {
                message: "payment_link must start with http:// or https://".into(),
            });
        }
    }

    if marketplace::get_monetization_by_plugin(&state.db.pool, plugin_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .is_none()
    {
        marketplace::create_monetization_record(
            &state.db.pool,
            Uuid::new_v4(),
            plugin_id,
            ctx.user_id,
        )
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;
    }

    marketplace::update_monetization_price(
        &state.db.pool,
        plugin_id,
        body.price_cents,
        body.payment_link,
    )
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    let monetization = marketplace::get_monetization_by_plugin(&state.db.pool, plugin_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .ok_or_else(|| NexusError::NotFound {
            resource: "marketplace_monetization".into(),
        })?;

    Ok(Json(monetization))
}

async fn create_purchase_intent(
    State(state): State<Arc<AppState>>,
    Path(plugin_id): Path<Uuid>,
) -> NexusResult<Json<PurchaseIntentResponse>> {
    let monetization = marketplace::get_monetization_by_plugin(&state.db.pool, plugin_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .ok_or_else(|| NexusError::NotFound {
            resource: "marketplace_monetization".into(),
        })?;

    if monetization.is_monetized {
        let has_payment_link = monetization
            .payment_link
            .as_ref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);

        if !has_payment_link {
            return Err(NexusError::Validation {
                message: "paid plugin is missing payment_link configuration".into(),
            });
        }

        marketplace::increment_purchase_count(&state.db.pool, plugin_id)
            .await
            .map_err(|e| NexusError::Internal(e.into()))?;
    }

    let refreshed = marketplace::get_monetization_by_plugin(&state.db.pool, plugin_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .ok_or_else(|| NexusError::NotFound {
            resource: "marketplace_monetization".into(),
        })?;

    Ok(Json(PurchaseIntentResponse {
        plugin_id,
        is_monetized: refreshed.is_monetized,
        price_cents: refreshed.price_cents,
        currency: Some(refreshed.currency),
        payment_link: refreshed.payment_link,
        purchase_count: refreshed.purchase_count,
    }))
}

// ── Security Scanning Handlers ────────────────────────────────────────────────────

async fn trigger_security_scan(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(plugin_id): Path<Uuid>,
) -> NexusResult<Json<SecurityScanResponse>> {
    // Load flags and check permissions
    ctx.load_flags(&state.db.pool)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    if !ctx.can_review_plugins().await {
        return Err(NexusError::Forbidden);
    }

    let plugin = marketplace::get_plugin(&state.db.pool, plugin_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .ok_or_else(|| NexusError::NotFound {
            resource: "marketplace_plugin".into(),
        })?;

    let scan_result = run_security_scan_pipeline(
        &state.db.pool,
        plugin_id,
        ctx.user_id,
        &plugin.manifest_url,
        plugin.source_url.as_deref(),
        false,
    )
    .await
    .map_err(|e| NexusError::Internal(anyhow::Error::msg(e)))?;

    Ok(Json(SecurityScanResponse {
        scan_id: scan_result.id,
        plugin_id,
        passed: scan_result.passed,
        threat_level: scan_result.threat_level,
        issues_count: scan_result.issues.len(),
        scanner: scan_result.scanner,
    }))
}

async fn run_security_scan_pipeline(
    pool: &sqlx::AnyPool,
    plugin_id: Uuid,
    actor_id: Uuid,
    manifest_url: &str,
    source_url: Option<&str>,
    automated: bool,
) -> Result<nexus_common::security_scanning::ScanResult, String> {
    let start_note = if automated {
        "Automated security scan started"
    } else {
        "Manual security scan started"
    };

    marketplace::update_review_status(
        pool,
        plugin_id,
        ReviewStatus::Scanning,
        actor_id,
        None,
        Some(start_note),
    )
    .await
    .map_err(|e| e.to_string())?;

    let _ = marketplace::log_review(
        pool,
        Uuid::new_v4(),
        plugin_id,
        Some(actor_id),
        "submitted",
        "scanning",
        if automated {
            "auto_scan_started"
        } else {
            "manual_scan_started"
        },
        None,
        Some(start_note),
    )
    .await;

    let scanner = MockMalwareScanner::new();
    let scan_result = scanner
        .scan_manifest(plugin_id, manifest_url, source_url)
        .await
        .map_err(|e| e.to_string())?;

    let scan_payload = serde_json::json!({
        "scan_id": scan_result.id,
        "scanner": scan_result.scanner,
        "passed": scan_result.passed,
        "threat_level": scan_result.threat_level,
        "issues": scan_result.issues,
        "mode": if automated { "automated" } else { "manual" },
    });

    if let Err(error) = marketplace::mark_security_scan(pool, plugin_id, &scan_payload).await {
        tracing::warn!(%plugin_id, error_kind = %nexus_common::logsafe::err_kind(&error), "failed to persist scan result");
    }

    let is_critical = scan_result.threat_level == "critical" && !scan_result.issues.is_empty();
    let next_status = if is_critical {
        ReviewStatus::Quarantined
    } else {
        ReviewStatus::Review
    };
    let reason = if is_critical {
        Some("Automatic quarantine: Critical security threats detected")
    } else {
        None
    };
    let notes = if is_critical {
        Some("Security scan quarantined plugin")
    } else {
        Some("Security scan complete; queued for manual review")
    };

    marketplace::update_review_status(pool, plugin_id, next_status, actor_id, reason, notes)
        .await
        .map_err(|e| e.to_string())?;

    let _ = marketplace::log_review(
        pool,
        Uuid::new_v4(),
        plugin_id,
        Some(actor_id),
        "scanning",
        if is_critical { "quarantined" } else { "review" },
        if is_critical {
            "scan_quarantined"
        } else {
            "scan_passed"
        },
        reason,
        notes,
    )
    .await;

    Ok(scan_result)
}
