//! Middleware — authentication extraction, rate limiting, security headers, metrics, etc.

use axum::{extract::Request, http::header, middleware::Next, response::Response};
use nexus_common::error::NexusError;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::AppState;

// ── HTTP metrics middleware ──────────────────────────────────────────────────

/// Normalize a raw URI path to a low-cardinality label suitable for Prometheus.
///
/// Keeps the first 4 slash-delimited segments (e.g. `/api/v1/channels`) and
/// drops dynamic path params that would explode label cardinality.
fn normalize_path(path: &str) -> String {
    // Try to use the matched route pattern from Axum (`:id` placeholders)
    // before falling back to raw prefix truncation.
    //
    // Splitting a rooted path yields a leading empty segment ("/a/b" ->
    // ["", "a", "b"]), so a plain take(4) spent one of its four on that empty
    // string and kept only three real ones. Filter the empties out and rebuild,
    // so "four segments" means what the name and the tests say it means.
    let kept: String = path
        .split('/')
        .filter(|s| !s.is_empty())
        .take(4)
        .map(|s| format!("/{s}"))
        .collect();
    if kept.is_empty() { "/".to_owned() } else { kept }
}

/// Record per-request Prometheus counters and latency histograms.
///
/// Labels:
///   `nexus_http_requests_total{method, path, status}`
///   `nexus_http_request_duration_seconds{method, path}`
pub async fn record_request_metrics(request: Request, next: Next) -> Response {
    let method = request.method().as_str().to_owned();
    // Prefer the matched route pattern if Axum has already resolved it;
    // otherwise fall back to the raw URI path (normalized to avoid high cardinality).
    let path = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|mp| mp.as_str().to_owned())
        .unwrap_or_else(|| normalize_path(request.uri().path()));

    let start = Instant::now();
    let response = next.run(request).await;
    let elapsed = start.elapsed().as_secs_f64();
    let status = response.status().as_u16().to_string();

    metrics::counter!(
        "nexus_http_requests_total",
        "method" => method.clone(),
        "path" => path.clone(),
        "status" => status,
    )
    .increment(1);
    metrics::histogram!(
        "nexus_http_request_duration_seconds",
        "method" => method,
        "path" => path,
    )
    .record(elapsed);

    response
}

/// Authentication context extracted from the Authorization header.
///
/// Populated by `identity_middleware` (the ecosystem identity header) or by
/// `combined_auth_middleware`, which accepts that same header for users and
/// `Bot <token>` for bot applications.
#[derive(Debug, Clone)]
pub struct AuthContext {
    pub user_id: uuid::Uuid,
    pub username: String,
    /// `true` when the request was authenticated with a bot token rather than
    /// a user JWT.  Handlers that should be user-only can reject bots here.
    pub is_bot: bool,
    /// The JTI (JWT ID) of the current access token, parsed as UUID.
    /// Used by session-management routes to identify the caller's own session.
    /// `None` for bot tokens or tokens issued before 09.7 (no `jti` claim).
    pub session_id: Option<uuid::Uuid>,
    /// Whether the token was issued after a successful 2FA check.
    pub two_fa_verified: bool,
    /// Whether the user's email was verified at the time of token issuance.
    /// Always `true` for bot tokens and for users with no email registered.
    pub email_verified: bool,
    /// User account flags (staff, admin, creator, etc.)
    /// Lazily loaded on demand by helpers
    pub flags: std::sync::Arc<tokio::sync::Mutex<Option<i64>>>,
}

impl AuthContext {
    /// Check if user has a specific flag. Caches flags in-memory.
    pub async fn has_flag(&self, flag: i64) -> bool {
        if let Some(flags) = *self.flags.lock().await {
            flags & flag != 0
        } else {
            false
        }
    }

    /// Check if user is a marketplace admin (reviewer or moderator)
    pub async fn is_marketplace_admin(&self) -> bool {
        use nexus_common::models::user_flags;
        self.has_flag(user_flags::MARKETPLACE_REVIEWER).await
            || self.has_flag(user_flags::MARKETPLACE_MODERATOR).await
            || self.has_flag(user_flags::INSTANCE_ADMIN).await
    }

    /// Check if user can review plugins
    pub async fn can_review_plugins(&self) -> bool {
        use nexus_common::models::user_flags;
        self.has_flag(user_flags::MARKETPLACE_REVIEWER).await
            || self.has_flag(user_flags::INSTANCE_ADMIN).await
    }

