-- V50: the user splits a wrong entity merge (名寄せ, FR-ST-10 / issue #290).
--
-- A split takes some of a person's (or project's) evidence — provenance events, and for people
-- aliases / addresses / handles — and gives it to a NEW row. The decision must outlive the
-- moment: the Dream Cycle re-reads past events, and the extractor that fused them in the first
-- place would otherwise link the same events straight back to the original row.
--
-- 1. `state_merge_blocks`: one row per split — "the user said `kept_id` and `split_id` are not
--    the same entity". Any automatic resolver that would fuse the pair (or pick one of them for
--    an ambiguous name) consults it. Ids only; no names, no text.
-- 2. `state_split_events`: the events the user moved in that split. An extractor about to link
--    one of these events to `kept_id` links it to where the user put it instead. Only evidence
--    captured at or before the split is redirected (`created_at`), so a reused event id never
--    inherits the decision.
--
-- Additive: two new tables and one index. Old binaries ignore them.
-- Rollback: docs/migrations/V50-rollback.md.

CREATE TABLE state_merge_blocks (
    id          INTEGER PRIMARY KEY,
    state_table TEXT    NOT NULL CHECK (state_table IN ('people', 'projects')),
    kept_id     INTEGER NOT NULL,
    split_id    INTEGER NOT NULL,
    created_at  INTEGER NOT NULL,
    CHECK (kept_id <> split_id),
    UNIQUE (state_table, kept_id, split_id)
) STRICT;

CREATE TABLE state_split_events (
    block_id INTEGER NOT NULL REFERENCES state_merge_blocks (id) ON DELETE CASCADE,
    event_id INTEGER NOT NULL,
    PRIMARY KEY (block_id, event_id)
) STRICT;

CREATE INDEX idx_state_split_events_event ON state_split_events (event_id);
