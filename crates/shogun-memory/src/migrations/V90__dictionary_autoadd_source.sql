-- Dictionary auto-add v2: where a learned term came from, which notch notice it belongs to, and
-- whether the user ever chose on/off (2026-10-19 owner decision: on unless the user turned it off).
-- Rollback: docs/migrations/V90-rollback.md.
ALTER TABLE dictionary_autoadd_corrections
    ADD COLUMN source TEXT NOT NULL DEFAULT 'local' CHECK (source IN ('local', 'model'));
ALTER TABLE dictionary_autoadd_corrections ADD COLUMN group_id INTEGER;
ALTER TABLE dictionary_autoadd_settings
    ADD COLUMN user_set INTEGER NOT NULL DEFAULT 0 CHECK (user_set IN (0, 1));
-- Someone who turned it on earlier and then off chose off; it stays off.
UPDATE dictionary_autoadd_settings SET user_set = 1 WHERE enabled = 0 AND consent_version != 0;
