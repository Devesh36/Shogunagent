-- Hosted connector and relay credentials (docs/implementation-plan.md E5-T5).
--
-- Provisioning, not a refinery migration (same rule as shared-store.sql): apply once per
-- database with
--   psql "$RELAY_DATABASE_URL" -v ON_ERROR_STOP=1 -f apps/api/sql/connector-tokens.sql
-- Idempotent (IF NOT EXISTS) and additive only — never DROP / RENAME / ALTER COLUMN here.
--
-- The rows behind `shogun_integrations::token_store::RemoteTokenStore`: one KMS envelope per
-- principal × slot. No plaintext column exists to write a token into (CLAUDE.md invariant 7):
-- `ciphertext` is the KMS-sealed value, bound to (principal_id, account) as associated data so a
-- row copied to another principal or slot fails to open; `key_id` names the KMS key version that
-- sealed it, for rotation. `account` is the slot name (`<source>-tokenset`, `batch-relay-bearer`).
-- No request or response bodies, no provider identity beyond the slot name.
--
-- Nothing reads or writes this table yet: the gateway's KMS client and Postgres backend land with
-- the hosted store.

CREATE TABLE IF NOT EXISTS connector_tokens (
  principal_id TEXT   NOT NULL,
  account      TEXT   NOT NULL,
  key_id       TEXT   NOT NULL,
  ciphertext   BYTEA  NOT NULL,
  updated_at   BIGINT NOT NULL,
  PRIMARY KEY (principal_id, account)
);
