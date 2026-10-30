//! Widget endpoint routes — embeddable customer chat for Nexus-Hosting sites.
//!
//! Public (no auth):
//!   GET  /widget/:id/config       — widget config (name, greeting, active)
//!   POST /widget/:id/messages     — submit a guest message (rate-limited)
//!   GET  /widget/:id/messages     — fetch messages for this visitor
//!
//! Authenticated (owner only):
//!   GET  /widget                  — list my widgets
//!   POST /widget                  — create a widget
//!   DELETE /widget/:id            — delete a widget

use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    http::{HeaderMap, StatusCode},
    middleware,
    routing::{delete, get, post},
};
use nexus_common::{
    error::{NexusError, NexusResult},
    models::widget::{
        CreateWidgetRequest, SubmitWidgetMessageRequest, WidgetConfig, WidgetEndpoint,
        WidgetMessage,
    },
};
use nexus_db::repository::widget_endpoints;
use rand::Rng;
use rand::distr::Alphanumeric;
use serde::Deserialize;
use std::sync::Arc;

use crate::{
    AppState,
    middleware::{AuthContext, check_rate_limit_with_fallback, extract_client_tag},
};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // Authenticated management
        .route(
            "/widget",
            get(list_widgets)
                .post(create_widget)
                .route_layer(middleware::from_fn(
                    crate::middleware::combined_auth_middleware,
                )),
        )
        .route(
            "/widget/{widget_id}",
            delete(delete_widget).route_layer(middleware::from_fn(
                crate::middleware::combined_auth_middleware,
            )),
        )
        // Public — no auth required
        .route("/widget/{widget_id}/config", get(get_widget_config))
        .route(
            "/widget/{widget_id}/messages",
            post(submit_message).get(get_messages),
        )
}

fn generate_widget_id() -> String {
    rand::rng()
        .sample_iter(Alphanumeric)
        .take(24)
        .map(char::from)
        .collect::<String>()
        .to_lowercase()
}

// ── Authenticated management ─────────────────────────────────────────────────

async fn list_widgets(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> NexusResult<Json<Vec<WidgetEndpoint>>> {
    let widgets =
        widget_endpoints::list_widgets_for_owner(&state.db.pool, auth.user_id).await?;
    Ok(Json(widgets))
}

async fn create_widget(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<CreateWidgetRequest>,
) -> NexusResult<(StatusCode, Json<WidgetEndpoint>)> {
    let tag = extract_client_tag(&headers);
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:widget:create:{}", auth.user_id),
        10,
        3600,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:widget:create:tag:{tag}"),
        20,
        3600,
    )
    .await?;

    if body.name.trim().is_empty() || body.name.len() > 100 {
        return Err(NexusError::Validation {
            message: "name must be 1–100 characters".into(),
        });
    }

    let widget_id = generate_widget_id();

    let widget = widget_endpoints::create_widget(
        &state.db.pool,
        auth.user_id,
        body.channel_id,
        body.name.trim(),
        &widget_id,
        body.allowed_origins.as_deref(),
        body.greeting.as_deref(),
    )
    .await?;

    Ok((StatusCode::CREATED, Json(widget)))
}

async fn delete_widget(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(widget_id): Path<String>,
) -> NexusResult<StatusCode> {
    let deleted =
        widget_endpoints::delete_widget(&state.db.pool, &widget_id, auth.user_id).await?;

    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(NexusError::NotFound {
            resource: "widget".to_string(),
        })
    }
}

// ── Public endpoints ──────────────────────────────────────────────────────────

async fn get_widget_config(
    State(state): State<Arc<AppState>>,
    Path(widget_id): Path<String>,
) -> NexusResult<Json<WidgetConfig>> {
    let config = widget_endpoints::get_widget_config(&state.db.pool, &widget_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "widget".to_string(),
        })?;

    if !config.active {
        return Err(NexusError::NotFound {
            resource: "widget".to_string(),
        });
    }

    Ok(Json(config))
}

async fn submit_message(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(widget_id): Path<String>,
    Json(body): Json<SubmitWidgetMessageRequest>,
) -> NexusResult<(StatusCode, Json<WidgetMessage>)> {
    let tag = extract_client_tag(&headers);

    // Rate limit: 5 messages per minute per visitor_id, 30 per IP tag
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:widget:msg:{}:{}", widget_id, body.visitor_id),
        5,
        60,
    )
    .await?;
    check_rate_limit_with_fallback(
        state.db.redis.as_ref(),
        format!("rl:widget:msg:tag:{tag}"),
        30,
        60,
    )
    .await?;

    if body.visitor_id.is_empty() || body.visitor_id.len() > 64 {
        return Err(NexusError::Validation {
            message: "visitor_id must be 1–64 characters".into(),
        });
    }

    let content = body.content.trim().to_string();
    if content.is_empty() || content.len() > 2000 {
        return Err(NexusError::Validation {
            message: "content must be 1–2000 characters".into(),
        });
    }

    let widget = widget_endpoints::get_widget_by_id(&state.db.pool, &widget_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "widget".to_string(),
        })?;

    if !widget.active {
        return Err(NexusError::NotFound {
            resource: "widget".to_string(),
        });
    }

    let visitor_name = body.visitor_name.as_deref().map(|n| {
        let n = n.trim();
        if n.len() > 50 { &n[..50] } else { n }
    });

    let msg = widget_endpoints::insert_widget_message(
        &state.db.pool,
        &widget_id,
        &body.visitor_id,
        visitor_name,
        &content,
    )
    .await?;

    Ok((StatusCode::CREATED, Json(msg)))
}

#[derive(Deserialize)]
struct GetMessagesQuery {
    visitor_id: String,
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    50
}

async fn get_messages(
    State(state): State<Arc<AppState>>,
    Path(widget_id): Path<String>,
    Query(q): Query<GetMessagesQuery>,
) -> NexusResult<Json<Vec<WidgetMessage>>> {
    if q.visitor_id.is_empty() || q.visitor_id.len() > 64 {
        return Err(NexusError::Validation {
            message: "visitor_id must be 1–64 characters".into(),
        });
    }

    let limit = q.limit.clamp(1, 100);

    // Verify widget exists and is active
    let widget = widget_endpoints::get_widget_by_id(&state.db.pool, &widget_id)
        .await?
        .ok_or(NexusError::NotFound {
            resource: "widget".to_string(),
        })?;

    if !widget.active {
        return Err(NexusError::NotFound {
            resource: "widget".to_string(),
        });
    }

    let msgs =
        widget_endpoints::get_widget_messages(&state.db.pool, &widget_id, &q.visitor_id, limit)
            .await?;

    Ok(Json(msgs))
}
