-- Zero-retention privacy: Chat never stores a client address or user agent.
-- Scrub first so nothing survives even if a later statement is interrupted,
-- then drop the columns. Rate limits key on the proxy's opaque client tag.
UPDATE refresh_tokens SET ip_address = NULL, user_agent = NULL;
ALTER TABLE refresh_tokens DROP COLUMN IF EXISTS ip_address;
ALTER TABLE refresh_tokens DROP COLUMN IF EXISTS user_agent;
UPDATE instance_audit_log SET ip_address = NULL, user_agent = NULL;
ALTER TABLE instance_audit_log DROP COLUMN IF EXISTS ip_address;
ALTER TABLE instance_audit_log DROP COLUMN IF EXISTS user_agent;
ALTER TABLE push_subscriptions DROP COLUMN IF EXISTS user_agent;
