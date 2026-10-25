-- V42: contradiction flags on state (issue #253).
--
-- When a new state row disagrees with an existing one about the same thing — today: two open
-- commitments that read as the same promise but name different due dates — neither is silently
-- overwritten. Both rows stay, and one `state_disputes` row records the disagreement until the
-- user (or an AI client through the Memory API) resolves it. Fusion reads the open ones and passes
-- the disputed rows as "possibly", never as fact.
--
-- The row holds ids and the two disputed values only (a due timestamp), never captured text.
-- No foreign keys: state rows can be pruned (recompute::prune_faded) or forgotten, and a dispute
-- whose rows are gone simply stops being listed.
--
-- Additive: one new table and one index. Rollback: docs/migrations/V42-rollback.md.
CREATE TABLE state_disputes (
    id            INTEGER PRIMARY KEY,
    state_table   TEXT    NOT NULL,
    field         TEXT    NOT NULL,
    earlier_id    INTEGER NOT NULL,
    later_id      INTEGER NOT NULL,
    earlier_value TEXT,
    later_value   TEXT,
    status        TEXT    NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'resolved')),
    resolution    TEXT             CHECK (resolution IS NULL OR resolution IN
                                          ('kept_earlier', 'kept_later', 'dismissed_both', 'kept_both')),
    created_at    INTEGER NOT NULL,
    resolved_at   INTEGER,
    UNIQUE (state_table, field, earlier_id, later_id)
);

CREATE INDEX idx_state_disputes_open ON state_disputes (status, state_table);
