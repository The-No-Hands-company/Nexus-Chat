//! Device & key management routes — register devices, upload key material.
//!
//! POST   /devices                           — Register a new device + upload initial keys
//! GET    /devices                           — List my devices
//! GET    /devices/:device_id                — Get device info
//! DELETE /devices/:device_id               — Revoke a device
//! POST   /devices/:device_id/signed-pre-key — Rotate signed pre-key
//! POST   /devices/:device_id/one-time-pre-keys — Upload more OTPks
//! GET    /devices/:device_id/one-time-pre-keys/count — Remaining OTPk count
//! GET    /users/:user_id/key-bundle         — Fetch key bundles for all devices (X3DH initiator)
//! GET    /users/:user_id/devices/:device_id/key-bundle — Fetch bundle for one device

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    middleware,
    routing::{get, post},
};
use nexus_common::{
    crypto::{validate_x25519_key, verify_signed_pre_key},
    error::{NexusError, NexusResult},
    gateway_event::GatewayEvent,
    models::crypto::{
        Device, E2eeSession, KeyBundle, OtpkCountResponse, RegisterDeviceRequest,
        RotateSignedPreKeyRequest, UpdateSessionRequest, UploadOtpkRequest,
    },
};
use nexus_db::repository::keystore;
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    AppState,
    middleware::{AuthContext, check_rate_limit_with_fallback, extract_client_tag},
};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // Own device management
        .route("/devices", post(register_device).get(list_my_devices))
        .route(
            "/devices/{device_id}",
            get(get_device).delete(delete_device),
        )
        .route(
            "/devices/{device_id}/signed-pre-key",
            post(rotate_signed_pre_key),
        )
        .route(
            "/devices/{device_id}/one-time-pre-keys",
            post(upload_one_time_pre_keys),
        )
        .route(
            "/devices/{device_id}/one-time-pre-keys/count",
            get(count_one_time_pre_keys),
        )
        // Session management (double-ratchet state persistence + resumption)
        .route("/devices/{device_id}/sessions", get(list_sessions))
        .route(
            "/devices/{device_id}/sessions/{remote_device_id}",
            get(get_session).put(put_session).delete(delete_session),
        )
        // Key bundles (for X3DH initiators)
        .route("/users/{user_id}/key-bundle", get(get_all_key_bundles))
        .route(
            "/users/{user_id}/devices/{device_id}/key-bundle",
            get(get_device_key_bundle),
        )
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ))
}

// ============================================================
// POST /devices — Register a new device
// ============================================================

async fn register_device(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<RegisterDeviceRequest>,
) -> NexusResult<Json<Device>> {
    // Rate limiting: 5 device registrations per day per user
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:e2ee:device:{}", auth.user_id),
        5,
        86400,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:e2ee:device:tag:{tag}"),
        10,
        86400,
    )
    .await?;
    // Verify the key material before persisting.
    //
    // This checks the signature, not just its shape. The signed pre-key
    // signature is the only thing binding this pre-key to this identity: a
    // peer starting an X3DH session trusts the bundle we hand it, and cannot
    // check the binding itself at first contact. Accepting an unverified
    // signature meant serving pre-keys that the identity key never authorised.
    verify_signed_pre_key(
        &body.identity_key,
        &body.signed_pre_key,
        &body.signed_pre_key_sig,
    )
    .map_err(|e| NexusError::Validation {
        message: format!("signed pre-key: {e}"),
    })?;

    let device_type_str = body
        .device_type
        .map(|t| {
            serde_json::to_value(t)
                .unwrap_or_default()
                .as_str()
                .unwrap_or("unknown")
                .to_owned()
        })
        .unwrap_or_else(|| "unknown".into());

    let device = keystore::create_device(
        &state.db.pool,
        auth.user_id,
        &body.name,
        &device_type_str,
        &body.identity_key,
        &body.signed_pre_key,
        &body.signed_pre_key_sig,
        body.signed_pre_key_id,
    )
    .await
    .map_err(NexusError::Internal)?;

    // Upload initial one-time pre-keys
    if !body.one_time_pre_keys.is_empty() {
        let pairs: Vec<(i32, String)> = body
            .one_time_pre_keys
            .iter()
            .map(|k| (k.key_id, k.public_key.clone()))
            .collect();
        keystore::insert_one_time_pre_keys(&state.db.pool, device.id, &pairs)
            .await
            .map_err(NexusError::Internal)?;
    }

    Ok(Json(device))
}

