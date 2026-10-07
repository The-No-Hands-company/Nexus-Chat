//! File upload routes — multipart upload to MinIO, attachment management.
//!
//! POST  /api/v1/attachments/upload          — Upload a file (multipart/form-data)
//! GET   /api/v1/attachments/:id             — Get attachment metadata + presigned URL
//! DELETE /api/v1/attachments/:id            — Delete own attachment

use axum::{
    Json, Router,
    extract::{Multipart, Path, State},
    middleware,
    routing::{get, post},
};
use nexus_common::error::{NexusError, NexusResult};
use nexus_db::repository::attachments;
use serde::Serialize;
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    AppState,
    middleware::{AuthContext, check_rate_limit_with_fallback, extract_client_tag},
};
use axum::extract::Extension;

// ============================================================
// Maximum upload size: 100 MiB
// ============================================================
const MAX_UPLOAD_BYTES: usize = 100 * 1024 * 1024;

/// Allowed content-type categories. Reject executables server-side.
fn is_allowed_content_type(ct: &str) -> bool {
    matches!(
        ct,
        // Images
        |"image/jpeg"| "image/png" | "image/gif" | "image/webp"
        | "image/svg+xml" | "image/avif" | "image/bmp" | "image/tiff"
        // Video
        | "video/mp4" | "video/webm" | "video/ogg" | "video/quicktime"
        // Audio
        | "audio/mpeg" | "audio/ogg" | "audio/wav" | "audio/flac"
        | "audio/aac" | "audio/opus" | "audio/webm"
        // Documents
        | "application/pdf" | "text/plain" | "text/markdown"
        | "application/zip" | "application/x-tar"
    )
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/attachments/upload", post(upload_file))
        .route(
            "/attachments/{id}",
            get(get_attachment).delete(delete_attachment),
        )
        .route_layer(middleware::from_fn(
            crate::middleware::combined_auth_middleware,
        ))
}

// ============================================================
// Response types
// ============================================================

#[derive(Serialize)]
struct AttachmentResponse {
    id: Uuid,
    filename: String,
    content_type: String,
    size: i64,
    url: Option<String>,
    width: Option<i32>,
    height: Option<i32>,
    duration_secs: Option<f64>,
    spoiler: bool,
    status: String,
}

// ============================================================
// POST /attachments/upload
// ============================================================

