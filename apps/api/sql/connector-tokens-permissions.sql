-- Defence in depth for the hosted connector token store (connector-tokens.sql), the same as
-- shared-store-permissions.sql does for the relay ledger. Run after connector-tokens.sql, as the
-- table owner (shogun_relay) with search_path=relay. Idempotent: re-running it changes nothing
-- (E48).
--
-- No behaviour change for the relay: shogun_relay OWNS these tables, and a table's owner is not
-- subject to its row-level security unless FORCE ROW LEVEL SECURITY is set (it is not here). What
-- it closes is every OTHER role: with RLS on and no policy, a role that is ever granted these
-- tables by mistake still reads and writes no row, and the Supabase API roles (anon /
-- authenticated) hold no privilege at all.
-- The rows are KMS envelopes (no plaintext column), so this is a second wall, not the first:
-- no other role can even read a ciphertext or learn which principal connected which service.
-- Rollback (if ever needed): ALTER TABLE <t> DISABLE ROW LEVEL SECURITY; nothing else to undo.
ALTER TABLE connector_tokens ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON connector_tokens FROM PUBLIC, anon, authenticated;
