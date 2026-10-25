-- V52: memory imported from another assistant (issues #384 / #391).
--
-- The user pastes a `shogun-memory-import/v1` package (or drops ChatGPT "Computer History"
-- Markdown, converted locally into the same package), reviews a preview, and commits the items
-- they selected. Nothing is written before that commit.
--
-- 1. `memory_imports`: one row per committed import — the provenance record every imported item
--    points at. `origin` is the platform the package came from; `provenance` is the label the
--    item carries everywhere (`imported:<origin>`), so an imported claim is never mistaken for
--    observed capture (event_log / state_provenance) or a user correction (user_pinned). The
--    package itself is not kept: only its size and xxh64 digest.
-- 2. `memory_import_items`: the selected claims. Confidence is CHECKed to <= 0.6 — an imported
--    claim is never High (the Medium ceiling knowledge_entities already uses for extracted
--    facts). `linked_table` / `linked_id` record a merge into an existing record — a link, never
--    an overwrite of the observed row.
--
-- Imported items deliberately do NOT go into people / projects / open_loops: those tables' evidence
-- is event_log rows (state_provenance FK), and the orphan sweep, pruning, split and Dream Cycle
-- re-extraction all assume that. See docs/specs/memory-import.md.
--
-- Additive: two new tables and two indexes. Old binaries ignore them.
-- Rollback: docs/migrations/V52-rollback.md.

CREATE TABLE memory_imports (
    id            INTEGER PRIMARY KEY,
    origin        TEXT    NOT NULL CHECK (origin IN ('chatgpt', 'claude', 'cursor', 'other', 'computer_history')),
    provenance    TEXT    NOT NULL CHECK (provenance = 'imported:' || origin),
    format        TEXT    NOT NULL CHECK (format IN ('shogun-memory-import/v1')),
    via           TEXT    NOT NULL CHECK (via IN ('ui', 'api')),
    generated_at  TEXT,
    coverage_note TEXT,
    package_bytes INTEGER NOT NULL,
    package_xxh64 TEXT    NOT NULL,
    item_count    INTEGER NOT NULL,
    created_at    INTEGER NOT NULL
) STRICT;

CREATE TABLE memory_import_items (
    id                   INTEGER PRIMARY KEY,
    import_id            INTEGER NOT NULL REFERENCES memory_imports (id) ON DELETE CASCADE,
    package_item         TEXT    NOT NULL,
    section              TEXT    NOT NULL CHECK (section IN
                             ('preference', 'working_style', 'goal', 'constraint', 'entity', 'relationship', 'open_loop')),
    kind                 TEXT,
    name                 TEXT    NOT NULL,
    target               TEXT,
    summary              TEXT,
    aliases              TEXT,
    status               TEXT    NOT NULL CHECK (status IN
                             ('active', 'uncertain', 'superseded', 'past', 'open', 'blocked', 'done')),
    confidence           REAL    NOT NULL CHECK (confidence >= 0.0 AND confidence <= 0.6),
    requested_confidence TEXT    NOT NULL CHECK (requested_confidence IN ('high', 'medium', 'low')),
    evidence             TEXT,
    match_key            TEXT    NOT NULL,
    linked_table         TEXT    CHECK (linked_table IN ('people', 'projects', 'open_loops', 'knowledge_entities')),
    linked_id            INTEGER,
    created_at           INTEGER NOT NULL,
    CHECK ((linked_table IS NULL) = (linked_id IS NULL))
) STRICT;

CREATE INDEX idx_memory_import_items_import ON memory_import_items (import_id);
CREATE INDEX idx_memory_import_items_match ON memory_import_items (section, match_key);
