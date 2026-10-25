-- V48: file exports into user-chosen folders (issue #397, LocalAction::WriteFile).
--
-- One row per export: asked for (an API caller's request waits in `awaiting_confirm` until the
-- user confirms it in the desktop — the action is L2), then written, cancelled or failed. The row
-- is also the traceability record of a *local* write: kind `local_write`, the byte count and an
-- xxh64 digest of the content — never the content itself, and no egress route (nothing leaves
-- the device; `traceability_log` stays the ledger of what did).
--
-- `source` / `source_id` name what was exported (a meeting session) — the provenance the file's
-- footer repeats. `folder` is the allowed folder the caller named; `path` is where the file
-- actually landed (a taken name moves to `name (2).md`, never overwrites).
--
-- Additive: one new table and one index. Rollback: docs/migrations/V48-rollback.md.
CREATE TABLE local_writes (
    id                 INTEGER PRIMARY KEY,
    kind               TEXT    NOT NULL DEFAULT 'local_write' CHECK (kind IN ('local_write')),
    source             TEXT    NOT NULL CHECK (source IN ('meeting_recap')),
    source_id          INTEGER NOT NULL,
    include_transcript INTEGER NOT NULL DEFAULT 0,
    format             TEXT    NOT NULL CHECK (format IN ('md', 'csv')),
    folder             TEXT    NOT NULL,
    name               TEXT    NOT NULL,
    origin             TEXT    NOT NULL CHECK (origin IN ('ui', 'api')),
    status             TEXT    NOT NULL CHECK (status IN
                           ('awaiting_confirm', 'writing', 'written', 'cancelled', 'failed')),
    path               TEXT,
    bytes              INTEGER,
    content_xxh64      TEXT,
    error              TEXT,
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL
) STRICT;

CREATE INDEX idx_local_writes_status ON local_writes (status, id);
