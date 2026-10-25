# V21 search-document projection rollback

V21 is additive. Preferred application rollback: deploy V20-compatible code and leave V21 tables
in place. Older code ignores them; canonical memory remains unchanged.

## Destructive schema rollback

Only reclaim V21 schema after all steps below are true:

1. Stop SHOGUN and create a verified encrypted backup with the documented SQLite backup path.
2. Confirm V22 or any later migration is absent. If present, roll it back first.
3. Deploy code whose `LATEST_SCHEMA_VERSION` is 20 before reopening the live database.
4. In one transaction, drop V21 canonical invalidation triggers (`search_doc_%`), then
   `search_documents_ai`, `search_documents_au`, and `search_documents_ad`; drop
   `search_documents_fts`; drop `search_document_provenance` (its indexes disappear with it);
   drop `search_documents`; drop the stable `search_document_invalidations` journal and
   `search_document_backfill_state` cursor last; finally delete only version 21 from
   `refinery_schema_history`.
5. Commit, run `PRAGMA foreign_key_check` and `PRAGMA quick_check`, then reopen with matching code.

Example trigger removal uses SQLite's exact V21 trigger inventory rather than a wildcard:

```sql
BEGIN IMMEDIATE;

DROP TRIGGER IF EXISTS search_doc_people_bd;
DROP TRIGGER IF EXISTS search_doc_people_bu;
DROP TRIGGER IF EXISTS search_doc_people_ai;
DROP TRIGGER IF EXISTS search_doc_projects_bd;
DROP TRIGGER IF EXISTS search_doc_projects_bu;
DROP TRIGGER IF EXISTS search_doc_projects_ai;
DROP TRIGGER IF EXISTS search_doc_commitments_bd;
DROP TRIGGER IF EXISTS search_doc_commitments_bu;
DROP TRIGGER IF EXISTS search_doc_commitments_ai;
DROP TRIGGER IF EXISTS search_doc_open_loops_bd;
DROP TRIGGER IF EXISTS search_doc_open_loops_bu;
DROP TRIGGER IF EXISTS search_doc_open_loops_ai;
DROP TRIGGER IF EXISTS search_doc_threads_bd;
DROP TRIGGER IF EXISTS search_doc_threads_bu;
DROP TRIGGER IF EXISTS search_doc_threads_ai;
DROP TRIGGER IF EXISTS search_doc_sessions_bd;
DROP TRIGGER IF EXISTS search_doc_sessions_bu;
DROP TRIGGER IF EXISTS search_doc_sessions_ai;
DROP TRIGGER IF EXISTS search_doc_session_notes_bd;
DROP TRIGGER IF EXISTS search_doc_session_notes_bu;
DROP TRIGGER IF EXISTS search_doc_session_notes_ai;
DROP TRIGGER IF EXISTS search_doc_meeting_recaps_bd;
DROP TRIGGER IF EXISTS search_doc_meeting_recaps_bu;
DROP TRIGGER IF EXISTS search_doc_meeting_recaps_ai;
DROP TRIGGER IF EXISTS search_doc_briefs_bd;
DROP TRIGGER IF EXISTS search_doc_briefs_bu;
DROP TRIGGER IF EXISTS search_doc_briefs_ai;
DROP TRIGGER IF EXISTS search_doc_lessons_bd;
DROP TRIGGER IF EXISTS search_doc_lessons_bu;
DROP TRIGGER IF EXISTS search_doc_lessons_ai;
DROP TRIGGER IF EXISTS search_doc_state_provenance_ai;
DROP TRIGGER IF EXISTS search_doc_state_provenance_au;
DROP TRIGGER IF EXISTS search_doc_state_provenance_ad;
DROP TRIGGER IF EXISTS search_doc_lesson_provenance_ai;
DROP TRIGGER IF EXISTS search_doc_lesson_provenance_au;
DROP TRIGGER IF EXISTS search_doc_lesson_provenance_ad;
DROP TRIGGER IF EXISTS search_doc_event_log_bd;
DROP TRIGGER IF EXISTS search_doc_transcript_segments_bd;
DROP TRIGGER IF EXISTS search_doc_feedback_events_bd;
DROP TRIGGER IF EXISTS search_doc_event_log_ai;
DROP TRIGGER IF EXISTS search_doc_event_log_au;
DROP TRIGGER IF EXISTS search_doc_transcript_segments_ai;
DROP TRIGGER IF EXISTS search_doc_transcript_segments_au;
DROP TRIGGER IF EXISTS search_doc_session_notes_related_ai;
DROP TRIGGER IF EXISTS search_doc_session_notes_related_au;
DROP TRIGGER IF EXISTS search_doc_session_notes_related_bd;
DROP TRIGGER IF EXISTS search_doc_meeting_recaps_related_ai;
DROP TRIGGER IF EXISTS search_doc_meeting_recaps_related_bu;
DROP TRIGGER IF EXISTS search_doc_meeting_recaps_related_bd;
DROP TRIGGER IF EXISTS search_documents_ai;
DROP TRIGGER IF EXISTS search_documents_au;
DROP TRIGGER IF EXISTS search_documents_ad;

DROP TABLE IF EXISTS search_documents_fts;
DROP TABLE IF EXISTS search_document_provenance;
DROP TABLE IF EXISTS search_documents;
DROP TABLE IF EXISTS search_document_invalidations;
DROP TABLE IF EXISTS search_document_backfill_state;
DELETE FROM refinery_schema_history WHERE version = 21;

COMMIT;
PRAGMA foreign_key_check;
PRAGMA quick_check;
```

## Projection-only repair

Do not edit `refinery_schema_history`. Call
`search_documents::recreate_projection_and_rebuild`; it drops only V21 derived storage, recreates
missing schema from embedded V21 DDL, reconciles canonical sources, and runs FTS5 `rebuild`. The
`search_document_invalidations` journal and `search_document_backfill_state` cursor are not dropped:
pending work survives repair, remains excluded from search, and resumes safely.
