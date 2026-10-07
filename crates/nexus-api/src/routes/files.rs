//! Static local file serving (lite mode).
//!
//! `GET /files/{*key}` — serves uploaded files from the local filesystem when
//! running without S3/MinIO.  In full mode these are served directly from
//! MinIO, so this route is a no-op (returns 404 for every request).

use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use std::sync::Arc;

use crate::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/files/{*key}", get(serve_file))
}

async fn serve_file(State(state): State<Arc<AppState>>, Path(key): Path<String>) -> Response {
    match state.storage.read_local_file(&key).await {
        Ok(Some((bytes, content_type))) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
            .body(Body::from(bytes))
            .unwrap(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error_kind = %nexus_common::logsafe::anyhow_kind(&e), "Failed to serve local file");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
