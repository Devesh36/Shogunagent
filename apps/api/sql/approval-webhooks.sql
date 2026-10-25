-- Outbound approval webhooks (docs/implementation-plan.md E15; apps/api/src/approvals/webhooks.ts).
--
-- Provisioning, not a refinery migration (same rule as agent-keys.sql): apply once per database
-- with
--   psql "$RELAY_DATABASE_URL" -v ON_ERROR_STOP=1 -f apps/api/sql/approval-webhooks.sql
-- Idempotent (IF NOT EXISTS) and additive only — never DROP / RENAME / ALTER COLUMN here.
--
-- Content-free by construction (CLAUDE.md invariant 2): ids, the endpoint URL the principal chose,
-- the subscribed event names and times. No signing secret (it is derived from
-- WEBHOOK_SIGNING_SECRET and the id, never stored) and no delivery bodies.
CREATE TABLE IF NOT EXISTS agent_webhooks (
  webhook_id   TEXT   PRIMARY KEY,
  principal_id TEXT   NOT NULL,
  url          TEXT   NOT NULL,
  events       TEXT   NOT NULL,
  created_at   BIGINT NOT NULL,
  deleted_at   BIGINT
);
CREATE INDEX IF NOT EXISTS agent_webhooks_principal ON agent_webhooks (principal_id);
