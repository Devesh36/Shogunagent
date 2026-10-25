-- Brief work is independent of the Dream classification ledger. No source text or retention pins.
CREATE TABLE brief_generation (
    date TEXT PRIMARY KEY,
    input_from_ts INTEGER NOT NULL,
    input_to_ts INTEGER NOT NULL,
    output_language TEXT NOT NULL,
    sources_json TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending','submitting','processing','uncertain','failed','no_input','credentials_unavailable','complete','expired')),
    provider_id TEXT,
    generation INTEGER NOT NULL DEFAULT 0,
    request_id TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK (input_from_ts <= input_to_ts)
) STRICT;
CREATE UNIQUE INDEX brief_generation_request_id_unique ON brief_generation(request_id);
-- A deleted/recreated day must not accept an older worker, even when its attempt number repeats.
CREATE TRIGGER brief_generation_new_identity AFTER INSERT ON brief_generation BEGIN
    UPDATE brief_generation SET request_id=lower(hex(randomblob(16))) WHERE date=new.date;
END;
-- Tombstone work before a source can disappear or change (also guards rowid reuse). Publishing
-- and this invalidation serialize in SQLite, including when another process deletes the source.
-- A manifest can only name events inside the row's own input window, so the window test on the
-- indexed columns runs first and the json_each walk only touches rows that could match: a bulk
-- "delete the last 24 hours" fires this once per event, and without the prefilter each firing
-- walked every ledger row's full manifest.
CREATE TRIGGER brief_generation_source_delete BEFORE DELETE ON event_log BEGIN
    UPDATE briefs SET generated=0, payload=CASE WHEN json_valid(payload)
        THEN json_set(payload,'$.what_happened',json('[]'),'$.charm_line',NULL)
        ELSE '{}' END
    WHERE date IN (SELECT date FROM brief_generation
        WHERE input_from_ts <= old.ts AND old.ts < input_to_ts AND EXISTS (
            SELECT 1 FROM json_each(sources_json) WHERE json_extract(value,'$[0]')=old.id
        ));
    UPDATE brief_generation SET state='expired', sources_json='[]', provider_id=NULL
    WHERE input_from_ts <= old.ts AND old.ts < input_to_ts AND EXISTS (
        SELECT 1 FROM json_each(sources_json) WHERE json_extract(value,'$[0]')=old.id
    );
END;
CREATE TRIGGER brief_generation_source_update BEFORE UPDATE OF ts,source,content,content_hash ON event_log BEGIN
    UPDATE briefs SET generated=0, payload=CASE WHEN json_valid(payload)
        THEN json_set(payload,'$.what_happened',json('[]'),'$.charm_line',NULL)
        ELSE '{}' END
    WHERE date IN (SELECT date FROM brief_generation
        WHERE input_from_ts <= old.ts AND old.ts < input_to_ts AND EXISTS (
            SELECT 1 FROM json_each(sources_json) WHERE json_extract(value,'$[0]')=old.id
        ));
    UPDATE brief_generation SET state='expired', sources_json='[]', provider_id=NULL
    WHERE input_from_ts <= old.ts AND old.ts < input_to_ts AND EXISTS (
        SELECT 1 FROM json_each(sources_json) WHERE json_extract(value,'$[0]')=old.id
    );
END;
-- Rollback: docs/migrations/V55-rollback.md (stop the app; drop the three triggers, then the
-- table). Existing briefs and Dream ledgers remain readable by the previous application version.
