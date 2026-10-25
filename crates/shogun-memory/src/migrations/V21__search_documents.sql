-- Rebuildable lexical projection over canonical, already-distilled memory.
-- Canonical writes never copy projection text here: source/evidence triggers only delete or mark
-- existing documents dirty. V21 reserves the period_summary document-kind value, but deliberately
-- reserves its period_summaries source mapping, but has no parent table, FK, trigger, row, or
-- builder; V22 owns those objects.

-- Durable keyset cursors keep initial canonical backfill bounded and crash-resumable. Optional
-- projection repair deliberately preserves this table.
CREATE TABLE IF NOT EXISTS search_document_backfill_state (
    source_table      TEXT PRIMARY KEY CHECK (source_table IN (
        'people', 'projects', 'commitments', 'open_loops', 'threads', 'sessions',
        'meeting_recaps', 'session_notes', 'briefs', 'lessons'
    )),
    last_source_rowid INTEGER NOT NULL DEFAULT 0 CHECK (last_source_rowid >= 0),
    is_complete       INTEGER NOT NULL DEFAULT 0 CHECK (is_complete IN (0, 1)),
    updated_at_ms     INTEGER NOT NULL DEFAULT 0
) STRICT;

INSERT OR IGNORE INTO search_document_backfill_state (source_table) VALUES
    ('people'), ('projects'), ('commitments'), ('open_loops'), ('threads'), ('sessions'),
    ('meeting_recaps'), ('session_notes'), ('briefs'), ('lessons');

