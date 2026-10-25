-- V44: runs of user-authored skills (issue #251).
--
-- A skill is a Markdown file the user owns (`<app-data>/skills/*.md`); this table records each
-- time one is asked to run and what it produced. A run can be asked for from the desktop, or from
-- an AI client over MCP / REST / CLI — those processes cannot reach the desktop's Agent-lane
-- credentials, so they enqueue a row here and the desktop executes it (invariant 6: same skills,
-- same levels, every face). A run above L1 asked for by an API client waits in
-- `awaiting_confirm` until the user confirms it in the desktop.
--
-- `output` is the generated text (it can quote the user's memory, so it lives in this database
-- and is wiped by delete-all). `input` is what the caller passed in, capped by the repository.
-- `skill` is the name at request time; no FK — the file can be renamed or deleted.
--
-- Additive: one new table and one index. Rollback: docs/migrations/V44-rollback.md.
CREATE TABLE skill_runs (
    id          INTEGER PRIMARY KEY,
    skill       TEXT    NOT NULL,
    level       TEXT    NOT NULL CHECK (level IN ('L1', 'L2', 'L3')),
    origin      TEXT    NOT NULL CHECK (origin IN ('ui', 'api', 'mcp', 'cli', 'schedule')),
    input       TEXT    NOT NULL DEFAULT '',
    status      TEXT    NOT NULL CHECK (status IN
                    ('awaiting_confirm', 'pending', 'running', 'done', 'failed', 'cancelled')),
    output      TEXT,
    error       TEXT,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE INDEX idx_skill_runs_status ON skill_runs (status, id);
