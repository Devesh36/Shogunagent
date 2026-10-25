-- Provider payouts (docs/implementation-plan.md E29; apps/api/src/payouts.ts, docs/api/payouts.md).
--
-- Provisioning, not a refinery migration (same rule as agent-keys.sql): apply once per database
-- with
--   psql "$RELAY_DATABASE_URL" -v ON_ERROR_STOP=1 -f apps/api/sql/provider-payouts.sql
-- Idempotent (IF NOT EXISTS) and additive only — never DROP / RENAME / ALTER COLUMN here. New
-- tables only; nothing in agent-keys.sql changes.
--
-- Content-free by construction (CLAUDE.md invariant 2): provider and tool ids, opaque payout
-- references (a Stripe Connect account id), outcomes, counters, amounts and timestamps. No request
-- or response body, no agent, no principal of the caller, no bank data.
--
-- Written in the dialect subset the unit tests' SQLite double accepts (test/sql-double.ts), so
-- this file is what test/payouts.test.ts runs the store against in CI.

-- A listed provider's payout account. Created by an operator when the listing PR is merged
-- (`POST /v1/admin/providers`), which names the principal whose agent keys act as the provider
-- (`/v1/providers/me/*`). `stripe_account_id` is the Connect Express account created at
-- onboarding; `payout_account_ref` is bound to it once Stripe reports the account can receive
-- payouts. status: unboarded | onboarding | active | restricted.
CREATE TABLE IF NOT EXISTS provider_accounts (
  provider_id         TEXT   PRIMARY KEY,
  owner_principal_id  TEXT   NOT NULL,
  stripe_account_id   TEXT,
  payout_account_ref  TEXT,
  status              TEXT   NOT NULL,
  created_at          BIGINT NOT NULL,
  updated_at          BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS provider_accounts_owner ON provider_accounts (owner_principal_id);

-- One row per settled router run: the idempotency record of the settlement seam
-- (`POST /v1/internal/router/settlements`, keyed by the router's run id). Kept 400 days like
-- receipts; the daily accruals below are what statements and payouts read.
CREATE TABLE IF NOT EXISTS provider_settlements (
  run_id          TEXT   PRIMARY KEY,
  provider_id     TEXT   NOT NULL,
  tool_id         TEXT   NOT NULL,
  day             TEXT   NOT NULL,
  outcome         TEXT   NOT NULL,
  provider_micros BIGINT NOT NULL CHECK (provider_micros >= 0),
  take_micros     BIGINT NOT NULL CHECK (take_micros >= 0),
  ts              BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS provider_settlements_ts ON provider_settlements (ts);

-- What a provider earned per UTC day. payee: connect (paid out through Stripe Connect) |
-- first_party (our own revenue, never paid out) | passthrough (an imported marketplace catalog,
-- settled on that marketplace's invoice). `payout_account_ref` is the reference the provider's
-- provider.json declared on the last run of the day (audit; payouts go to the bound account).
CREATE TABLE IF NOT EXISTS provider_accruals (
  provider_id         TEXT   NOT NULL,
  day                 TEXT   NOT NULL,
  payee               TEXT   NOT NULL,
  payout_account_ref  TEXT,
  runs                BIGINT NOT NULL DEFAULT 0,
  ok_runs             BIGINT NOT NULL DEFAULT 0,
  empty_runs          BIGINT NOT NULL DEFAULT 0,
  failed_runs         BIGINT NOT NULL DEFAULT 0,
  provider_micros     BIGINT NOT NULL DEFAULT 0,
  take_micros         BIGINT NOT NULL DEFAULT 0,
  PRIMARY KEY (provider_id, day)
);

-- Deductions from a provider's earnings: a refund of runs it was paid for (status applied), or a
-- dispute (open → won | lost). An open dispute holds the provider's payout; a lost one is
-- deducted. The primary key is the operator's Idempotency-Key.
CREATE TABLE IF NOT EXISTS provider_adjustments (
  entry_id     TEXT   PRIMARY KEY,
  provider_id  TEXT   NOT NULL,
  kind         TEXT   NOT NULL,
  micros       BIGINT NOT NULL CHECK (micros >= 0),
  status       TEXT   NOT NULL,
  reason       TEXT   NOT NULL,
  run_id       TEXT,
  ts           BIGINT NOT NULL,
  resolved_at  BIGINT
);
CREATE INDEX IF NOT EXISTS provider_adjustments_provider ON provider_adjustments (provider_id);

-- One payout decision per provider per month. status: pending (claimed; the transfer is being
-- sent with `idempotency_key`) | paid | held (open dispute, account not active, reference
-- mismatch) | below_minimum (carried to the next month). Amounts are cumulative, so a held or
-- carried balance is paid by a later month and nothing is paid twice.
CREATE TABLE IF NOT EXISTS provider_payouts (
  provider_id             TEXT   NOT NULL,
  month                   TEXT   NOT NULL,
  status                  TEXT   NOT NULL,
  reason                  TEXT,
  earned_micros           BIGINT NOT NULL,
  refunded_micros         BIGINT NOT NULL,
  disputes_lost_micros    BIGINT NOT NULL,
  previously_paid_micros  BIGINT NOT NULL,
  payable_micros          BIGINT NOT NULL,
  amount_micros           BIGINT NOT NULL,
  destination             TEXT,
  transfer_id             TEXT,
  idempotency_key         TEXT   NOT NULL,
  created_at              BIGINT NOT NULL,
  updated_at              BIGINT NOT NULL,
  PRIMARY KEY (provider_id, month)
);
