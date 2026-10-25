-- Keep ownership and undone suppression after typed correction text expires.
-- Rollback: docs/migrations/V65-rollback.md.
CREATE TABLE dictionary_autoadd_artifacts (
    correction_id INTEGER PRIMARY KEY,
    term_id INTEGER NOT NULL,
    alias_id INTEGER,
    canonical_alias_id INTEGER,
    created_term INTEGER NOT NULL CHECK (created_term IN (0, 1)),
    created_at_ms INTEGER NOT NULL
) STRICT;
INSERT INTO dictionary_autoadd_artifacts
    (correction_id, term_id, alias_id, canonical_alias_id, created_term, created_at_ms)
SELECT c.id, c.term_id, c.alias_id, c.canonical_alias_id, c.created_term, c.created_at_ms
FROM dictionary_autoadd_corrections c JOIN voice_terms t ON t.id = c.term_id
WHERE c.state = 'added' AND c.term_id IS NOT NULL
  AND EXISTS (
      SELECT 1 FROM dictionary_autoadd_corrections newest
      WHERE newest.term_id = c.term_id AND newest.state = 'added'
        AND newest.created_at_ms = t.updated_at_ms
  )
  AND EXISTS (
      SELECT 1 FROM voice_term_aliases alias
      WHERE alias.id = c.alias_id AND alias.term_id = c.term_id AND alias.alias = c.original
  )
  AND (c.created_term = 0 OR EXISTS (
      SELECT 1 FROM voice_term_aliases canonical
      WHERE canonical.id = c.canonical_alias_id AND canonical.term_id = c.term_id
        AND canonical.alias = c.corrected
  ));
CREATE INDEX idx_dictionary_autoadd_artifacts_age ON dictionary_autoadd_artifacts (created_at_ms);

CREATE TABLE dictionary_autoadd_secret (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    key BLOB NOT NULL CHECK (length(key) = 32)
) STRICT;
INSERT INTO dictionary_autoadd_secret (id, key) VALUES (1, randomblob(32));

CREATE TABLE dictionary_autoadd_suppression (
    pair_digest BLOB PRIMARY KEY CHECK (length(pair_digest) = 32),
    created_at_ms INTEGER NOT NULL,
    reason TEXT NOT NULL CHECK (reason IN ('undo', 'capacity'))
) STRICT;
CREATE INDEX idx_dictionary_autoadd_suppression_age ON dictionary_autoadd_suppression (created_at_ms);