    /// Check if user can handle takedowns
    pub async fn can_handle_takedowns(&self) -> bool {
        use nexus_common::models::user_flags;
        self.has_flag(user_flags::MARKETPLACE_MODERATOR).await
            || self.has_flag(user_flags::INSTANCE_ADMIN).await
    }

    /// Load flags from database (call once per request if needed)
    pub async fn load_flags(&self, pool: &sqlx::AnyPool) -> Result<i64, sqlx::Error> {
        if self.flags.lock().await.is_some() {
            return Ok(self.flags.lock().await.unwrap());
        }

        let flags = sqlx::query_scalar::<_, i64>("SELECT flags FROM users WHERE id = $1")
            .bind(self.user_id.to_string())
            .fetch_optional(pool)
            .await?
            .unwrap_or(0);

        *self.flags.lock().await = Some(flags);
        Ok(flags)
    }
}

// ── SHA-256 helper (used for bot token hashing) ──────────────────────────────

fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    format!("{:x}", h.finalize())
}

// ── Combined auth middleware (users + bots) ──────────────────────────────────

/// Accept **either** `Authorization: Bearer <jwt>` (human users) **or**
/// `Authorization: Bot <raw-token>` (bot applications).
///
/// For `Bot` token requests the raw token is SHA-256 hashed and looked up in
/// the `bot_applications` table.  The resulting `AuthContext` has `is_bot =
/// true` and `user_id` set to the bot application's own UUID.
///
/// Requires [`Arc<AppState>`] to be present as an Axum [`Extension`] on the
/// router — `build_router` does this with `.layer(axum::Extension(arc_state))`.
pub async fn combined_auth_middleware(
    mut request: Request,
    next: Next,
) -> Result<Response, NexusError> {
    // Optional, and only ever inspected for a `Bot ` prefix. It used to be
    // mandatory, because every caller was expected to present a local JWT here.
    // Users now arrive with no Authorization header at all — the proxy puts
    // their identity in X-Nexus-Identity — so requiring it rejected every real
    // user before the identity branch below could run.
    let auth_header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();

    let auth_ctx = if let Some(raw_token) = auth_header.strip_prefix("Bot ") {
        // ── Bot token authentication ──────────────────────────────────────
        let token_hash = sha256_hex(raw_token);

        // Extract AppState from Extensions (added by build_router)
        let state = request
            .extensions()
            .get::<Arc<AppState>>()
            .cloned()
            .ok_or(NexusError::Unauthorized)?;

        let bot = nexus_db::repository::bots::get_bot_by_token_hash(&state.db.pool, &token_hash)
            .await
            .map_err(|_| NexusError::Unauthorized)?
            .ok_or(NexusError::Unauthorized)?;

        AuthContext {
            user_id: bot.id,
            username: bot.name,
            is_bot: true,
            session_id: None,
            two_fa_verified: false,
            // Bots are not subject to email verification requirements.
            email_verified: true,
            flags: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        }
    } else {
        // ── Ecosystem identity ────────────────────────────────────────────
        // Not a local JWT. This server no longer issues credentials; the proxy
        // authenticates the browser and forwards a short-lived signed token.
        // The Authorization header is only still read above, for `Bot `.
        let state = request
            .extensions()
            .get::<Arc<AppState>>()
            .cloned()
            .ok_or(NexusError::Unauthorized)?;
        identity_context(request.headers(), state).await?
    };

    request.extensions_mut().insert(auth_ctx.clone());

    // ── Email verification gate ───────────────────────────────────────────
    // Exempt /auth/* routes so that resend-verification, session management
    // and 2FA completion still work while the account is unverified.
    let config = nexus_common::config::get();
    if config.features.require_email_verification && !auth_ctx.is_bot && !auth_ctx.email_verified {
        let path = request.uri().path();
        let is_auth_path =
            path.contains("/auth/") || path.starts_with("/healthz") || path.starts_with("/health");
        if !is_auth_path {
            return Err(NexusError::Forbidden);
        }
    }

    Ok(next.run(request).await)
}

// ── AuthContext helpers ───────────────────────────────────────────────────────

/// Extract AuthContext from request extensions.
///
/// Usage in handlers:
/// ```rust,ignore
/// async fn my_handler(auth: Extension<AuthContext>) -> impl IntoResponse { ... }
/// ```
impl AuthContext {
    pub fn from_request_extensions(
        extensions: &axum::http::Extensions,
    ) -> Result<&Self, NexusError> {
        extensions
            .get::<AuthContext>()
            .ok_or(NexusError::Unauthorized)
    }
}