// ============================================================
// GET /devices
// ============================================================

async fn list_my_devices(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> NexusResult<Json<Vec<Device>>> {
    let devices = keystore::list_devices(&state.db.pool, auth.user_id)
        .await
        .map_err(NexusError::Internal)?;
    Ok(Json(devices))
}

// ============================================================
// GET /devices/:device_id
// ============================================================

async fn get_device(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<Uuid>,
) -> NexusResult<Json<Device>> {
    let device = keystore::find_device(&state.db.pool, device_id)
        .await
        .map_err(NexusError::Internal)?
        .ok_or(NexusError::NotFound {
            resource: "Device".into(),
        })?;

    // Allow self-access or any user to see another's public key info
    // (key bundles are public — that's how E2EE works)
    let _ = auth;

    Ok(Json(device))
}

// ============================================================
// DELETE /devices/:device_id
// ============================================================

async fn delete_device(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<Uuid>,
) -> NexusResult<()> {
    let device = keystore::find_device(&state.db.pool, device_id)
        .await
        .map_err(NexusError::Internal)?
        .ok_or(NexusError::NotFound {
            resource: "Device".into(),
        })?;

    if device.user_id != auth.user_id {
        return Err(NexusError::Forbidden);
    }

    keystore::delete_device(&state.db.pool, device_id)
        .await
        .map_err(NexusError::Internal)?;

    Ok(())
}

// ============================================================
// POST /devices/:device_id/signed-pre-key — Rotate signed pre-key
// ============================================================

async fn rotate_signed_pre_key(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<Uuid>,
    Json(body): Json<RotateSignedPreKeyRequest>,
) -> NexusResult<()> {
    let device = keystore::find_device(&state.db.pool, device_id)
        .await
        .map_err(NexusError::Internal)?
        .ok_or(NexusError::NotFound {
            resource: "Device".into(),
        })?;

    if device.user_id != auth.user_id {
        return Err(NexusError::Forbidden);
    }

    // A rotation that re-submits the key already in place is not a rotation.
    // It is the shape of compliance without the substance: the timestamp moves,
    // any freshness check downstream is satisfied, and the key an attacker may
    // already hold stays live. Rejecting it means the recorded rotation time
    // always refers to a key that genuinely changed.
    if body.signed_pre_key == device.signed_pre_key {
        return Err(NexusError::Validation {
            message: "signed_pre_key is unchanged — rotation must supply a new key".into(),
        });
    }

    // Verified against the identity key stored at registration, not one sent
    // with this request — otherwise anyone who could reach this endpoint could
    // present a fresh identity alongside a matching signature and rotate the
    // device onto a pre-key they control. Binding to the stored key means only
    // the holder of the original identity private key can rotate.
    verify_signed_pre_key(
        &device.identity_key,
        &body.signed_pre_key,
        &body.signed_pre_key_sig,
    )
    .map_err(|e| NexusError::Validation {
        message: format!("signed pre-key: {e}"),
    })?;

    keystore::rotate_signed_pre_key(
        &state.db.pool,
        device_id,
        &body.signed_pre_key,
        &body.signed_pre_key_sig,
        body.signed_pre_key_id,
    )
    .await
    .map_err(NexusError::Internal)?;

    Ok(())
}

// ============================================================
// POST /devices/:device_id/one-time-pre-keys — Upload OTPks
// ============================================================

async fn upload_one_time_pre_keys(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<Uuid>,
    Json(body): Json<UploadOtpkRequest>,
) -> NexusResult<Json<OtpkCountResponse>> {
    let device = keystore::find_device(&state.db.pool, device_id)
        .await
        .map_err(NexusError::Internal)?
        .ok_or(NexusError::NotFound {
            resource: "Device".into(),
        })?;

    if device.user_id != auth.user_id {
        return Err(NexusError::Forbidden);
    }

    if body.keys.len() > 1000 {
        return Err(NexusError::Validation {
            message: "Cannot upload more than 1000 one-time pre-keys at once".into(),
        });
    }

    // One-time pre-keys were stored with no checking at all, so any string
    // reached a peer's key bundle. They are unsigned by design — X3DH does not
    // sign them, so the server cannot prove who made one — but it can insist
    // they are structurally X25519 keys. Serving 32 bytes of nonsense to a peer
    // fails a session setup in a way that looks like the peer's fault.
    for k in &body.keys {
        validate_x25519_key(&k.public_key, "one_time_pre_key").map_err(|e| {
            NexusError::Validation {
                message: format!("one_time_pre_key {}: {e}", k.key_id),
            }
        })?;
    }

    let pairs: Vec<(i32, String)> = body
        .keys
        .iter()
        .map(|k| (k.key_id, k.public_key.clone()))
        .collect();
    keystore::insert_one_time_pre_keys(&state.db.pool, device_id, &pairs)
        .await
        .map_err(NexusError::Internal)?;

    let remaining = keystore::count_one_time_pre_keys(&state.db.pool, device_id)
        .await
        .map_err(NexusError::Internal)?;

    Ok(Json(OtpkCountResponse {
        device_id,
        remaining,
    }))
}

// ============================================================
// GET /devices/:device_id/one-time-pre-keys/count
// ============================================================

async fn count_one_time_pre_keys(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<Uuid>,
) -> NexusResult<Json<OtpkCountResponse>> {
    let device = keystore::find_device(&state.db.pool, device_id)
        .await
        .map_err(NexusError::Internal)?
        .ok_or(NexusError::NotFound {
            resource: "Device".into(),
        })?;

    if device.user_id != auth.user_id {
        return Err(NexusError::Forbidden);
    }

    let remaining = keystore::count_one_time_pre_keys(&state.db.pool, device_id)
        .await
        .map_err(NexusError::Internal)?;

    Ok(Json(OtpkCountResponse {
        device_id,
        remaining,
    }))
}

// ============================================================
// Session management — double-ratchet state persistence
//
// The server stores each device's ratchet state as an opaque blob that the
// device encrypts locally (AES-256-GCM).  The server never reads it —
// it only provides safe storage so clients can resume sessions on reinstall
// or synchronise state across linked devices.
// ============================================================

/// GET /devices/:device_id/sessions — list all persisted sessions for this device.
async fn list_sessions(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<Uuid>,
) -> NexusResult<Json<Vec<E2eeSession>>> {
    // Caller must own the device.
    let device = keystore::find_device(&state.db.pool, device_id)
        .await
        .map_err(NexusError::Internal)?
        .ok_or(NexusError::NotFound {
            resource: "Device".into(),
        })?;
    if device.user_id != auth.user_id {
        return Err(NexusError::Forbidden);
    }
    keystore::touch_device(&state.db.pool, device_id)
        .await
        .map_err(NexusError::Internal)?;
    let sessions = keystore::list_sessions(&state.db.pool, device_id)
        .await
        .map_err(NexusError::Internal)?;
    Ok(Json(sessions))
}

/// GET /devices/:device_id/sessions/:remote_device_id — fetch session state for resumption.
async fn get_session(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((device_id, remote_device_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<Json<E2eeSession>> {
    let device = keystore::find_device(&state.db.pool, device_id)
        .await
        .map_err(NexusError::Internal)?
        .ok_or(NexusError::NotFound {
            resource: "Device".into(),
        })?;
    if device.user_id != auth.user_id {
        return Err(NexusError::Forbidden);
    }
    let session = keystore::get_session(&state.db.pool, device_id, remote_device_id)
        .await
        .map_err(NexusError::Internal)?
        .ok_or(NexusError::NotFound {
            resource: "Session".into(),
        })?;
    Ok(Json(session))
}

/// PUT /devices/:device_id/sessions/:remote_device_id — persist ratchet state after
/// each send or receive step.
///
/// Body: `{ "session_state": "<base64-encrypted-blob>", "ratchet_step": N }`
///
/// `session_state` is the client's ratchet state serialised and encrypted with its
/// local storage key before upload.  The server stores but never inspects it.
/// `ratchet_step` should be the client's local monotonic step counter.
async fn put_session(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((device_id, remote_device_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<UpdateSessionRequest>,
) -> NexusResult<Json<E2eeSession>> {
    let device = keystore::find_device(&state.db.pool, device_id)
        .await
        .map_err(NexusError::Internal)?
        .ok_or(NexusError::NotFound {
            resource: "Device".into(),
        })?;
    if device.user_id != auth.user_id {
        return Err(NexusError::Forbidden);
    }
    if body.session_state.is_empty() {
        return Err(NexusError::Validation {
            message: "session_state must not be empty".into(),
        });
    }
    if body.ratchet_step < 0 {
        return Err(NexusError::Validation {
            message: "ratchet_step must be non-negative".into(),
        });
    }
    // Verify remote device exists (prevents storing state for phantom devices).
    keystore::find_device(&state.db.pool, remote_device_id)
        .await
        .map_err(NexusError::Internal)?
        .ok_or(NexusError::NotFound {
            resource: "RemoteDevice".into(),
        })?;
    let session = keystore::upsert_session(
        &state.db.pool,
        device_id,
        remote_device_id,
        &body.session_state,
        body.ratchet_step,
    )
    .await
    .map_err(NexusError::Internal)?;
    Ok(Json(session))
}

/// DELETE /devices/:device_id/sessions/:remote_device_id — tear down a session
/// (e.g. after a safety-number change or explicit session reset).
async fn delete_session(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((device_id, remote_device_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<()> {
    let device = keystore::find_device(&state.db.pool, device_id)
        .await
        .map_err(NexusError::Internal)?
        .ok_or(NexusError::NotFound {
            resource: "Device".into(),
        })?;
    if device.user_id != auth.user_id {
        return Err(NexusError::Forbidden);
    }
    keystore::delete_session(&state.db.pool, device_id, remote_device_id)
        .await
        .map_err(NexusError::Internal)?;
    Ok(())
}

// ============================================================
// GET /users/:user_id/key-bundle — All bundles for a user's devices
// ============================================================

/// Remaining OTPKs below which we fire a LOW_PREKEYS gateway event.
/// Signal clients typically replenish when stock drops to ≤ 5.
const LOW_PREKEYS_THRESHOLD: i64 = 5;

async fn get_all_key_bundles(
    Extension(_auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(user_id): Path<Uuid>,
) -> NexusResult<Json<Vec<KeyBundle>>> {
    let bundles = keystore::get_all_key_bundles(&state.db.pool, user_id)
        .await
        .map_err(NexusError::Internal)?;

    // After atomically consuming OTPKs, check remaining stock per device and
    // fire a LOW_PREKEYS gateway event if below threshold.  The owner can then
    // proactively upload more OTPKs before they are exhausted.  We ignore
    // errors here — key replenishment notifications are best-effort.
    for bundle in &bundles {
        if bundle.one_time_pre_key.is_some() {
            let remaining = keystore::count_one_time_pre_keys(&state.db.pool, bundle.device_id)
                .await
                .unwrap_or(i64::MAX);
            if remaining < LOW_PREKEYS_THRESHOLD {
                let _ = state.gateway_tx.send(GatewayEvent {
                    event_type: "LOW_PREKEYS".into(),
                    data: serde_json::json!({
                        "device_id": bundle.device_id,
                        "remaining": remaining,
                        "threshold": LOW_PREKEYS_THRESHOLD,
                    }),
                    server_id: None,
                    channel_id: None,
                    user_id: Some(bundle.user_id),
                });
            }
        }
    }

    Ok(Json(bundles))
}

// ============================================================
// GET /users/:user_id/devices/:device_id/key-bundle
// ============================================================

async fn get_device_key_bundle(
    Extension(_auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path((_user_id, device_id)): Path<(Uuid, Uuid)>,
) -> NexusResult<Json<KeyBundle>> {
    let bundle = keystore::get_key_bundle(&state.db.pool, device_id)
        .await
        .map_err(NexusError::Internal)?
        .ok_or(NexusError::NotFound {
            resource: "Device".into(),
        })?;

    // If an OTPK was consumed, notify the owner if remaining stock is low.
    if bundle.one_time_pre_key.is_some() {
        let remaining = keystore::count_one_time_pre_keys(&state.db.pool, device_id)
            .await
            .unwrap_or(i64::MAX);
        if remaining < LOW_PREKEYS_THRESHOLD {
            let _ = state.gateway_tx.send(GatewayEvent {
                event_type: "LOW_PREKEYS".into(),
                data: serde_json::json!({
                    "device_id": device_id,
                    "remaining": remaining,
                    "threshold": LOW_PREKEYS_THRESHOLD,
                }),
                server_id: None,
                channel_id: None,
                user_id: Some(bundle.user_id),
            });
        }
    }

    Ok(Json(bundle))
}
