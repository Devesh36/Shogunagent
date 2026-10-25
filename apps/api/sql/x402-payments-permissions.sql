-- Defence in depth for the x402 replay guard (x402-payments.sql), the same as
-- shared-store-permissions.sql does for the relay ledger. Run after x402-payments.sql, as the table
-- owner (shogun_relay) with search_path=relay. Idempotent: re-running it changes nothing (E48).
--
-- No behaviour change for the relay: shogun_relay OWNS these tables, and a table's owner is not
-- subject to its row-level security unless FORCE ROW LEVEL SECURITY is set (it is not here). What
-- it closes is every OTHER role: with RLS on and no policy, a role that is ever granted these
-- tables by mistake still reads and writes no row, and the Supabase API roles (anon /
-- authenticated) hold no privilege at all.
-- Only needed with x402-payments.sql, which is applied only if X402_ENABLED=1 (off; §9 of
-- docs/deploy/production.md).
-- Rollback (if ever needed): ALTER TABLE <t> DISABLE ROW LEVEL SECURITY; nothing else to undo.
ALTER TABLE x402_payments ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON x402_payments FROM PUBLIC, anon, authenticated;
