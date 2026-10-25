-- Rollback: docs/migrations/V74-rollback.md
-- Reversible transcript correction/exclusion; originals remain local for provenance.
ALTER TABLE transcript_segments ADD COLUMN original_text TEXT;
ALTER TABLE transcript_segments ADD COLUMN edited_at INTEGER;
ALTER TABLE transcript_segments ADD COLUMN excluded INTEGER NOT NULL DEFAULT 0 CHECK (excluded IN (0, 1));
