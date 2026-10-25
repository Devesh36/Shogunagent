-- Run after asr-store.sql, as the table owner in search_path=relay.
-- Owner connections used by Hyperdrive retain access; API roles have none.
ALTER TABLE relay_asr_subjects ENABLE ROW LEVEL SECURITY;
ALTER TABLE relay_asr_months ENABLE ROW LEVEL SECURITY;
ALTER TABLE relay_asr_sessions ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON relay_asr_subjects, relay_asr_months, relay_asr_sessions FROM PUBLIC, anon, authenticated;

ALTER TABLE relay_asr_provider_usage ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON relay_asr_provider_usage FROM PUBLIC, anon, authenticated;
