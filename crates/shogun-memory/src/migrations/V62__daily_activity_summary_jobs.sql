-- V62: durable Batch jobs that upgrade canonical V22 day summaries to model-written prose.
-- Content remains in period_summaries; this table stores only frozen lineage and provider state.
-- Rollback: docs/migrations/V62-rollback.md. This migration also changes About You's
-- source-manifest meaning, so dropping this table alone would leave mixed source semantics.

CREATE TABLE IF NOT EXISTS daily_activity_summary_jobs (
    id                INTEGER PRIMARY KEY,
    local_date        TEXT NOT NULL,
    period_start_ms   INTEGER NOT NULL,
    period_end_ms     INTEGER NOT NULL,
    input_digest      TEXT NOT NULL UNIQUE,
    sources_json      TEXT NOT NULL
        CHECK (json_valid(sources_json) AND json_type(sources_json) = 'array'),
    state             TEXT NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending','generating','complete','failed','uncertain')),
    request_id        TEXT NOT NULL UNIQUE DEFAULT (lower(hex(randomblob(16)))),
    provider_batch_id TEXT,
    expected_custom_id TEXT,
    submitted_at      INTEGER,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    CHECK (period_start_ms < period_end_ms)
) STRICT;

DROP INDEX IF EXISTS daily_activity_summary_jobs_state;
CREATE INDEX daily_activity_summary_jobs_state
    ON daily_activity_summary_jobs(state,period_start_ms);

-- V60/V61 profile manifests cite raw event IDs. From V62 onward that manifest cites derived
-- day summaries instead.
-- Invalidate the published cache before removing its raw-event invalidation triggers, or a
-- later event deletion could leave private generated prose visible indefinitely. Keep frozen
-- attempt fields, request_id and Batch handles intact: known handles can be polled and
-- rejected by the new manifest fence; unknown submissions remain uncertain, not resubmitted.
UPDATE about_profile_generation
   SET summary=NULL, sources_json='[]', last_success_at=NULL,
       state=CASE WHEN state IN ('generating','uncertain') THEN state
                  WHEN provider_batch_id IS NOT NULL
                  THEN 'generating' ELSE 'pending' END,
       updated_at=CASE WHEN state IN ('generating','uncertain') OR provider_batch_id IS NOT NULL
                       THEN updated_at ELSE CAST(strftime('%s','now') AS INTEGER)*1000 END
 WHERE summary IS NOT NULL OR sources_json!='[]';

DROP TRIGGER about_profile_source_bd;
DROP TRIGGER about_profile_source_bu;

DROP TRIGGER IF EXISTS daily_activity_job_source_bd;
CREATE TRIGGER daily_activity_job_source_bd
BEFORE DELETE ON event_log
BEGIN
    UPDATE daily_activity_summary_jobs
       SET state=CASE WHEN state='generating' THEN state ELSE 'failed' END,
           updated_at=CAST(strftime('%s','now') AS INTEGER)*1000
     WHERE state IN ('pending','generating') AND EXISTS (
        SELECT 1 FROM json_each(sources_json) source
         WHERE json_extract(source.value,'$.event_id')=old.id
     );
END;

DROP TRIGGER IF EXISTS daily_activity_job_source_bu;
CREATE TRIGGER daily_activity_job_source_bu
BEFORE UPDATE OF ts,source,content,content_hash ON event_log
BEGIN
    UPDATE daily_activity_summary_jobs
       SET state=CASE WHEN state='generating' THEN state ELSE 'failed' END,
           updated_at=CAST(strftime('%s','now') AS INTEGER)*1000
     WHERE state IN ('pending','generating') AND EXISTS (
        SELECT 1 FROM json_each(sources_json) source
         WHERE json_extract(source.value,'$.event_id')=old.id
     );
END;

