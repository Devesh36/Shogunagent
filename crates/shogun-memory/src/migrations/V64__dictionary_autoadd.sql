-- Local correction observation. Off until the current auto-add disclosure is accepted.
-- Rollback: docs/migrations/V64-rollback.md.
CREATE TABLE dictionary_autoadd_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    enabled INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    consent_version INTEGER NOT NULL DEFAULT 0,
    generation INTEGER NOT NULL DEFAULT 0
) STRICT;
INSERT INTO dictionary_autoadd_settings (id) VALUES (1);

CREATE TABLE dictionary_autoadd_corrections (
    -- IDs are shown to the running notch for confirmation/Undo. Never reuse after pruning.
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    original TEXT NOT NULL CHECK (length(original) BETWEEN 2 AND 60),
    corrected TEXT NOT NULL CHECK (length(corrected) BETWEEN 2 AND 60),
    observations INTEGER NOT NULL DEFAULT 1 CHECK (observations IN (1, 2)),
    last_observation_id INTEGER NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'added', 'undone', 'suppressed')),
    created_at_ms INTEGER NOT NULL,
    term_id INTEGER,
    alias_id INTEGER,
    canonical_alias_id INTEGER,
    term_updated_at_ms INTEGER,
    created_term INTEGER NOT NULL DEFAULT 0 CHECK (created_term IN (0, 1)),
    UNIQUE (original, corrected)
) STRICT;
CREATE INDEX idx_dictionary_autoadd_corrections_age ON dictionary_autoadd_corrections (created_at_ms);
