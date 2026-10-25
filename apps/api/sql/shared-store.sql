-- Shared transactional store for the AI gateway (issue #29 FR-SVC-04).
--
-- Provisioning, not a refinery migration: this is the relay's server-side ledger, not the
-- desktop memory DB. Apply once per database with
--   psql "$RELAY_DATABASE_URL" -v ON_ERROR_STOP=1 -f apps/api/sql/shared-store.sql
-- Idempotent (IF NOT EXISTS) and additive only — never DROP / RENAME / ALTER COLUMN here; the
-- ledger holds permanent request tombstones and a column change would need an explicit,
-- reviewed migration step. See docs/ai-gateway-runbook.md §9-4.
--
-- Content-free by construction: counters, hashes, opaque ids and dates. No chunk text, no
-- licence keys (licence ids are the token's `lic` claim, not the Keychain key), no IPs.
--
-- Written in the dialect subset the unit tests' in-memory Postgres double accepts too, so this
-- very file is what apps/api/test/pg-store.test.ts runs the store against in CI.
--
-- Retention (rows past their window) is deleted only by the periodic sweep
-- (`SqlUsageStore.maintenance`), never inside a request transaction.

-- Per-licence daily counters: chunks, and since the 2026-10-10 audit (H1) the licence's booked
-- spend in micro-USD — the per-licence budget.
CREATE TABLE IF NOT EXISTS relay_usage_days (
  date          TEXT   NOT NULL,
  license_id    TEXT   NOT NULL,
  chunks        BIGINT NOT NULL CHECK (chunks >= 0),
  spend_micros  BIGINT NOT NULL DEFAULT 0 CHECK (spend_micros >= 0),
  PRIMARY KEY (date, license_id)
);
-- Additive upgrade for a store provisioned before spend_micros existed (no-op on a fresh one).
-- No inline CHECK here: Postgres splits it into a separate ADD CONSTRAINT that still runs when the
-- column already exists, piling up an auto-named duplicate constraint on every re-apply. The CHECK
-- lives in CREATE TABLE above; the application never writes a negative value.
ALTER TABLE relay_usage_days ADD COLUMN IF NOT EXISTS spend_micros BIGINT NOT NULL DEFAULT 0;

-- client_request_id identities (#343). Rows are permanent tombstones: deleting one would
-- authorize a duplicate paid submission.
CREATE TABLE IF NOT EXISTS relay_usage_idem (
  license_id   TEXT NOT NULL,
  device_id    TEXT NOT NULL,
  purpose      TEXT NOT NULL,
  request_id   TEXT NOT NULL,
  hash         TEXT NOT NULL,
  date         TEXT NOT NULL,
  provider_id  TEXT,
  relay_id     TEXT,
  claim_token  TEXT,
  state        TEXT,
  PRIMARY KEY (license_id, device_id, purpose, request_id)
);
CREATE INDEX IF NOT EXISTS relay_usage_idem_batch ON relay_usage_idem ((COALESCE(relay_id, provider_id)));

-- Batch ownership (GET /v1/batch/{id} may only read back the licence's own batches).
CREATE TABLE IF NOT EXISTS relay_usage_owners (
  batch_id     TEXT PRIMARY KEY,
  license_id   TEXT NOT NULL,
  date         TEXT NOT NULL,
  device_id    TEXT,
  provider_id  TEXT
);
CREATE INDEX IF NOT EXISTS relay_usage_owners_date ON relay_usage_owners (date);

-- Global daily spend breaker (FR-SVC-08), micro-USD.
CREATE TABLE IF NOT EXISTS relay_spend_days (
  date    TEXT   PRIMARY KEY,
  micros  BIGINT NOT NULL CHECK (micros >= 0)
);
-- `settled` flips once, by whichever path settles first (the client's results read or the relay's
-- settlement sweep); `license_id` lets a settlement move the licence's counter too (H1).
-- `checked_at` (ms since the epoch, NULL = never) is when the sweep last found the batch still
-- processing; the sweep skips a batch checked within SETTLE_RECHECK_SECONDS so one tick's budget
-- goes to batches that may have ended (second-pass review of PR #1022).
CREATE TABLE IF NOT EXISTS relay_spend_batches (
  batch_id         TEXT     PRIMARY KEY,
  date             TEXT     NOT NULL,
  estimate_micros  BIGINT   NOT NULL CHECK (estimate_micros >= 0),
  settled          SMALLINT NOT NULL DEFAULT 0,
  license_id       TEXT,
  checked_at       BIGINT
);
ALTER TABLE relay_spend_batches ADD COLUMN IF NOT EXISTS license_id TEXT;
ALTER TABLE relay_spend_batches ADD COLUMN IF NOT EXISTS checked_at BIGINT;
CREATE INDEX IF NOT EXISTS relay_spend_batches_open ON relay_spend_batches (settled, date);

-- Token buckets shared by every relay instance (licence + device + route keys).
CREATE TABLE IF NOT EXISTS relay_rate_buckets (
  bucket_key  TEXT             PRIMARY KEY,
  tokens      DOUBLE PRECISION NOT NULL,
  at_ms       BIGINT           NOT NULL
);
CREATE INDEX IF NOT EXISTS relay_rate_buckets_at ON relay_rate_buckets (at_ms);