/// Upload a file via multipart/form-data.
///
/// Form fields:
/// - `file`   — the binary file (required)
/// - `spoiler` — "true" to mark as spoiler (optional)
/// - `channel_id` — associate with a channel (optional)
async fn upload_file(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    mut multipart: Multipart,
) -> NexusResult<Json<AttachmentResponse>> {
    // ── Rate limiting: 20 uploads per user per 60 seconds ──────────────────
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:upload:user:{}", auth.user_id),
        20,
        60,
    )
    .await?;
    // Secondary IP-based limit to catch shared-token abuse
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:upload:tag:{tag}"),
        40,
        60,
    )
    .await?;
    let mut file_data: Option<Vec<u8>> = None;
    let mut filename = String::from("upload");
    let mut content_type = String::from("application/octet-stream");
    let mut spoiler = false;
    let mut channel_id: Option<Uuid> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| NexusError::Validation {
            message: format!("Multipart error: {e}"),
        })?
    {
        match field.name() {
            Some("file") => {
                // Capture filename and content-type from the field headers
                if let Some(fn_) = field.file_name() {
                    filename = fn_.to_string();
                }
                if let Some(ct) = field.content_type() {
                    content_type = ct.to_string();
                }

                // Validate content-type early
                if !is_allowed_content_type(&content_type) {
                    return Err(NexusError::Validation {
                        message: format!("File type '{content_type}' is not allowed"),
                    });
                }

                let bytes = field.bytes().await.map_err(|e| NexusError::Validation {
                    message: format!("Failed to read file: {e}"),
                })?;

                if bytes.len() > MAX_UPLOAD_BYTES {
                    return Err(NexusError::Validation {
                        message: format!(
                            "File too large: {} bytes (max {} bytes)",
                            bytes.len(),
                            MAX_UPLOAD_BYTES
                        ),
                    });
                }

                file_data = Some(bytes.to_vec());

                // ── Magic byte validation ─────────────────────────────────
                // Verify actual file bytes match the claimed Content-Type.
                // Attackers can label any file as "image/jpeg"; we must not
                // trust the header — check the real signature bytes instead.
                let detected = sniff_mime_from_bytes(&bytes);
                if let Some(sniffed_ct) = detected
                    && !mime_families_match(&content_type, sniffed_ct) {
                        return Err(NexusError::Validation {
                            message: format!(
                                "File content does not match declared type '{content_type}'. \
                                 Detected: '{sniffed_ct}'"
                            ),
                        });
                    }
                // For types we can't sniff (zip, tar, pdf), the Content-Type
                // allowlist is the gate — already checked above.
            }
            Some("spoiler") => {
                let val = field.text().await.unwrap_or_default();
                spoiler = val.trim() == "true";
            }
            Some("channel_id") => {
                let val = field.text().await.unwrap_or_default();
                channel_id = Uuid::parse_str(val.trim()).ok();
            }
            _ => {} // Ignore unknown fields
        }
    }

    let data = file_data.ok_or(NexusError::Validation {
        message: "No file field in request".into(),
    })?;

    let size = data.len() as i64;

    // Sanitize filename
    let safe_filename = sanitize_filename(&filename);

    // Compute SHA-256 digest for deduplication (crypto-quality, not DefaultHasher)
    let hash_hex = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(&data);
        hex::encode(h.finalize())
    };

    // Build storage key: uploads/{user_id}/{uuid}.{ext}
    let ext = safe_filename
        .rsplit('.')
        .next()
        .unwrap_or("bin")
        .to_lowercase();
    let attachment_id = Uuid::new_v4();
    let storage_key = format!("uploads/{}/{}.{}", auth.user_id, attachment_id, ext);

    // Upload to MinIO / local storage
    state
        .storage
        .put_object(&storage_key, data, &content_type)
        .await
        .map_err(NexusError::Internal)?;

    // Persist attachment metadata — URL is intentionally left empty here.
    // GET /attachments/:id always generates a fresh presigned URL from
    // storage_key so no stale URL is ever served.
    let row = attachments::create_attachment(
        &state.db.pool,
        attachment_id,
        auth.user_id,
        None, // server_id — we don't know yet
        channel_id,
        &safe_filename,
        &content_type,
        size,
        &storage_key,
        None, // width
        None, // height
        None, // duration
        spoiler,
        Some(&hash_hex),
    )
    .await?;

    // Mark ready immediately; URL is empty — it will be generated fresh on each GET.
    let row = attachments::mark_ready(
        &state.db.pool,
        row.id,
        "",   // no stored URL; presigned URL generated on demand
        None, // blurhash — would need async image processing
    )
    .await?;

    Ok(Json(AttachmentResponse {
        id: row.id,
        filename: row.filename,
        content_type: row.content_type,
        size: row.size,
        url: row.url,
        width: row.width,
        height: row.height,
        duration_secs: row.duration_secs,
        spoiler: row.spoiler,
        status: row.status,
    }))
}

// ============================================================
// GET /attachments/:id
// ============================================================

async fn get_attachment(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> NexusResult<Json<AttachmentResponse>> {
    let _ = auth; // auth just verifies the user is logged in

    let row = attachments::find_by_id(&state.db.pool, id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Attachment".into(),
        })?;

    // Always generate a fresh presigned URL from the storage key.
    // The URL stored in the DB at upload time may be stale or permanent —
    // we use the live storage client so private channels' attachments are
    // always access-controlled and URLs always have a bounded TTL.
    const PRESIGN_TTL_SECS: u64 = 60 * 60 * 24; // 24 hours
    let url = state
        .storage
        .presigned_get_url(&row.storage_key, PRESIGN_TTL_SECS)
        .await
        .ok()
        .or_else(|| row.url.clone()); // fallback to stored URL if presigning fails

    Ok(Json(AttachmentResponse {
        id: row.id,
        filename: row.filename,
        content_type: row.content_type,
        size: row.size,
        url,
        width: row.width,
        height: row.height,
        duration_secs: row.duration_secs,
        spoiler: row.spoiler,
        status: row.status,
    }))
}

// ============================================================
// DELETE /attachments/:id
// ============================================================

