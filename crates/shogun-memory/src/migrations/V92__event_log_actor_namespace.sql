-- Rollback: docs/migrations/V92-rollback.md
-- V92 (ShogunAgent E2-T3): who wrote each event and which namespace it belongs to.
-- `actor` names the writer ("human:owner", "agent:<id>"); rows written before V92 read as NULL
-- and are treated as the owner's. `namespace` separates the shared world model ('world') from an
-- agent's private scratch ('agent_scratch:<agent_id>'); every pre-V92 row is 'world'. No tenant
-- column: the principal's file is the tenant boundary (plan §2.2).
ALTER TABLE event_log ADD COLUMN actor TEXT;
ALTER TABLE event_log ADD COLUMN namespace TEXT NOT NULL DEFAULT 'world';
CREATE INDEX event_log_namespace_ts ON event_log (namespace, ts);
CREATE INDEX event_log_actor_ts ON event_log (actor, ts);
