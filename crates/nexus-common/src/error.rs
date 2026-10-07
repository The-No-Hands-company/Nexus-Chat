//! Centralized error types for Nexus.
//!
//! Uses `thiserror` for ergonomic error definitions and provides HTTP-friendly
//! error variants that can be directly converted to API responses.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// Core application error type used across all Nexus services.
#[derive(Debug, thiserror::Error)]
pub enum NexusError {
    // === Auth errors ===
    #[error("Invalid credentials")]
    InvalidCredentials,

    #[error("Token expired")]
    TokenExpired,

    #[error("Invalid token")]
    InvalidToken,

    #[error("Unauthorized")]
    Unauthorized,

    // === Resource errors ===
    #[error("{resource} not found")]
    NotFound { resource: String },

    #[error("{resource} already exists")]
    AlreadyExists { resource: String },

    // === Validation errors ===
    #[error("Validation failed: {message}")]
    Validation { message: String },

    // === Permission errors ===
    #[error("Missing permission: {permission}")]
    MissingPermission { permission: String },

    #[error("Forbidden")]
    Forbidden,

    // === Rate limiting ===
    #[error("Rate limited. Retry after {retry_after_ms}ms")]
    RateLimited { retry_after_ms: u64 },

    // === Server capacity ===
    #[error("Limit reached: {message}")]
    LimitReached { message: String },

    // === Not built yet ===
    /// The endpoint exists and is routed, but the capability behind it is not
    /// implemented.
    ///
    /// This is not the same as an empty result, and the difference matters. A
    /// moderation endpoint that answers `[]` tells a caller "we looked and
    /// found nothing"; if nothing ever looked, that is a false negative
    /// dressed as a clean bill of health. 501 says plainly that the feature
    /// does not exist yet, which a client can act on.
    #[error("{feature} is not implemented on this server")]
    NotImplemented { feature: String },

    // === Infrastructure errors ===
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("Redis error: {0}")]
    Redis(#[from] redis::RedisError),

    #[error("Internal error: {0}")]
    Internal(#[from] anyhow::Error),
}

/// JSON error response body sent to clients.
#[derive(Serialize)]
struct ErrorResponse {
    code: u16,
    error: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_ms: Option<u64>,
}

impl NexusError {
    /// Map error to HTTP status code.
    #[must_use]
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::InvalidCredentials
            | Self::InvalidToken
            | Self::TokenExpired
            | Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::NotFound { .. } => StatusCode::NOT_FOUND,
            Self::AlreadyExists { .. } => StatusCode::CONFLICT,
            Self::Validation { .. } => StatusCode::BAD_REQUEST,
            Self::MissingPermission { .. } | Self::Forbidden | Self::LimitReached { .. } => {
                StatusCode::FORBIDDEN
            }
            Self::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::NotImplemented { .. } => StatusCode::NOT_IMPLEMENTED,
            Self::Database(_) | Self::Redis(_) | Self::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }

    /// Error code string for programmatic handling by clients.
    #[must_use]
    pub fn error_code(&self) -> &str {
        match self {
            Self::InvalidCredentials => "INVALID_CREDENTIALS",
            Self::TokenExpired => "TOKEN_EXPIRED",
            Self::InvalidToken => "INVALID_TOKEN",
            Self::Unauthorized => "UNAUTHORIZED",
            Self::NotFound { .. } => "NOT_FOUND",
            Self::AlreadyExists { .. } => "ALREADY_EXISTS",
            Self::Validation { .. } => "VALIDATION_ERROR",
            Self::MissingPermission { .. } => "MISSING_PERMISSION",
            Self::NotImplemented { .. } => "NOT_IMPLEMENTED",
            Self::Forbidden => "FORBIDDEN",
            Self::RateLimited { .. } => "RATE_LIMITED",
            Self::LimitReached { .. } => "LIMIT_REACHED",
            Self::Database(_) => "DATABASE_ERROR",
            Self::Redis(_) => "CACHE_ERROR",
            Self::Internal(_) => "INTERNAL_ERROR",
        }
    }
}

impl IntoResponse for NexusError {
    fn into_response(self) -> Response {
        let status = self.status_code();

        // Don't leak internal details to clients
        let message = match &self {
            NexusError::Database(e) => {
                tracing::error!(error_kind = %crate::logsafe::sqlx_kind(e), "Database error");
                "An internal error occurred".to_string()
            }
            NexusError::Redis(_) => {
                tracing::error!("Redis error");
                "An internal error occurred".to_string()
            }
            NexusError::Internal(e) => {
                tracing::error!(error_kind = %crate::logsafe::anyhow_kind(e), "Internal error");
                "An internal error occurred".to_string()
            }
            other => other.to_string(),
        };

        let retry_after_ms = if let NexusError::RateLimited { retry_after_ms } = &self {
            Some(*retry_after_ms)
        } else {
            None
        };

        let body = ErrorResponse {
            code: status.as_u16(),
            error: self.error_code().to_string(),
            message,
            retry_after_ms,
        };

        (status, axum::Json(body)).into_response()
    }
}

