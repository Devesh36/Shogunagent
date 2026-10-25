-- Rollback: docs/migrations/V93-rollback.md
-- V93 (ShogunAgent E2-T3): the last writer and the namespace of every state row. `actor_last` is
-- the actor of the most recent create/update ("human:owner", "agent:<id>"); pre-V93 rows read as
-- NULL (the owner's). `namespace` mirrors event_log.namespace (V92): 'world' for the shared model,
-- 'agent_scratch:<agent_id>' for an agent's private rows, which default reads leave out.
ALTER TABLE people ADD COLUMN actor_last TEXT;
ALTER TABLE people ADD COLUMN namespace TEXT NOT NULL DEFAULT 'world';
ALTER TABLE projects ADD COLUMN actor_last TEXT;
ALTER TABLE projects ADD COLUMN namespace TEXT NOT NULL DEFAULT 'world';
ALTER TABLE commitments ADD COLUMN actor_last TEXT;
ALTER TABLE commitments ADD COLUMN namespace TEXT NOT NULL DEFAULT 'world';
ALTER TABLE open_loops ADD COLUMN actor_last TEXT;
ALTER TABLE open_loops ADD COLUMN namespace TEXT NOT NULL DEFAULT 'world';
CREATE INDEX people_namespace ON people (namespace);
CREATE INDEX projects_namespace ON projects (namespace);
CREATE INDEX commitments_namespace ON commitments (namespace);
CREATE INDEX open_loops_namespace ON open_loops (namespace);
