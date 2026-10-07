-- Zero-retention privacy: no stored client address. The lite schema only ever
-- had refresh_tokens.ip_address (no user_agent column, no instance_audit_log
-- or push_subscriptions table), so that is the only column to scrub and drop.
UPDATE refresh_tokens SET ip_address = NULL;
ALTER TABLE refresh_tokens DROP COLUMN ip_address;
