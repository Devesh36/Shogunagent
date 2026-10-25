-- #1101: content-free live admission; apply before LIVE_AI_ENABLED=1.
-- Existing Batch tables are untouched. Monetary units are integer micro-USD.
CREATE TABLE IF NOT EXISTS relay_live_subjects (
  subject TEXT PRIMARY KEY,
  blocked INTEGER NOT NULL DEFAULT 0 CHECK (blocked IN (0,1))
);
CREATE TABLE IF NOT EXISTS relay_live_counters (
  subject TEXT NOT NULL,
  period TEXT NOT NULL,
  spent BIGINT NOT NULL DEFAULT 0 CHECK (spent >= 0),
  PRIMARY KEY(subject, period)
);
CREATE TABLE IF NOT EXISTS relay_live_requests (
  id TEXT PRIMARY KEY,
  subject TEXT NOT NULL,
  device TEXT NOT NULL,
  provider TEXT NOT NULL,
  lane TEXT NOT NULL,
  day TEXT NOT NULL,
  month TEXT NOT NULL,
  reserved BIGINT NOT NULL CHECK (reserved >= 0),
  started BIGINT NOT NULL,
  expires BIGINT NOT NULL,
  settled INTEGER NOT NULL DEFAULT 0 CHECK (settled IN (0,1))
);
CREATE INDEX IF NOT EXISTS relay_live_subject_started ON relay_live_requests(subject, started);
CREATE INDEX IF NOT EXISTS relay_live_active ON relay_live_requests(subject, expires);
CREATE TABLE IF NOT EXISTS relay_live_admin_actions (
  id TEXT PRIMARY KEY,
  actor TEXT NOT NULL,
  subject TEXT NOT NULL,
  blocked INTEGER NOT NULL CHECK (blocked IN (0,1)),
  ts BIGINT NOT NULL
);
-- RLS / grants are provisioned separately: live-store-access.sql.
