//! Widget endpoint models — embeddable customer chat for Nexus-Hosting sites.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A widget endpoint record (returned to authenticated owners).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WidgetEndpoint {
    pub id: Uuid,
    pub owner_id: Uuid,
    pub channel_id: Uuid,
    pub name: String,
    /// Public token embedded in the HTML snippet — identifies the widget.
    pub widget_id: String,
    pub allowed_origins: Option<String>,
    pub greeting: Option<String>,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Create a new widget endpoint.
#[derive(Debug, Deserialize)]
pub struct CreateWidgetRequest {
    pub channel_id: Uuid,
    pub name: String,
    pub allowed_origins: Option<String>,
    pub greeting: Option<String>,
}

/// A message submitted by a website visitor through the widget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WidgetMessage {
    pub id: Uuid,
    pub widget_id: String,
    pub visitor_id: String,
    pub visitor_name: Option<String>,
    pub content: String,
    pub reply_message_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// Payload sent by a visitor when submitting a message.
#[derive(Debug, Deserialize)]
pub struct SubmitWidgetMessageRequest {
    /// Browser-generated UUID stored in localStorage — not a user account.
    pub visitor_id: String,
    pub visitor_name: Option<String>,
    pub content: String,
}

/// Public widget config returned by GET /widget/:id/config — no secrets.
#[derive(Debug, Serialize)]
pub struct WidgetConfig {
    pub widget_id: String,
    pub name: String,
    pub greeting: Option<String>,
    pub active: bool,
}
