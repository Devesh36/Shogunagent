-- Rollback: docs/migrations/V83-rollback.md
-- V83: hourly review signals and the loop consolidator (owner decision 2026-10-14, Goldfish parity).
--
-- `loop_signals` is what the hourly review reports: a typed observation (commitment, blocker,
-- question, decision, followup) with the event it came from and the exact quote. Append-only: a
-- signal never closes and is never shown; it is only input for the consolidator, which alone
-- writes `commitments` / `open_loops`. One row per (window, kind, normalised text). The quote is
-- captured text, so the row goes with its event (ON DELETE CASCADE). `loop_signal_reads` records
-- which signals the consolidator has read.
--
-- On `commitments` / `open_loops`:
-- * `horizon` — set by the consolidator only (same_day / short / project). NULL = never confirmed.
-- * `aged_at` — when the horizon timer retired the row (status is moved to cancelled / closed at
--   the same time; the V1 CHECK has no 'aged'). Reversible: the row is kept.
-- * `merged_into` — the row this duplicate was folded into. The row is kept.
--
-- Additive only: one new table and nullable columns. Older binaries name their columns on
-- INSERT and SELECT, so they ignore them.

CREATE TABLE loop_signals (
    id         INTEGER PRIMARY KEY,
    window_id  TEXT    NOT NULL,
    kind       TEXT    NOT NULL CHECK (kind IN ('commitment', 'blocker', 'question', 'decision', 'followup')),
    text       TEXT    NOT NULL,
    norm       TEXT    NOT NULL,
    event_id   INTEGER NOT NULL REFERENCES event_log (id) ON DELETE CASCADE,
    quote      TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (window_id, kind, norm)
) STRICT;

CREATE INDEX idx_loop_signals_event ON loop_signals (event_id);

-- The signals a consolidator call has read. Each call takes the newest unread signals first, so
-- this hour's go in at once and an older backlog fills the remaining room, call by call.
CREATE TABLE loop_signal_reads (
    signal_id INTEGER PRIMARY KEY REFERENCES loop_signals (id) ON DELETE CASCADE,
    read_at   INTEGER NOT NULL
) STRICT;

ALTER TABLE commitments ADD COLUMN horizon TEXT CHECK (horizon IN ('same_day', 'short', 'project'));
ALTER TABLE commitments ADD COLUMN aged_at INTEGER;
ALTER TABLE commitments ADD COLUMN merged_into INTEGER;
ALTER TABLE open_loops  ADD COLUMN horizon TEXT CHECK (horizon IN ('same_day', 'short', 'project'));
ALTER TABLE open_loops  ADD COLUMN aged_at INTEGER;
ALTER TABLE open_loops  ADD COLUMN merged_into INTEGER;
