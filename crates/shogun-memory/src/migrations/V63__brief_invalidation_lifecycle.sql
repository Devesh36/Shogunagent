-- V63: keep completed briefs stable when unrelated history is rewritten, while fencing stale work.
-- V55's migration is already applied on main; preserve its columns, ledger rows, and state CHECK.
-- Rollback: docs/migrations/V63-rollback.md.

DROP TRIGGER IF EXISTS brief_generation_event_log_ai;
DROP TRIGGER IF EXISTS brief_generation_source_delete;
DROP TRIGGER IF EXISTS brief_generation_source_update;

-- Do not expire a frozen brief merely because an uncited event arrived in its time window.
-- Rust revalidates the selected manifest before submission and publication; source mutation
-- triggers below still invalidate any event the saved request actually cited.

-- Every expiry path retires provider handles before the row can be retried. `provider_id` is the
-- pre-existing V55 column and remains the sole provider handle for compatibility with main DBs.
CREATE TRIGGER brief_generation_expired_clear_handles
AFTER UPDATE OF state ON brief_generation
WHEN new.state = 'expired'
 AND (new.provider_id IS NOT NULL OR new.expected_custom_id IS NOT NULL OR new.submitted_at IS NOT NULL)
BEGIN
    UPDATE brief_generation
    SET provider_id = NULL, expected_custom_id = NULL, submitted_at = NULL
    WHERE date = new.date;
END;

-- A cited source rewrite/delete invalidates completed prose and any matching in-flight request.
CREATE TRIGGER brief_generation_source_update
BEFORE UPDATE OF ts, source, content, content_hash ON event_log
WHEN EXISTS (
    SELECT 1 FROM brief_generation AS generation
    WHERE old.ts >= generation.input_from_ts AND old.ts < generation.input_to_ts
      AND EXISTS (
        SELECT 1 FROM json_each(generation.sources_json) AS source
        WHERE json_extract(source.value, '$[0]') = old.id
    )
)
BEGIN
    UPDATE briefs
    SET generated = 0,
        payload = CASE WHEN json_valid(payload)
            THEN json_set(payload, '$.what_happened', json('[]'), '$.charm_line', NULL)
            ELSE '{}' END
    WHERE generated = 1
      AND date IN (
        SELECT generation.date FROM brief_generation AS generation
        WHERE old.ts >= generation.input_from_ts AND old.ts < generation.input_to_ts
          AND EXISTS (
            SELECT 1 FROM json_each(generation.sources_json) AS source
            WHERE json_extract(source.value, '$[0]') = old.id
          )
      );

    UPDATE brief_generation
    SET state = 'expired', sources_json = '[]', provider_id = NULL,
        updated_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000
    WHERE old.ts >= input_from_ts AND old.ts < input_to_ts
      AND EXISTS (
        SELECT 1 FROM json_each(sources_json) AS source
        WHERE json_extract(source.value, '$[0]') = old.id
      );
END;

CREATE TRIGGER brief_generation_source_delete
BEFORE DELETE ON event_log
BEGIN
    UPDATE briefs
    SET generated = 0,
        payload = CASE WHEN json_valid(payload)
            THEN json_set(payload, '$.what_happened', json('[]'), '$.charm_line', NULL)
            ELSE '{}' END
    WHERE date IN (
        SELECT date FROM brief_generation
        WHERE input_from_ts <= old.ts AND old.ts < input_to_ts
          AND EXISTS (
              SELECT 1 FROM json_each(sources_json)
              WHERE json_extract(value, '$[0]') = old.id
          )
    );

    UPDATE brief_generation
    SET state = 'expired', sources_json = '[]', provider_id = NULL,
        updated_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000
    WHERE input_from_ts <= old.ts AND old.ts < input_to_ts
      AND EXISTS (
          SELECT 1 FROM json_each(sources_json)
          WHERE json_extract(value, '$[0]') = old.id
      );
END;
