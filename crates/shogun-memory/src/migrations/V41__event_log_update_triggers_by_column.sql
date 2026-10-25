-- V41: scope the three `AFTER/BEFORE UPDATE ON event_log` triggers to the columns whose change
-- actually invalidates what they maintain.
--
-- Why. The capture dedup path calls `event_log::touch` every ~2 s for an unchanged window
-- (`UPDATE event_log SET last_seen_at = …, dwell_ms = …`). V1's `event_log_au` fires on *any*
-- UPDATE, so each touch deleted and re-inserted the full body (≤32 KiB) into the trigram FTS;
-- V22's `period_summary_event_log_bu` marked every containing hour/day/week/month summary dirty
-- (so the nightly Compression job regenerated summaries for periods with no new content); V21's
-- `search_doc_event_log_au` invalidated the thread/session documents. Reading one window for an
-- hour ≈ 1,800 touches ≈ ~100 MB of trigram re-tokenisation per hour, plus FTS tombstone bloat —
-- directly against the 5 % idle-CPU SLO and the storage budget.
--
-- What changes. Same trigger names, same bodies, but with `UPDATE OF <columns>` so a touch
-- (last_seen_at / dwell_ms only) fires none of them. Content changes (visual-recall OCR
-- re-scan via `update_content_and_hash`), `ts` changes, and thread/session re-parenting keep
-- firing exactly as before.
--
-- Refinery keeps applied migrations immutable, so V1/V21/V22 are not edited; the triggers are
-- dropped and recreated here. `search_documents::ensure_projection_schema` re-runs V21's SQL at
-- runtime with `CREATE TRIGGER IF NOT EXISTS`, which leaves this narrower definition in place.
--
-- Also adds the index the Hot-layer startup rebuild has always needed:
-- `event_log.rs::rebuild_from_warm` reads `WHERE last_seen_at >= ? ORDER BY last_seen_at, id`,
-- which without this index is a full scan + sort of event_log on every launch.
--
-- Not a schema-shape change (no table/column dropped or renamed). Rollback: docs/migrations/V41-rollback.md.

DROP TRIGGER IF EXISTS event_log_au;
CREATE TRIGGER event_log_au AFTER UPDATE OF content, window_title ON event_log BEGIN
    INSERT INTO event_fts (event_fts, rowid, content, window_title)
    VALUES ('delete', old.id, old.content, old.window_title);
    INSERT INTO event_fts (rowid, content, window_title)
    VALUES (new.id, new.content, new.window_title);
END;

DROP TRIGGER IF EXISTS period_summary_event_log_bu;
CREATE TRIGGER period_summary_event_log_bu BEFORE UPDATE OF ts, content ON event_log BEGIN
    UPDATE period_summaries SET is_dirty = 1, dirty_at_ms = CAST(strftime('%s','now') AS INTEGER)*1000
     WHERE (old.ts >= period_start_ms AND old.ts < period_end_ms)
        OR (new.ts >= period_start_ms AND new.ts < period_end_ms);
    UPDATE period_summary_shard_runs SET state='pending', failure_class=NULL,
      updated_at_ms=CAST(strftime('%s','now') AS INTEGER)*1000
     WHERE (old.ts >= period_start_ms AND old.ts < period_end_ms)
        OR (new.ts >= period_start_ms AND new.ts < period_end_ms);
END;

DROP TRIGGER IF EXISTS search_doc_event_log_au;
CREATE TRIGGER search_doc_event_log_au AFTER UPDATE OF content, thread_key, session_id ON event_log BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'threads',CAST(id AS TEXT),'evidence_changed',updated_at,new.ts FROM threads WHERE thread_key IN(old.thread_key,new.thread_key) AND true
  ON CONFLICT(source_table,source_id) DO NOTHING;
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'sessions',CAST(id AS TEXT),'evidence_changed',updated_at,new.ts FROM sessions WHERE id IN(old.session_id,new.session_id) AND true
  ON CONFLICT(source_table,source_id) DO NOTHING;
END;

CREATE INDEX IF NOT EXISTS idx_event_log_last_seen ON event_log(last_seen_at, id);
