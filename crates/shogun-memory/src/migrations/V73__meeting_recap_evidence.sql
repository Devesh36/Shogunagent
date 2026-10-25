-- Rollback: docs/migrations/V73-rollback.md
-- Validated transcript passages supporting generated minutes. NULL on older or edited recaps.
ALTER TABLE meeting_recaps ADD COLUMN evidence TEXT;
