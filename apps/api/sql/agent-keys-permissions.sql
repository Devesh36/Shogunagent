-- Defence in depth for the agent tables (sql/agent-keys.sql), the same as
-- shared-store-permissions.sql does for the relay ledger. Run after agent-keys.sql, as the table
-- owner (shogun_relay) with search_path=relay. Idempotent.
ALTER TABLE agent_principals ENABLE ROW LEVEL SECURITY;
ALTER TABLE agent_keys ENABLE ROW LEVEL SECURITY;
ALTER TABLE agent_wallets ENABLE ROW LEVEL SECURITY;
ALTER TABLE agent_topups ENABLE ROW LEVEL SECURITY;
ALTER TABLE agent_receipts ENABLE ROW LEVEL SECURITY;
ALTER TABLE agent_spend_periods ENABLE ROW LEVEL SECURITY;
ALTER TABLE agent_disputes ENABLE ROW LEVEL SECURITY;
ALTER TABLE agent_pow_spent ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON agent_principals, agent_keys, agent_wallets, agent_topups, agent_receipts, agent_spend_periods, agent_disputes, agent_pow_spent FROM PUBLIC, anon, authenticated;
