-- Defence in depth for the provider payout ledger (provider-payouts.sql), the same as
-- shared-store-permissions.sql does for the relay ledger. Run after provider-payouts.sql, as the
-- table owner (shogun_relay) with search_path=relay. Idempotent: re-running it changes nothing
-- (E48).
--
-- No behaviour change for the relay: shogun_relay OWNS these tables, and a table's owner is not
-- subject to its row-level security unless FORCE ROW LEVEL SECURITY is set (it is not here). What
-- it closes is every OTHER role: with RLS on and no policy, a role that is ever granted these
-- tables by mistake still reads and writes no row, and the Supabase API roles (anon /
-- authenticated) hold no privilege at all.
-- Rollback (if ever needed): ALTER TABLE <t> DISABLE ROW LEVEL SECURITY; nothing else to undo.
ALTER TABLE provider_accounts ENABLE ROW LEVEL SECURITY;
ALTER TABLE provider_settlements ENABLE ROW LEVEL SECURITY;
ALTER TABLE provider_accruals ENABLE ROW LEVEL SECURITY;
ALTER TABLE provider_adjustments ENABLE ROW LEVEL SECURITY;
ALTER TABLE provider_payouts ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON provider_accounts, provider_settlements, provider_accruals, provider_adjustments, provider_payouts FROM PUBLIC, anon, authenticated;
