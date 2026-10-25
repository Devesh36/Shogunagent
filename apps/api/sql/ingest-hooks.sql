-- Ingest hook registrations (docs/implementation-plan.md E44, docs/api/ingest-sources.md).
--
-- Provisioning, not a refinery migration (same rule as shared-store.sql): apply once per
-- database with
--   psql "$RELAY_DATABASE_URL" -v ON_ERROR_STOP=1 -f apps/api/sql/ingest-hooks.sql
-- before INGEST_HOOKS_SECRET is set. Idempotent (IF NOT EXISTS) and additive only — never
-- DROP / RENAME / ALTER COLUMN here. Rollback: nothing reads the table once INGEST_HOOKS_SECRET
-- is unset; `DROP TABLE ingest_hooks` removes it (registrations are lost, deliveries already
-- stored stay in each principal's own store).
--
-- One row per hook: which principal a provider delivery belongs to and what its signature is
-- checked against. No delivery body, no event, no plaintext secret has a column here (CLAUDE.md
-- invariants 2 and 7):
--   generation           derived secrets (GitHub, Google Calendar) are HMAC(INGEST_HOOKS_SECRET,
--                        hook id, generation); only the number is stored.
--   sealed_secret        provider-issued secrets (Linear, Slack) and Notion's verification
--                        token, as an AES-256-GCM envelope bound to the hook id.
--   route_key            Gmail: an HMAC of the mailbox address, never the address.
--   create_key_hash      SHA-256 of the creating request's Idempotency-Key.
-- A deleted hook keeps its row (audit) with its envelopes and route cleared.

CREATE TABLE IF NOT EXISTS ingest_hooks (
  hook_id                TEXT    PRIMARY KEY,
  principal_id           TEXT    NOT NULL,
  tenant_id              TEXT    NOT NULL,
  key_id                 TEXT    NOT NULL,
  provider               TEXT    NOT NULL,
  status                 TEXT    NOT NULL,
  generation             INTEGER NOT NULL,
  sealed_secret          TEXT,
  previous_generation    INTEGER,
  previous_sealed_secret TEXT,
  previous_valid_until   BIGINT,
  route_key              TEXT,
  create_key_hash        TEXT,
  created_at             BIGINT  NOT NULL,
  rotated_at             BIGINT,
  last_delivery_at       BIGINT,
  deleted_at             BIGINT
);

CREATE INDEX IF NOT EXISTS ingest_hooks_principal ON ingest_hooks (principal_id) WHERE deleted_at IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS ingest_hooks_route ON ingest_hooks (provider, route_key) WHERE deleted_at IS NULL AND route_key IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS ingest_hooks_create_key ON ingest_hooks (principal_id, create_key_hash) WHERE deleted_at IS NULL AND create_key_hash IS NOT NULL;
