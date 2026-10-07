//! Object storage client — supports S3/MinIO (full mode) and local filesystem (lite mode).

use anyhow::{Context, Result};
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::{
    Client,
    config::{Builder as S3Builder, Credentials, Region},
    primitives::ByteStream,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Percent-decode a URL-encoded path key, converting sequences like %2E or %2F
/// back to their ASCII equivalents before path traversal checks.
fn percent_decode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8 as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Storage configuration (loaded from app config).
#[derive(Debug, Clone)]
pub struct StorageConfig {
    /// MinIO / S3 endpoint URL (e.g. `http://localhost:9000`). Empty = local mode.
    pub endpoint: String,
    /// Access key
    pub access_key: String,
    /// Secret key
    pub secret_key: String,
    /// Bucket name
    pub bucket: String,
    /// Region (use `us-east-1` for MinIO)
    pub region: String,
    /// Public CDN base URL for direct asset links (optional).
    pub public_url: Option<String>,
}

// ── Backend ───────────────────────────────────────────────────────────────────

enum StorageBackend {
    S3(
        Client,
        String,         /* bucket */
        Option<String>, /* public_url */
    ),
    Local(PathBuf /* data_dir */, String /* public_base */),
}

/// Unified storage client — S3/MinIO or local filesystem.
#[derive(Clone)]
pub struct StorageClient {
    inner: Arc<StorageBackend>,
}

impl StorageClient {
    /// Initialise an S3/MinIO client.
    pub fn new(cfg: &StorageConfig) -> Result<Self> {
        let creds = Credentials::new(
            &cfg.access_key,
            &cfg.secret_key,
            None,
            None,
            "nexus-storage",
        );
        let s3_cfg = S3Builder::new()
            .endpoint_url(&cfg.endpoint)
            .credentials_provider(creds)
            .region(Region::new(cfg.region.clone()))
            .force_path_style(true)
            .build();

        Ok(Self {
            inner: Arc::new(StorageBackend::S3(
                Client::from_conf(s3_cfg),
                cfg.bucket.clone(),
                cfg.public_url.clone(),
            )),
        })
    }

    /// Initialise a local-filesystem client (lite mode).
    ///
    /// `data_dir`    — directory where uploaded files are written  
    /// `public_base` — HTTP base URL served by the API  (e.g. `http://localhost:8080/files`)
    pub fn new_local(data_dir: impl Into<PathBuf>, public_base: impl Into<String>) -> Result<Self> {
        let dir: PathBuf = data_dir.into();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Cannot create storage dir: {}", dir.display()))?;
        Ok(Self {
            inner: Arc::new(StorageBackend::Local(dir, public_base.into())),
        })
    }

    // ── Core upload ───────────────────────────────────────────────────────────

    pub async fn put_object(&self, key: &str, data: Vec<u8>, content_type: &str) -> Result<String> {
        match self.inner.as_ref() {
            StorageBackend::S3(client, bucket, _) => {
                client
                    .put_object()
                    .bucket(bucket)
                    .key(key)
                    .content_type(content_type)
                    .body(ByteStream::from(data))
                    .send()
                    .await
                    .with_context(|| format!("S3: failed to upload {key}"))?;
                Ok(key.to_string())
            }
            StorageBackend::Local(dir, _) => {
                let dest = dir.join(key.replace('/', std::path::MAIN_SEPARATOR_STR));
                if let Some(parent) = dest.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(&dest, &data)
                    .await
                    .with_context(|| format!("Local: failed to write {key}"))?;
                Ok(key.to_string())
            }
        }
    }

    // ── URL generation ────────────────────────────────────────────────────────

    pub async fn presigned_get_url(&self, key: &str, expiry_secs: u64) -> Result<String> {
        match self.inner.as_ref() {
            StorageBackend::S3(client, bucket, public_url) => {
                if let Some(base) = public_url {
                    return Ok(format!("{}/{}/{}", base.trim_end_matches('/'), bucket, key));
                }
                let cfg = PresigningConfig::expires_in(Duration::from_secs(expiry_secs))
                    .context("Failed to build presigning config")?;
                let req = client
                    .get_object()
                    .bucket(bucket)
                    .key(key)
                    .presigned(cfg)
                    .await
                    .with_context(|| format!("S3: failed to presign {key}"))?;
                Ok(req.uri().to_string())
            }
            StorageBackend::Local(_, base) => Ok(format!("{}/{}", base.trim_end_matches('/'), key)),
        }
    }

    pub fn public_url(&self, key: &str) -> Option<String> {
        match self.inner.as_ref() {
            StorageBackend::S3(_, bucket, Some(base)) => {
                Some(format!("{}/{}/{}", base.trim_end_matches('/'), bucket, key))
            }
            StorageBackend::S3(_, _, None) => None,
            StorageBackend::Local(_, base) => {
                Some(format!("{}/{}", base.trim_end_matches('/'), key))
            }
        }
    }

    // ── Deletion ──────────────────────────────────────────────────────────────

    pub async fn delete_object(&self, key: &str) -> Result<()> {
        match self.inner.as_ref() {
            StorageBackend::S3(client, bucket, _) => {
                client
                    .delete_object()
                    .bucket(bucket)
                    .key(key)
                    .send()
                    .await
                    .with_context(|| format!("S3: failed to delete {key}"))?;
                Ok(())
            }
            StorageBackend::Local(dir, _) => {
                let path = dir.join(key.replace('/', std::path::MAIN_SEPARATOR_STR));
                if path.exists() {
                    tokio::fs::remove_file(&path)
                        .await
                        .with_context(|| format!("Local: failed to delete {key}"))?;
                }
                Ok(())
            }
        }
    }

    /// Read a file from local storage and return its bytes + content-type.
    /// Returns `Ok(None)` for files that don't exist, `Ok(None)` for S3 backends
    /// (caller should redirect to presigned URL instead).
    pub async fn read_local_file(&self, key: &str) -> Result<Option<(Vec<u8>, String)>> {
        match self.inner.as_ref() {
            StorageBackend::S3(_, _, _) => Ok(None),
            StorageBackend::Local(dir, _) => {
                // ── Path traversal guard ──────────────────────────────────────
                // Strip leading slashes and reject any key that contains path
                // components that could escape the data directory:
                //   - "../" and ".."  (Unix traversal)
                //   - "..\" and null bytes (Windows / exotic)
                //   - URL-encoded sequences (%2e%2e, %2f, etc.) — decode first
                let decoded = percent_decode(key);
                let safe_key = decoded.trim_start_matches('/');

                if safe_key.contains('\0')
                    || safe_key.split('/').any(|seg| seg == ".." || seg == ".")
                    || safe_key.contains('\\')
                {
                    tracing::warn!("Path traversal attempt blocked");
                    return Ok(None);
                }

                let candidate = dir.join(safe_key.replace('/', std::path::MAIN_SEPARATOR_STR));

                // Canonicalize both paths and verify the candidate is a
                // descendant of the data directory. This catches symlink escapes
                // and any traversal sequences that slipped through the above.
                let canon_dir = match dir.canonicalize() {
                    Ok(p) => p,
                    Err(_) => return Ok(None),
                };
                let canon_candidate = match candidate.canonicalize() {
                    Ok(p) => p,
                    Err(_) => return Ok(None), // file doesn't exist
                };
                if !canon_candidate.starts_with(&canon_dir) {
                    tracing::error!("Path traversal: resolved path escapes data directory");
                    return Ok(None);
                }

                let bytes = tokio::fs::read(&canon_candidate)
                    .await
                    .with_context(|| format!("Local: failed to read {key}"))?;
                let ct = mime_guess::from_path(&canon_candidate)
                    .first_raw()
                    .unwrap_or("application/octet-stream")
                    .to_owned();
                Ok(Some((bytes, ct)))
            }
        }
    }

    pub async fn upload_file(
        &self,
        key: &str,
        path: &std::path::Path,
        content_type: &str,
    ) -> Result<String> {
        let data = tokio::fs::read(path)
            .await
            .with_context(|| format!("Failed to read {}", path.display()))?;
        self.put_object(key, data, content_type).await
    }

    // ── Bucket bootstrap ──────────────────────────────────────────────────────

    pub async fn ensure_bucket(&self) -> Result<()> {
        match self.inner.as_ref() {
            StorageBackend::S3(client, bucket, _) => {
                if client.head_bucket().bucket(bucket).send().await.is_err() {
                    tracing::info!(bucket = %bucket, "Creating S3 bucket");
                    client
                        .create_bucket()
                        .bucket(bucket)
                        .send()
                        .await
                        .context("Failed to create S3 bucket")?;
                }
                Ok(())
            }
            StorageBackend::Local(dir, _) => {
                tokio::fs::create_dir_all(dir)
                    .await
                    .with_context(|| format!("Failed to create data dir: {}", dir.display()))?;
                Ok(())
            }
        }
    }
}
