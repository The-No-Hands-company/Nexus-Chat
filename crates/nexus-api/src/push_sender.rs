//! Web Push notification sender using VAPID (RFC 8292 / RFC 8030).
//!
//! We implement VAPID signing manually using `p256` + `jsonwebtoken` rather
//! than pulling in a heavy web-push crate. The P-256 ECDH key pair is either
//! loaded from `NEXUS__PUSH__VAPID_PRIVATE_KEY` (base64url-encoded 32-byte
//! scalar) or generated on first run and saved to `nexus.toml`.
//!
//! # How it works
//!
//! 1. On startup, `PushSender::init()` loads or generates the VAPID key pair.
//! 2. When a notifiable event fires (new @mention, DM), the API calls
//!    `PushSender::notify_user()`.
//! 3. `notify_user()` fetches all push subscriptions for the user and sends a
//!    VAPID-authenticated POST to each endpoint's push service (Google FCM,
//!    Mozilla Autopush, etc.).
//! 4. Endpoints that return 410 Gone are removed (user unsubscribed externally).

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sqlx::AnyPool;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Payload sent in the push notification body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushPayload {
    /// Short notification title (e.g. "alice mentioned you").
    pub title: String,
    /// Longer body text (e.g. the message snippet).
    pub body: String,
    /// Icon URL (optional — relative path served by the web client).
    pub icon: Option<String>,
    /// Deep-link URL to open when the notification is clicked.
    pub url: Option<String>,
    /// The channel or DM the event occurred in.
    pub channel_id: Option<Uuid>,
}

/// Lightweight VAPID push sender.
///
/// Clone-cheap — the inner `Client` and key material are `Arc`-wrapped
/// by the `reqwest::Client`.
#[derive(Clone)]
pub struct PushSender {
    /// VAPID private key scalar bytes (32 bytes, P-256).
    private_key_bytes: Vec<u8>,
    /// Base64url-encoded uncompressed public key (65 bytes) for the
    /// `Authorization: vapid` header and the VAPID public-key endpoint.
    pub public_key_b64: String,
    /// Subject claim for the VAPID JWT (mailto: or https: URI).
    subject: String,
    http: Client,
}

impl PushSender {
    /// Create a `PushSender` from a raw 32-byte P-256 private key scalar.
    ///
    /// `private_key_b64url` — base64url-encoded 32-byte scalar.
    /// `subject`            — `mailto:admin@example.com` or `https://example.com`
    pub fn from_private_key(private_key_b64url: &str, subject: &str) -> anyhow::Result<Self> {
        let private_key_bytes = B64
            .decode(private_key_b64url.trim())
            .map_err(|e| anyhow::anyhow!("Invalid VAPID private key base64: {e}"))?;

        if private_key_bytes.len() != 32 {
            anyhow::bail!(
                "VAPID private key must be 32 bytes, got {}",
                private_key_bytes.len()
            );
        }

        // Derive the public key using the p256 crate
        use p256::SecretKey;
        let secret = SecretKey::from_slice(&private_key_bytes)
            .map_err(|e| anyhow::anyhow!("Invalid P-256 scalar: {e}"))?;
        let public_key = secret.public_key();

        // Uncompressed point: 0x04 || X (32 bytes) || Y (32 bytes) = 65 bytes
        use p256::elliptic_curve::sec1::ToEncodedPoint;
        let uncompressed = public_key.to_encoded_point(false);
        let public_key_b64 = B64.encode(uncompressed.as_bytes());

        Ok(Self {
            private_key_bytes,
            public_key_b64,
            subject: subject.to_string(),
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()?,
        })
    }

    /// Generate a new random P-256 key pair and return the private key as
    /// base64url. Store this value in `NEXUS__PUSH__VAPID_PRIVATE_KEY`.
    pub fn generate_private_key() -> String {
        use p256::SecretKey;
        use rand_core::OsRng;
        let key = SecretKey::random(&mut OsRng);
        B64.encode(key.to_bytes().as_slice())
    }

    /// Send a push notification to every registered subscription of `user_id`.
    ///
    /// Subscriptions that return HTTP 410 Gone are removed automatically.
    pub async fn notify_user(
        &self,
        pool: &AnyPool,
        user_id: Uuid,
        payload: &PushPayload,
    ) -> anyhow::Result<()> {
        let subs = nexus_db::repository::push_subscriptions::list_for_user(pool, user_id).await?;

        if subs.is_empty() {
            return Ok(());
        }

        let body_json = serde_json::to_vec(payload)?;
        let mut gone_endpoints: Vec<String> = Vec::new();

        for sub in &subs {
            match self
                .send_one(&sub.endpoint, &sub.p256dh, &sub.auth, &body_json)
                .await
            {
                Ok(()) => {
                    let _ =
                        nexus_db::repository::push_subscriptions::touch(pool, &sub.endpoint).await;
                }
                Err(PushError::Gone) => {
                    gone_endpoints.push(sub.endpoint.clone());
                }
                Err(e) => {
                    tracing::warn!(error_kind = %nexus_common::logsafe::err_kind(&e), "Push notification delivery failed");
                }
            }
        }

        if !gone_endpoints.is_empty() {
            let _ =
                nexus_db::repository::push_subscriptions::delete_gone(pool, &gone_endpoints).await;
        }

        Ok(())
    }

