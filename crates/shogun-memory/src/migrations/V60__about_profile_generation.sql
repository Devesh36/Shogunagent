-- V60: one source-backed, weekly generated "About You" summary.
-- Rollback: docs/migrations/V60-rollback.md.
-- Manual profile fields stay in memory_api.json and are never written here.

CREATE TABLE IF NOT EXISTS about_profile_generation (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    summary         TEXT,
    sources_json    TEXT NOT NULL DEFAULT '[]'
        CHECK (json_valid(sources_json) AND json_type(sources_json) = 'array'),
    attempt_sources_json TEXT NOT NULL DEFAULT '[]'
        CHECK (json_valid(attempt_sources_json) AND json_type(attempt_sources_json) = 'array'),
    state           TEXT NOT NULL DEFAULT 'locked'
        CHECK (state IN ('locked','pending','generating','complete','no_input','failed','uncertain')),
    active_days     INTEGER NOT NULL DEFAULT 0 CHECK (active_days >= 0),
    generation      INTEGER NOT NULL DEFAULT 0 CHECK (generation >= 0),
    request_id      TEXT NOT NULL UNIQUE DEFAULT (lower(hex(randomblob(16)))),
    last_success_at INTEGER,
    last_checked_at INTEGER,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    provider_batch_id TEXT CHECK (provider_batch_id IS NULL OR length(trim(provider_batch_id)) > 0),
    expected_custom_id TEXT CHECK (expected_custom_id IS NULL OR length(trim(expected_custom_id)) > 0),
    submitted_at INTEGER,
    attempt_context_digest TEXT NOT NULL DEFAULT '',
    CHECK (summary IS NULL OR length(trim(summary)) > 0)
) STRICT;

-- A generated profile cannot outlive a source the model cited. Clear only generated prose;
-- the editable profile in memory_api.json is outside this table and remains untouched.
DROP TRIGGER IF EXISTS about_profile_source_bd;
CREATE TRIGGER about_profile_source_bd
BEFORE DELETE ON event_log
WHEN EXISTS (
    SELECT 1 FROM about_profile_generation AS generation,
         json_each(generation.sources_json) AS source
    WHERE generation.id = 1 AND json_extract(source.value, '$[0]') = old.id
)
BEGIN
    UPDATE about_profile_generation
       SET summary = NULL, sources_json = '[]', attempt_sources_json = '[]', state = 'pending',
           last_success_at = NULL,
           updated_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000
     WHERE id = 1;
END;

DROP TRIGGER IF EXISTS about_profile_source_bu;
CREATE TRIGGER about_profile_source_bu
BEFORE UPDATE OF ts, source, content, content_hash ON event_log
WHEN EXISTS (
    SELECT 1 FROM about_profile_generation AS generation,
         json_each(generation.sources_json) AS source
    WHERE generation.id = 1 AND json_extract(source.value, '$[0]') = old.id
)
BEGIN
    UPDATE about_profile_generation
       SET summary = NULL, sources_json = '[]', attempt_sources_json = '[]', state = 'pending',
           last_success_at = NULL,
           updated_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000
     WHERE id = 1;
END;
