-- Defence in depth for the approval webhook table (approval-webhooks.sql), the same as
-- shared-store-permissions.sql does for the relay ledger. Run after approval-webhooks.sql, as the
-- table owner (shogun_relay) with search_path=relay. Idempotent: re-running it changes nothing
-- (E48).
--
-- No behaviour change for the relay: shogun_relay OWNS these tables, and a table's owner is not
-- subject to its row-level security unless FORCE ROW LEVEL SECURITY is set (it is not here). What
-- it closes is every OTHER role: with RLS on and no policy, a role that is ever granted these
-- tables by mistake still reads and writes no row, and the Supabase API roles (anon /
-- authenticated) hold no privilege at all.
-- Rollback (if ever needed): ALTER TABLE <t> DISABLE ROW LEVEL SECURITY; nothing else to undo.
ALTER TABLE agent_webhooks ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON agent_webhooks FROM PUBLIC, anon, authenticated;
