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

-- ---------------------------------------------------------------------------------------------
-- E19: usage billing to production grade (src/billing/ledger.ts). Additive: re-applying this file
-- to a database provisioned before E19 adds the columns and tables below and changes nothing else.
-- ---------------------------------------------------------------------------------------------

-- Receipts carry the list price next to the (possibly discounted) settled cost, how the call was
-- funded (`wallet` | `commit`; NULL on rows written before E19 = wallet) and the commit applied.
ALTER TABLE agent_receipts ADD COLUMN IF NOT EXISTS list_cost BIGINT;
ALTER TABLE agent_receipts ADD COLUMN IF NOT EXISTS funding TEXT;
ALTER TABLE agent_receipts ADD COLUMN IF NOT EXISTS commit_id TEXT;
CREATE INDEX IF NOT EXISTS agent_receipts_principal_month ON agent_receipts (principal_id, month);

-- Dispute resolution: status moves open → resolved | rejected once; `credit_micros` is what the
-- resolution credited (the ledger entry is `dispute:<dispute_id>` in agent_adjustments).
ALTER TABLE agent_disputes ADD COLUMN IF NOT EXISTS credit_micros BIGINT;
ALTER TABLE agent_disputes ADD COLUMN IF NOT EXISTS resolved_at BIGINT;

-- Annual commit contracts. One subject (`principal` | `tenant`) has at most one commit per
-- instant (the admin route refuses overlaps). `drawn` is discounted, commit-funded settled spend;
-- `overage` the part of it above `amount`; `overage_reported` how much of that has become Stripe
-- usage records (whole cents).
CREATE TABLE IF NOT EXISTS agent_commits (
  commit_id                 TEXT   PRIMARY KEY,
  subject_kind              TEXT   NOT NULL,
  subject_id                TEXT   NOT NULL,
  amount_micros             BIGINT NOT NULL CHECK (amount_micros >= 0),
  discount_bps              BIGINT NOT NULL CHECK (discount_bps >= 1000 AND discount_bps <= 2500),
  starts_at                 BIGINT NOT NULL,
  ends_at                   BIGINT NOT NULL,
  drawn_micros              BIGINT NOT NULL DEFAULT 0 CHECK (drawn_micros >= 0),
  overage_micros            BIGINT NOT NULL DEFAULT 0 CHECK (overage_micros >= 0),
  overage_reported_micros   BIGINT NOT NULL DEFAULT 0 CHECK (overage_reported_micros >= 0),
  created_at                BIGINT NOT NULL,
  stripe_customer_id        TEXT,
  stripe_subscription_item  TEXT
);
CREATE INDEX IF NOT EXISTS agent_commits_subject ON agent_commits (subject_kind, subject_id, starts_at);

-- Commit overage turned into Stripe usage records. `report_id` is the Stripe Idempotency-Key and
-- is derived from the commit and the reported high-water mark, so a retried claim cannot double.
CREATE TABLE IF NOT EXISTS agent_commit_overage_reports (
  report_id             TEXT   PRIMARY KEY,
  commit_id             TEXT   NOT NULL,
  subscription_item     TEXT   NOT NULL,
  cents                 BIGINT NOT NULL CHECK (cents > 0),
  ts                    BIGINT NOT NULL,
  sent_at               BIGINT
);
CREATE INDEX IF NOT EXISTS agent_commit_overage_unsent ON agent_commit_overage_reports (sent_at, ts);

-- Signed ledger entries: operator credits (+) and refunds (−). The primary key is the idempotency
-- key. `applied_micros` is what the wallet moved by (a debit stops at zero). `reason` is a fixed
-- code (src/billing/ledger.ts ADJUSTMENT_REASONS), never free text.
CREATE TABLE IF NOT EXISTS agent_adjustments (
  entry_id        TEXT   PRIMARY KEY,
  principal_id    TEXT   NOT NULL,
  micros          BIGINT NOT NULL,
  applied_micros  BIGINT NOT NULL,
  kind            TEXT   NOT NULL,
  reason          TEXT   NOT NULL,
  ref             TEXT,
  ts              BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS agent_adjustments_principal ON agent_adjustments (principal_id, ts);

-- Per-charge refunded high-water mark (Stripe `charge.refunded` and the admin refund route converge
-- on it, so a refund is debited once whichever arrives first).
CREATE TABLE IF NOT EXISTS agent_charge_refunds (
  charge_id        TEXT   PRIMARY KEY,
  principal_id     TEXT   NOT NULL,
  refunded_micros  BIGINT NOT NULL CHECK (refunded_micros >= 0)
);

-- Which principal a Stripe PaymentIntent paid for (written when its Checkout completes), so a later
-- refund event finds the wallet without trusting metadata alone.
CREATE TABLE IF NOT EXISTS agent_stripe_payments (
  payment_intent  TEXT   PRIMARY KEY,
  principal_id    TEXT   NOT NULL,
  session_id      TEXT   NOT NULL,
  micros          BIGINT NOT NULL CHECK (micros >= 0),
  ts              BIGINT NOT NULL
);

-- Checkout sessions the top-up route created: open → completed | expired, once.
CREATE TABLE IF NOT EXISTS agent_checkout_sessions (
  session_id    TEXT   PRIMARY KEY,
  principal_id  TEXT   NOT NULL,
  micros        BIGINT NOT NULL CHECK (micros >= 0),
  status        TEXT   NOT NULL,
  created_at    BIGINT NOT NULL,
  updated_at    BIGINT NOT NULL
);

-- Closed monthly statements: the rendered JSON (ids, units, counters, amounts — no reason text,
-- no bodies) and its SHA-256. Written once; never updated.
CREATE TABLE IF NOT EXISTS agent_statements (
  principal_id  TEXT   NOT NULL,
  month         TEXT   NOT NULL,
  body          TEXT   NOT NULL,
  hash          TEXT   NOT NULL,
  closed_at     BIGINT NOT NULL,
  PRIMARY KEY (principal_id, month)
);

-- E14-T3: a ShogunAI licence subject (a licence id or `account:<acc>`) linked to a principal, with
-- the plan and when a token proving it last verified. The monthly Pro grant reads this.
CREATE TABLE IF NOT EXISTS agent_licence_links (
  licence_subject  TEXT   PRIMARY KEY,
  principal_id     TEXT   NOT NULL,
  plan             TEXT   NOT NULL,
  verified_at      BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS agent_licence_links_plan ON agent_licence_links (plan, verified_at);
