-- Rollback: docs/migrations/V95-rollback.md
-- V95 (ShogunAgent E2-T3): deletion markers for the export format v1 and sync. A row deleted from
-- a principal's store (an event, a state row, a relation) leaves (kind, id, deleted_at, reason)
-- here so an incremental export or a sync peer can retire its copy; the deleted content itself is
-- never kept. One marker per (kind, id); a re-deletion refreshes the time.
CREATE TABLE export_tombstones (
    kind       TEXT    NOT NULL,
    id         INTEGER NOT NULL,
    deleted_at INTEGER NOT NULL,
    reason     TEXT    NOT NULL DEFAULT '',
    PRIMARY KEY (kind, id)
) STRICT;
CREATE INDEX export_tombstones_deleted_at ON export_tombstones (deleted_at);