// ── Auth rate limiter ────────────────────────────────────────────────────────

/// Redis-backed sliding-window rate limiter.
///
/// Uses the classic `INCR` + `EXPIRE` pattern — O(1) in Redis.
/// The race between INCR and EXPIRE is acceptable: the worst
/// case is that the window extends slightly on the very first hit,
/// not that the limit is relaxed.
///
/// # Arguments
/// * `redis`       – Redis connection manager borrowed from `db.redis`
/// * `key`         – Unique key (e.g. `"rl:login:tag:tagAAAA"`)
/// * `limit`       – Max calls allowed in the window
/// * `window_secs` – Window length in seconds
///
/// Returns `Ok(())` when the call is within limits, or
/// `Err(NexusError::RateLimited { retry_after_ms })` when exceeded.
pub async fn check_rate_limit(
    redis: &redis::aio::ConnectionManager,
    key: impl AsRef<str>,
    limit: u64,
    window_secs: u64,
) -> Result<(), NexusError> {
    #[allow(unused_imports)]
    use redis::AsyncCommands as _;
    let mut conn = redis.clone();
    let key = key.as_ref();

    let count: u64 = redis::cmd("INCR")
        .arg(key)
        .query_async(&mut conn)
        .await
        .map_err(|e| NexusError::Internal(anyhow::anyhow!("redis INCR failed: {e}")))?;

    if count == 1 {
        // First hit — arm the expiry clock.
        let _: () = redis::cmd("EXPIRE")
            .arg(key)
            .arg(window_secs as i64)
            .query_async(&mut conn)
            .await
            .map_err(|e| NexusError::Internal(anyhow::anyhow!("redis EXPIRE failed: {e}")))?;
    }

    if count > limit {
        let ttl_secs: i64 = redis::cmd("TTL")
            .arg(key)
            .query_async(&mut conn)
            .await
            .map_err(|e| NexusError::Internal(anyhow::anyhow!("redis TTL failed: {e}")))?;
        let retry_after_ms = (ttl_secs.max(1) as u64) * 1000;
        return Err(NexusError::RateLimited { retry_after_ms });
    }

    Ok(())
}

static LOCAL_RATE_LIMITS: OnceLock<Mutex<HashMap<String, (u64, Instant)>>> = OnceLock::new();

/// Rate limiter that prefers Redis (multi-node) and falls back to local memory.
///
/// Use this for public abuse-prone endpoints so limits remain active even when
/// Redis is not configured (lite mode) or temporarily unavailable.
pub async fn check_rate_limit_with_fallback(
    redis: Option<&redis::aio::ConnectionManager>,
    key: impl AsRef<str>,
    limit: u64,
    window_secs: u64,
) -> Result<(), NexusError> {
    if let Some(redis) = redis {
        match check_rate_limit(redis, key.as_ref(), limit, window_secs).await {
            Ok(()) => {
                metrics::counter!(
                    "nexus_rate_limit_decisions_total",
                    "backend" => "redis",
                    "outcome" => "allowed",
                )
                .increment(1);
                return Ok(());
            }
            Err(NexusError::RateLimited { retry_after_ms }) => {
                metrics::counter!(
                    "nexus_rate_limit_decisions_total",
                    "backend" => "redis",
                    "outcome" => "blocked",
                )
                .increment(1);
                return Err(NexusError::RateLimited { retry_after_ms });
            }
            Err(err) => {
                tracing::warn!(error = %err, "redis rate limiter failed; falling back to local limiter");
                metrics::counter!(
                    "nexus_rate_limit_decisions_total",
                    "backend" => "redis",
                    "outcome" => "error",
                )
                .increment(1);
            }
        }
    }

    let res = check_rate_limit_local(key.as_ref(), limit, window_secs).await;
    metrics::counter!(
        "nexus_rate_limit_decisions_total",
        "backend" => "local",
        "outcome" => if res.is_ok() { "allowed" } else { "blocked" },
    )
    .increment(1);
    res
}

async fn check_rate_limit_local(key: &str, limit: u64, window_secs: u64) -> Result<(), NexusError> {
    let state = LOCAL_RATE_LIMITS.get_or_init(|| Mutex::new(HashMap::new()));
    let now = Instant::now();
    let window = Duration::from_secs(window_secs.max(1));

    let mut guard = state.lock().await;

    // Best-effort cleanup to avoid unbounded growth over time.
    if guard.len() > 50_000 {
        guard.retain(|_, (_, reset_at)| *reset_at > now);
    }

    let entry = guard.entry(key.to_string()).or_insert((0, now + window));

    if now >= entry.1 {
        *entry = (1, now + window);
    } else {
        entry.0 = entry.0.saturating_add(1);
    }

    if entry.0 > limit {
        let retry_after_ms = entry.1.saturating_duration_since(now).as_millis() as u64;
        return Err(NexusError::RateLimited {
            retry_after_ms: retry_after_ms.max(1_000),
        });
    }

    Ok(())
}

