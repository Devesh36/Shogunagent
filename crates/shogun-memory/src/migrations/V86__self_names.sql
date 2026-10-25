-- Rollback: docs/migrations/V86-rollback.md
-- V86: the names the user goes by in chat apps, learned from what the screen shows (2026-10-15).
--
-- self_name_evidence  one row per (name, window) that showed the name is the user's:
--                     kind dm      = the only other author in a one-to-one chat titled for someone else
--                          echo    = the header above a message the user just sent
--                          review  = the hourly review named it from a one-to-one chat or the user's own text
--                     Two windows (or one echo) make the name an alias of the user's own people row.
-- memory_markers      small per-database values: a scan watermark and one-time passes.
--
-- Additive only: two new tables, no change to existing ones.

CREATE TABLE self_name_evidence (
    name_key   TEXT NOT NULL,
    name       TEXT NOT NULL,
    window_key TEXT NOT NULL,
    kind       TEXT NOT NULL,
    seen_at    INTEGER NOT NULL,
    PRIMARY KEY (name_key, window_key)
);

CREATE TABLE memory_markers (
    key   TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);
