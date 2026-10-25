-- x402 payment replay guard (docs/implementation-plan.md E30; apps/api/src/billing/x402.ts).
-- Only needed when X402_ENABLED=1 (off by default; docs/decisions/0002-x402.md).
--
-- Provisioning, not a refinery migration (same rule as agent-keys.sql): apply once per database
-- with
--   psql "$RELAY_DATABASE_URL" -v ON_ERROR_STOP=1 -f apps/api/sql/x402-payments.sql
-- Idempotent (IF NOT EXISTS) and additive only — never DROP / RENAME / ALTER COLUMN here.
--
-- Content-free: the SHA-256 of the signed payment authorization (never the authorization), the
-- receipt it funded, its state (claimed | settled | failed | unsettled | retryable) and timestamps. A payload
-- presented twice finds its hash here and is refused. Rows are deleted 7 days after their last
-- update, past any authorization window the gateway accepts.
CREATE TABLE IF NOT EXISTS x402_payments (
  payload_hash  TEXT   PRIMARY KEY,
  receipt_id    TEXT   NOT NULL,
  state         TEXT   NOT NULL,
  created_at    BIGINT NOT NULL,
  updated_at    BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS x402_payments_updated ON x402_payments (updated_at);
