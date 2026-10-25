-- Defence in depth for the router's settlement outbox (settlement-outbox.sql), the same pattern as
-- apps/api/sql/shared-store-permissions.sql. Run after settlement-outbox.sql, as the table owner
-- (the router's own database role, the one its Hyperdrive configuration connects as).
-- Idempotent: re-running it changes nothing.
--
-- The router's role owns the table and is not subject to its row-level security (FORCE is not
-- set). Every OTHER role, including the Supabase API roles and the relay's own role, reads and
-- writes no row: the router and apps/api share nothing but the settlement route.
-- Rollback (if ever needed): ALTER TABLE router_settlement_outbox DISABLE ROW LEVEL SECURITY.
ALTER TABLE router_settlement_outbox ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON router_settlement_outbox FROM PUBLIC, anon, authenticated;
