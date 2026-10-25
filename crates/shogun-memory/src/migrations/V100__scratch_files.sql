-- Rollback: docs/migrations/V100-rollback.md
-- V100 (ShogunAgent E6): an agent's scratch files, the `/memories/scratch/<agent_id>/**` half of
-- the virtual filesystem (`memory.fs`). One row per (namespace, path): `namespace` is the agent's
-- own `agent_scratch:<agent_id>` (never 'world' — world paths are projections of the state tables
-- and are not stored here), `path` is the file's path below `/memories/scratch/<agent_id>/`,
-- `content` the current text, `actor` the last writer (V92 vocabulary). Text only, bounded by the
-- tool; nothing here is read by the world model, search or the review unless asked for by scope.
CREATE TABLE scratch_files (
    namespace  TEXT    NOT NULL CHECK (namespace LIKE 'agent_scratch:%'),
    path       TEXT    NOT NULL,
    content    TEXT    NOT NULL,
    actor      TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (namespace, path)
) STRICT;
CREATE INDEX scratch_files_updated_at ON scratch_files (namespace, updated_at);
