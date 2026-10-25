-- V45: user-defined scheduled jobs (issue #252).
--
-- A schedule runs one of the user's skills (#251, `skill_runs`) on chosen weekdays at a local
-- time. It never runs anything itself: when a slot comes due, the desktop enqueues an ordinary
-- skill run with origin 'schedule' — L1 runs, anything above L1 waits for the user, and a skill run
-- never sends (invariants 4 and 6). A Mac asleep through a slot catches up once on wake (latest
-- slot only, within 12 hours). A daily cap bounds unattended Agent-lane spend (invariant 5).
--
--   days          — weekday bitmask, Monday = 1 … Sunday = 64
--   minute_of_day — local wall-clock minute, 0..1439
--   last_slot_at  — the slot instant (epoch ms) last acted on, fired or skipped
--   last_outcome  — 'queued' | 'skipped_cap' | 'skipped_missing' | 'skipped_stale'
--
-- Additive: one new table. Rollback: docs/migrations/V45-rollback.md.
CREATE TABLE scheduled_jobs (
    id            INTEGER PRIMARY KEY,
    skill         TEXT    NOT NULL,
    input         TEXT    NOT NULL DEFAULT '',
    days          INTEGER NOT NULL CHECK (days BETWEEN 1 AND 127),
    minute_of_day INTEGER NOT NULL CHECK (minute_of_day BETWEEN 0 AND 1439),
    enabled       INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    origin        TEXT    NOT NULL CHECK (origin IN ('ui', 'api', 'mcp', 'cli')),
    last_slot_at  INTEGER,
    last_run_id   INTEGER,
    last_outcome  TEXT,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);
