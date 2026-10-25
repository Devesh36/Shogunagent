-- Crash-recovered meeting recap attempts and quiet notice delivery.
-- Rollback: docs/migrations/V89-rollback.md.
CREATE TABLE meeting_recovery (
    session_id INTEGER PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    attempted INTEGER NOT NULL DEFAULT 0 CHECK(attempted IN (0, 1)),
    notified INTEGER NOT NULL DEFAULT 0 CHECK(notified IN (0, 1))
) STRICT;