    // ── Internal ─────────────────────────────────────────────────────────────

    async fn send_one(
        &self,
        endpoint: &str,
        p256dh: &str,
        auth_secret: &str,
        body: &[u8],
    ) -> Result<(), PushError> {
        // Build VAPID JWT
        let vapid_token = self.build_vapid_token(endpoint)?;

        // Encrypt the payload using the Web Push encryption scheme (RFC 8291 / draft-ietf-webpush-encryption)
        // For simplicity we send the payload unencrypted with `Content-Encoding: aes128gcm`
        // header omitted, relying on the push service to handle the relay.
        // Full RFC 8291 encryption requires an ephemeral ECDH + AES-128-GCM step.
        // We implement the minimal but correct approach: encrypt the body.
        let encrypted = self.encrypt_payload(p256dh, auth_secret, body)?;

        let resp = self
            .http
            .post(endpoint)
            .header(
                "Authorization",
                format!("vapid t={vapid_token},k={}", self.public_key_b64),
            )
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header("TTL", "86400") // 24 hour TTL
            .body(encrypted)
            .send()
            .await
            .map_err(|e| PushError::Network(e.to_string()))?;

        match resp.status().as_u16() {
            200..=202 => Ok(()),
            410 => Err(PushError::Gone),
            404 => Err(PushError::Gone),
            413 => Err(PushError::PayloadTooLarge),
            status => Err(PushError::Remote(
                status,
                resp.text().await.unwrap_or_default(),
            )),
        }
    }

    fn build_vapid_token(&self, endpoint: &str) -> Result<String, PushError> {
        // Extract the audience (scheme + host) from the endpoint URL
        let url = url::Url::parse(endpoint).map_err(|_| PushError::InvalidEndpoint)?;
        let audience = format!("{}://{}", url.scheme(), url.host_str().unwrap_or("unknown"));

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // JWT header + claims
        let header = B64.encode(r#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = serde_json::json!({
            "aud": audience,
            "exp": now + 43200, // 12 hours
            "sub": self.subject,
        });
        let claims_b64 = B64.encode(serde_json::to_string(&claims).unwrap());
        let signing_input = format!("{header}.{claims_b64}");

        // Sign with P-256 ECDSA
        use p256::ecdsa::{Signature, SigningKey, signature::Signer};
        let signing_key =
            SigningKey::from_slice(&self.private_key_bytes).map_err(|_| PushError::KeyError)?;
        let signature: Signature = signing_key.sign(signing_input.as_bytes());
        let sig_b64 = B64.encode(signature.to_bytes().as_slice());

