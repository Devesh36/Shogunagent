-- V43: calendar occurrences as state (FR-MT-06, issue #263).
--
-- A synced calendar event is about the future, so it is held as state — one row per event, kept
-- current by the sync — not only as the event log's append-only record. The row carries the
-- structured facts meeting notes need: when it runs (epoch ms, read from the calendar's RFC3339),
-- where (`location` may be a meeting URL: it never leaves the device), and who is invited
-- (`attendees`, a JSON array of {email, name, is_self, response}), which feeds speaker naming and
-- identity resolution.
--
-- `sessions.calendar_occurrence_id` (V7) finally has something to point at. No foreign keys, the
-- same choice V7 made: a re-synced or deleted event must not strand or block a meeting session.
--
-- Additive: one new table and one index. Rollback: docs/migrations/V43-rollback.md.
CREATE TABLE calendar_occurrences (
    id          INTEGER PRIMARY KEY,
    source      TEXT    NOT NULL,
    external_id TEXT    NOT NULL,
    title       TEXT,
    start_at    INTEGER NOT NULL,
    end_at      INTEGER NOT NULL,
    all_day     INTEGER NOT NULL DEFAULT 0 CHECK (all_day IN (0, 1)),
    location    TEXT,
    attendees   TEXT    NOT NULL DEFAULT '[]',
    status      TEXT    NOT NULL DEFAULT 'confirmed' CHECK (status IN ('confirmed', 'cancelled')),
    -- The newest event_log row this occurrence was synced from (provenance).
    event_id    INTEGER,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    UNIQUE (source, external_id)
);

CREATE INDEX idx_calendar_occurrences_time ON calendar_occurrences (start_at, end_at);