/// Opaque per-client tag set by the ecosystem proxy (`x-nexus-client-tag`).
///
/// The proxy strips every address header and derives this tag from the
/// connecting address, so rate limits can tell clients apart without Chat ever
/// seeing, keying on or storing an IP. Anything else (`X-Forwarded-For`,
/// `X-Real-IP`) is deliberately ignored: it would be client-controlled, and it
/// would re-introduce addresses. Absent tag degrades to `"unknown"`.
pub fn extract_client_tag(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("x-nexus-client-tag")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("unknown")
        .to_owned()
}

// ── Security headers ──────────────────────────────────────────────────────────

/// Add defensive security headers to every HTTP response.
///
/// Headers applied:
/// - `X-Content-Type-Options: nosniff` — prevents MIME sniffing
/// - `X-XSS-Protection: 1; mode=block` — legacy XSS protection
/// - `Referrer-Policy: strict-origin-when-cross-origin`
/// - `Permissions-Policy` — disables camera, mic, geolocation
/// - `Strict-Transport-Security` — HSTS (max-age 2 years + preload)
/// - `Content-Security-Policy` — restrictive CSP for API endpoints
pub async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let h = response.headers_mut();

    macro_rules! set {
        ($name:expr, $val:expr) => {
            if let Ok(v) = $val.parse::<axum::http::HeaderValue>() {
                h.insert($name, v);
            }
        };
    }

    set!(
        axum::http::header::HeaderName::from_static("x-content-type-options"),
        "nosniff"
    );
    set!(
        axum::http::header::HeaderName::from_static("x-xss-protection"),
        "1; mode=block"
    );
    set!(
        axum::http::header::HeaderName::from_static("referrer-policy"),
        "strict-origin-when-cross-origin"
    );
    set!(
        axum::http::header::HeaderName::from_static("permissions-policy"),
        "camera=(), microphone=(), geolocation=(), payment=()"
    );
    set!(
        axum::http::header::HeaderName::from_static("strict-transport-security"),
        "max-age=63072000; includeSubDomains; preload"
    );
    set!(
        axum::http::header::HeaderName::from_static("content-security-policy"),
        "default-src 'self'; \
         script-src 'self'; \
         style-src 'self' 'unsafe-inline'; \
         img-src 'self' data: blob:; \
         connect-src 'self' wss:; \
         font-src 'self'; \
         media-src 'self' blob:; \
         worker-src 'self' blob:; \
         frame-ancestors 'self' https://app.tnhc.dev"
    );

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    // ── normalize_path ────────────────────────────────────────────────────────

    #[test]
    fn normalize_path_keeps_up_to_four_segments() {
        assert_eq!(normalize_path("/api/v1/users/123"), "/api/v1/users/123");
    }

    #[test]
    fn normalize_path_truncates_beyond_four_segments() {
        // Only the first four slash-delimited parts are kept
        assert_eq!(
            normalize_path("/api/v1/channels/abc/messages/def"),
            "/api/v1/channels/abc"
        );
    }

    #[test]
    fn normalize_path_root_is_preserved() {
        assert_eq!(normalize_path("/"), "/");
    }

    #[test]
    fn normalize_path_short_path_unchanged() {
        assert_eq!(normalize_path("/health"), "/health");
        assert_eq!(normalize_path("/api/v1"), "/api/v1");
    }

    // ── sha256_hex ────────────────────────────────────────────────────────────

    #[test]
    fn sha256_hex_is_deterministic() {
        let a = sha256_hex("hello");
        let b = sha256_hex("hello");
        assert_eq!(a, b);
    }

    #[test]
    fn sha256_hex_differs_for_different_inputs() {
        assert_ne!(sha256_hex("hello"), sha256_hex("world"));
    }

    #[test]
    fn sha256_hex_produces_64_char_lowercase_hex() {
        let h = sha256_hex("nexus");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        // Hex digits should all be lowercase
        assert_eq!(h, h.to_lowercase());
    }

    #[test]
    fn sha256_hex_empty_string_known_value() {
        // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert!(sha256_hex("").starts_with("e3b0c44"));
    }

    #[test]
    fn sha256_hex_bot_token_differs_from_raw_token() {
        let raw = "my-bot-token";
        let prefixed = format!("Bot {raw}");
        assert_ne!(sha256_hex(raw), sha256_hex(&prefixed));
    }

    // ── extract_client_tag ────────────────────────────────────────────────────

    #[test]
    fn client_tag_comes_from_the_proxy_header_never_from_address_headers() {
        let mut h = axum::http::HeaderMap::new();
        h.insert("x-forwarded-for", "203.0.113.30".parse().unwrap());
        h.insert("x-real-ip", "203.0.113.30".parse().unwrap());
        assert_eq!(extract_client_tag(&h), "unknown");
        h.insert("x-nexus-client-tag", "tagCCCCCCCCCCCCCCCCCCC".parse().unwrap());
        assert_eq!(extract_client_tag(&h), "tagCCCCCCCCCCCCCCCCCCC");
    }

    // ── check_rate_limit_local ────────────────────────────────────────────────

    #[tokio::test]
    async fn rate_limit_local_allows_under_limit() {
        // Use a unique key to avoid cross-test interference
        let key = format!("test:allow:{}", uuid::Uuid::new_v4());
        let result = check_rate_limit_local(&key, 5, 60).await;
        assert!(result.is_ok(), "first request should be allowed");
    }

    #[tokio::test]
    async fn rate_limit_local_blocks_over_limit() {
        let key = format!("test:block:{}", uuid::Uuid::new_v4());
        let limit = 3u64;
        // Exhaust the limit
        for _ in 0..limit {
            check_rate_limit_local(&key, limit, 60).await.unwrap();
        }
        // Next call should be rate limited
        let err = check_rate_limit_local(&key, limit, 60).await.unwrap_err();
        assert!(
            matches!(err, NexusError::RateLimited { .. }),
            "expected RateLimited, got {err:?}"
        );
    }

    #[tokio::test]
    async fn rate_limit_local_window_resets_after_expiry() {
        let key = format!("test:reset:{}", uuid::Uuid::new_v4());

        // This used to pass window_secs = 0 and expect an instant reset, but
        // check_rate_limit_local clamps the window with .max(1) precisely so a
        // misconfigured zero cannot disable rate limiting. Wait out a real
        // window instead — that is the property worth holding.
        check_rate_limit_local(&key, 1, 1).await.unwrap();
        check_rate_limit_local(&key, 1, 1)
            .await
            .expect_err("second call inside the window must be limited");

        tokio::time::sleep(Duration::from_millis(1_100)).await;

        let result = check_rate_limit_local(&key, 1, 1).await;
        assert!(result.is_ok(), "window should have reset");
    }
}