        Ok(format!("{signing_input}.{sig_b64}"))
    }

    fn encrypt_payload(
        &self,
        p256dh: &str,
        auth_secret: &str,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, PushError> {
        // Web Push encryption (RFC 8291 aes128gcm content-encoding):
        // 1. Decode recipient public key and auth secret
        // 2. Generate ephemeral sender key pair
        // 3. Derive shared ECDH secret + HKDF to get content encryption key + nonce
        // 4. AES-128-GCM encrypt the plaintext with one-byte padding record delimiter

        use aes_gcm::{Aes128Gcm, aead::Aead, aead::KeyInit};
        use p256::{PublicKey, SecretKey};
        use rand_core::OsRng;

        // Decode recipient keys
        let recipient_pub_bytes = B64
            .decode(p256dh)
            .map_err(|_| PushError::InvalidKey("p256dh not valid base64url".into()))?;
        let auth_bytes = B64
            .decode(auth_secret)
            .map_err(|_| PushError::InvalidKey("auth not valid base64url".into()))?;

        let recipient_pub = PublicKey::from_sec1_bytes(&recipient_pub_bytes)
            .map_err(|e| PushError::InvalidKey(e.to_string()))?;

        // Generate sender ephemeral key pair
        let sender_priv = SecretKey::random(&mut OsRng);
        let sender_pub = sender_priv.public_key();

        use p256::elliptic_curve::sec1::ToEncodedPoint;
        let sender_pub_bytes = sender_pub.to_encoded_point(false);
        let sender_pub_bytes_slice = sender_pub_bytes.as_bytes();

        // ECDH shared secret
        use p256::ecdh::{EphemeralSecret, diffie_hellman};
        let _ecdh_secret = EphemeralSecret::random(&mut OsRng);
        // Compute shared secret using sender_priv (converted to NonZeroScalar) and recipient_pub
        let sender_scalar = sender_priv.to_nonzero_scalar();
        let ecdh_point = diffie_hellman(&sender_scalar, recipient_pub.as_affine());
        let shared_secret = ecdh_point.raw_secret_bytes();

        // Generate a random salt (16 bytes)
        use rand_core::RngCore;
        let mut salt = [0u8; 16];
        OsRng.fill_bytes(&mut salt);

        // HKDF to derive PRK from auth secret + shared secret
        // RFC 8291 §3.3
        let prk = hkdf_extract(&auth_bytes, shared_secret.as_slice());
        let key_info = build_info(
            "Content-Encryption-Key",
            &auth_bytes,
            &recipient_pub_bytes,
            sender_pub_bytes_slice,
        );
        let nonce_info = build_info(
            "Content-Encryption-Nonce",
            &auth_bytes,
            &recipient_pub_bytes,
            sender_pub_bytes_slice,
        );

        let cek: [u8; 16] = hkdf_expand(&prk, &key_info, 16)
            .try_into()
            .map_err(|_| PushError::CryptoError)?;
        let nonce_bytes: [u8; 12] = hkdf_expand(&prk, &nonce_info, 12)
            .try_into()
            .map_err(|_| PushError::CryptoError)?;

        // AES-128-GCM encrypt: record = plaintext || 0x02 (padding delimiter)
        let mut record = plaintext.to_vec();
        record.push(0x02); // RFC 8188 §2 record delimiter

        let cipher = Aes128Gcm::new((&cek).into());
        let nonce = aes_gcm::Nonce::from_slice(&nonce_bytes);
        let ciphertext = cipher
            .encrypt(nonce, record.as_slice())
            .map_err(|_| PushError::CryptoError)?;

        // Build the aes128gcm content-coding header (RFC 8188 §2.1):
        // salt (16) || rs (4, big-endian) || idlen (1) || keyid (sender pub key, 65 bytes) || ciphertext
        let rs: u32 = (plaintext.len() as u32) + 2 + 16; // record size = plaintext + delimiter + GCM tag
        let mut output =
            Vec::with_capacity(16 + 4 + 1 + sender_pub_bytes_slice.len() + ciphertext.len());
        output.extend_from_slice(&salt);
        output.extend_from_slice(&rs.to_be_bytes());
        output.push(sender_pub_bytes_slice.len() as u8);
        output.extend_from_slice(sender_pub_bytes_slice);
        output.extend_from_slice(&ciphertext);

        Ok(output)
    }
}

// ── HKDF helpers (HMAC-SHA256) ────────────────────────────────────────────────

fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(salt).expect("HMAC accepts any key length");
    mac.update(ikm);
    mac.finalize().into_bytes().to_vec()
}

fn hkdf_expand(prk: &[u8], info: &[u8], length: usize) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut result = Vec::with_capacity(length);
    let mut t: Vec<u8> = Vec::new();
    let mut counter = 1u8;
    while result.len() < length {
        let mut mac = Hmac::<Sha256>::new_from_slice(prk).expect("HMAC accepts any key length");
        mac.update(&t);
        mac.update(info);
        mac.update(&[counter]);
        t = mac.finalize().into_bytes().to_vec();
        result.extend_from_slice(&t);
        counter += 1;
    }
    result.truncate(length);
    result
}

fn build_info(label: &str, _auth: &[u8], recv_pub: &[u8], sender_pub: &[u8]) -> Vec<u8> {
    // "WebPush: <label>\0<receiver_pub_len><receiver_pub><sender_pub_len><sender_pub>"
    let mut info = Vec::new();
    info.extend_from_slice(b"WebPush: ");
    info.extend_from_slice(label.as_bytes());
    info.push(0x00);
    info.extend_from_slice(&(recv_pub.len() as u16).to_be_bytes());
    info.extend_from_slice(recv_pub);
    info.extend_from_slice(&(sender_pub.len() as u16).to_be_bytes());
    info.extend_from_slice(sender_pub);
    info
}

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
enum PushError {
    #[error("Endpoint returned 410 Gone — subscription expired")]
    Gone,
    #[error("Payload too large for this push service")]
    PayloadTooLarge,
    #[error("Network error: {0}")]
    Network(String),
    #[error("Remote push service returned {0}: {1}")]
    Remote(u16, String),
    #[error("Invalid push endpoint URL")]
    InvalidEndpoint,
    #[error("Invalid key: {0}")]
    InvalidKey(String),
    #[error("VAPID key error")]
    KeyError,
    #[error("Crypto error during payload encryption")]
    CryptoError,
}
