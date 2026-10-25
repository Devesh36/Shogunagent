-- Rollback: docs/migrations/V104-rollback.md
-- V104 (ShogunAgent E49): outcomes of commitments and open loops read off later messages.
--
-- `loop_updates`: one row per (state row, event, kind) where a later message of the row's thread
-- says it was done (`resolved`), called off (`cancelled`) or moved (`redated`, with the new and the
-- previous due date). `quote` is the exact span of the event that says so (captured text, so the
-- row goes with its event: ON DELETE CASCADE). `applied` = 1 when the local path closed or re-dated
-- a local candidate (below 0.5, never Batch-reviewed, not the user's); a row the loop consolidator
-- or the user owns is never changed here, and its update waits for the consolidator (`read_at`).
-- No confidence is stored or raised.
--
-- `loop_update_scan`: the last event id the outcome pass read (one row).
--
-- Additive only: two new tables, no existing table, column, trigger or index changes.

CREATE TABLE loop_updates (
    id              INTEGER PRIMARY KEY,
    kind            TEXT    NOT NULL CHECK (kind IN ('resolved', 'cancelled', 'redated')),
    state_table     TEXT    NOT NULL CHECK (state_table IN ('commitments', 'open_loops')),
    state_id        INTEGER NOT NULL,
    event_id        INTEGER NOT NULL REFERENCES event_log (id) ON DELETE CASCADE,
    quote           TEXT    NOT NULL,
    due_at          INTEGER,
    previous_due_at INTEGER,
    applied         INTEGER NOT NULL DEFAULT 0 CHECK (applied IN (0, 1)),
    read_at         INTEGER,
    created_at      INTEGER NOT NULL,
    UNIQUE (state_table, state_id, event_id, kind)
) STRICT;

CREATE INDEX idx_loop_updates_event ON loop_updates (event_id);
CREATE INDEX idx_loop_updates_unread ON loop_updates (read_at, state_table, state_id);

CREATE TABLE loop_update_scan (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    last_event_id INTEGER NOT NULL
) STRICT;
