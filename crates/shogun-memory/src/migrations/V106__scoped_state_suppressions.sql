-- V106: new corrections/deletions suppress extraction only in the affected namespace.
-- V49 records have no namespace or row id, and deleted rows/provenance cannot recover one.
-- Keep those records unchanged as legacy-global suppressions. Do not guess a backfill.
-- New binaries write only this table and read it alongside the legacy table.
-- Rollback: docs/migrations/V106-rollback.md.
CREATE TABLE state_suppressions_scoped (
    id            INTEGER PRIMARY KEY,
    state_table   TEXT    NOT NULL CHECK (state_table IN ('people', 'projects', 'commitments', 'open_loops')),
    namespace     TEXT    NOT NULL CHECK (namespace = 'world' OR (namespace LIKE 'agent_scratch:%' AND length(namespace) > 14)),
    match_digest  TEXT    NOT NULL,
    reason        TEXT    NOT NULL CHECK (reason IN ('deleted', 'edited')),
    suppressed_at INTEGER NOT NULL,
    UNIQUE (state_table, namespace, match_digest)
) STRICT;
