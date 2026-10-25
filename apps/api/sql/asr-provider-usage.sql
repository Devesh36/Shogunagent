-- Additive provider counter floors. Rollback: docs/migrations/relay-asr-rollback.md.
CREATE TABLE IF NOT EXISTS relay_asr_provider_usage (
  scope text NOT NULL,
  period text NOT NULL,
  seconds bigint NOT NULL CHECK (seconds >= 0),
  PRIMARY KEY(scope,period)
);
ALTER TABLE relay_asr_provider_usage ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON relay_asr_provider_usage FROM PUBLIC, anon, authenticated;
