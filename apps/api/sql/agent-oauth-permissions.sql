-- Defence in depth for the OAuth tables (sql/agent-oauth.sql), as agent-keys-permissions.sql
-- does for the agent tables. Run after agent-oauth.sql, as the table owner. Idempotent.
ALTER TABLE oauth_codes ENABLE ROW LEVEL SECURITY;
ALTER TABLE oauth_families ENABLE ROW LEVEL SECURITY;
ALTER TABLE oauth_tokens ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON oauth_codes, oauth_families, oauth_tokens FROM PUBLIC, anon, authenticated;
