-- Defence in depth for the router's run idempotency store (run-idempotency.sql), the same pattern
-- as settlement-outbox-permissions.sql. Run after run-idempotency.sql, as the table owner (the
-- router's own database role, the one its Hyperdrive configuration connects as).
-- Idempotent: re-running it changes nothing.
--
-- The router's role owns the table and is not subject to its row-level security (FORCE is not
-- set). Every OTHER role, including the Supabase API roles and the relay's own role, reads and
-- writes no row.
-- Rollback (if ever needed): ALTER TABLE router_run_idempotency DISABLE ROW LEVEL SECURITY.
ALTER TABLE router_run_idempotency ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON router_run_idempotency FROM PUBLIC, anon, authenticated;
