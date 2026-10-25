-- OAuth 2.1 + Client ID Metadata Documents for MCP clients (docs/implementation-plan.md E3-T2;
-- src/oauth/). Provisioning, not a refinery migration (same rule as agent-keys.sql): apply once
-- per database, after agent-keys.sql, with
--   psql "$RELAY_DATABASE_URL" -v ON_ERROR_STOP=1 -f apps/api/sql/agent-oauth.sql
-- Idempotent (IF NOT EXISTS) and additive only.
--
-- No secrets: a code, an access token and a refresh token are stored only as the SHA-256 of the
-- secret (`*_hash`). There is no client table: a client is its Client ID Metadata Document URL,
-- fetched and cached in memory (src/oauth/cimd.ts); there is no Dynamic Client Registration.

-- Authorization codes: single use (`used_at` flips once), 60 seconds.
CREATE TABLE IF NOT EXISTS oauth_codes (
  code_hash       TEXT   PRIMARY KEY,
  client_id       TEXT   NOT NULL,
  redirect_uri    TEXT   NOT NULL,
  code_challenge  TEXT   NOT NULL,
  key_id          TEXT   NOT NULL,
  principal_id    TEXT   NOT NULL,
  scopes          TEXT   NOT NULL,
  resource        TEXT   NOT NULL,
  created_at      BIGINT NOT NULL,
  expires_at      BIGINT NOT NULL,
  used_at         BIGINT
);
CREATE INDEX IF NOT EXISTS oauth_codes_expires ON oauth_codes (expires_at);

-- A family: every token minted from one consent. Revoking it revokes them all (a replayed code,
-- a reused refresh token, or POST /oauth/revoke).
CREATE TABLE IF NOT EXISTS oauth_families (
  family_id     TEXT   PRIMARY KEY,
  client_id     TEXT   NOT NULL,
  key_id        TEXT   NOT NULL,
  principal_id  TEXT   NOT NULL,
  code_hash     TEXT   NOT NULL,
  created_at    BIGINT NOT NULL,
  revoked_at    BIGINT
);
CREATE INDEX IF NOT EXISTS oauth_families_code ON oauth_families (code_hash);

-- Access tokens (`kind = access`, short-lived) and refresh tokens (`kind = refresh`, rotated:
-- `used_at` flips once; a second presentation revokes the family).
CREATE TABLE IF NOT EXISTS oauth_tokens (
  token_hash    TEXT   PRIMARY KEY,
  kind          TEXT   NOT NULL,
  family_id     TEXT   NOT NULL,
  client_id     TEXT   NOT NULL,
  key_id        TEXT   NOT NULL,
  principal_id  TEXT   NOT NULL,
  scopes        TEXT   NOT NULL,
  resource      TEXT   NOT NULL,
  created_at    BIGINT NOT NULL,
  expires_at    BIGINT NOT NULL,
  used_at       BIGINT
);
CREATE INDEX IF NOT EXISTS oauth_tokens_family ON oauth_tokens (family_id);
CREATE INDEX IF NOT EXISTS oauth_tokens_expires ON oauth_tokens (expires_at);
