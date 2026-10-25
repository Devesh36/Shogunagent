-- Defence in depth for the enterprise tables (sql/enterprise.sql), as agent-keys-permissions.sql
-- does for the agent tables. Run after enterprise.sql, as the table owner. Idempotent.
ALTER TABLE enterprise_tenants ENABLE ROW LEVEL SECURITY;
ALTER TABLE enterprise_members ENABLE ROW LEVEL SECURITY;
ALTER TABLE enterprise_admins ENABLE ROW LEVEL SECURITY;
ALTER TABLE enterprise_scim_users ENABLE ROW LEVEL SECURITY;
ALTER TABLE enterprise_scim_groups ENABLE ROW LEVEL SECURITY;
ALTER TABLE enterprise_saml_replay ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON enterprise_tenants, enterprise_members, enterprise_admins, enterprise_scim_users, enterprise_scim_groups, enterprise_saml_replay FROM PUBLIC, anon, authenticated;
