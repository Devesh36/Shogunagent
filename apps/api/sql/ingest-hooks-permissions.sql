-- Defence in depth for the ingest hook registrations (ingest-hooks.sql), the same as
-- connector-tokens-permissions.sql. Run after ingest-hooks.sql, as the table owner
-- (shogun_relay) with search_path=relay. Idempotent: re-running it changes nothing (E44).
--
-- No behaviour change for the relay: shogun_relay OWNS the table, and a table's owner is not
-- subject to its row-level security unless FORCE ROW LEVEL SECURITY is set (it is not here). What
-- it closes is every OTHER role: with RLS on and no policy, a role granted the table by mistake
-- still reads no row, and the Supabase API roles (anon / authenticated) hold no privilege at all.
-- Rollback (if ever needed): ALTER TABLE ingest_hooks DISABLE ROW LEVEL SECURITY; nothing else.
ALTER TABLE ingest_hooks ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON ingest_hooks FROM PUBLIC, anon, authenticated;
