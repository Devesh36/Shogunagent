BEGIN;
-- Run as the schema owner. No anonymous/authenticated PostgREST access.
ALTER TABLE relay_live_subjects ENABLE ROW LEVEL SECURITY;
ALTER TABLE relay_live_counters ENABLE ROW LEVEL SECURITY;
ALTER TABLE relay_live_requests ENABLE ROW LEVEL SECURITY;
ALTER TABLE relay_live_admin_actions ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON relay_live_subjects, relay_live_counters, relay_live_requests, relay_live_admin_actions FROM PUBLIC, anon, authenticated;
-- Use the existing dedicated relay role; never Supabase's service role.
GRANT SELECT, INSERT, UPDATE, DELETE ON relay_live_subjects, relay_live_counters, relay_live_requests TO shogun_relay;
GRANT INSERT ON relay_live_admin_actions TO shogun_relay;
CREATE POLICY relay_live_subjects_role ON relay_live_subjects TO shogun_relay USING (true) WITH CHECK (true);
CREATE POLICY relay_live_counters_role ON relay_live_counters TO shogun_relay USING (true) WITH CHECK (true);
CREATE POLICY relay_live_requests_role ON relay_live_requests TO shogun_relay USING (true) WITH CHECK (true);
CREATE POLICY relay_live_admin_role ON relay_live_admin_actions FOR INSERT TO shogun_relay WITH CHECK (true);

COMMIT;
