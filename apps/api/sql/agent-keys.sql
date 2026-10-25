-- Agent authentication and the prepaid wallet (docs/implementation-plan.md E3-T1, E3-T3, E9).
--
-- Provisioning, not a refinery migration (same rule as shared-store.sql): apply once per
-- database with
--   psql "$RELAY_DATABASE_URL" -v ON_ERROR_STOP=1 -f apps/api/sql/agent-keys.sql
-- Idempotent (IF NOT EXISTS) and additive only — never DROP / RENAME / ALTER COLUMN here.
--
-- Content-free by construction (CLAUDE.md invariant 2): opaque ids, hashes, counters, dates.
-- No key material: `agent_keys.hash` is PBKDF2-SHA256 over the key with a per-row salt
-- (src/agent-keys.ts); `prefix` is the first characters of the random part, for lookup only.
-- No request or response bodies: a receipt is method, tool name, unit, quantity and cost.
--
-- Written in the dialect subset the unit tests' SQLite double accepts (test/sql-double.ts), so
-- this file is what test/agent-store.test.ts runs the store against in CI.

-- A principal: one person's world model (tenancy §2.2). Self-registration (POST /v1/agents)
-- creates a tenant of one, so tenant_id = principal_id there. Caps NULL = the defaults (src/caps.ts).
CREATE TABLE IF NOT EXISTS agent_principals (
  principal_id     TEXT   PRIMARY KEY,
  tenant_id        TEXT   NOT NULL,
  created_at       BIGINT NOT NULL,
  ref              TEXT,
  cap_day_micros   BIGINT,
  cap_month_micros BIGINT
);

-- API keys. `scopes` is a comma-separated subset of read,write,run,admin. `mode` is live|test.
-- `expires_at` / `revoked_at` are ms since the epoch, NULL = never.
CREATE TABLE IF NOT EXISTS agent_keys (
  key_id           TEXT   PRIMARY KEY,
  prefix           TEXT   NOT NULL,
  hash             TEXT   NOT NULL,
  mode             TEXT   NOT NULL,
  principal_id     TEXT   NOT NULL,
  tenant_id        TEXT   NOT NULL,
  scopes           TEXT   NOT NULL,
  created_at       BIGINT NOT NULL,
  expires_at       BIGINT,
  revoked_at       BIGINT,
  ref              TEXT,
  cap_day_micros   BIGINT,
  cap_month_micros BIGINT
);
CREATE INDEX IF NOT EXISTS agent_keys_prefix ON agent_keys (prefix);
CREATE INDEX IF NOT EXISTS agent_keys_principal ON agent_keys (principal_id);

-- The wallet: micro-USD. `reserved` is held by unsettled receipts; spendable = balance - reserved.
CREATE TABLE IF NOT EXISTS agent_wallets (
  principal_id  TEXT   PRIMARY KEY,
  balance       BIGINT NOT NULL DEFAULT 0 CHECK (balance >= 0),
  reserved      BIGINT NOT NULL DEFAULT 0 CHECK (reserved >= 0),
  updated_at    BIGINT NOT NULL
);

-- Credits. The primary key is the idempotency key: a Stripe event id (`evt_…`), or
-- `grant:signup:<principal>` for the free allowance. A duplicate insert credits nothing.
CREATE TABLE IF NOT EXISTS agent_topups (
  topup_id      TEXT   PRIMARY KEY,
  principal_id  TEXT   NOT NULL,
  micros        BIGINT NOT NULL CHECK (micros >= 0),
  source        TEXT   NOT NULL,
  ts            BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS agent_topups_principal ON agent_topups (principal_id, ts);

-- Per-call receipts (E9-T7) doubling as the reservation ledger: `estimate` is held at
-- reservation, `cost` is the settled figure, `settled` flips once (reserve-then-settle, the
-- same pattern as relay_spend_batches).
CREATE TABLE IF NOT EXISTS agent_receipts (
  receipt_id    TEXT     PRIMARY KEY,
  principal_id  TEXT     NOT NULL,
  key_id        TEXT     NOT NULL,
  method        TEXT     NOT NULL,
  name          TEXT,
  unit          TEXT     NOT NULL,
  quantity      BIGINT   NOT NULL CHECK (quantity >= 0),
  estimate      BIGINT   NOT NULL CHECK (estimate >= 0),
  cost          BIGINT   NOT NULL DEFAULT 0 CHECK (cost >= 0),
  settled       SMALLINT NOT NULL DEFAULT 0,
  day           TEXT     NOT NULL,
  month         TEXT     NOT NULL,
  ts            BIGINT   NOT NULL
);
CREATE INDEX IF NOT EXISTS agent_receipts_principal_ts ON agent_receipts (principal_id, ts);

-- Cap counters (E9-T3): subject is `key:<key_id>` or `principal:<principal_id>`; period is a UTC
-- day `YYYY-MM-DD` or month `YYYY-MM`. Reserved estimates are booked here and corrected at
-- settlement, so a cap sees reservations, not just settled cost.
CREATE TABLE IF NOT EXISTS agent_spend_periods (
  subject  TEXT   NOT NULL,
  period   TEXT   NOT NULL,
  micros   BIGINT NOT NULL DEFAULT 0 CHECK (micros >= 0),
  PRIMARY KEY (subject, period)
);

-- Disputes (E9-T7). One open dispute per receipt; `reason` is a short category/free text the
-- agent sent (bounded in code), never a request body.
CREATE TABLE IF NOT EXISTS agent_disputes (
  dispute_id    TEXT   PRIMARY KEY,
  receipt_id    TEXT   NOT NULL,
  principal_id  TEXT   NOT NULL,
  reason        TEXT   NOT NULL,
  status        TEXT   NOT NULL DEFAULT 'open',
  ts            BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS agent_disputes_receipt ON agent_disputes (receipt_id);

-- Spent proof-of-work challenges (POST /v1/agents): a challenge is single-use until it expires.
-- `challenge_hash` is SHA-256 of the challenge string; rows past `expires_at` are deleted by the
-- store's maintenance.
CREATE TABLE IF NOT EXISTS agent_pow_spent (
  challenge_hash  TEXT   PRIMARY KEY,
  expires_at      BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS agent_pow_spent_expires ON agent_pow_spent (expires_at);
