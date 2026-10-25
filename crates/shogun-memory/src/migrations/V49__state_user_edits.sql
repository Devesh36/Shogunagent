-- V49: the user can correct and delete the world model (issue #290, slice 1).
--
-- 1. `user_pinned` / `user_edited_at` on the four state tables. A row the user edited is pinned:
--    confidence 1.0, and no automatic pass (decay, corroboration, pruning, extraction promotion,
--    mail-triage refresh) writes over it. Later evidence may still link to it (a provenance row)
--    and a disagreeing new commitment still raises a `state_disputes` flag — but the user's reading
--    is never silently replaced.
--
-- 2. `state_suppressions`: what the user deleted (or rewrote) must not come straight back from
--    the same old events on the next Dream Cycle consolidation or re-capture. One row per
--    suppressed belief, keyed by a digest of the identity the extractor would re-derive (the
--    description for commitments / open loops, the extraction key for people / projects). Only
--    evidence at or before `suppressed_at` is suppressed — something said again later is new
--    evidence and is extracted as usual. The row holds a digest, never the text.
--
-- Additive: eight columns with defaults and one new table. Old binaries ignore all of it.
-- Rollback: docs/migrations/V49-rollback.md.

ALTER TABLE people      ADD COLUMN user_pinned INTEGER NOT NULL DEFAULT 0;
ALTER TABLE projects    ADD COLUMN user_pinned INTEGER NOT NULL DEFAULT 0;
ALTER TABLE commitments ADD COLUMN user_pinned INTEGER NOT NULL DEFAULT 0;
ALTER TABLE open_loops  ADD COLUMN user_pinned INTEGER NOT NULL DEFAULT 0;

ALTER TABLE people      ADD COLUMN user_edited_at INTEGER;
ALTER TABLE projects    ADD COLUMN user_edited_at INTEGER;
ALTER TABLE commitments ADD COLUMN user_edited_at INTEGER;
ALTER TABLE open_loops  ADD COLUMN user_edited_at INTEGER;

CREATE TABLE state_suppressions (
    id            INTEGER PRIMARY KEY,
    state_table   TEXT    NOT NULL CHECK (state_table IN ('people', 'projects', 'commitments', 'open_loops')),
    match_digest  TEXT    NOT NULL,
    reason        TEXT    NOT NULL CHECK (reason IN ('deleted', 'edited')),
    suppressed_at INTEGER NOT NULL,
    UNIQUE (state_table, match_digest)
) STRICT;
