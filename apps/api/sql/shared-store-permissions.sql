-- Defence in depth for the relay's core ledger (the six tables of shared-store.sql), the same as
-- asr-store-permissions.sql does for the speech tables. Run after shared-store.sql, as the table
-- owner (shogun_relay) with search_path=relay. Idempotent: re-running it changes nothing.
--
-- No behaviour change for the relay: shogun_relay OWNS these tables, and a table's owner is not
-- subject to its row-level security unless FORCE ROW LEVEL SECURITY is set (it is not here), and
-- keeps every privilege a REVOKE from other roles leaves. What it closes is every OTHER role:
-- with RLS on and no policy, a role that is ever granted these tables by mistake still reads and
-- writes no row, and the Supabase API roles (anon / authenticated) hold no privilege at all.
-- Rollback (if ever needed): ALTER TABLE <t> DISABLE ROW LEVEL SECURITY; nothing else to undo.
ALTER TABLE relay_usage_days ENABLE ROW LEVEL SECURITY;
ALTER TABLE relay_usage_idem ENABLE ROW LEVEL SECURITY;
ALTER TABLE relay_usage_owners ENABLE ROW LEVEL SECURITY;
ALTER TABLE relay_spend_days ENABLE ROW LEVEL SECURITY;
ALTER TABLE relay_spend_batches ENABLE ROW LEVEL SECURITY;
ALTER TABLE relay_rate_buckets ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON relay_usage_days, relay_usage_idem, relay_usage_owners, relay_spend_days, relay_spend_batches, relay_rate_buckets FROM PUBLIC, anon, authenticated;
