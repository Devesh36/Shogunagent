-- Rollback: docs/migrations/V96-rollback.md
-- V96 (ShogunAgent E2-T3): per-principal metadata other than the schema version (which stays in
-- refinery_schema_history): export_cursor and sync_cursor for incremental export and the ShogunAI
-- sync, the principal id the file was created for, the import source. Small keyed values only;
-- never a secret, never content.
CREATE TABLE principal_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;