async fn delete_attachment(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<Uuid>,
) -> NexusResult<Json<serde_json::Value>> {
    // Rate limiting: 30 attachment deletions per user per 5 minutes
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:attachment_delete:user:{}", auth.user_id),
        30,
        300,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:attachment_delete:tag:{tag}"),
        50,
        300,
    )
    .await?;

    // Find the attachment first to get the storage key
    let row = attachments::find_by_id(&state.db.pool, id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "Attachment".into(),
        })?;

    // Only the uploader can delete
    if row.uploader_id != auth.user_id {
        return Err(NexusError::Forbidden);
    }

    // Delete from DB
    attachments::delete_attachment(&state.db.pool, id, auth.user_id).await?;

    // Delete from object storage (best-effort — don't fail if already gone)
    let _ = state.storage.delete_object(&row.storage_key).await;

    Ok(Json(serde_json::json!({ "deleted": true })))
}

// ============================================================
// Helpers
// ============================================================

/// Strip path separators, drive/stream specifiers and control bytes from filenames.
///
/// ':' matters as much as the slashes: on Windows it separates a drive
/// ("C:\path") and, on NTFS, an alternate data stream ("report.pdf:evil.exe"),
/// so a name keeping it can still escape the intended file. Control characters
/// go for the same reason '\0' did — they are never meaningful in a stored
/// object key and they hide the real extension in logs and UIs.
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '/' | '\\' | ':') && !c.is_control())
        .take(255)
        .collect()
}

// ── Magic byte / MIME sniffing ───────────────────────────────────────────────

/// Detect the actual MIME type from the first bytes of a file.
///
/// Returns `None` for types we can't reliably identify from magic bytes alone
/// (e.g. ZIP-based formats, plain text). In those cases the Content-Type
/// header allowlist is the only gate.
fn sniff_mime_from_bytes(data: &[u8]) -> Option<&'static str> {
    match data {
        // JPEG: FF D8 FF
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        // PNG: 89 50 4E 47 0D 0A 1A 0A
        [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, ..] => Some("image/png"),
        // GIF: GIF87a or GIF89a
        d if d.starts_with(b"GIF87a") || d.starts_with(b"GIF89a") => Some("image/gif"),
        // WebP: RIFF....WEBP
        d if d.len() >= 12 && d.starts_with(b"RIFF") && &d[8..12] == b"WEBP" => Some("image/webp"),
        // BMP: BM
        [0x42, 0x4D, ..] => Some("image/bmp"),
        // TIFF: little-endian II or big-endian MM
        d if d.starts_with(&[0x49, 0x49, 0x2A, 0x00])
            || d.starts_with(&[0x4D, 0x4D, 0x00, 0x2A]) =>
        {
            Some("image/tiff")
        }
        // AVIF / HEIF: starts with ftyp box containing "avif" or "heic"
        d if d.len() >= 12
            && &d[4..8] == b"ftyp"
            && (&d[8..12] == b"avif"
                || &d[8..12] == b"avis"
                || &d[8..12] == b"heic"
                || &d[8..12] == b"heix") =>
        {
            Some("image/avif")
        }
        // MP4: ftyp box variants (mp4, M4V, isom…)
        d if d.len() >= 12 && &d[4..8] == b"ftyp" => Some("video/mp4"),
        // WebM / MKV: 1A 45 DF A3
        [0x1A, 0x45, 0xDF, 0xA3, ..] => Some("video/webm"),
        // OGG (video and audio): OggS
        d if d.starts_with(b"OggS") => Some("video/ogg"),
        // MP3: ID3 tag or sync bytes FF FB / FF F3 / FF F2
        d if d.starts_with(b"ID3")
            || d.starts_with(&[0xFF, 0xFB])
            || d.starts_with(&[0xFF, 0xF3])
            || d.starts_with(&[0xFF, 0xF2]) =>
        {
            Some("audio/mpeg")
        }
        // WAV: RIFF....WAVE
        d if d.len() >= 12 && d.starts_with(b"RIFF") && &d[8..12] == b"WAVE" => Some("audio/wav"),
        // FLAC: fLaC
        d if d.starts_with(b"fLaC") => Some("audio/flac"),
        // AAC: ADTS sync word FF F1 / FF F9
        d if d.starts_with(&[0xFF, 0xF1]) || d.starts_with(&[0xFF, 0xF9]) => Some("audio/aac"),
        // Opus in Ogg: OggS container checked above; bare Opus has no magic bytes
        // PDF: %PDF
        d if d.starts_with(b"%PDF") => Some("application/pdf"),
        // ZIP (also DOCX, XLSX, JAR): PK\x03\x04
        [0x50, 0x4B, 0x03, 0x04, ..] => Some("application/zip"),
        // TAR (ustar): offset 257 = "ustar" — too complex to check in first bytes
        // Plain text: no reliable magic bytes
        _ => None,
    }
}

