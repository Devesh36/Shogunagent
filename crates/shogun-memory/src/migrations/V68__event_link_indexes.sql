-- Rollback: docs/migrations/V68-rollback.md
-- #780: three lookups that had no index.
--
-- 1. screen_frames(event_id). `idx_screen_frames_timeline` (V58) leads with created_at_ms, so a
--    lookup by event_id scanned every frame: once per event in "Delete last hour / 24 hours"
--    (the FK child of event_log), once per evidence hit in chat and notch context assembly.
-- 2. period_summary_event_provenance(event_id). Its primary key leads with summary_id, so the
--    ON DELETE CASCADE from event_log scanned the whole table for every deleted event.
-- 3. period_summaries / period_summary_shard_runs by period_end_ms. The V22 triggers run on every
--    event insert, update and delete with `ts >= period_start_ms AND ts < period_end_ms`. With an
--    index leading on period_end_ms that is a range read of the periods ending after `ts` (for
--    a capture write, the few current ones) instead of a scan that grows with every night's
--    summaries.
--
-- Additive only: indexes, no data changes, no existing index dropped.
CREATE INDEX idx_screen_frames_event ON screen_frames (event_id);

CREATE INDEX idx_period_summary_event_provenance_event
    ON period_summary_event_provenance (event_id);

CREATE INDEX idx_period_summaries_end
    ON period_summaries (period_end_ms, period_start_ms);

CREATE INDEX idx_period_summary_shard_runs_end
    ON period_summary_shard_runs (period_end_ms, period_start_ms);
