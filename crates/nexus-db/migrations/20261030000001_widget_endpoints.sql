-- Widget endpoints: embeddable customer chat widgets for Nexus-Hosting sites.
-- A widget has a public widget_id (embedded in the HTML) and a secret token
-- (never exposed in the browser). Guest messages go to a designated channel.

CREATE TABLE widget_endpoints (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id    UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    channel_id  UUID NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    -- Public token embedded in the <script> tag — not secret, identifies the widget
    widget_id   TEXT NOT NULL UNIQUE,
    -- Hashed secret (SHA-256 hex) for future server-to-server use; not used by guests
    secret_hash TEXT,
    -- Optional CORS origin whitelist (comma-separated). NULL = allow all.
    allowed_origins TEXT,
    -- Soft greeting shown in the widget before the user types
    greeting    TEXT,
    active      BOOLEAN NOT NULL DEFAULT TRUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_widget_endpoints_owner   ON widget_endpoints(owner_id);
CREATE INDEX idx_widget_endpoints_channel ON widget_endpoints(channel_id);
CREATE INDEX idx_widget_endpoints_active  ON widget_endpoints(active);

-- Guest messages submitted via widget (no user account required).
-- Each row is one visitor message; replies from the channel are read via SSE.
CREATE TABLE widget_messages (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    widget_id   TEXT NOT NULL REFERENCES widget_endpoints(widget_id) ON DELETE CASCADE,
    -- Pseudonymous visitor identifier (browser-generated UUID, not a user account)
    visitor_id  TEXT NOT NULL,
    visitor_name TEXT,
    content     TEXT NOT NULL CHECK (char_length(content) BETWEEN 1 AND 2000),
    -- NULL until a team member replies; set to the nexus message id
    reply_message_id UUID,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_widget_messages_widget   ON widget_messages(widget_id);
CREATE INDEX idx_widget_messages_visitor  ON widget_messages(visitor_id);
CREATE INDEX idx_widget_messages_created  ON widget_messages(created_at);
