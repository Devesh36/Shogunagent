-- Rollback: docs/migrations/V88-rollback.md
-- V88: Evening Wrap prose on the Batch lane, the Morning Brief's ledger (V55/V59/V63) for the
-- evening. The generated lines live on the row (`highlights_json`); the local Wrap sections stay
-- computed live. No source text is stored, only (event id, digest) pairs as in brief_generation.
CREATE TABLE wrap_generation (
    date TEXT PRIMARY KEY,
    input_from_ts INTEGER NOT NULL,
    input_to_ts INTEGER NOT NULL,
    output_language TEXT NOT NULL,
    sources_json TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending','submitting','processing','uncertain','failed','no_input','credentials_unavailable','complete','expired')),
    provider_id TEXT,
    expected_custom_id TEXT,
    submitted_at INTEGER,
    highlights_json TEXT NOT NULL DEFAULT '[]',
    generation INTEGER NOT NULL DEFAULT 0,
    request_id TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK (input_from_ts <= input_to_ts)
) STRICT;
CREATE UNIQUE INDEX wrap_generation_request_id_unique ON wrap_generation(request_id);

-- A deleted/recreated day must not accept an older worker, even when its attempt number repeats.
CREATE TRIGGER wrap_generation_new_identity AFTER INSERT ON wrap_generation BEGIN
    UPDATE wrap_generation SET request_id = lower(hex(randomblob(16))) WHERE date = new.date;
END;

-- Every expiry path retires provider handles and published prose before the row can be retried.
CREATE TRIGGER wrap_generation_expired_clear
AFTER UPDATE OF state ON wrap_generation
WHEN new.state = 'expired'
 AND (new.provider_id IS NOT NULL OR new.expected_custom_id IS NOT NULL OR new.submitted_at IS NOT NULL
      OR new.highlights_json <> '[]')
BEGIN
    UPDATE wrap_generation
    SET provider_id = NULL, expected_custom_id = NULL, submitted_at = NULL, highlights_json = '[]'
    WHERE date = new.date;
END;

-- A cited source rewrite/delete invalidates completed prose and any matching in-flight request.
-- The window test on indexed columns runs before the json_each walk (see V55).
CREATE TRIGGER wrap_generation_source_update
BEFORE UPDATE OF ts, source, content, content_hash ON event_log
BEGIN
    UPDATE wrap_generation
    SET state = 'expired', sources_json = '[]', provider_id = NULL, highlights_json = '[]',
        updated_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000
    WHERE old.ts >= input_from_ts AND old.ts < input_to_ts
      AND EXISTS (
        SELECT 1 FROM json_each(sources_json) AS source
        WHERE json_extract(source.value, '$[0]') = old.id
      );
END;

CREATE TRIGGER wrap_generation_source_delete
BEFORE DELETE ON event_log
BEGIN
    UPDATE wrap_generation
    SET state = 'expired', sources_json = '[]', provider_id = NULL, highlights_json = '[]',
        updated_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000
    WHERE old.ts >= input_from_ts AND old.ts < input_to_ts
      AND EXISTS (
        SELECT 1 FROM json_each(sources_json) AS source
        WHERE json_extract(source.value, '$[0]') = old.id
      );
END;
