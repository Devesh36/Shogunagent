-- V69: preserve per-claim source spans without inventing evidence for existing links.
-- NULL means the precise span was not recorded. No source data or request identity changes.
-- Rollback: docs/migrations/V69-rollback.md.
ALTER TABLE state_provenance ADD COLUMN quote TEXT;
-- Only validated Batch results mark a source as reviewed; existing links remain unreviewed.
ALTER TABLE state_provenance ADD COLUMN batch_reviewed INTEGER NOT NULL DEFAULT 0 CHECK (batch_reviewed IN (0, 1));

-- Pinning a date/status preserves source links; wording corrections do not claim old proof.
-- Older pins have no field-level history, so preserve their conservative edited-context treatment.
ALTER TABLE people ADD COLUMN wording_edited INTEGER NOT NULL DEFAULT 0 CHECK (wording_edited IN (0,1));
UPDATE people SET wording_edited=1 WHERE user_pinned=1;
ALTER TABLE projects ADD COLUMN wording_edited INTEGER NOT NULL DEFAULT 0 CHECK (wording_edited IN (0,1));
UPDATE projects SET wording_edited=1 WHERE user_pinned=1;
ALTER TABLE commitments ADD COLUMN wording_edited INTEGER NOT NULL DEFAULT 0 CHECK (wording_edited IN (0,1));
UPDATE commitments SET wording_edited=1 WHERE user_pinned=1;
ALTER TABLE open_loops ADD COLUMN wording_edited INTEGER NOT NULL DEFAULT 0 CHECK (wording_edited IN (0,1));
UPDATE open_loops SET wording_edited=1 WHERE user_pinned=1;

-- Stream the earliest valid source without sorting/loading every recapture's text first.
CREATE INDEX idx_state_provenance_source ON state_provenance(state_table, state_id, event_id);
