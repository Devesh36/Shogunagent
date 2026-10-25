-- Rollback: docs/migrations/V94-rollback.md
-- V94 (ShogunAgent E2-T3): the router's run ledger, one row per `run(tool_id, ...)` an actor made
-- through this principal. Feeds per-user `discover` ranking (which tools this person's agents
-- actually use and whether they followed the hints) and the router's learning. Counts, ids,
-- cost and latency only: no input, no output, no body of any kind (invariant 2).
CREATE TABLE tool_runs (
    id            INTEGER PRIMARY KEY,
    ts            INTEGER NOT NULL,
    actor         TEXT    NOT NULL,
    tool_id       TEXT    NOT NULL,
    task_hint     TEXT,
    outcome       TEXT    NOT NULL CHECK (outcome IN ('ok', 'error', 'denied', 'pending', 'timeout')),
    cost_micro    INTEGER NOT NULL DEFAULT 0,
    latency_ms    INTEGER NOT NULL DEFAULT 0,
    hint_followed INTEGER NOT NULL DEFAULT 0 CHECK (hint_followed IN (0, 1))
) STRICT;
CREATE INDEX tool_runs_ts ON tool_runs (ts);
CREATE INDEX tool_runs_actor_tool ON tool_runs (actor, tool_id, ts);