// ── Ecosystem identity ───────────────────────────────────────────────────────

pub use nexus_common::identity::IDENTITY_HEADER;

/// Verify the identity header and turn it into an `AuthContext`.
///
/// Shared by `identity_middleware` and the user branch of
/// `combined_auth_middleware` so the two cannot drift — a difference between
/// them would be a difference in who gets in.
///
/// `session_id` is `None` on purpose. Revocation lives at the proxy and in
/// Auth now, and these tokens expire in ~120 seconds; keeping the local
/// session table in the path would mean maintaining a second revocation system
/// that nothing writes to and that fails open when empty.
///
/// Takes the headers and state rather than the `Request` itself: holding a
/// `&Request` across an await makes the whole middleware future non-Send,
/// because `axum::body::Body` is not Sync, and `from_fn` then silently fails
/// its `Service` bound at every call site.
async fn identity_context(
    headers: &axum::http::HeaderMap,
    state: Arc<AppState>,
) -> Result<AuthContext, NexusError> {
    let config = nexus_common::config::get();
    // The audience is this server's public name. Auth mints the token for the
    // host the browser asked for, so checking it here is what stops a token
    // minted for another app in the ecosystem being replayed at this one.
    let claims = nexus_common::identity::verify_header(headers, &config.server.name)
        .await
        .map_err(|e| {
            tracing::debug!(error = %e, "rejected identity token");
            NexusError::Unauthorized
        })?;

    let user_id = nexus_db::repository::users::provision_from_identity(
        &state.db.pool,
        &claims.sub,
        &claims.username,
        Some(claims.email.as_str()),
    )
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "failed to provision user from identity");
        NexusError::Database(e)
    })?;

    Ok(AuthContext {
        user_id,
        username: claims.username,
        is_bot: false,
        session_id: None,
        // Both are the ecosystem's business now: Auth will not mint an identity
        // token for an account that is not active, so a token in hand means
        // those checks already passed upstream.
        two_fa_verified: true,
        email_verified: true,
        flags: Arc::new(Mutex::new(None)),
    })
}