/// Convenience type alias for `Result`s using `NexusError`.
pub type NexusResult<T> = Result<T, NexusError>;

#[cfg(test)]
mod tests {
    use super::*;

    // ── status_code mapping ───────────────────────────────────────────────────

    #[test]
    fn not_implemented_is_501_and_names_the_feature() {
        let e = NexusError::NotImplemented {
            feature: "Toxicity scoring".to_string(),
        };
        assert_eq!(e.status_code(), StatusCode::NOT_IMPLEMENTED);
        assert_eq!(e.error_code(), "NOT_IMPLEMENTED");
        // The message has to say which capability is missing. "Not
        // implemented" alone leaves a client guessing whether it asked wrongly
        // or the server cannot answer at all.
        assert!(e.to_string().contains("Toxicity scoring"));
    }

    #[test]
    fn not_implemented_is_distinct_from_not_found() {
        // 404 means "no such thing here"; 501 means "this server does not do
        // that yet". A client retrying a 404 against another node is sensible;
        // retrying a 501 is not, and conflating them hides which is which.
        let missing = NexusError::NotFound {
            resource: "Channel".to_string(),
        };
        let unbuilt = NexusError::NotImplemented {
            feature: "Raid detection".to_string(),
        };
        assert_ne!(missing.status_code(), unbuilt.status_code());
        assert_ne!(missing.error_code(), unbuilt.error_code());
    }

    #[test]
    fn invalid_credentials_is_401() {
        assert_eq!(
            NexusError::InvalidCredentials.status_code(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn token_expired_is_401() {
        assert_eq!(
            NexusError::TokenExpired.status_code(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn not_found_is_404() {
        let e = NexusError::NotFound {
            resource: "User".into(),
        };
        assert_eq!(e.status_code(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn already_exists_is_409() {
        let e = NexusError::AlreadyExists {
            resource: "Username".into(),
        };
        assert_eq!(e.status_code(), StatusCode::CONFLICT);
    }

    #[test]
    fn validation_error_is_400() {
        let e = NexusError::Validation {
            message: "bad input".into(),
        };
        assert_eq!(e.status_code(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn missing_permission_is_403() {
        let e = NexusError::MissingPermission {
            permission: "BAN_MEMBERS".into(),
        };
        assert_eq!(e.status_code(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn forbidden_is_403() {
        assert_eq!(NexusError::Forbidden.status_code(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn rate_limited_is_429() {
        let e = NexusError::RateLimited {
            retry_after_ms: 1000,
        };
        assert_eq!(e.status_code(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn limit_reached_is_403() {
        let e = NexusError::LimitReached {
            message: "max servers".into(),
        };
        assert_eq!(e.status_code(), StatusCode::FORBIDDEN);
    }

    // ── error_code strings ────────────────────────────────────────────────────

    #[test]
    fn error_codes_are_screaming_snake_case() {
        let cases = [
            (NexusError::InvalidCredentials, "INVALID_CREDENTIALS"),
            (NexusError::TokenExpired, "TOKEN_EXPIRED"),
            (NexusError::InvalidToken, "INVALID_TOKEN"),
            (NexusError::Unauthorized, "UNAUTHORIZED"),
            (
                NexusError::NotFound {
                    resource: "x".into(),
                },
                "NOT_FOUND",
            ),
            (
                NexusError::AlreadyExists {
                    resource: "x".into(),
                },
                "ALREADY_EXISTS",
            ),
            (
                NexusError::Validation {
                    message: "x".into(),
                },
                "VALIDATION_ERROR",
            ),
            (
                NexusError::MissingPermission {
                    permission: "x".into(),
                },
                "MISSING_PERMISSION",
            ),
            (NexusError::Forbidden, "FORBIDDEN"),
            (
                NexusError::RateLimited { retry_after_ms: 0 },
                "RATE_LIMITED",
            ),
            (
                NexusError::LimitReached {
                    message: "x".into(),
                },
                "LIMIT_REACHED",
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(err.error_code(), expected, "wrong code for {err:?}");
        }
    }

    // ── Display messages ──────────────────────────────────────────────────────

    #[test]
    fn not_found_message_includes_resource() {
        let e = NexusError::NotFound {
            resource: "Channel".into(),
        };
        assert!(e.to_string().contains("Channel"));
    }

    #[test]
    fn missing_permission_message_includes_permission_name() {
        let e = NexusError::MissingPermission {
            permission: "SEND_MESSAGES".into(),
        };
        assert!(e.to_string().contains("SEND_MESSAGES"));
    }

    #[test]
    fn rate_limited_message_includes_retry_after() {
        let e = NexusError::RateLimited {
            retry_after_ms: 5000,
        };
        assert!(e.to_string().contains("5000"));
    }

    #[test]
    fn validation_message_is_propagated() {
        let e = NexusError::Validation {
            message: "username too short".into(),
        };
        assert!(e.to_string().contains("username too short"));
    }
}
