-- Rollback: docs/migrations/V97-rollback.md
-- V97 (ShogunAgent E2-T3): typed relation rows between state entities, in place of a graph
-- database. (subject, predicate, object) with a validity interval (`valid_to` NULL = still true),
-- a confidence in the same 0..1 band the state tables use, and the event that evidences the row
-- (`provenance_event_id`), so a relation is never a fact without a source (invariant 5).
CREATE TABLE relations (
    id                  INTEGER PRIMARY KEY,
    subject_kind        TEXT    NOT NULL CHECK (subject_kind IN ('people', 'projects', 'commitments', 'open_loops')),
    subject_id          INTEGER NOT NULL,
    predicate           TEXT    NOT NULL,
    object_kind         TEXT    NOT NULL CHECK (object_kind IN ('people', 'projects', 'commitments', 'open_loops')),
    object_id           INTEGER NOT NULL,
    valid_from          INTEGER NOT NULL,
    valid_to            INTEGER,
    confidence          REAL    NOT NULL CHECK (confidence >= 0.0 AND confidence <= 1.0),
    provenance_event_id INTEGER NOT NULL REFERENCES event_log (id),
    actor               TEXT,
    namespace           TEXT    NOT NULL DEFAULT 'world',
    created_at          INTEGER NOT NULL,
    CHECK (valid_to IS NULL OR valid_to >= valid_from)
) STRICT;
CREATE INDEX relations_subject ON relations (subject_kind, subject_id, predicate);
CREATE INDEX relations_object ON relations (object_kind, object_id, predicate);
CREATE INDEX relations_provenance ON relations (provenance_event_id);