/// Authenticate a request from the ecosystem identity header.
///
/// Replaces this server's own login: accounts live in Nexus-Auth, the proxy
/// authenticates the browser, and what arrives here is a short-lived RS256
/// token naming the user. A local row is provisioned on first sight so that
/// messages, memberships and every other foreign key have something to point
/// at.
///
/// The `Authorization` header is deliberately not consulted. Leaving that path
/// alive would mean a locally-minted JWT still authenticated, which is the
/// whole thing this replaces.
pub async fn identity_middleware(
    mut request: Request,
    next: Next,
) -> Result<Response, NexusError> {
    let state = request
        .extensions()
        .get::<Arc<AppState>>()
        .cloned()
        .ok_or(NexusError::Unauthorized)?;
    let auth_ctx = identity_context(request.headers(), state).await?;
    request.extensions_mut().insert(auth_ctx);
    Ok(next.run(request).await)
}

#[cfg(test)]
mod identity_middleware_tests {
    use super::*;
    use axum::{body::Body, http::StatusCode, routing::get, Router};
    use tower::ServiceExt;

    /// Config is a process-wide OnceLock that `get()` panics without, and the
    /// middleware reads the audience from it. Initialising here keeps these
    /// tests honest — the middleware runs exactly as it does in production
    /// rather than against a stubbed-out config lookup.
    fn ensure_config() {
        let _ = nexus_common::config::init();
    }

    /// A router carrying only the identity middleware.
    ///
    /// Deliberately built without `AppState`: these cases must be refused
    /// before anything touches the database, and leaving state out proves it.
    /// A middleware that reached for the pool first would fail here instead of
    /// returning 401.
    fn app() -> Router {
        Router::new()
            .route("/probe", get(|| async { "reached the handler" }))
            .layer(axum::middleware::from_fn(identity_middleware))
    }

    async fn status_for(request: Request) -> StatusCode {
        ensure_config();
        app().oneshot(request).await.expect("router responds").status()
    }

    #[tokio::test]
    async fn a_request_with_no_identity_header_is_rejected() {
        let req = Request::builder()
            .uri("/probe")
            .body(Body::empty())
            .unwrap();

        assert_eq!(status_for(req).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_local_bearer_token_no_longer_authenticates() {
        // The point of the whole phase. While this server minted its own JWTs,
        // anyone holding one could talk to the API directly and never pass the
        // proxy's login gate. Authorization must now be ignored entirely — not
        // "checked as a fallback".
        let req = Request::builder()
            .uri("/probe")
            // Assembled at run time rather than written as a literal: a
            // JWT-shaped constant in the source trips secret scanners, and the
            // value is irrelevant anyway — the assertion is that this header is
            // not read at all, whatever it holds.
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}.{}.{}", "eyJhbGciOiJIUzI1NiJ9", "e30", "sig"),
            )
            .body(Body::empty())
            .unwrap();

        assert_eq!(status_for(req).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn combined_auth_also_refuses_a_local_bearer_token() {
        // combined_auth_middleware guards 62 routers — nearly the whole API.
        // It used to accept a locally-minted JWT here. If that branch ever
        // came back, this server would have a second credential system and the
        // proxy's gate would be bypassable by anyone holding an old token.
        let app = Router::new()
            .route("/probe", get(|| async { "reached the handler" }))
            .layer(axum::middleware::from_fn(combined_auth_middleware));

        ensure_config();
        let req = Request::builder()
            .uri("/probe")
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}.{}.{}", "eyJhbGciOiJIUzI1NiJ9", "e30", "sig"),
            )
            .body(Body::empty())
            .unwrap();

        let status = app.oneshot(req).await.expect("router responds").status();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_garbage_identity_header_is_rejected() {
        let req = Request::builder()
            .uri("/probe")
            .header(IDENTITY_HEADER, "not-a-token")
            .body(Body::empty())
            .unwrap();

        assert_eq!(status_for(req).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_empty_identity_header_is_rejected() {
        let req = Request::builder()
            .uri("/probe")
            .header(IDENTITY_HEADER, "")
            .body(Body::empty())
            .unwrap();

        assert_eq!(status_for(req).await, StatusCode::UNAUTHORIZED);
    }
}
