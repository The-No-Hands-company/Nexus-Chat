-- Privacy Part 3 — E2EE at rest.
--
-- Private keys must never leave the client. The server previously generated
-- Kyber-1024 + Dilithium-5 key pairs and stored both halves in kem_secret and
-- signing_secret. Clients now generate keys locally and register only their
-- public keys. Wipe any stored secrets and remove the columns.

UPDATE phantom_identities SET kem_secret = NULL, signing_secret = NULL;

ALTER TABLE phantom_identities DROP COLUMN IF EXISTS kem_secret;
ALTER TABLE phantom_identities DROP COLUMN IF EXISTS signing_secret;