-- Stable canonical-side repair journal. Projection repair deliberately never drops this table:
-- it is the durable privacy boundary when an optional projection object is absent or corrupt.
CREATE TABLE IF NOT EXISTS search_document_invalidations (
    id                              INTEGER PRIMARY KEY,
    source_table                    TEXT NOT NULL CHECK (source_table IN (
        'people', 'projects', 'commitments', 'open_loops', 'threads', 'sessions',
        'meeting_recaps', 'session_notes', 'briefs', 'lessons'
    )),
    source_id                       TEXT NOT NULL,
    invalidation_kind               TEXT NOT NULL CHECK (invalidation_kind IN (
        'source_delete', 'source_changed', 'evidence_changed', 'evidence_deleted'
    )),
    required_owner_updated_after_ms INTEGER,
    invalidated_at_ms               INTEGER NOT NULL,
    UNIQUE (source_table, source_id)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_search_document_invalidations_scan
    ON search_document_invalidations (id, source_table, source_id);
CREATE INDEX IF NOT EXISTS idx_search_document_invalidations_kind
    ON search_document_invalidations (invalidation_kind, invalidated_at_ms);

CREATE TABLE IF NOT EXISTS search_documents (
    id                   INTEGER PRIMARY KEY,
    document_kind        TEXT NOT NULL CHECK (document_kind IN (
        'person', 'project', 'commitment', 'open_loop', 'thread_summary',
        'session_summary', 'meeting_recap', 'meeting_note', 'brief', 'lesson',
        'period_summary'
    )),
    source_table         TEXT NOT NULL CHECK (source_table IN (
        'people', 'projects', 'commitments', 'open_loops', 'threads', 'sessions',
        'meeting_recaps', 'session_notes', 'briefs', 'lessons', 'period_summaries'
    )),
    source_id            TEXT NOT NULL,
    title                TEXT NOT NULL,
    body                 TEXT NOT NULL,
    period_start_ms      INTEGER,
    period_end_ms        INTEGER,
    confidence           REAL NOT NULL CHECK (confidence >= 0.0 AND confidence <= 1.0),
    generation_kind      TEXT NOT NULL CHECK (generation_kind IN ('canonical', 'extractive', 'model')),
    generation_model     TEXT,
    generated_at_ms      INTEGER,
    content_digest       TEXT NOT NULL,
    source_updated_at_ms INTEGER NOT NULL,
    is_dirty             INTEGER NOT NULL DEFAULT 0 CHECK (is_dirty IN (0, 1)),
    dirty_at_ms          INTEGER,
    last_projected_at_ms INTEGER NOT NULL,
    last_failure_class   TEXT,
    created_at_ms        INTEGER NOT NULL,
    updated_at_ms        INTEGER NOT NULL,
    CHECK ((period_start_ms IS NULL AND period_end_ms IS NULL)
        OR (period_start_ms IS NOT NULL AND period_end_ms IS NOT NULL
            AND period_start_ms < period_end_ms)),
    CHECK ((generation_kind = 'model' AND generation_model IS NOT NULL)
        OR (generation_kind IN ('canonical', 'extractive') AND generation_model IS NULL)),
    CHECK (
        (document_kind = 'person' AND source_table = 'people') OR
        (document_kind = 'project' AND source_table = 'projects') OR
        (document_kind = 'commitment' AND source_table = 'commitments') OR
        (document_kind = 'open_loop' AND source_table = 'open_loops') OR
        (document_kind = 'thread_summary' AND source_table = 'threads') OR
        (document_kind = 'session_summary' AND source_table = 'sessions') OR
        (document_kind = 'meeting_recap' AND source_table = 'meeting_recaps') OR
        (document_kind = 'meeting_note' AND source_table = 'session_notes') OR
        (document_kind = 'brief' AND source_table = 'briefs') OR
        (document_kind = 'lesson' AND source_table = 'lessons') OR
        (document_kind = 'period_summary' AND source_table = 'period_summaries')
    ),
    UNIQUE (source_table, source_id)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_search_documents_ready
    ON search_documents (is_dirty, document_kind, period_start_ms, period_end_ms);
CREATE INDEX IF NOT EXISTS idx_search_documents_freshness
    ON search_documents (is_dirty, source_updated_at_ms, last_projected_at_ms);

CREATE VIRTUAL TABLE IF NOT EXISTS search_documents_fts USING fts5(
    title,
    body,
    document_kind UNINDEXED,
    content='search_documents',
    content_rowid='id',
    tokenize='trigram'
);

CREATE TRIGGER IF NOT EXISTS search_documents_ai AFTER INSERT ON search_documents BEGIN
    INSERT INTO search_documents_fts (rowid, title, body, document_kind)
    VALUES (new.id, new.title, new.body, new.document_kind);
END;
CREATE TRIGGER IF NOT EXISTS search_documents_ad AFTER DELETE ON search_documents BEGIN
    INSERT INTO search_documents_fts (search_documents_fts, rowid, title, body, document_kind)
    VALUES ('delete', old.id, old.title, old.body, old.document_kind);
END;
CREATE TRIGGER IF NOT EXISTS search_documents_au
AFTER UPDATE OF title, body, document_kind ON search_documents BEGIN
    INSERT INTO search_documents_fts (search_documents_fts, rowid, title, body, document_kind)
    VALUES ('delete', old.id, old.title, old.body, old.document_kind);
    INSERT INTO search_documents_fts (rowid, title, body, document_kind)
    VALUES (new.id, new.title, new.body, new.document_kind);
END;

CREATE TABLE IF NOT EXISTS search_document_provenance (
    id                    INTEGER PRIMARY KEY,
    document_id           INTEGER NOT NULL REFERENCES search_documents (id) ON DELETE CASCADE,
    evidence_kind         TEXT NOT NULL CHECK (evidence_kind IN (
        'event', 'session', 'transcript_segment', 'session_note', 'meeting_recap',
        'brief', 'feedback_event'
    )),
    event_id              INTEGER REFERENCES event_log (id) ON DELETE CASCADE,
    session_id            INTEGER REFERENCES sessions (id) ON DELETE CASCADE,
    transcript_segment_id INTEGER REFERENCES transcript_segments (id) ON DELETE CASCADE,
    session_note_id       INTEGER REFERENCES session_notes (id) ON DELETE CASCADE,
    meeting_recap_id      INTEGER REFERENCES meeting_recaps (id) ON DELETE CASCADE,
    brief_date            TEXT REFERENCES briefs (date) ON DELETE CASCADE,
    feedback_event_id     INTEGER REFERENCES feedback_events (id) ON DELETE CASCADE,
    weight                REAL NOT NULL DEFAULT 1.0 CHECK (weight > 0.0 AND weight <= 1.0),
    CHECK (
        (evidence_kind = 'event' AND event_id IS NOT NULL AND session_id IS NULL
         AND transcript_segment_id IS NULL AND session_note_id IS NULL
         AND meeting_recap_id IS NULL AND brief_date IS NULL AND feedback_event_id IS NULL) OR
        (evidence_kind = 'session' AND event_id IS NULL AND session_id IS NOT NULL
         AND transcript_segment_id IS NULL AND session_note_id IS NULL
         AND meeting_recap_id IS NULL AND brief_date IS NULL AND feedback_event_id IS NULL) OR
        (evidence_kind = 'transcript_segment' AND event_id IS NULL AND session_id IS NULL
         AND transcript_segment_id IS NOT NULL AND session_note_id IS NULL
         AND meeting_recap_id IS NULL AND brief_date IS NULL AND feedback_event_id IS NULL) OR
        (evidence_kind = 'session_note' AND event_id IS NULL AND session_id IS NULL
         AND transcript_segment_id IS NULL AND session_note_id IS NOT NULL
         AND meeting_recap_id IS NULL AND brief_date IS NULL AND feedback_event_id IS NULL) OR
        (evidence_kind = 'meeting_recap' AND event_id IS NULL AND session_id IS NULL
         AND transcript_segment_id IS NULL AND session_note_id IS NULL
         AND meeting_recap_id IS NOT NULL AND brief_date IS NULL AND feedback_event_id IS NULL) OR
        (evidence_kind = 'brief' AND event_id IS NULL AND session_id IS NULL
         AND transcript_segment_id IS NULL AND session_note_id IS NULL
         AND meeting_recap_id IS NULL AND brief_date IS NOT NULL AND feedback_event_id IS NULL) OR
        (evidence_kind = 'feedback_event' AND event_id IS NULL AND session_id IS NULL
         AND transcript_segment_id IS NULL AND session_note_id IS NULL
         AND meeting_recap_id IS NULL AND brief_date IS NULL AND feedback_event_id IS NOT NULL)
    )
) STRICT;

CREATE UNIQUE INDEX IF NOT EXISTS search_doc_prov_event_uq
    ON search_document_provenance (document_id, event_id) WHERE event_id IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS search_doc_prov_session_uq
    ON search_document_provenance (document_id, session_id) WHERE session_id IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS search_doc_prov_transcript_segment_uq
    ON search_document_provenance (document_id, transcript_segment_id)
    WHERE transcript_segment_id IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS search_doc_prov_session_note_uq
    ON search_document_provenance (document_id, session_note_id)
    WHERE session_note_id IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS search_doc_prov_meeting_recap_uq
    ON search_document_provenance (document_id, meeting_recap_id)
    WHERE meeting_recap_id IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS search_doc_prov_brief_uq
    ON search_document_provenance (document_id, brief_date) WHERE brief_date IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS search_doc_prov_feedback_event_uq
    ON search_document_provenance (document_id, feedback_event_id)
    WHERE feedback_event_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS search_doc_prov_event_idx
    ON search_document_provenance (event_id, document_id) WHERE event_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS search_doc_prov_session_idx
    ON search_document_provenance (session_id, document_id) WHERE session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS search_doc_prov_transcript_segment_idx
    ON search_document_provenance (transcript_segment_id, document_id)
    WHERE transcript_segment_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS search_doc_prov_session_note_idx
    ON search_document_provenance (session_note_id, document_id)
    WHERE session_note_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS search_doc_prov_meeting_recap_idx
    ON search_document_provenance (meeting_recap_id, document_id)
    WHERE meeting_recap_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS search_doc_prov_brief_idx
    ON search_document_provenance (brief_date, document_id) WHERE brief_date IS NOT NULL;
CREATE INDEX IF NOT EXISTS search_doc_prov_feedback_event_idx
    ON search_document_provenance (feedback_event_id, document_id)
    WHERE feedback_event_id IS NOT NULL;

-- Canonical source triggers write only content-free journal rows. They never reference optional
-- projection tables, so canonical persistence stays available during projection repair.
CREATE TRIGGER IF NOT EXISTS search_doc_people_ai AFTER INSERT ON people BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  VALUES('people',CAST(new.id AS TEXT),'source_changed',NULL,new.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_changed',required_owner_updated_after_ms=NULL,invalidated_at_ms=excluded.invalidated_at_ms;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_people_bu BEFORE UPDATE ON people BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  VALUES('people',CAST(old.id AS TEXT),'source_changed',old.updated_at,new.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_changed',required_owner_updated_after_ms=MAX(COALESCE(required_owner_updated_after_ms,excluded.required_owner_updated_after_ms),excluded.required_owner_updated_after_ms),invalidated_at_ms=excluded.invalidated_at_ms;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_people_bd BEFORE DELETE ON people BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,invalidated_at_ms)
  VALUES('people',CAST(old.id AS TEXT),'source_delete',old.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_delete',required_owner_updated_after_ms=NULL,invalidated_at_ms=excluded.invalidated_at_ms;
END;

CREATE TRIGGER IF NOT EXISTS search_doc_projects_ai AFTER INSERT ON projects BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms) VALUES('projects',CAST(new.id AS TEXT),'source_changed',NULL,new.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_changed',required_owner_updated_after_ms=NULL,invalidated_at_ms=excluded.invalidated_at_ms;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_projects_bu BEFORE UPDATE ON projects BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms) VALUES('projects',CAST(old.id AS TEXT),'source_changed',old.updated_at,new.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_changed',required_owner_updated_after_ms=MAX(COALESCE(required_owner_updated_after_ms,excluded.required_owner_updated_after_ms),excluded.required_owner_updated_after_ms),invalidated_at_ms=excluded.invalidated_at_ms;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_projects_bd BEFORE DELETE ON projects BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,invalidated_at_ms) VALUES('projects',CAST(old.id AS TEXT),'source_delete',old.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_delete',required_owner_updated_after_ms=NULL,invalidated_at_ms=excluded.invalidated_at_ms;
END;

CREATE TRIGGER IF NOT EXISTS search_doc_commitments_ai AFTER INSERT ON commitments BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms) VALUES('commitments',CAST(new.id AS TEXT),'source_changed',NULL,new.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_changed',required_owner_updated_after_ms=NULL,invalidated_at_ms=excluded.invalidated_at_ms;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_commitments_bu BEFORE UPDATE ON commitments BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms) VALUES('commitments',CAST(old.id AS TEXT),'source_changed',old.updated_at,new.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_changed',required_owner_updated_after_ms=MAX(COALESCE(required_owner_updated_after_ms,excluded.required_owner_updated_after_ms),excluded.required_owner_updated_after_ms),invalidated_at_ms=excluded.invalidated_at_ms;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_commitments_bd BEFORE DELETE ON commitments BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,invalidated_at_ms) VALUES('commitments',CAST(old.id AS TEXT),'source_delete',old.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_delete',required_owner_updated_after_ms=NULL,invalidated_at_ms=excluded.invalidated_at_ms;
END;

CREATE TRIGGER IF NOT EXISTS search_doc_open_loops_ai AFTER INSERT ON open_loops BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms) VALUES('open_loops',CAST(new.id AS TEXT),'source_changed',NULL,new.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_changed',required_owner_updated_after_ms=NULL,invalidated_at_ms=excluded.invalidated_at_ms;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_open_loops_bu BEFORE UPDATE ON open_loops BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms) VALUES('open_loops',CAST(old.id AS TEXT),'source_changed',old.updated_at,new.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_changed',required_owner_updated_after_ms=MAX(COALESCE(required_owner_updated_after_ms,excluded.required_owner_updated_after_ms),excluded.required_owner_updated_after_ms),invalidated_at_ms=excluded.invalidated_at_ms;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_open_loops_bd BEFORE DELETE ON open_loops BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,invalidated_at_ms) VALUES('open_loops',CAST(old.id AS TEXT),'source_delete',old.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_delete',required_owner_updated_after_ms=NULL,invalidated_at_ms=excluded.invalidated_at_ms;
END;

CREATE TRIGGER IF NOT EXISTS search_doc_threads_ai AFTER INSERT ON threads BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms) VALUES('threads',CAST(new.id AS TEXT),'source_changed',NULL,new.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_changed',required_owner_updated_after_ms=NULL,invalidated_at_ms=excluded.invalidated_at_ms;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_threads_bu BEFORE UPDATE ON threads BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms) VALUES('threads',CAST(old.id AS TEXT),'source_changed',old.updated_at,new.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_changed',required_owner_updated_after_ms=MAX(COALESCE(required_owner_updated_after_ms,excluded.required_owner_updated_after_ms),excluded.required_owner_updated_after_ms),invalidated_at_ms=excluded.invalidated_at_ms;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_threads_bd BEFORE DELETE ON threads BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,invalidated_at_ms) VALUES('threads',CAST(old.id AS TEXT),'source_delete',old.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_delete',required_owner_updated_after_ms=NULL,invalidated_at_ms=excluded.invalidated_at_ms;
END;

CREATE TRIGGER IF NOT EXISTS search_doc_sessions_ai AFTER INSERT ON sessions BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms) VALUES('sessions',CAST(new.id AS TEXT),'source_changed',NULL,new.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_changed',required_owner_updated_after_ms=NULL,invalidated_at_ms=excluded.invalidated_at_ms;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_sessions_bu BEFORE UPDATE ON sessions BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms) VALUES('sessions',CAST(old.id AS TEXT),'source_changed',old.updated_at,new.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_changed',required_owner_updated_after_ms=MAX(COALESCE(required_owner_updated_after_ms,excluded.required_owner_updated_after_ms),excluded.required_owner_updated_after_ms),invalidated_at_ms=excluded.invalidated_at_ms;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_sessions_bd BEFORE DELETE ON sessions BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,invalidated_at_ms) VALUES('sessions',CAST(old.id AS TEXT),'source_delete',old.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_delete',required_owner_updated_after_ms=NULL,invalidated_at_ms=excluded.invalidated_at_ms;
END;

CREATE TRIGGER IF NOT EXISTS search_doc_session_notes_ai AFTER INSERT ON session_notes BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms) VALUES('session_notes',CAST(new.id AS TEXT),'source_changed',NULL,new.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_changed',required_owner_updated_after_ms=NULL,invalidated_at_ms=excluded.invalidated_at_ms;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_session_notes_bu BEFORE UPDATE ON session_notes BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms) VALUES('session_notes',CAST(old.id AS TEXT),'source_changed',old.updated_at,new.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_changed',required_owner_updated_after_ms=MAX(COALESCE(required_owner_updated_after_ms,excluded.required_owner_updated_after_ms),excluded.required_owner_updated_after_ms),invalidated_at_ms=excluded.invalidated_at_ms;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_session_notes_bd BEFORE DELETE ON session_notes BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,invalidated_at_ms) VALUES('session_notes',CAST(old.id AS TEXT),'source_delete',old.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_delete',required_owner_updated_after_ms=NULL,invalidated_at_ms=excluded.invalidated_at_ms;
END;

CREATE TRIGGER IF NOT EXISTS search_doc_meeting_recaps_ai AFTER INSERT ON meeting_recaps BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms) VALUES('meeting_recaps',CAST(new.id AS TEXT),'source_changed',NULL,new.created_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_changed',required_owner_updated_after_ms=NULL,invalidated_at_ms=excluded.invalidated_at_ms;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_meeting_recaps_bu BEFORE UPDATE ON meeting_recaps BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms) VALUES('meeting_recaps',CAST(old.id AS TEXT),'source_changed',old.created_at,new.created_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_changed',required_owner_updated_after_ms=MAX(COALESCE(required_owner_updated_after_ms,excluded.required_owner_updated_after_ms),excluded.required_owner_updated_after_ms),invalidated_at_ms=excluded.invalidated_at_ms;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_meeting_recaps_bd BEFORE DELETE ON meeting_recaps BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,invalidated_at_ms) VALUES('meeting_recaps',CAST(old.id AS TEXT),'source_delete',old.created_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_delete',required_owner_updated_after_ms=NULL,invalidated_at_ms=excluded.invalidated_at_ms;
END;

CREATE TRIGGER IF NOT EXISTS search_doc_briefs_ai AFTER INSERT ON briefs BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms) VALUES('briefs',new.date,'source_changed',NULL,new.built_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_changed',required_owner_updated_after_ms=NULL,invalidated_at_ms=excluded.invalidated_at_ms;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_briefs_bu BEFORE UPDATE ON briefs BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms) VALUES('briefs',old.date,'source_changed',old.built_at,new.built_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_changed',required_owner_updated_after_ms=MAX(COALESCE(required_owner_updated_after_ms,excluded.required_owner_updated_after_ms),excluded.required_owner_updated_after_ms),invalidated_at_ms=excluded.invalidated_at_ms;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_briefs_bd BEFORE DELETE ON briefs BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,invalidated_at_ms) VALUES('briefs',old.date,'source_delete',old.built_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_delete',required_owner_updated_after_ms=NULL,invalidated_at_ms=excluded.invalidated_at_ms;
END;

CREATE TRIGGER IF NOT EXISTS search_doc_lessons_ai AFTER INSERT ON lessons BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms) VALUES('lessons',CAST(new.id AS TEXT),'source_changed',NULL,new.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_changed',required_owner_updated_after_ms=NULL,invalidated_at_ms=excluded.invalidated_at_ms;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_lessons_bu BEFORE UPDATE ON lessons BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms) VALUES('lessons',CAST(old.id AS TEXT),'source_changed',old.updated_at,new.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_changed',required_owner_updated_after_ms=MAX(COALESCE(required_owner_updated_after_ms,excluded.required_owner_updated_after_ms),excluded.required_owner_updated_after_ms),invalidated_at_ms=excluded.invalidated_at_ms;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_lessons_bd BEFORE DELETE ON lessons BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,invalidated_at_ms) VALUES('lessons',CAST(old.id AS TEXT),'source_delete',old.updated_at)
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind='source_delete',required_owner_updated_after_ms=NULL,invalidated_at_ms=excluded.invalidated_at_ms;
END;

-- State and lesson relation changes invalidate old and new owners without reading projection.
CREATE TRIGGER IF NOT EXISTS search_doc_state_provenance_ai AFTER INSERT ON state_provenance BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT new.state_table,CAST(new.state_id AS TEXT),'evidence_changed',
    CASE new.state_table WHEN 'people' THEN (SELECT updated_at FROM people WHERE id=new.state_id) WHEN 'projects' THEN (SELECT updated_at FROM projects WHERE id=new.state_id) WHEN 'commitments' THEN (SELECT updated_at FROM commitments WHERE id=new.state_id) ELSE (SELECT updated_at FROM open_loops WHERE id=new.state_id) END,
    CAST(strftime('%s','now') AS INTEGER)*1000
  ON CONFLICT(source_table,source_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_state_provenance_au AFTER UPDATE ON state_provenance BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT old.state_table,CAST(old.state_id AS TEXT),'evidence_changed',CASE old.state_table WHEN 'people' THEN (SELECT updated_at FROM people WHERE id=old.state_id) WHEN 'projects' THEN (SELECT updated_at FROM projects WHERE id=old.state_id) WHEN 'commitments' THEN (SELECT updated_at FROM commitments WHERE id=old.state_id) ELSE (SELECT updated_at FROM open_loops WHERE id=old.state_id) END,CAST(strftime('%s','now') AS INTEGER)*1000
  ON CONFLICT(source_table,source_id) DO NOTHING;
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT new.state_table,CAST(new.state_id AS TEXT),'evidence_changed',CASE new.state_table WHEN 'people' THEN (SELECT updated_at FROM people WHERE id=new.state_id) WHEN 'projects' THEN (SELECT updated_at FROM projects WHERE id=new.state_id) WHEN 'commitments' THEN (SELECT updated_at FROM commitments WHERE id=new.state_id) ELSE (SELECT updated_at FROM open_loops WHERE id=new.state_id) END,CAST(strftime('%s','now') AS INTEGER)*1000
  ON CONFLICT(source_table,source_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_state_provenance_ad AFTER DELETE ON state_provenance BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT old.state_table,CAST(old.state_id AS TEXT),'evidence_deleted',CASE old.state_table WHEN 'people' THEN (SELECT updated_at FROM people WHERE id=old.state_id) WHEN 'projects' THEN (SELECT updated_at FROM projects WHERE id=old.state_id) WHEN 'commitments' THEN (SELECT updated_at FROM commitments WHERE id=old.state_id) ELSE (SELECT updated_at FROM open_loops WHERE id=old.state_id) END,CAST(strftime('%s','now') AS INTEGER)*1000
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind=excluded.invalidation_kind, required_owner_updated_after_ms=excluded.required_owner_updated_after_ms, invalidated_at_ms=excluded.invalidated_at_ms;
END;

CREATE TRIGGER IF NOT EXISTS search_doc_lesson_provenance_ai AFTER INSERT ON lesson_provenance BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'lessons',CAST(new.lesson_id AS TEXT),'evidence_changed',updated_at,CAST(strftime('%s','now') AS INTEGER)*1000 FROM lessons WHERE id=new.lesson_id AND true
  ON CONFLICT(source_table,source_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_lesson_provenance_au AFTER UPDATE ON lesson_provenance BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'lessons',CAST(old.lesson_id AS TEXT),'evidence_changed',updated_at,CAST(strftime('%s','now') AS INTEGER)*1000 FROM lessons WHERE id=old.lesson_id AND true
  ON CONFLICT(source_table,source_id) DO NOTHING;
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'lessons',CAST(new.lesson_id AS TEXT),'evidence_changed',updated_at,CAST(strftime('%s','now') AS INTEGER)*1000 FROM lessons WHERE id=new.lesson_id AND true
  ON CONFLICT(source_table,source_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_lesson_provenance_ad AFTER DELETE ON lesson_provenance BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'lessons',CAST(old.lesson_id AS TEXT),'evidence_deleted',updated_at,CAST(strftime('%s','now') AS INTEGER)*1000 FROM lessons WHERE id=old.lesson_id AND true
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind=excluded.invalidation_kind, required_owner_updated_after_ms=excluded.required_owner_updated_after_ms, invalidated_at_ms=excluded.invalidated_at_ms;
END;

-- Evidence content changes conservatively invalidate the canonical owners that derive from it.
CREATE TRIGGER IF NOT EXISTS search_doc_event_log_bd BEFORE DELETE ON event_log BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT state_table,CAST(state_id AS TEXT),'evidence_deleted',CASE state_table WHEN 'people' THEN (SELECT updated_at FROM people WHERE id=state_id) WHEN 'projects' THEN (SELECT updated_at FROM projects WHERE id=state_id) WHEN 'commitments' THEN (SELECT updated_at FROM commitments WHERE id=state_id) ELSE (SELECT updated_at FROM open_loops WHERE id=state_id) END,old.ts FROM state_provenance WHERE event_id=old.id AND true
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind=excluded.invalidation_kind, required_owner_updated_after_ms=excluded.required_owner_updated_after_ms, invalidated_at_ms=excluded.invalidated_at_ms;
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'threads',CAST(id AS TEXT),'evidence_deleted',updated_at,old.ts FROM threads WHERE thread_key=old.thread_key AND true
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind=excluded.invalidation_kind, required_owner_updated_after_ms=excluded.required_owner_updated_after_ms, invalidated_at_ms=excluded.invalidated_at_ms;
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'sessions',CAST(id AS TEXT),'evidence_deleted',updated_at,old.ts FROM sessions WHERE id=old.session_id AND true
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind=excluded.invalidation_kind, required_owner_updated_after_ms=excluded.required_owner_updated_after_ms, invalidated_at_ms=excluded.invalidated_at_ms;
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'briefs',date,'evidence_deleted',built_at,old.ts FROM briefs
   WHERE CASE WHEN json_valid(briefs.payload) THEN
     EXISTS (SELECT 1 FROM json_tree(briefs.payload,'$.commitments_due') AS item WHERE item.key='provenance_event_id' AND item.type='integer' AND item.value=old.id)
     OR EXISTS (SELECT 1 FROM json_tree(briefs.payload,'$.open_loops') AS item WHERE item.key='provenance_event_id' AND item.type='integer' AND item.value=old.id)
   ELSE 0 END
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind=excluded.invalidation_kind, required_owner_updated_after_ms=excluded.required_owner_updated_after_ms, invalidated_at_ms=excluded.invalidated_at_ms;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_event_log_ai AFTER INSERT ON event_log BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'threads',CAST(id AS TEXT),'evidence_changed',updated_at,new.ts FROM threads WHERE thread_key=new.thread_key AND true
  ON CONFLICT(source_table,source_id) DO NOTHING;
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'sessions',CAST(id AS TEXT),'evidence_changed',updated_at,new.ts FROM sessions WHERE id=new.session_id AND true
  ON CONFLICT(source_table,source_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_event_log_au AFTER UPDATE ON event_log BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'threads',CAST(id AS TEXT),'evidence_changed',updated_at,new.ts FROM threads WHERE thread_key IN(old.thread_key,new.thread_key) AND true
  ON CONFLICT(source_table,source_id) DO NOTHING;
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'sessions',CAST(id AS TEXT),'evidence_changed',updated_at,new.ts FROM sessions WHERE id IN(old.session_id,new.session_id) AND true
  ON CONFLICT(source_table,source_id) DO NOTHING;
END;

CREATE TRIGGER IF NOT EXISTS search_doc_transcript_segments_ai AFTER INSERT ON transcript_segments BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'sessions',CAST(id AS TEXT),'evidence_changed',updated_at,new.created_at FROM sessions WHERE id=new.session_id AND true
  ON CONFLICT(source_table,source_id) DO NOTHING;
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'meeting_recaps',CAST(id AS TEXT),'evidence_changed',created_at,new.created_at FROM meeting_recaps WHERE session_id=new.session_id AND true
  ON CONFLICT(source_table,source_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_transcript_segments_au AFTER UPDATE ON transcript_segments BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'sessions',CAST(id AS TEXT),'evidence_changed',updated_at,new.created_at FROM sessions WHERE id IN(old.session_id,new.session_id) AND true
  ON CONFLICT(source_table,source_id) DO NOTHING;
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'meeting_recaps',CAST(id AS TEXT),'evidence_changed',created_at,new.created_at FROM meeting_recaps WHERE session_id IN(old.session_id,new.session_id) AND true
  ON CONFLICT(source_table,source_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_transcript_segments_bd BEFORE DELETE ON transcript_segments BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'sessions',CAST(id AS TEXT),'evidence_deleted',updated_at,old.created_at FROM sessions WHERE id=old.session_id AND true
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind=excluded.invalidation_kind, required_owner_updated_after_ms=excluded.required_owner_updated_after_ms, invalidated_at_ms=excluded.invalidated_at_ms;
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'meeting_recaps',CAST(id AS TEXT),'evidence_deleted',created_at,old.created_at FROM meeting_recaps WHERE session_id=old.session_id AND true
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind=excluded.invalidation_kind, required_owner_updated_after_ms=excluded.required_owner_updated_after_ms, invalidated_at_ms=excluded.invalidated_at_ms;
END;

CREATE TRIGGER IF NOT EXISTS search_doc_session_notes_related_ai AFTER INSERT ON session_notes BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'sessions',CAST(id AS TEXT),'evidence_changed',updated_at,new.updated_at FROM sessions WHERE id=new.session_id AND true
  ON CONFLICT(source_table,source_id) DO NOTHING;
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'meeting_recaps',CAST(id AS TEXT),'evidence_changed',created_at,new.updated_at FROM meeting_recaps WHERE session_id=new.session_id AND true
  ON CONFLICT(source_table,source_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_session_notes_related_au AFTER UPDATE ON session_notes BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'sessions',CAST(id AS TEXT),'evidence_changed',updated_at,new.updated_at FROM sessions WHERE id IN(old.session_id,new.session_id) AND true
  ON CONFLICT(source_table,source_id) DO NOTHING;
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'meeting_recaps',CAST(id AS TEXT),'evidence_changed',created_at,new.updated_at FROM meeting_recaps WHERE session_id IN(old.session_id,new.session_id) AND true
  ON CONFLICT(source_table,source_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_session_notes_related_bd BEFORE DELETE ON session_notes BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'sessions',CAST(id AS TEXT),'evidence_deleted',updated_at,old.updated_at FROM sessions WHERE id=old.session_id AND true
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind=excluded.invalidation_kind, required_owner_updated_after_ms=excluded.required_owner_updated_after_ms, invalidated_at_ms=excluded.invalidated_at_ms;
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'meeting_recaps',CAST(id AS TEXT),'evidence_deleted',created_at,old.updated_at FROM meeting_recaps WHERE session_id=old.session_id AND true
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind=excluded.invalidation_kind, required_owner_updated_after_ms=excluded.required_owner_updated_after_ms, invalidated_at_ms=excluded.invalidated_at_ms;
END;

CREATE TRIGGER IF NOT EXISTS search_doc_meeting_recaps_related_ai AFTER INSERT ON meeting_recaps BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'sessions',CAST(id AS TEXT),'evidence_changed',updated_at,new.created_at FROM sessions WHERE id=new.session_id AND true
  ON CONFLICT(source_table,source_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_meeting_recaps_related_bu BEFORE UPDATE ON meeting_recaps BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'sessions',CAST(id AS TEXT),'evidence_changed',updated_at,new.created_at FROM sessions WHERE id IN(old.session_id,new.session_id) AND true
  ON CONFLICT(source_table,source_id) DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS search_doc_meeting_recaps_related_bd BEFORE DELETE ON meeting_recaps BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'sessions',CAST(id AS TEXT),'evidence_deleted',updated_at,old.created_at FROM sessions WHERE id=old.session_id AND true
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind=excluded.invalidation_kind, required_owner_updated_after_ms=excluded.required_owner_updated_after_ms, invalidated_at_ms=excluded.invalidated_at_ms;
END;

CREATE TRIGGER IF NOT EXISTS search_doc_feedback_events_bd BEFORE DELETE ON feedback_events BEGIN
  INSERT INTO search_document_invalidations(source_table,source_id,invalidation_kind,required_owner_updated_after_ms,invalidated_at_ms)
  SELECT 'lessons',CAST(l.id AS TEXT),'evidence_deleted',l.updated_at,old.ts_ms FROM lessons l JOIN lesson_provenance p ON p.lesson_id=l.id WHERE p.feedback_event_id=old.id AND true
  ON CONFLICT(source_table,source_id) DO UPDATE SET invalidation_kind=excluded.invalidation_kind, required_owner_updated_after_ms=excluded.required_owner_updated_after_ms, invalidated_at_ms=excluded.invalidated_at_ms;
END;
