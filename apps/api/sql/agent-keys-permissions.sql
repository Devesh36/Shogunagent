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
-- E19 tables.
ALTER TABLE agent_commits ENABLE ROW LEVEL SECURITY;
ALTER TABLE agent_commit_overage_reports ENABLE ROW LEVEL SECURITY;
ALTER TABLE agent_adjustments ENABLE ROW LEVEL SECURITY;
ALTER TABLE agent_charge_refunds ENABLE ROW LEVEL SECURITY;
ALTER TABLE agent_stripe_payments ENABLE ROW LEVEL SECURITY;
ALTER TABLE agent_checkout_sessions ENABLE ROW LEVEL SECURITY;
ALTER TABLE agent_statements ENABLE ROW LEVEL SECURITY;
ALTER TABLE agent_licence_links ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON agent_commits, agent_commit_overage_reports, agent_adjustments, agent_charge_refunds, agent_stripe_payments, agent_checkout_sessions, agent_statements, agent_licence_links FROM PUBLIC, anon, authenticated;
