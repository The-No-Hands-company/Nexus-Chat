//! Log-safe error labels.
//!
//! TNHC's zero-retention rule: logs hold technical events only. The `Display`
//! of a `sqlx::Error` or `reqwest::Error` can embed bind parameters, request
//! URLs (with path and query) or addresses, so those must never be formatted
//! into a log line. [`err_kind`] reduces any error to a short fixed label that
//! never carries payload data.

use std::any::Any;

/// Fixed label for a sqlx error: the variant name plus the SQLSTATE code for
/// database errors (a 5-character class code, never the message).
pub fn sqlx_kind(e: &sqlx::Error) -> String {
    let v = match e {
        sqlx::Error::Configuration(_) => "configuration",
        sqlx::Error::Database(d) => {
            return match d.code() {
                Some(c) => format!("sqlx:database:{c}"),
                None => "sqlx:database".to_string(),
            };
        }
        sqlx::Error::Io(_) => "io",
        sqlx::Error::Tls(_) => "tls",
        sqlx::Error::Protocol(_) => "protocol",
        sqlx::Error::RowNotFound => "row_not_found",
        sqlx::Error::TypeNotFound { .. } => "type_not_found",
        sqlx::Error::ColumnIndexOutOfBounds { .. } => "column_index_out_of_bounds",
        sqlx::Error::ColumnNotFound(_) => "column_not_found",
        sqlx::Error::ColumnDecode { .. } => "column_decode",
        sqlx::Error::Decode(_) => "decode",
        sqlx::Error::Encode(_) => "encode",
        sqlx::Error::PoolTimedOut => "pool_timed_out",
        sqlx::Error::PoolClosed => "pool_closed",
        sqlx::Error::WorkerCrashed => "worker_crashed",
        _ => "other",
    };
    format!("sqlx:{v}")
}

/// Fixed label for a reqwest error (never the URL or message).
pub fn reqwest_kind(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "reqwest:timeout"
    } else if e.is_connect() {
        "reqwest:connect"
    } else if e.is_status() {
        "reqwest:status"
    } else if e.is_decode() {
        "reqwest:decode"
    } else if e.is_body() {
        "reqwest:body"
    } else if e.is_builder() {
        "reqwest:builder"
    } else if e.is_request() {
        "reqwest:request"
    } else {
        "reqwest:other"
    }
}

/// Reduce any error to a payload-free label: sqlx/reqwest get their variant
/// label, anything else gets its Rust type name.
pub fn err_kind<E: std::error::Error + 'static>(e: &E) -> String {
    let any: &dyn Any = e;
    if let Some(s) = any.downcast_ref::<sqlx::Error>() {
        return sqlx_kind(s);
    }
    if let Some(r) = any.downcast_ref::<reqwest::Error>() {
        return reqwest_kind(r).to_string();
    }
    std::any::type_name::<E>().to_string()
}

/// Label for an `anyhow::Error`: the root cause's sqlx/reqwest label when it
/// has one, otherwise a fixed `anyhow`.
pub fn anyhow_kind(e: &anyhow::Error) -> String {
    for cause in e.chain() {
        if let Some(s) = cause.downcast_ref::<sqlx::Error>() {
            return sqlx_kind(s);
        }
        if let Some(r) = cause.downcast_ref::<reqwest::Error>() {
            return reqwest_kind(r).to_string();
        }
    }
    "anyhow".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlx_label_has_no_payload() {
        assert_eq!(sqlx_kind(&sqlx::Error::RowNotFound), "sqlx:row_not_found");
        let e = sqlx::Error::ColumnNotFound("alice@example.com".into());
        assert!(!sqlx_kind(&e).contains("alice"));
    }

    #[test]
    fn generic_error_is_type_name_only() {
        let e = std::fmt::Error;
        assert_eq!(err_kind(&e), "core::fmt::Error");
        let io = std::io::Error::new(std::io::ErrorKind::Other, "bob@example.com");
        assert!(!err_kind(&io).contains("bob"));
    }

    #[test]
    fn anyhow_label_has_no_message() {
        let e = anyhow::anyhow!("carol@example.com failed");
        assert_eq!(anyhow_kind(&e), "anyhow");
    }
}
