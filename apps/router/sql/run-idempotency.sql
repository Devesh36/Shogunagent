-- The router's cross-isolate run idempotency store (docs/implementation-plan.md E48;
-- apps/router/src/idempotency.ts, apps/router/README.md "Idempotency").
--
-- Provisioning, not a refinery migration (the same rule as settlement-outbox.sql): apply once per
-- database the router's Hyperdrive binding points at, as the router's role, with
--   psql "$ROUTER_DATABASE_URL" -v ON_ERROR_STOP=1 -f apps/router/sql/run-idempotency.sql
--   psql "$ROUTER_DATABASE_URL" -v ON_ERROR_STOP=1 -f apps/router/sql/run-idempotency-permissions.sql
-- Idempotent (IF NOT EXISTS) and additive only: never DROP / RENAME / ALTER COLUMN here.
--
-- Content-free by construction (CLAUDE.md invariants 1, 2): one row per (actor, idempotency key)
-- holding the outcome REFERENCE of a run, never its body. `actor` is the router's opaque 12-hex
-- hash of the caller's key; `key_hash` is the SHA-256 of the agent's idempotency key (the key
-- itself is not stored); `fingerprint` is a salted one-way hash of (actor, key, tool, input) used
-- only to detect a reused key with a different input. Then the run id, the state, the outcome and
-- the usage numbers the caller was charged. No input, no result, no header, no credential.
--
-- Written in the dialect subset the router's SQLite test double accepts
-- (test/idempotency.test.ts), so this file is what CI runs the store against.
--
-- Life of a row:
--   running   claimed atomically right before the run reserves money. A second claim for the same
--             key (another isolate, a retry) does not win: the caller is told the run is in
--             progress. A row still running after the outbox's abandon window (10 minutes) is a
--             run whose isolate died; a retry with the same input takes it over.
--   (deleted) the run failed or was refused, so nothing was charged: the claim is released and the
--             same key may be retried.
--   done      the run produced a result: written before its settlement. Replays of the key answer
--             with the run id and these numbers. Expires 24 hours after completion; the router's
--             cron deletes expired rows.
CREATE TABLE IF NOT EXISTS router_run_idempotency (
  actor                 TEXT    NOT NULL,
  key_hash              TEXT    NOT NULL,
  tool_id               TEXT    NOT NULL,
  fingerprint           TEXT    NOT NULL,
  run_id                TEXT    NOT NULL,
  state                 TEXT    NOT NULL,
  claimed_at            BIGINT  NOT NULL,
  expires_at            BIGINT  NOT NULL,
  outcome               TEXT,
  usage_unit            TEXT,
  usage_quantity        DOUBLE PRECISION,
  usage_cost_micro_usd  BIGINT,
  completed_at          BIGINT,
  PRIMARY KEY (actor, key_hash)
);
CREATE INDEX IF NOT EXISTS router_run_idempotency_expires ON router_run_idempotency (expires_at);
