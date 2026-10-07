//! Import & migration routes — Phase 19-01.
//!
//! POST   /servers/{server_id}/imports             — Start a data import job
//! GET    /servers/{server_id}/imports             — List import jobs for a server
//! GET    /servers/{server_id}/imports/{import_id} — Get import job status
//! POST   /servers/{server_id}/bulk-invite         — Send bulk email invitations

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::HeaderMap,
    middleware,
    routing::{get, post},
};
use nexus_common::error::{NexusError, NexusResult};
use nexus_common::models::Member;
use nexus_common::models::ecosystem::{BulkInvitation, ImportJob};
use nexus_common::permissions::Permissions;
use nexus_db::repository::{bulk_invitations, import_jobs, members, roles, servers};
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    AppState,
    middleware::{AuthContext, check_rate_limit_with_fallback, extract_client_tag},
};

// ── Router ────────────────────────────────────────────────────────────────────

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/servers/{server_id}/imports",
            post(create_import).get(list_imports),
        )
        .route("/servers/{server_id}/imports/{import_id}", get(get_import))
        .route("/servers/{server_id}/bulk-invite", post(create_bulk_invite))
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ))
}

// ── Request Bodies ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CreateImportRequest {
    /// Nexus-native import source kind.
    ///
    /// Canonical values:
    /// - `nexus_portability`
    /// - `portable_export`
    /// - `archive_bundle`
    /// - `custom_plugin`
    ///
    /// Backward-compatible alias accepted on input: `source_platform`.
    #[serde(alias = "source_platform")]
    source_kind: String,
    /// Opaque JSON metadata for portability/import plugins.
    metadata: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct BulkInviteRequest {
    /// Array of email addresses
    emails: Vec<String>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn create_import(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    headers: HeaderMap,
    Path(server_id): Path<Uuid>,
    Json(body): Json<CreateImportRequest>,
) -> NexusResult<Json<ImportJob>> {
    require_server_admin_or_owner(&state, server_id, ctx.user_id).await?;

    let source_kind = normalize_source_kind(&body.source_kind).ok_or_else(|| NexusError::Validation {
        message: "source_kind must be one of: nexus_portability, portable_export, archive_bundle, custom_plugin".into(),
    })?;

    if source_kind == "custom_plugin" {
        let has_plugin_key = metadata_has_plugin_hint(body.metadata.as_ref());
        if !has_plugin_key {
            return Err(NexusError::Validation {
                message: "custom_plugin imports require metadata.plugin_id or metadata.plugin_slug"
                    .into(),
            });
        }
    }

    if source_kind.is_empty() {
        return Err(NexusError::Validation {
            message: "source_kind is required".into(),
        });
    }

    let id = Uuid::new_v4();
    let metadata = body.metadata.unwrap_or(serde_json::json!({}));

    if !metadata.is_object() {
        return Err(NexusError::Validation {
            message: "metadata must be a JSON object".into(),
        });
    }

    let metadata_bytes =
        serde_json::to_vec(&metadata).map_err(|e| NexusError::Internal(e.into()))?;
    if metadata_bytes.len() > 2 * 1024 * 1024 {
        return Err(NexusError::Validation {
            message: "metadata payload exceeds 2MB limit".into(),
        });
    }

    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:import_create:tag:{tag}:server:{server_id}"),
        5,
        3600,
    )
    .await?;

    let job = import_jobs::create_import_job(
        &state.db.pool,
        id,
        server_id,
        ctx.user_id,
        source_kind,
        &metadata,
    )
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(job))
}

fn normalize_source_kind(input: &str) -> Option<&'static str> {
    match input.trim().to_ascii_lowercase().as_str() {
        "nexus_portability" | "nexus" => Some("nexus_portability"),
        "portable_export" | "portable" | "user_export" => Some("portable_export"),
        "archive_bundle" | "archive" => Some("archive_bundle"),
        "custom_plugin" | "plugin" => Some("custom_plugin"),
        // Backward compatibility for old external-source labels.
        "discord" | "slack" | "matrix" => Some("portable_export"),
        _ => None,
    }
}

fn metadata_has_plugin_hint(metadata: Option<&serde_json::Value>) -> bool {
    let Some(value) = metadata else {
        return false;
    };
    value.get("plugin_id").is_some() || value.get("plugin_slug").is_some()
}

async fn list_imports(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(server_id): Path<Uuid>,
) -> NexusResult<Json<Vec<ImportJob>>> {
    require_server_admin_or_owner(&state, server_id, ctx.user_id).await?;

    let jobs = import_jobs::list_import_jobs(&state.db.pool, server_id, 100)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(jobs))
}

async fn get_import(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path((server_id, import_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<Json<ImportJob>> {
    require_server_admin_or_owner(&state, server_id, ctx.user_id).await?;

    let job = import_jobs::get_import_job(&state.db.pool, import_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .ok_or_else(|| NexusError::NotFound {
            resource: "import_job".into(),
        })?;

    if job.server_id != server_id {
        return Err(NexusError::NotFound {
            resource: "import_job".into(),
        });
    }

    Ok(Json(job))
}

async fn create_bulk_invite(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    headers: HeaderMap,
    Path(server_id): Path<Uuid>,
    Json(body): Json<BulkInviteRequest>,
) -> NexusResult<Json<BulkInvitation>> {
    require_server_admin_or_owner(&state, server_id, ctx.user_id).await?;

    if body.emails.is_empty() || body.emails.len() > 500 {
        return Err(NexusError::Validation {
            message: "emails must contain 1 to 500 entries".into(),
        });
    }

    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:bulk_invite:tag:{tag}:server:{server_id}"),
        10,
        3600,
    )
    .await?;

    let id = Uuid::new_v4();
    let invite_code = format!("bulk-{}", Uuid::new_v4().simple());
    let emails_json =
        serde_json::to_value(&body.emails).map_err(|e| NexusError::Internal(e.into()))?;
    let total = body.emails.len() as i32;

    let inv = bulk_invitations::create_bulk_invitation(
        &state.db.pool,
        id,
        server_id,
        ctx.user_id,
        &emails_json,
        total,
        Some(invite_code.as_str()),
    )
    .await
    .map_err(|e| NexusError::Internal(e.into()))?;

    Ok(Json(inv))
}

async fn require_server_admin_or_owner(
    state: &AppState,
    server_id: Uuid,
    user_id: Uuid,
) -> NexusResult<()> {
    let server = servers::find_by_id(&state.db.pool, server_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?
        .ok_or_else(|| NexusError::NotFound {
            resource: "server".into(),
        })?;

    if server.owner_id == user_id {
        return Ok(());
    }

    let member: Option<Member> = members::find_member(&state.db.pool, user_id, server_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;
    let Some(member) = member else {
        return Err(NexusError::Forbidden);
    };

    let all_roles = roles::list_server_roles(&state.db.pool, server_id)
        .await
        .map_err(|e| NexusError::Internal(e.into()))?;

    let base = all_roles
        .iter()
        .find(|r| r.is_default)
        .map(|r| Permissions::from_bits_truncate(r.permissions))
        .unwrap_or_else(Permissions::empty);

    let bits = all_roles
        .iter()
        .filter(|r| !r.is_default && member.roles.contains(&r.id))
        .map(|r| Permissions::from_bits_truncate(r.permissions))
        .fold(base, |acc, p| acc | p);

    if bits.contains(Permissions::MANAGE_SERVER) || bits.contains(Permissions::ADMINISTRATOR) {
        Ok(())
    } else {
        Err(NexusError::Forbidden)
    }
}