-- V41 ignores source-only changes for generic compression. Daily cloud eligibility depends on
-- source, so capture -> excluded must hide and regenerate this specific derived row.
DROP TRIGGER IF EXISTS daily_activity_summary_source_bu;
CREATE TRIGGER daily_activity_summary_source_bu
AFTER UPDATE OF source ON event_log
BEGIN
    UPDATE period_summaries SET is_dirty=1,
           dirty_at_ms=CAST(strftime('%s','now') AS INTEGER)*1000
     WHERE level='day' AND generation_model LIKE 'daily_activity_summary/%'
       AND id IN (SELECT summary_id FROM period_summary_event_provenance WHERE event_id=old.id);
    UPDATE period_summary_shard_runs SET state='pending',failure_class=NULL,
           updated_at_ms=CAST(strftime('%s','now') AS INTEGER)*1000
     WHERE level='day' AND EXISTS (
        SELECT 1 FROM period_summaries s
         JOIN period_summary_event_provenance p ON p.summary_id=s.id
        WHERE p.event_id=old.id AND s.level='day'
          AND s.period_start_ms=period_summary_shard_runs.period_start_ms
          AND s.period_end_ms=period_summary_shard_runs.period_end_ms
     );
END;

DROP TRIGGER IF EXISTS about_profile_period_bd;
CREATE TRIGGER about_profile_period_bd
BEFORE DELETE ON period_summaries
WHEN EXISTS (
    SELECT 1 FROM about_profile_generation generation,
         json_each(json_extract(generation.sources_json,'$[1]')) source
     WHERE generation.id=1 AND json_extract(source.value,'$[0]')=old.id
)
OR EXISTS (
    SELECT 1 FROM about_profile_generation generation,
         json_each(json_extract(generation.attempt_sources_json,'$[1]')) source
     WHERE generation.id=1 AND json_extract(source.value,'$[0]')=old.id
)
BEGIN
    UPDATE about_profile_generation
       SET summary=NULL,sources_json='[]',last_success_at=NULL,
           attempt_sources_json=CASE WHEN state='generating' THEN attempt_sources_json ELSE '[]' END,
           attempt_context_digest=CASE WHEN state='generating' THEN attempt_context_digest ELSE '' END,
           provider_batch_id=CASE WHEN state='generating' THEN provider_batch_id ELSE NULL END,
           expected_custom_id=CASE WHEN state='generating' THEN expected_custom_id ELSE NULL END,
           submitted_at=CASE WHEN state='generating' THEN submitted_at ELSE NULL END,
           state=CASE WHEN state='generating' THEN state ELSE 'pending' END,
           updated_at=CAST(strftime('%s','now') AS INTEGER)*1000 WHERE id=1;
END;

DROP TRIGGER IF EXISTS about_profile_period_bu;
CREATE TRIGGER about_profile_period_bu
BEFORE UPDATE OF content_digest,is_dirty ON period_summaries
WHEN EXISTS (
    SELECT 1 FROM about_profile_generation generation,
         json_each(json_extract(generation.sources_json,'$[1]')) source
     WHERE generation.id=1 AND json_extract(source.value,'$[0]')=old.id
)
OR EXISTS (
    SELECT 1 FROM about_profile_generation generation,
         json_each(json_extract(generation.attempt_sources_json,'$[1]')) source
     WHERE generation.id=1 AND json_extract(source.value,'$[0]')=old.id
)
BEGIN
    UPDATE about_profile_generation
       SET summary=NULL,sources_json='[]',last_success_at=NULL,
           attempt_sources_json=CASE WHEN state='generating' THEN attempt_sources_json ELSE '[]' END,
           attempt_context_digest=CASE WHEN state='generating' THEN attempt_context_digest ELSE '' END,
           provider_batch_id=CASE WHEN state='generating' THEN provider_batch_id ELSE NULL END,
           expected_custom_id=CASE WHEN state='generating' THEN expected_custom_id ELSE NULL END,
           submitted_at=CASE WHEN state='generating' THEN submitted_at ELSE NULL END,
           state=CASE WHEN state='generating' THEN state ELSE 'pending' END,
           updated_at=CAST(strftime('%s','now') AS INTEGER)*1000 WHERE id=1;
END;