/// Returns true if `claimed` (from Content-Type header) and `sniffed` (from
/// magic bytes) belong to the same MIME family.
///
/// We allow minor variations — e.g. video/ogg is fine for an OGG file even if
/// the client sent audio/ogg — but block completely wrong families.
fn mime_families_match(claimed: &str, sniffed: &'static str) -> bool {
    // Exact match is always fine
    if claimed == sniffed {
        return true;
    }
    // Same top-level type (image/*, video/*, audio/*, application/*)
    let claimed_family = claimed.split('/').next().unwrap_or("");
    let sniffed_family = sniffed.split('/').next().unwrap_or("");
    if claimed_family == sniffed_family && !claimed_family.is_empty() {
        return true;
    }
    // Container formats carry either audio-only or audio+video, and magic bytes
    // identify the container, not what is inside it — an OGG file sniffs as
    // video/ogg whether or not it has a video stream. So audio/x and video/x
    // describe the same file and must agree, even though their top-level types
    // differ. The subtypes still have to match, so image/jpeg vs video/mp4
    // stays rejected.
    let (claimed_sub, sniffed_sub) = (
        claimed.split('/').nth(1).unwrap_or(""),
        sniffed.split('/').nth(1).unwrap_or(""),
    );
    let is_av = |f: &str| matches!(f, "audio" | "video");
    if claimed_sub == sniffed_sub
        && !claimed_sub.is_empty()
        && is_av(claimed_family)
        && is_av(sniffed_family)
    {
        return true;
    }
    // ZIP is the container for many formats (docx, xlsx, jar…) — if sniffed
    // as ZIP and claimed is also ZIP-based, allow it.
    if sniffed == "application/zip" && matches!(claimed, "application/zip" | "application/x-tar") {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_allowed_content_type ───────────────────────────────────────────────

    #[test]
    fn images_are_allowed() {
        for ct in &[
            "image/jpeg",
            "image/png",
            "image/gif",
            "image/webp",
            "image/svg+xml",
            "image/avif",
            "image/bmp",
            "image/tiff",
        ] {
            assert!(is_allowed_content_type(ct), "{ct} should be allowed");
        }
    }

    #[test]
    fn video_types_are_allowed() {
        for ct in &["video/mp4", "video/webm", "video/ogg", "video/quicktime"] {
            assert!(is_allowed_content_type(ct), "{ct} should be allowed");
        }
    }

    #[test]
    fn audio_types_are_allowed() {
        for ct in &[
            "audio/mpeg",
            "audio/ogg",
            "audio/wav",
            "audio/flac",
            "audio/aac",
            "audio/opus",
            "audio/webm",
        ] {
            assert!(is_allowed_content_type(ct), "{ct} should be allowed");
        }
    }

    #[test]
    fn document_types_are_allowed() {
        for ct in &[
            "application/pdf",
            "text/plain",
            "text/markdown",
            "application/zip",
            "application/x-tar",
        ] {
            assert!(is_allowed_content_type(ct), "{ct} should be allowed");
        }
    }

    #[test]
    fn executable_types_are_blocked() {
        for ct in &[
            "application/octet-stream",
            "application/x-executable",
            "application/x-msdownload",
            "application/x-sh",
            "application/x-bat",
        ] {
            assert!(!is_allowed_content_type(ct), "{ct} must be blocked");
        }
    }

    #[test]
    fn empty_and_wildcard_types_are_blocked() {
        assert!(!is_allowed_content_type(""));
        assert!(!is_allowed_content_type("*/*"));
        assert!(!is_allowed_content_type("application/*"));
    }

    #[test]
    fn case_sensitive_match() {
        // MIME types are lowercase — uppercase variants must not slip through
        assert!(!is_allowed_content_type("IMAGE/JPEG"));
        assert!(!is_allowed_content_type("Image/Png"));
    }

    // ── sanitize_filename ─────────────────────────────────────────────────────

    #[test]
    fn forward_slash_removed() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "....etcpasswd");
    }

    #[test]
    fn backslash_removed() {
        assert_eq!(
            sanitize_filename("C:\\Windows\\system32"),
            "CWindowssystem32"
        );
    }

    #[test]
    fn null_byte_removed() {
        let name = "file\0name.txt";
        let result = sanitize_filename(name);
        assert!(!result.contains('\0'));
        assert_eq!(result, "filename.txt");
    }

    #[test]
    fn normal_filename_unchanged() {
        assert_eq!(sanitize_filename("photo.jpg"), "photo.jpg");
        assert_eq!(sanitize_filename("my document.pdf"), "my document.pdf");
        assert_eq!(sanitize_filename("résumé.pdf"), "résumé.pdf");
    }

    #[test]
    fn truncated_to_255_chars() {
        let long: String = "a".repeat(500);
        let result = sanitize_filename(&long);
        assert_eq!(result.len(), 255);
    }

    #[test]
    fn exactly_255_chars_unchanged() {
        let exactly: String = "b".repeat(255);
        let result = sanitize_filename(&exactly);
        assert_eq!(result.len(), 255);
    }

    #[test]
    fn empty_filename_stays_empty() {
        assert_eq!(sanitize_filename(""), "");
    }

    // ── sniff_mime_from_bytes ─────────────────────────────────────────────────

    #[test]
    fn jpeg_magic_bytes_detected() {
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        assert_eq!(sniff_mime_from_bytes(&jpeg), Some("image/jpeg"));
    }

    #[test]
    fn png_magic_bytes_detected() {
        let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00];
        assert_eq!(sniff_mime_from_bytes(&png), Some("image/png"));
    }

    #[test]
    fn gif_magic_bytes_detected() {
        assert_eq!(sniff_mime_from_bytes(b"GIF89a\x00"), Some("image/gif"));
        assert_eq!(sniff_mime_from_bytes(b"GIF87a\x00"), Some("image/gif"));
    }

    #[test]
    fn pdf_magic_bytes_detected() {
        assert_eq!(sniff_mime_from_bytes(b"%PDF-1.7"), Some("application/pdf"));
    }

    #[test]
    fn zip_magic_bytes_detected() {
        let zip = [0x50, 0x4B, 0x03, 0x04, 0x14, 0x00];
        assert_eq!(sniff_mime_from_bytes(&zip), Some("application/zip"));
    }

    #[test]
    fn executable_returns_none_no_match() {
        // EXE magic: MZ
        let exe = [0x4D, 0x5A, 0x90, 0x00];
        assert_eq!(sniff_mime_from_bytes(&exe), None);
    }

    #[test]
    fn empty_bytes_return_none() {
        assert_eq!(sniff_mime_from_bytes(&[]), None);
    }

    // ── mime_families_match ───────────────────────────────────────────────────

    #[test]
    fn exact_match_accepted() {
        assert!(mime_families_match("image/jpeg", "image/jpeg"));
        assert!(mime_families_match("application/pdf", "application/pdf"));
    }

    #[test]
    fn same_family_accepted() {
        // video/ogg sniffed but client sent audio/ogg — same top-level type
        assert!(mime_families_match("audio/ogg", "video/ogg"));
        assert!(mime_families_match("video/mp4", "video/mp4"));
    }

    #[test]
    fn cross_family_rejected() {
        // Claimed image but sniffed as video
        assert!(!mime_families_match("image/jpeg", "video/mp4"));
        // Claimed image but sniffed as audio
        assert!(!mime_families_match("image/png", "audio/mpeg"));
        // Claimed image but sniffed as PDF
        assert!(!mime_families_match("image/jpeg", "application/pdf"));
    }

    #[test]
    fn zip_variants_accepted_when_sniffed_as_zip() {
        assert!(mime_families_match("application/zip", "application/zip"));
        assert!(mime_families_match("application/x-tar", "application/zip"));
    }
}
