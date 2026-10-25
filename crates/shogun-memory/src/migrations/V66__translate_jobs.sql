-- Rollback: docs/migrations/V66-rollback.md
-- V66: translation and reply-draft requests from the Memory API (issue #279, invariant 6).
--
-- Translating text and drafting a reply run on the Agent lane with the user's own credential
-- (subscription delegation or an API key, invariant 5). Only the desktop holds that credential:
-- the MCP / REST / CLI processes never read the Keychain. So an API request is a row here, the
-- desktop claims it, runs the same core prompt / parse the notch panel uses, and writes the
-- result back — the pattern `skill_runs` (V44) set.
--
-- This is a hand-off, not a history (FR-TL-20): a row lives for minutes. The API face deletes it
-- as soon as it has handed the result to its caller, and every request / claim sweeps rows older
-- than ten minutes whatever their status. Text columns are masked for secrets before they are
-- written, and delete-all wipes the table.
--
-- Additive: one new table and one index (plus AUTOINCREMENT's sqlite_sequence row).
CREATE TABLE translate_jobs (
    -- AUTOINCREMENT: rows are deleted within minutes, and a plain rowid would hand a swept
    -- request's id to the next one — a caller polling an old id must never read someone
    -- else's result.
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT    NOT NULL CHECK (kind IN ('translate', 'reply_draft')),
    origin      TEXT    NOT NULL CHECK (origin IN ('api', 'mcp', 'cli')),
    request     TEXT    NOT NULL,
    status      TEXT    NOT NULL CHECK (status IN ('pending', 'running', 'done', 'failed')),
    result      TEXT,
    error       TEXT,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE INDEX idx_translate_jobs_status ON translate_jobs (status, id);
