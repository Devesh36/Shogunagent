-- Rollback: docs/migrations/V72-rollback.md
-- V72: AI memory search requests from the Memory API (issue #940 B2b, invariant 6).
--
-- AI search widens a question and reranks the matches with the user's own Agent-lane credential
-- (subscription delegation or an API key, invariant 5). Only the desktop holds that credential:
-- the MCP / REST / CLI processes never read the Keychain. So an API request with `mode: "ai"` is a
-- row here, the desktop claims it, runs the same search the Full UI runs, and writes the result
-- back — the hand-off `translate_jobs` (V66) uses.
--
-- This is a hand-off, not a history: a row lives for minutes. The API face deletes it as soon as
-- it has handed the result over (or given up waiting), and every request / claim sweeps rows older
-- than ten minutes whatever their status. Text is masked for secrets before it is written, and
-- delete-all wipes the table.
--
-- Additive: one new table and one index (plus AUTOINCREMENT's sqlite_sequence row).
CREATE TABLE ai_search_jobs (
    -- AUTOINCREMENT: rows are deleted within minutes, and a plain rowid would hand a swept
    -- request's id to the next one.
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    origin      TEXT    NOT NULL CHECK (origin IN ('api', 'mcp', 'cli')),
    request     TEXT    NOT NULL,
    status      TEXT    NOT NULL CHECK (status IN ('pending', 'running', 'done', 'failed')),
    result      TEXT,
    error       TEXT,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE INDEX idx_ai_search_jobs_status ON ai_search_jobs (status, id);
