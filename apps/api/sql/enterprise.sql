-- Enterprise tenants, tenant admins, SSO, SCIM provisioning and the SAML replay cache
-- (docs/implementation-plan.md E26; src/enterprise/). Provisioning, not a refinery migration (same
-- rule as agent-keys.sql): apply once per database, after agent-keys.sql and agent-oauth.sql, with
--   psql "$RELAY_DATABASE_URL" -v ON_ERROR_STOP=1 -f apps/api/sql/enterprise.sql
-- Idempotent (IF NOT EXISTS) and additive only — never DROP / RENAME / ALTER COLUMN here.
--
-- Identity metadata, never memory content (CLAUDE.md invariant 2). Secrets are stored hashed or
-- sealed: the SCIM bearer as its SHA-256 (`scim_token_hash`), an OIDC client secret only sealed
-- with AES-256-GCM under a key derived from ENTERPRISE_SECRET inside `sso_json`
-- (`client_secret_sealed`). A SAML assertion id is kept only as a SHA-256 for its validity window.
-- No assertion, ID token, claim set or SCIM request body is stored.
--
-- Written in the dialect subset the unit tests' SQLite double accepts (test/sql-double.ts).

-- A tenant (an organization). `region` is where its principals are served (us | eu | jp | …);
-- `plan` is self-serve | enterprise; `sso_json` the SSO configuration (protocol, enforced, OIDC
-- issuer and client id, SAML entity id, SSO URL and signing certificates).
CREATE TABLE IF NOT EXISTS enterprise_tenants (
  tenant_id        TEXT   PRIMARY KEY,
  name             TEXT   NOT NULL,
  region           TEXT   NOT NULL,
  plan             TEXT   NOT NULL,
  sso_json         TEXT,
  scim_token_hash  TEXT,
  idempotency_key  TEXT,
  created_at       BIGINT NOT NULL,
  updated_at       BIGINT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS enterprise_tenants_scim ON enterprise_tenants (scim_token_hash);
CREATE UNIQUE INDEX IF NOT EXISTS enterprise_tenants_idem ON enterprise_tenants (idempotency_key);

-- Membership: the primary key makes "a principal belongs to at most one tenant" a constraint.
CREATE TABLE IF NOT EXISTS enterprise_members (
  principal_id  TEXT   PRIMARY KEY,
  tenant_id     TEXT   NOT NULL,
  created_at    BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS enterprise_members_tenant ON enterprise_members (tenant_id);

-- Tenant admins: people who sign in with the tenant's IdP. Separate from agent keys: an admin
-- holds no key; a session is a short-lived signed cookie. `subject` binds on first sign-in.
CREATE TABLE IF NOT EXISTS enterprise_admins (
  admin_id    TEXT   PRIMARY KEY,
  tenant_id   TEXT   NOT NULL,
  email       TEXT   NOT NULL,
  subject     TEXT,
  created_at  BIGINT NOT NULL,
  removed_at  BIGINT
);
CREATE INDEX IF NOT EXISTS enterprise_admins_tenant ON enterprise_admins (tenant_id);

-- SCIM users. `user_id` is the principal id the user provisions. A DELETE leaves a tombstone:
-- attributes cleared, `deleted_at` set, the principal store purged at `delete_after`.
CREATE TABLE IF NOT EXISTS enterprise_scim_users (
  user_id       TEXT     PRIMARY KEY,
  tenant_id     TEXT     NOT NULL,
  user_name     TEXT     NOT NULL,
  user_name_lc  TEXT,
  external_id   TEXT,
  display_name  TEXT,
  given_name    TEXT,
  family_name   TEXT,
  emails_json   TEXT     NOT NULL,
  active        SMALLINT NOT NULL,
  created_at    BIGINT   NOT NULL,
  updated_at    BIGINT   NOT NULL,
  deleted_at    BIGINT,
  delete_after  BIGINT,
  purged_at     BIGINT
);
-- userName is unique per tenant among live users (a tombstone's user_name_lc is NULL).
CREATE UNIQUE INDEX IF NOT EXISTS enterprise_scim_users_name ON enterprise_scim_users (tenant_id, user_name_lc);
CREATE INDEX IF NOT EXISTS enterprise_scim_users_purge ON enterprise_scim_users (delete_after);

-- SCIM groups = Teams (E18 world-read grants among members). `members` is a comma list of
-- principal ids.
CREATE TABLE IF NOT EXISTS enterprise_scim_groups (
  group_id      TEXT   PRIMARY KEY,
  tenant_id     TEXT   NOT NULL,
  display_name  TEXT   NOT NULL,
  external_id   TEXT,
  members       TEXT   NOT NULL,
  created_at    BIGINT NOT NULL,
  updated_at    BIGINT NOT NULL,
  deleted_at    BIGINT
);
CREATE UNIQUE INDEX IF NOT EXISTS enterprise_scim_groups_name ON enterprise_scim_groups (tenant_id, display_name) WHERE deleted_at IS NULL;

-- SAML replay cache: SHA-256 of tenant and assertion id, kept until the assertion expires.
CREATE TABLE IF NOT EXISTS enterprise_saml_replay (
  assertion_key  TEXT   PRIMARY KEY,
  expires_at     BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS enterprise_saml_replay_expires ON enterprise_saml_replay (expires_at);
