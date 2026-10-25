-- Access rules for the live lane's tables (live-store.sql, #1101). Run after live-store.sql, as
-- the schema owner with search_path=relay. Re-runnable (E48): every statement is idempotent and
-- each policy is created only when it does not exist yet, so a second run changes nothing and
-- does not fail. No anonymous/authenticated PostgREST access.
BEGIN;
ALTER TABLE relay_live_subjects ENABLE ROW LEVEL SECURITY;
ALTER TABLE relay_live_counters ENABLE ROW LEVEL SECURITY;
ALTER TABLE relay_live_requests ENABLE ROW LEVEL SECURITY;
ALTER TABLE relay_live_admin_actions ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON relay_live_subjects, relay_live_counters, relay_live_requests, relay_live_admin_actions FROM PUBLIC, anon, authenticated;
-- Use the existing dedicated relay role; never Supabase's service role.
GRANT SELECT, INSERT, UPDATE, DELETE ON relay_live_subjects, relay_live_counters, relay_live_requests TO shogun_relay;
GRANT INSERT ON relay_live_admin_actions TO shogun_relay;
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_policy WHERE polrelid = 'relay_live_subjects'::regclass AND polname = 'relay_live_subjects_role') THEN
    CREATE POLICY relay_live_subjects_role ON relay_live_subjects TO shogun_relay USING (true) WITH CHECK (true);
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_policy WHERE polrelid = 'relay_live_counters'::regclass AND polname = 'relay_live_counters_role') THEN
    CREATE POLICY relay_live_counters_role ON relay_live_counters TO shogun_relay USING (true) WITH CHECK (true);
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_policy WHERE polrelid = 'relay_live_requests'::regclass AND polname = 'relay_live_requests_role') THEN
    CREATE POLICY relay_live_requests_role ON relay_live_requests TO shogun_relay USING (true) WITH CHECK (true);
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_policy WHERE polrelid = 'relay_live_admin_actions'::regclass AND polname = 'relay_live_admin_role') THEN
    CREATE POLICY relay_live_admin_role ON relay_live_admin_actions FOR INSERT TO shogun_relay WITH CHECK (true);
  END IF;
END
$$;

COMMIT;
