-- Additive ASR admission schema. Run as shogun_relay with search_path=relay.
-- Rollback: docs/migrations/relay-asr-rollback.md (disable ASR_ENABLED; retain data).
CREATE TABLE IF NOT EXISTS relay_asr_subjects (
  subject text PRIMARY KEY,
  blocked integer NOT NULL DEFAULT 0 CHECK (blocked IN (0,1))
);
CREATE TABLE IF NOT EXISTS relay_asr_months (
  subject text NOT NULL,
  month text NOT NULL,
  seconds bigint NOT NULL CHECK (seconds >= 0),
  PRIMARY KEY(subject,month)
);
CREATE TABLE IF NOT EXISTS relay_asr_sessions (
  id text PRIMARY KEY,
  subject text NOT NULL,
  device text NOT NULL,
  month text NOT NULL,
  started bigint NOT NULL,
  last_mint bigint NOT NULL,
  expires bigint NOT NULL,
  seconds bigint NOT NULL CHECK (seconds > 0),
  streams integer NOT NULL CHECK (streams IN (1,2)),
  day text NOT NULL,
  micros bigint NOT NULL CHECK (micros > 0),
  finished integer NOT NULL DEFAULT 0 CHECK (finished IN (0,1))
);
CREATE INDEX IF NOT EXISTS relay_asr_sessions_subject ON relay_asr_sessions(subject,started);

-- Additive provider counter floors. Rollback: docs/migrations/relay-asr-rollback.md.
CREATE TABLE IF NOT EXISTS relay_asr_provider_usage (
  scope text NOT NULL,
  period text NOT NULL,
  seconds bigint NOT NULL CHECK (seconds >= 0),
  PRIMARY KEY(scope,period)
);
