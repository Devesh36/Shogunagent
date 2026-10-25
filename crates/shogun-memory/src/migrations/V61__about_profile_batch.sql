-- V61: preserve About You's frozen in-flight attempt when cited source changes.
-- Rollback: docs/migrations/V61-rollback.md.

-- V60 defines these columns; verify the Batch metadata contract before replacing triggers.
SELECT provider_batch_id, expected_custom_id, submitted_at, attempt_context_digest
FROM about_profile_generation WHERE 0;

-- Published evidence can be removed while a newer weekly Batch is in flight. Clear the stale
-- published overview, but keep that frozen attempt and provider handle so the lifecycle can poll
-- once and apply its own exact-manifest fence. Outside an in-flight attempt, return to pending.
DROP TRIGGER about_profile_source_bd;
CREATE TRIGGER about_profile_source_bd
BEFORE DELETE ON event_log
WHEN EXISTS (
    SELECT 1 FROM about_profile_generation AS generation,
         json_each(generation.sources_json) AS source
    WHERE generation.id = 1 AND json_extract(source.value, '$[0]') = old.id
)
BEGIN
    UPDATE about_profile_generation
       SET summary = NULL, sources_json = '[]',
           attempt_sources_json = CASE WHEN state = 'generating' THEN attempt_sources_json ELSE '[]' END,
           attempt_context_digest = CASE WHEN state = 'generating' THEN attempt_context_digest ELSE '' END,
           provider_batch_id = CASE WHEN state = 'generating' THEN provider_batch_id ELSE NULL END,
           expected_custom_id = CASE WHEN state = 'generating' THEN expected_custom_id ELSE NULL END,
           submitted_at = CASE WHEN state = 'generating' THEN submitted_at ELSE NULL END,
           state = CASE WHEN state = 'generating' THEN state ELSE 'pending' END,
           last_success_at = NULL,
           updated_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000
     WHERE id = 1;
END;

DROP TRIGGER about_profile_source_bu;
CREATE TRIGGER about_profile_source_bu
BEFORE UPDATE OF ts, source, content, content_hash ON event_log
WHEN EXISTS (
    SELECT 1 FROM about_profile_generation AS generation,
         json_each(generation.sources_json) AS source
    WHERE generation.id = 1 AND json_extract(source.value, '$[0]') = old.id
)
BEGIN
    UPDATE about_profile_generation
       SET summary = NULL, sources_json = '[]',
           attempt_sources_json = CASE WHEN state = 'generating' THEN attempt_sources_json ELSE '[]' END,
           attempt_context_digest = CASE WHEN state = 'generating' THEN attempt_context_digest ELSE '' END,
           provider_batch_id = CASE WHEN state = 'generating' THEN provider_batch_id ELSE NULL END,
           expected_custom_id = CASE WHEN state = 'generating' THEN expected_custom_id ELSE NULL END,
           submitted_at = CASE WHEN state = 'generating' THEN submitted_at ELSE NULL END,
           state = CASE WHEN state = 'generating' THEN state ELSE 'pending' END,
           last_success_at = NULL,
           updated_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000
     WHERE id = 1;
END;
