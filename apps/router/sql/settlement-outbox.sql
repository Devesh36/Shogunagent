-- The router's durable settlement outbox (docs/implementation-plan.md E33; apps/router/src/outbox.ts,
-- docs/api/payouts.md "Delivery").
--
-- Provisioning, not a refinery migration (the same rule as apps/api/sql/*.sql): apply once per
-- database the router's Hyperdrive binding points at, with
--   psql "$ROUTER_DATABASE_URL" -v ON_ERROR_STOP=1 -f apps/router/sql/settlement-outbox.sql
--   psql "$ROUTER_DATABASE_URL" -v ON_ERROR_STOP=1 -f apps/router/sql/settlement-outbox-permissions.sql
-- Idempotent (IF NOT EXISTS) and additive only: never DROP / RENAME / ALTER COLUMN here.
--
-- Content-free by construction (CLAUDE.md invariants 1, 2): one row per run the router settled,
-- holding exactly the fields of a `RunSettlement` (src/settlement.ts) plus delivery bookkeeping.
-- Tool and provider ids, an opaque payout reference, the marketplace tag, an outcome, a count, an
-- amount, the wallet reservation id. No input, no output, no body, no actor, no key, no header.
--
-- Written in the dialect subset the router's SQLite test double accepts (test/outbox.test.ts),
-- so this file is what CI runs the store against.
--
-- Life of a row:
--   open       written BEFORE apps/api is asked to hold money for the run: a draft settlement
--              (outcome transport_error, $0). If the router dies mid-run, the retry job promotes
--              a draft older than the abandon window to pending, and its delivery releases the
--              hold at $0 (failures cost nothing). A refused reservation deletes the draft.
--   pending    the run's final settlement, written BEFORE it is sent. Delivered inline once, then
--              by the retry job (cron) with exponential backoff until apps/api answers 2xx.
--   delivered  apps/api recorded it (idempotent by run_id). Deleted after 30 days.
--   dead       apps/api rejected it as malformed (400/404/413/422). Kept for an operator, never
--              deleted: `UPDATE router_settlement_outbox SET state='pending', next_attempt_at=0
--              WHERE state='dead'` re-queues after a fix.
CREATE TABLE IF NOT EXISTS router_settlement_outbox (
  run_id              TEXT     PRIMARY KEY,
  state               TEXT     NOT NULL,
  receipt_id          TEXT,
  ts                  BIGINT   NOT NULL,
  tool_id             TEXT     NOT NULL,
  provider_id         TEXT     NOT NULL,
  first_party         SMALLINT NOT NULL,
  payout_account_ref  TEXT,
  marketplace         TEXT,
  outcome             TEXT     NOT NULL,
  quantity            BIGINT   NOT NULL,
  provider_micro_usd  BIGINT   NOT NULL,
  attempts            INTEGER  NOT NULL DEFAULT 0,
  next_attempt_at     BIGINT   NOT NULL,
  last_status         INTEGER,
  created_at          BIGINT   NOT NULL,
  updated_at          BIGINT   NOT NULL,
  delivered_at        BIGINT
);
CREATE INDEX IF NOT EXISTS router_settlement_outbox_due ON router_settlement_outbox (state, next_attempt_at);
CREATE INDEX IF NOT EXISTS router_settlement_outbox_open ON router_settlement_outbox (state, created_at);
