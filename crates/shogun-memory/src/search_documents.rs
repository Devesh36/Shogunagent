//! Rebuildable full-text projection over canonical, already-distilled memory.
//!
//! Canonical repositories do not call this module from their write transactions. V21's
//! content-free triggers only delete or dirty existing projection rows; reconciliation builds
//! sanitized text after canonical persistence has succeeded.

use std::fmt;

use rusqlite::{params, params_from_iter, Connection, OptionalExtension, ToSql};

const V21_SCHEMA_SQL: &str = include_str!("migrations/V21__search_documents.sql");
const MAX_PAGE: usize = 100;

/// Fixed, content-free failures from projection construction.
#[derive(Debug, thiserror::Error)]
pub enum SearchDocumentError {
    #[error("sqlite")]
    Sqlite(#[from] rusqlite::Error),
    #[error("invalid document: {0}")]
    InvalidDocument(&'static str),
}

/// Closed V21 document kinds. `PeriodSummary` is reserved but cannot be built until V22.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SearchDocumentKind {
    Person,
    Project,
    Commitment,
    OpenLoop,
    ThreadSummary,
    SessionSummary,
    MeetingRecap,
    MeetingNote,
    Brief,
    Lesson,
    PeriodSummary,
}

impl SearchDocumentKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Project => "project",
            Self::Commitment => "commitment",
            Self::OpenLoop => "open_loop",
            Self::ThreadSummary => "thread_summary",
            Self::SessionSummary => "session_summary",
            Self::MeetingRecap => "meeting_recap",
            Self::MeetingNote => "meeting_note",
            Self::Brief => "brief",
            Self::Lesson => "lesson",
            Self::PeriodSummary => "period_summary",
        }
    }

    fn parse(value: &str) -> Result<Self, SearchDocumentError> {
        match value {
            "person" => Ok(Self::Person),
            "project" => Ok(Self::Project),
            "commitment" => Ok(Self::Commitment),
            "open_loop" => Ok(Self::OpenLoop),
            "thread_summary" => Ok(Self::ThreadSummary),
            "session_summary" => Ok(Self::SessionSummary),
            "meeting_recap" => Ok(Self::MeetingRecap),
            "meeting_note" => Ok(Self::MeetingNote),
            "brief" => Ok(Self::Brief),
            "lesson" => Ok(Self::Lesson),
            "period_summary" => Ok(Self::PeriodSummary),
            _ => Err(SearchDocumentError::InvalidDocument("document_kind")),
        }
    }
}

impl fmt::Display for SearchDocumentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Typed canonical owner identity. V21 intentionally has no period-summary source variant.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourceRef {
    Person(i64),
    Project(i64),
    Commitment(i64),
    OpenLoop(i64),
    Thread(i64),
    Session(i64),
    MeetingRecap(i64),
    SessionNote(i64),
    Brief(String),
    Lesson(i64),
    PeriodSummary(i64),
}

impl SourceRef {
    pub const fn kind(&self) -> SearchDocumentKind {
        match self {
            Self::Person(_) => SearchDocumentKind::Person,
            Self::Project(_) => SearchDocumentKind::Project,
            Self::Commitment(_) => SearchDocumentKind::Commitment,
            Self::OpenLoop(_) => SearchDocumentKind::OpenLoop,
            Self::Thread(_) => SearchDocumentKind::ThreadSummary,
            Self::Session(_) => SearchDocumentKind::SessionSummary,
            Self::MeetingRecap(_) => SearchDocumentKind::MeetingRecap,
            Self::SessionNote(_) => SearchDocumentKind::MeetingNote,
            Self::Brief(_) => SearchDocumentKind::Brief,
            Self::Lesson(_) => SearchDocumentKind::Lesson,
            Self::PeriodSummary(_) => SearchDocumentKind::PeriodSummary,
        }
    }

    pub const fn table(&self) -> &'static str {
        match self {
            Self::Person(_) => "people",
            Self::Project(_) => "projects",
            Self::Commitment(_) => "commitments",
            Self::OpenLoop(_) => "open_loops",
            Self::Thread(_) => "threads",
            Self::Session(_) => "sessions",
            Self::MeetingRecap(_) => "meeting_recaps",
            Self::SessionNote(_) => "session_notes",
            Self::Brief(_) => "briefs",
            Self::Lesson(_) => "lessons",
            Self::PeriodSummary(_) => "period_summaries",
        }
    }

    pub fn id(&self) -> String {
        match self {
            Self::Person(id)
            | Self::Project(id)
            | Self::Commitment(id)
            | Self::OpenLoop(id)
            | Self::Thread(id)
            | Self::Session(id)
            | Self::MeetingRecap(id)
            | Self::SessionNote(id)
            | Self::Lesson(id) => id.to_string(),
            Self::PeriodSummary(id) => id.to_string(),
            Self::Brief(date) => date.clone(),
        }
    }

    fn parse(table: &str, id: &str) -> Result<Self, SearchDocumentError> {
        let numeric = || {
            id.parse::<i64>()
                .map_err(|_| SearchDocumentError::InvalidDocument("source_id"))
        };
        match table {
            "people" => Ok(Self::Person(numeric()?)),
            "projects" => Ok(Self::Project(numeric()?)),
            "commitments" => Ok(Self::Commitment(numeric()?)),
            "open_loops" => Ok(Self::OpenLoop(numeric()?)),
            "threads" => Ok(Self::Thread(numeric()?)),
            "sessions" => Ok(Self::Session(numeric()?)),
            "meeting_recaps" => Ok(Self::MeetingRecap(numeric()?)),
            "session_notes" => Ok(Self::SessionNote(numeric()?)),
            "briefs" => Ok(Self::Brief(id.to_owned())),
            "lessons" => Ok(Self::Lesson(numeric()?)),
            "period_summaries" => Ok(Self::PeriodSummary(numeric()?)),
            _ => Err(SearchDocumentError::InvalidDocument("source_table")),
        }
    }
}

/// How projected text was produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenerationMeta {
    Canonical,
    Extractive { generated_at_ms: Option<i64> },
    Model { model: String, generated_at_ms: i64 },
}

impl GenerationMeta {
    fn columns(&self) -> Result<(&'static str, Option<&str>, Option<i64>), SearchDocumentError> {
        match self {
            Self::Canonical => Ok(("canonical", None, None)),
            Self::Extractive { generated_at_ms } => Ok(("extractive", None, *generated_at_ms)),
            Self::Model {
                model,
                generated_at_ms,
            } if !model.trim().is_empty() => {
                Ok(("model", Some(model.as_str()), Some(*generated_at_ms)))
            }
            Self::Model { .. } => Err(SearchDocumentError::InvalidDocument("generation_model")),
        }
    }

    fn from_columns(
        kind: &str,
        model: Option<String>,
        generated_at_ms: Option<i64>,
    ) -> Result<Self, SearchDocumentError> {
        match (kind, model, generated_at_ms) {
            ("canonical", None, _) => Ok(Self::Canonical),
            ("extractive", None, generated_at_ms) => Ok(Self::Extractive { generated_at_ms }),
            ("model", Some(model), Some(generated_at_ms)) if !model.is_empty() => Ok(Self::Model {
                model,
                generated_at_ms,
            }),
            _ => Err(SearchDocumentError::InvalidDocument("generation_metadata")),
        }
    }
}

/// Freshness fields stored beside every projection row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Freshness {
    pub source_updated_at_ms: i64,
    pub is_dirty: bool,
    pub dirty_at_ms: Option<i64>,
    pub last_projected_at_ms: i64,
}

/// One normalized canonical evidence link.
#[derive(Debug, Clone, PartialEq)]
pub enum EvidenceRef {
    Event { id: i64, weight: f64 },
    Session { id: i64, weight: f64 },
    TranscriptSegment { id: i64, weight: f64 },
    SessionNote { id: i64, weight: f64 },
    MeetingRecap { id: i64, weight: f64 },
    Brief { date: String, weight: f64 },
    FeedbackEvent { id: i64, weight: f64 },
}

impl EvidenceRef {
    fn weight(&self) -> f64 {
        match self {
            Self::Event { weight, .. }
            | Self::Session { weight, .. }
            | Self::TranscriptSegment { weight, .. }
            | Self::SessionNote { weight, .. }
            | Self::MeetingRecap { weight, .. }
            | Self::Brief { weight, .. }
            | Self::FeedbackEvent { weight, .. } => *weight,
        }
    }

    fn validate(&self) -> Result<(), SearchDocumentError> {
        if self.weight() > 0.0 && self.weight() <= 1.0 {
            Ok(())
        } else {
            Err(SearchDocumentError::InvalidDocument("evidence_weight"))
        }
    }
}

/// Hydrated projection row with stable, normalized provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchDocument {
    pub id: i64,
    pub kind: SearchDocumentKind,
    pub source: SourceRef,
    pub title: String,
    pub body: String,
    pub period_start_ms: Option<i64>,
    pub period_end_ms: Option<i64>,
    pub confidence: f64,
    pub generation: GenerationMeta,
    pub content_digest: String,
    pub freshness: Freshness,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub evidence: Vec<EvidenceRef>,
}

/// Builder output. Fields stay private so source-specific builders own text and provenance.
#[derive(Debug, Clone)]
pub struct BuiltDocument {
    kind: SearchDocumentKind,
    source: SourceRef,
    title: String,
    body: String,
    period_start_ms: Option<i64>,
    period_end_ms: Option<i64>,
    confidence: f64,
    generation: GenerationMeta,
    source_updated_at_ms: i64,
    evidence: Vec<EvidenceRef>,
}

/// One FTS-ranked identifier. Hydration separately enforces freshness.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentCandidate {
    pub id: i64,
    pub kind: SearchDocumentKind,
    pub rank: f64,
}

/// Stable page of dirty canonical owners.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyPage {
    pub sources: Vec<SourceRef>,
    pub next_cursor: Option<i64>,
}

/// Result of rebuilding one canonical source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebuildOutcome {
    Upserted,
    Unchanged,
    Deleted,
    UnchangedAbsent,
    Deferred,
    Failed(&'static str),
}

/// Content-free totals from a full deterministic rebuild.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RebuildReport {
    pub visited: usize,
    pub upserted: usize,
    pub deleted: usize,
    pub deferred: usize,
    pub failed: usize,
}

/// Content-free health of the rebuildable derived-search projection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProjectionFreshnessMetrics {
    pub total_documents: i64,
    pub dirty_documents: i64,
    pub pending_invalidations: i64,
    pub oldest_dirty_at_ms: Option<i64>,
}

pub fn freshness_metrics(
    conn: &Connection,
) -> Result<ProjectionFreshnessMetrics, SearchDocumentError> {
    let (total_documents, dirty_documents, oldest_dirty_at_ms) = conn.query_row(
        "SELECT count(*), COALESCE(sum(is_dirty), 0), min(dirty_at_ms) FROM search_documents",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let pending_invalidations = conn.query_row(
        "SELECT count(*) FROM search_document_invalidations",
        [],
        |row| row.get(0),
    )?;
    Ok(ProjectionFreshnessMetrics {
        total_documents,
        dirty_documents,
        pending_invalidations,
        oldest_dirty_at_ms,
    })
}

#[derive(Debug)]
struct Invalidation {
    kind: String,
    required_owner_updated_after_ms: Option<i64>,
}

enum BuildResult {
    Document(BuiltDocument),
    Absent,
    Failed(&'static str),
}

struct BuiltContent {
    title: String,
    body: String,
    period: Option<(i64, i64)>,
    confidence: f64,
    generation: GenerationMeta,
    source_updated_at_ms: i64,
    evidence: Vec<EvidenceRef>,
}

/// Recreate any missing projection objects without touching refinery history.
pub fn ensure_projection_schema(conn: &Connection) -> Result<(), SearchDocumentError> {
    conn.execute_batch(V21_SCHEMA_SQL)?;
    Ok(())
}

/// Rebuild the FTS mirror from current `search_documents` rows.
pub fn rebuild_fts(conn: &Connection) -> Result<(), SearchDocumentError> {
    conn.execute(
        "INSERT INTO search_documents_fts(search_documents_fts) VALUES('rebuild')",
        [],
    )?;
    Ok(())
}

/// Atomically upsert one built document and replace all of its typed provenance.
pub fn upsert_document(
    conn: &mut Connection,
    document: BuiltDocument,
    now_ms: i64,
) -> Result<i64, SearchDocumentError> {
    let tx = conn.transaction()?;
    let id = upsert_document_in(&tx, &document, now_ms)?;
    tx.commit()?;
    Ok(id)
}

fn upsert_document_in(
    conn: &Connection,
    document: &BuiltDocument,
    now_ms: i64,
) -> Result<i64, SearchDocumentError> {
    validate_built(document)?;
    let source_id = document.source.id();
    let (generation_kind, generation_model, generated_at_ms) = document.generation.columns()?;
    let content_digest = crate::briefs::digest(&format!("{}\0{}", document.title, document.body));
    let id = conn.query_row(
        "INSERT INTO search_documents
           (document_kind, source_table, source_id, title, body, period_start_ms, period_end_ms,
            confidence, generation_kind, generation_model, generated_at_ms, content_digest,
            source_updated_at_ms, is_dirty, dirty_at_ms, last_projected_at_ms,
            last_failure_class, created_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                 0, NULL, ?14, NULL, ?14, ?14)
         ON CONFLICT(source_table, source_id) DO UPDATE SET
           document_kind = excluded.document_kind,
           title = excluded.title,
           body = excluded.body,
           period_start_ms = excluded.period_start_ms,
           period_end_ms = excluded.period_end_ms,
           confidence = excluded.confidence,
           generation_kind = excluded.generation_kind,
           generation_model = excluded.generation_model,
           generated_at_ms = excluded.generated_at_ms,
           content_digest = excluded.content_digest,
           source_updated_at_ms = excluded.source_updated_at_ms,
           is_dirty = 0,
           dirty_at_ms = NULL,
           last_projected_at_ms = excluded.last_projected_at_ms,
           last_failure_class = NULL,
           updated_at_ms = excluded.updated_at_ms
         RETURNING id",
        params![
            document.kind.as_str(),
            document.source.table(),
            source_id,
            document.title,
            document.body,
            document.period_start_ms,
            document.period_end_ms,
            document.confidence,
            generation_kind,
            generation_model,
            generated_at_ms,
            content_digest,
            document.source_updated_at_ms,
            now_ms,
        ],
        |row| row.get(0),
    )?;
    conn.execute(
        "DELETE FROM search_document_provenance WHERE document_id = ?1",
        [id],
    )?;
    for evidence in &document.evidence {
        insert_evidence(conn, id, evidence)?;
    }
    Ok(id)
}

fn validate_built(document: &BuiltDocument) -> Result<(), SearchDocumentError> {
    if document.kind != document.source.kind() {
        return Err(SearchDocumentError::InvalidDocument("kind_source_pair"));
    }
    if document.title.trim().is_empty() && document.body.trim().is_empty() {
        return Err(SearchDocumentError::InvalidDocument("empty_text"));
    }
    if !(0.0..=1.0).contains(&document.confidence) {
        return Err(SearchDocumentError::InvalidDocument("confidence"));
    }
    match (document.period_start_ms, document.period_end_ms) {
        (None, None) => {}
        (Some(start), Some(end)) if start < end => {}
        _ => return Err(SearchDocumentError::InvalidDocument("period_bounds")),
    }
    document.generation.columns()?;
    for evidence in &document.evidence {
        evidence.validate()?;
    }
    Ok(())
}

fn insert_evidence(
    conn: &Connection,
    document_id: i64,
    evidence: &EvidenceRef,
) -> Result<(), SearchDocumentError> {
    let (kind, event_id, session_id, transcript_id, note_id, recap_id, brief_date, feedback_id) =
        match evidence {
            EvidenceRef::Event { id, .. } => {
                ("event", Some(*id), None, None, None, None, None, None)
            }
            EvidenceRef::Session { id, .. } => {
                ("session", None, Some(*id), None, None, None, None, None)
            }
            EvidenceRef::TranscriptSegment { id, .. } => (
                "transcript_segment",
                None,
                None,
                Some(*id),
                None,
                None,
                None,
                None,
            ),
            EvidenceRef::SessionNote { id, .. } => (
                "session_note",
                None,
                None,
                None,
                Some(*id),
                None,
                None,
                None,
            ),
            EvidenceRef::MeetingRecap { id, .. } => (
                "meeting_recap",
                None,
                None,
                None,
                None,
                Some(*id),
                None,
                None,
            ),
            EvidenceRef::Brief { date, .. } => (
                "brief",
                None,
                None,
                None,
                None,
                None,
                Some(date.as_str()),
                None,
            ),
            EvidenceRef::FeedbackEvent { id, .. } => (
                "feedback_event",
                None,
                None,
                None,
                None,
                None,
                None,
                Some(*id),
            ),
        };
    conn.execute(
        "INSERT INTO search_document_provenance
           (document_id, evidence_kind, event_id, session_id, transcript_segment_id,
            session_note_id, meeting_recap_id, brief_date, feedback_event_id, weight)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            document_id,
            kind,
            event_id,
            session_id,
            transcript_id,
            note_id,
            recap_id,
            brief_date,
            feedback_id,
            evidence.weight(),
        ],
    )?;
    Ok(())
}

/// Delete a projection row by canonical owner. Canonical data is untouched.
pub fn delete_source(conn: &Connection, source: &SourceRef) -> Result<usize, SearchDocumentError> {
    Ok(conn.execute(
        "DELETE FROM search_documents WHERE source_table = ?1 AND source_id = ?2",
        params![source.table(), source.id()],
    )?)
}

/// Journal one owner for rebuild without building content or touching projection storage.
pub fn mark_dirty_for_source(
    conn: &Connection,
    source: &SourceRef,
    now_ms: i64,
) -> Result<usize, SearchDocumentError> {
    let baseline = conn
        .query_row(
            "SELECT source_updated_at_ms FROM search_documents
              WHERE source_table = ?1 AND source_id = ?2",
            params![source.table(), source.id()],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    Ok(conn.execute(
        "INSERT INTO search_document_invalidations
           (source_table, source_id, invalidation_kind,
            required_owner_updated_after_ms, invalidated_at_ms)
         VALUES (?1, ?2, 'source_changed', ?3, ?4)
         ON CONFLICT(source_table, source_id) DO UPDATE SET
           invalidation_kind = 'source_changed',
           required_owner_updated_after_ms = excluded.required_owner_updated_after_ms,
           invalidated_at_ms = excluded.invalidated_at_ms",
        params![source.table(), source.id(), baseline, now_ms],
    )?)
}

/// Mark every document citing one typed evidence row dirty.
pub fn mark_dirty_for_evidence(
    conn: &Connection,
    evidence: &EvidenceRef,
    now_ms: i64,
) -> Result<usize, SearchDocumentError> {
    mark_dirty_for_evidence_kind(conn, evidence, now_ms, "evidence_changed")
}

fn mark_dirty_for_evidence_kind(
    conn: &Connection,
    evidence: &EvidenceRef,
    now_ms: i64,
    invalidation_kind: &'static str,
) -> Result<usize, SearchDocumentError> {
    evidence.validate()?;
    let (column, value): (&str, Box<dyn ToSql>) = match evidence {
        EvidenceRef::Event { id, .. } => ("event_id", Box::new(*id)),
        EvidenceRef::Session { id, .. } => ("session_id", Box::new(*id)),
        EvidenceRef::TranscriptSegment { id, .. } => ("transcript_segment_id", Box::new(*id)),
        EvidenceRef::SessionNote { id, .. } => ("session_note_id", Box::new(*id)),
        EvidenceRef::MeetingRecap { id, .. } => ("meeting_recap_id", Box::new(*id)),
        EvidenceRef::Brief { date, .. } => ("brief_date", Box::new(date.clone())),
        EvidenceRef::FeedbackEvent { id, .. } => ("feedback_event_id", Box::new(*id)),
    };
    let sql = format!(
        "INSERT OR REPLACE INTO search_document_invalidations
           (source_table, source_id, invalidation_kind,
            required_owner_updated_after_ms, invalidated_at_ms)
         SELECT d.source_table, d.source_id, '{invalidation_kind}', d.source_updated_at_ms, ?2
           FROM search_document_provenance p
           JOIN search_documents d ON d.id = p.document_id
          WHERE p.{column} = ?1"
    );
    Ok(conn.execute(&sql, params![value, now_ms])?)
}

/// Delete one canonical event with V21 projection ordering kept in one place.
///
/// Projection cleanup is best-effort: its stable invalidation triggers remain the fallback, and a
/// missing rebuildable projection object must never prevent canonical deletion.
pub fn delete_canonical_event(conn: &mut Connection, event_id: i64) -> Result<(), rusqlite::Error> {
    let tx = conn.transaction()?;
    let _ = delete_canonical_event_in(&tx, event_id)?;
    tx.commit()
}

pub(crate) fn delete_canonical_event_in(
    conn: &Connection,
    event_id: i64,
) -> Result<bool, rusqlite::Error> {
    let event_ts = conn
        .query_row(
            "SELECT ts FROM event_log WHERE id = ?1",
            [event_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(event_ts) = event_ts else {
        return Ok(false);
    };

    // This path represents an explicit user deletion. Erase any unfinished Dream snapshot text
    // first so the pinning trigger permits canonical removal while the cycle retains only the id
    // needed to account for an already-submitted provider result.
    crate::dream_cycles::mark_event_deleted_explicit(conn, event_id)?;

    let evidence = EvidenceRef::Event {
        id: event_id,
        weight: 1.0,
    };
    let _ = mark_dirty_for_evidence_kind(conn, &evidence, event_ts, "evidence_deleted");
    let _ = conn.execute(
        "DELETE FROM search_document_provenance WHERE event_id = ?1",
        [event_id],
    );

    conn.execute(
        "DELETE FROM state_provenance WHERE event_id = ?1",
        [event_id],
    )?;
    conn.execute("DELETE FROM screen_frames WHERE event_id = ?1", [event_id])?;
    conn.execute("DELETE FROM event_vec WHERE rowid = ?1", [event_id])?;
    conn.execute(
        "DELETE FROM cold_embeddings WHERE event_id = ?1",
        [event_id],
    )?;
    let deleted = conn.execute("DELETE FROM event_log WHERE id = ?1", [event_id])?;
    Ok(deleted == 1)
}

/// List invalidated sources in stable journal order.
pub fn list_dirty(
    conn: &Connection,
    cursor: Option<i64>,
    limit: usize,
) -> Result<DirtyPage, SearchDocumentError> {
    let limit = limit.clamp(1, MAX_PAGE);
    let mut stmt = conn.prepare(
        "SELECT id, source_table, source_id FROM search_document_invalidations
          WHERE id > ?1 ORDER BY id LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![cursor.unwrap_or(0), limit as i64], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut sources = Vec::new();
    let mut next_cursor = None;
    for row in rows {
        let (id, table, source_id) = row?;
        next_cursor = Some(id);
        sources.push(SourceRef::parse(&table, &source_id)?);
    }
    Ok(DirtyPage {
        sources,
        next_cursor,
    })
}

/// Search clean projection rows through V21's trigram FTS table.
pub fn search_documents_fts(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<DocumentCandidate>, SearchDocumentError> {
    search_documents_fts_filtered(
        conn,
        query,
        None,
        None,
        crate::search::MemoryQueryFocus::Any,
        limit,
    )
}

pub fn search_documents_fts_filtered(
    conn: &Connection,
    query: &str,
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    focus: crate::search::MemoryQueryFocus,
    limit: usize,
) -> Result<Vec<DocumentCandidate>, SearchDocumentError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let Some(fts_query) = crate::search::fts_query(query) else {
        return Ok(Vec::new());
    };
    let mut stmt = conn.prepare(
        "SELECT d.id, d.document_kind, bm25(search_documents_fts)
           FROM search_documents_fts
           JOIN search_documents d ON d.id = search_documents_fts.rowid
          WHERE search_documents_fts MATCH ?1 AND d.is_dirty = 0
            AND (?3 IS NULL OR d.period_end_ms IS NULL OR d.period_end_ms>?3)
            AND (?4 IS NULL OR d.period_start_ms IS NULL OR d.period_start_ms<?4)
            AND (
              ?5=0
              OR (?5=1 AND d.document_kind IN (?6,?7,?8))
              OR (?5=2 AND d.document_kind IN (?9,?10))
              OR (?5=3 AND d.document_kind=?11)
              OR (?5=4 AND d.document_kind=?12)
              OR (?5=5 AND d.document_kind=?13)
              OR (?5=6 AND d.document_kind=?14)
              OR (?5=7 AND d.document_kind=?15))
            AND NOT EXISTS (
                SELECT 1 FROM search_document_invalidations i
                 WHERE i.source_table = d.source_table AND i.source_id = d.source_id)
          ORDER BY bm25(search_documents_fts), d.id
          LIMIT ?2",
    )?;
    let rows = stmt.query_map(
        params![
            fts_query,
            limit.min(MAX_PAGE) as i64,
            from_ms,
            to_ms,
            focus.code(),
            "session_summary",
            "meeting_recap",
            "meeting_note",
            "commitment",
            "open_loop",
            "brief",
            "period_summary",
            "person",
            "project",
            "thread_summary",
        ],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, f64>(2)?,
            ))
        },
    )?;
    rows.map(|row| {
        let (id, kind, rank) = row?;
        Ok(DocumentCandidate {
            id,
            kind: SearchDocumentKind::parse(&kind)?,
            rank,
        })
    })
    .collect()
}

/// Hydrate clean rows in deterministic ID order, excluding stale or missing canonical owners.
pub fn hydrate_documents(
    conn: &Connection,
    ids: &[i64],
) -> Result<Vec<SearchDocument>, SearchDocumentError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!(
        "SELECT id, document_kind, source_table, source_id, title, body,
                period_start_ms, period_end_ms, confidence, generation_kind,
                generation_model, generated_at_ms, content_digest, source_updated_at_ms,
                is_dirty, dirty_at_ms, last_projected_at_ms, created_at_ms, updated_at_ms
           FROM search_documents
          WHERE is_dirty = 0
            AND NOT EXISTS (
                SELECT 1 FROM search_document_invalidations i
                 WHERE i.source_table = search_documents.source_table
                   AND i.source_id = search_documents.source_id)
            AND id IN ({placeholders}) ORDER BY id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(ids.iter()), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Option<i64>>(6)?,
            row.get::<_, Option<i64>>(7)?,
            row.get::<_, f64>(8)?,
            row.get::<_, String>(9)?,
            row.get::<_, Option<String>>(10)?,
            row.get::<_, Option<i64>>(11)?,
            row.get::<_, String>(12)?,
            row.get::<_, i64>(13)?,
            row.get::<_, i64>(14)? != 0,
            row.get::<_, Option<i64>>(15)?,
            row.get::<_, i64>(16)?,
            row.get::<_, i64>(17)?,
            row.get::<_, i64>(18)?,
        ))
    })?;
    let mut documents = Vec::new();
    for row in rows {
        let (
            id,
            kind,
            source_table,
            source_id,
            title,
            body,
            period_start_ms,
            period_end_ms,
            confidence,
            generation_kind,
            generation_model,
            generated_at_ms,
            content_digest,
            source_updated_at_ms,
            is_dirty,
            dirty_at_ms,
            last_projected_at_ms,
            created_at_ms,
            updated_at_ms,
        ) = row?;
        let source = SourceRef::parse(&source_table, &source_id)?;
        let kind = SearchDocumentKind::parse(&kind)?;
        let canonical_matches = if kind == source.kind()
            && canonical_freshness(conn, &source)? == Some(source_updated_at_ms)
        {
            match build_source(conn, &source)? {
                BuildResult::Document(document) => {
                    logical_document_id(conn, &document)? == Some(id)
                }
                BuildResult::Absent | BuildResult::Failed(_) => false,
            }
        } else {
            false
        };
        if !canonical_matches {
            if matches!(
                source,
                SourceRef::Person(_)
                    | SourceRef::Project(_)
                    | SourceRef::Commitment(_)
                    | SourceRef::OpenLoop(_)
            ) {
                // Rebuild the projection without changing the canonical memory on a read.
                // A read-only/busy store must still omit stale results if journaling fails.
                let now_ms = conn.query_row(
                    "SELECT CAST(strftime('%s','now') AS INTEGER) * 1000",
                    [],
                    |row| row.get(0),
                )?;
                let _ = mark_dirty_for_source(conn, &source, now_ms);
            }
            let _ = conn.execute(
                "UPDATE search_documents
                    SET is_dirty = 1,
                        dirty_at_ms = CAST(strftime('%s','now') AS INTEGER) * 1000
                  WHERE id = ?1",
                [id],
            );
            continue;
        }
        documents.push(SearchDocument {
            id,
            kind,
            source,
            title,
            body,
            period_start_ms,
            period_end_ms,
            confidence,
            generation: GenerationMeta::from_columns(
                &generation_kind,
                generation_model,
                generated_at_ms,
            )?,
            content_digest,
            freshness: Freshness {
                source_updated_at_ms,
                is_dirty,
                dirty_at_ms,
                last_projected_at_ms,
            },
            created_at_ms,
            updated_at_ms,
            evidence: load_evidence(conn, id)?,
        });
    }
    Ok(documents)
}

fn canonical_freshness(
    conn: &Connection,
    source: &SourceRef,
) -> Result<Option<i64>, SearchDocumentError> {
    let found = match source {
        SourceRef::Person(id) => freshness_i64(conn, "people", "updated_at", "id", *id, None)?,
        SourceRef::Project(id) => freshness_i64(conn, "projects", "updated_at", "id", *id, None)?,
        SourceRef::Commitment(id) => {
            freshness_i64(conn, "commitments", "updated_at", "id", *id, None)?
        }
        SourceRef::OpenLoop(id) => {
            freshness_i64(conn, "open_loops", "updated_at", "id", *id, None)?
        }
        SourceRef::Thread(id) => freshness_i64(
            conn,
            "threads",
            "updated_at",
            "id",
            *id,
            Some("summary IS NOT NULL AND trim(summary) <> ''"),
        )?,
        SourceRef::Session(id) => freshness_i64(
            conn,
            "sessions",
            "updated_at",
            "id",
            *id,
            Some("summary IS NOT NULL AND trim(summary) <> ''"),
        )?,
        SourceRef::MeetingRecap(id) => {
            freshness_i64(conn, "meeting_recaps", "created_at", "id", *id, None)?
        }
        SourceRef::SessionNote(id) => {
            freshness_i64(conn, "session_notes", "updated_at", "id", *id, None)?
        }
        SourceRef::Lesson(id) => {
            freshness_i64(conn, "lessons", "updated_at", "id", *id, Some("active = 1"))?
        }
        SourceRef::PeriodSummary(id) => freshness_i64(
            conn,
            "period_summaries",
            "updated_at_ms",
            "id",
            *id,
            Some("is_dirty = 0"),
        )?,
        SourceRef::Brief(date) => conn
            .query_row(
                "SELECT built_at FROM briefs WHERE date = ?1",
                [date],
                |row| row.get(0),
            )
            .optional()?,
    };
    Ok(found)
}

fn freshness_i64(
    conn: &Connection,
    table: &str,
    freshness: &str,
    key: &str,
    id: i64,
    predicate: Option<&str>,
) -> Result<Option<i64>, rusqlite::Error> {
    let suffix = predicate.map_or(String::new(), |p| format!(" AND {p}"));
    conn.query_row(
        &format!("SELECT {freshness} FROM {table} WHERE {key} = ?1{suffix}"),
        [id],
        |row| row.get(0),
    )
    .optional()
}

fn load_evidence(
    conn: &Connection,
    document_id: i64,
) -> Result<Vec<EvidenceRef>, SearchDocumentError> {
    // The only indexes on `document_id` are the per-kind partial unique indexes, which SQLite
    // uses only when the query repeats their `<column> IS NOT NULL` predicate. A bare
    // `document_id = ?` scanned the whole provenance table once per search hit (~9 ms each on a
    // 117k-row store, the bulk of `memory.search` latency). One branch per partial index keeps
    // every lookup indexed without a migration.
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT * FROM ({}) ORDER BY evidence_kind, COALESCE(event_id, session_id,
                transcript_segment_id, session_note_id, meeting_recap_id, feedback_event_id),
                brief_date",
        [
            "event_id",
            "session_id",
            "transcript_segment_id",
            "session_note_id",
            "meeting_recap_id",
            "brief_date",
            "feedback_event_id",
        ]
        .map(|column| format!(
            "SELECT evidence_kind, event_id, session_id, transcript_segment_id, session_note_id,
                    meeting_recap_id, brief_date, feedback_event_id, weight
               FROM search_document_provenance
              WHERE document_id = ?1 AND {column} IS NOT NULL"
        ))
        .join(" UNION ALL ")
    ))?;
    let rows = stmt.query_map([document_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, Option<i64>>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<i64>>(7)?,
            row.get::<_, f64>(8)?,
        ))
    })?;
    rows.map(|row| {
        let (kind, event, session, transcript, note, recap, brief, feedback, weight) = row?;
        match kind.as_str() {
            "event" => Ok(EvidenceRef::Event {
                id: event.ok_or(SearchDocumentError::InvalidDocument("event_evidence"))?,
                weight,
            }),
            "session" => Ok(EvidenceRef::Session {
                id: session.ok_or(SearchDocumentError::InvalidDocument("session_evidence"))?,
                weight,
            }),
            "transcript_segment" => Ok(EvidenceRef::TranscriptSegment {
                id: transcript
                    .ok_or(SearchDocumentError::InvalidDocument("transcript_evidence"))?,
                weight,
            }),
            "session_note" => Ok(EvidenceRef::SessionNote {
                id: note.ok_or(SearchDocumentError::InvalidDocument("note_evidence"))?,
                weight,
            }),
            "meeting_recap" => Ok(EvidenceRef::MeetingRecap {
                id: recap.ok_or(SearchDocumentError::InvalidDocument("recap_evidence"))?,
                weight,
            }),
            "brief" => Ok(EvidenceRef::Brief {
                date: brief.ok_or(SearchDocumentError::InvalidDocument("brief_evidence"))?,
                weight,
            }),
            "feedback_event" => Ok(EvidenceRef::FeedbackEvent {
                id: feedback.ok_or(SearchDocumentError::InvalidDocument("feedback_evidence"))?,
                weight,
            }),
            _ => Err(SearchDocumentError::InvalidDocument("evidence_kind")),
        }
    })
    .collect()
}

/// Build and persist one canonical source, or remove/retain its projection as appropriate.
pub fn rebuild_source(
    conn: &mut Connection,
    source: &SourceRef,
    now_ms: i64,
) -> Result<RebuildOutcome, SearchDocumentError> {
    let tx = conn.transaction()?;
    let outcome = rebuild_source_in(&tx, source, now_ms)?;
    tx.commit()?;
    Ok(outcome)
}

fn rebuild_source_in(
    conn: &Connection,
    source: &SourceRef,
    now_ms: i64,
) -> Result<RebuildOutcome, SearchDocumentError> {
    let invalidation = load_invalidation(conn, source)?;
    match build_source(conn, source)? {
        BuildResult::Document(document) => {
            if invalidation.as_ref().is_some_and(|invalidation| {
                invalidation.required_owner_updated_after_ms.is_some_and(|required| {
                    match invalidation.kind.as_str() {
                        // Evidence changes require canonical re-derivation, not merely another
                        // projection attempt at the same owner freshness.
                        "evidence_changed" | "evidence_deleted" => {
                            document.source_updated_at_ms <= required
                        }
                        "source_changed" => document.source_updated_at_ms < required,
                        "source_delete" => false,
                        _ => true,
                    }
                })
            }) {
                return Ok(RebuildOutcome::Deferred);
            }
            if logical_document_id(conn, &document)?.is_some() {
                clear_invalidation(conn, source)?;
                return Ok(RebuildOutcome::Unchanged);
            }
            upsert_document_in(conn, &document, now_ms)?;
            clear_invalidation(conn, source)?;
            Ok(RebuildOutcome::Upserted)
        }
        BuildResult::Absent => {
            if absent_brief_requires_regeneration(conn, source, invalidation.as_ref())? {
                return Ok(RebuildOutcome::Deferred);
            }
            let removed = delete_source(conn, source)?;
            clear_invalidation(conn, source)?;
            Ok(if removed == 0 {
                RebuildOutcome::UnchangedAbsent
            } else {
                RebuildOutcome::Deleted
            })
        }
        BuildResult::Failed(class) => {
            conn.execute(
                "UPDATE search_documents
                    SET is_dirty = 1, dirty_at_ms = ?3, last_failure_class = ?4
                  WHERE source_table = ?1 AND source_id = ?2",
                params![source.table(), source.id(), now_ms, class],
            )?;
            Ok(RebuildOutcome::Failed(class))
        }
    }
}

fn absent_brief_requires_regeneration(
    conn: &Connection,
    source: &SourceRef,
    invalidation: Option<&Invalidation>,
) -> Result<bool, SearchDocumentError> {
    let Some(invalidation) = invalidation else {
        return Ok(false);
    };
    if !matches!(
        invalidation.kind.as_str(),
        "evidence_changed" | "evidence_deleted"
    ) {
        return Ok(false);
    }
    let Some(required) = invalidation.required_owner_updated_after_ms else {
        return Ok(false);
    };
    let SourceRef::Brief(date) = source else {
        return Ok(false);
    };
    let regenerated: Option<bool> = conn
        .query_row(
            "SELECT built_at > ?2 FROM briefs WHERE date = ?1",
            params![date, required],
            |row| row.get(0),
        )
        .optional()?;
    Ok(regenerated != Some(true))
}

/// Best-effort low-frequency adapter for canonical write boundaries.
///
/// Canonical writes call this only after their durable mutation succeeded. Any projection error
/// leaves V21's invalidation journal pending for later reconciliation.
pub fn project_source_after_write(
    conn: &Connection,
    source: &SourceRef,
    now_ms: i64,
) -> RebuildOutcome {
    rebuild_source_in(conn, source, now_ms).unwrap_or(RebuildOutcome::Failed("projection_write"))
}

/// Reconcile one bounded page of pending owners. Deferred owners rotate to the durable journal
/// tail, so later ready work cannot starve. Re-running after interruption is safe: completed
/// owners have already left the journal and unchanged logical rows are not rewritten.
pub fn reconcile_invalidated(
    conn: &Connection,
    limit: usize,
    now_ms: i64,
) -> Result<RebuildReport, SearchDocumentError> {
    ensure_projection_schema(conn)?;
    let page = list_dirty(conn, None, limit)?;
    let mut report = RebuildReport::default();
    for source in page.sources {
        report.visited += 1;
        match rebuild_source_in(conn, &source, now_ms)? {
            RebuildOutcome::Upserted => report.upserted += 1,
            RebuildOutcome::Deleted => report.deleted += 1,
            RebuildOutcome::Deferred => {
                report.deferred += 1;
                conn.execute(
                    "UPDATE search_document_invalidations
                        SET id = (SELECT COALESCE(MAX(id), 0) + 1
                                    FROM search_document_invalidations)
                      WHERE source_table = ?1 AND source_id = ?2",
                    params![source.table(), source.id()],
                )?;
            }
            RebuildOutcome::Failed(_) => report.failed += 1,
            RebuildOutcome::Unchanged | RebuildOutcome::UnchangedAbsent => {}
        }
    }
    Ok(report)
}

/// Project one durable keyset page of canonical rows that predate V21.
///
/// Open performs no corpus scan. Local maintenance calls this bounded seam; each cursor advances
/// only after projection succeeds or the stable invalidation journal owns the retry.
pub fn backfill_batch(
    conn: &Connection,
    limit: usize,
    now_ms: i64,
) -> Result<RebuildReport, SearchDocumentError> {
    ensure_projection_schema(conn)?;
    let mut remaining = limit.clamp(1, MAX_PAGE);
    let mut report = RebuildReport::default();
    while remaining > 0 {
        let Some((source_table, after_rowid)) = next_backfill_cursor(conn)? else {
            break;
        };
        let sources = backfill_sources(conn, &source_table, after_rowid, remaining)?;
        if sources.is_empty() {
            finish_backfill_table_if_exhausted(conn, &source_table, now_ms)?;
            continue;
        }
        let page_exhausted = sources.len() < remaining;
        for (rowid, source) in sources {
            conn.execute(
                "INSERT OR IGNORE INTO search_document_invalidations
                   (source_table, source_id, invalidation_kind,
                    required_owner_updated_after_ms, invalidated_at_ms)
                 VALUES (?1, ?2, 'source_changed', NULL, ?3)",
                params![source.table(), source.id(), now_ms],
            )?;
            report.visited += 1;
            match rebuild_source_in(conn, &source, now_ms) {
                Ok(RebuildOutcome::Upserted) => report.upserted += 1,
                Ok(RebuildOutcome::Deleted) => report.deleted += 1,
                Ok(RebuildOutcome::Deferred) => report.deferred += 1,
                Ok(RebuildOutcome::Failed(_)) | Err(_) => report.failed += 1,
                Ok(RebuildOutcome::Unchanged | RebuildOutcome::UnchangedAbsent) => {}
            }
            conn.execute(
                "UPDATE search_document_backfill_state
                    SET last_source_rowid = ?2, updated_at_ms = ?3
                  WHERE source_table = ?1 AND last_source_rowid < ?2",
                params![source_table, rowid, now_ms],
            )?;
            remaining -= 1;
        }
        if page_exhausted {
            finish_backfill_table_if_exhausted(conn, &source_table, now_ms)?;
        }
    }
    Ok(report)
}

fn next_backfill_cursor(
    conn: &Connection,
) -> Result<Option<(String, i64)>, SearchDocumentError> {
    Ok(conn
        .query_row(
            "SELECT source_table, last_source_rowid
               FROM search_document_backfill_state
              WHERE is_complete = 0
              ORDER BY CASE source_table
                  WHEN 'people' THEN 0 WHEN 'projects' THEN 1
                  WHEN 'commitments' THEN 2 WHEN 'open_loops' THEN 3
                  WHEN 'threads' THEN 4 WHEN 'sessions' THEN 5
                  WHEN 'meeting_recaps' THEN 6 WHEN 'session_notes' THEN 7
                  WHEN 'briefs' THEN 8 WHEN 'lessons' THEN 9
                  ELSE 10 END
              LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

fn backfill_sources(
    conn: &Connection,
    source_table: &str,
    after_rowid: i64,
    limit: usize,
) -> Result<Vec<(i64, SourceRef)>, SearchDocumentError> {
    if source_table == "briefs" {
        let mut stmt = conn.prepare(
            "SELECT rowid, date FROM briefs WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![after_rowid, limit as i64], |row| {
            Ok((row.get(0)?, SourceRef::Brief(row.get(1)?)))
        })?;
        return Ok(rows.collect::<Result<Vec<_>, _>>()?);
    }
    let (sql, wrap): (&str, fn(i64) -> SourceRef) = match source_table {
        "people" => (
            "SELECT rowid, id FROM people WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
            SourceRef::Person,
        ),
        "projects" => (
            "SELECT rowid, id FROM projects WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
            SourceRef::Project,
        ),
        "commitments" => (
            "SELECT rowid, id FROM commitments WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
            SourceRef::Commitment,
        ),
        "open_loops" => (
            "SELECT rowid, id FROM open_loops WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
            SourceRef::OpenLoop,
        ),
        "threads" => (
            "SELECT rowid, id FROM threads WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
            SourceRef::Thread,
        ),
        "sessions" => (
            "SELECT rowid, id FROM sessions WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
            SourceRef::Session,
        ),
        "meeting_recaps" => (
            "SELECT rowid, id FROM meeting_recaps WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
            SourceRef::MeetingRecap,
        ),
        "session_notes" => (
            "SELECT rowid, id FROM session_notes WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
            SourceRef::SessionNote,
        ),
        "lessons" => (
            "SELECT rowid, id FROM lessons WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
            SourceRef::Lesson,
        ),
        _ => return Err(SearchDocumentError::InvalidDocument("backfill_source_table")),
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![after_rowid, limit as i64], |row| {
        Ok((row.get(0)?, wrap(row.get(1)?)))
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn finish_backfill_table_if_exhausted(
    conn: &Connection,
    source_table: &str,
    now_ms: i64,
) -> Result<(), SearchDocumentError> {
    let canonical_table = match source_table {
        "people" => "people",
        "projects" => "projects",
        "commitments" => "commitments",
        "open_loops" => "open_loops",
        "threads" => "threads",
        "sessions" => "sessions",
        "meeting_recaps" => "meeting_recaps",
        "session_notes" => "session_notes",
        "briefs" => "briefs",
        "lessons" => "lessons",
        _ => return Err(SearchDocumentError::InvalidDocument("backfill_source_table")),
    };
    conn.execute(
        &format!(
            "UPDATE search_document_backfill_state
                SET is_complete = 1, updated_at_ms = ?2
              WHERE source_table = ?1
                AND NOT EXISTS (
                    SELECT 1 FROM {canonical_table}
                     WHERE rowid > search_document_backfill_state.last_source_rowid)"
        ),
        params![source_table, now_ms],
    )?;
    Ok(())
}

fn logical_document_id(
    conn: &Connection,
    document: &BuiltDocument,
) -> Result<Option<i64>, SearchDocumentError> {
    let source_id = document.source.id();
    let (generation_kind, generation_model, generated_at_ms) = document.generation.columns()?;
    let content_digest = crate::briefs::digest(&format!("{}\0{}", document.title, document.body));
    let row = conn
        .query_row(
            "SELECT id FROM search_documents
              WHERE source_table = ?1 AND source_id = ?2 AND document_kind = ?3
                AND title = ?4 AND body = ?5
                AND period_start_ms IS ?6 AND period_end_ms IS ?7
                AND confidence = ?8 AND generation_kind = ?9
                AND generation_model IS ?10 AND generated_at_ms IS ?11
                AND content_digest = ?12 AND source_updated_at_ms = ?13
                AND is_dirty = 0 AND last_failure_class IS NULL",
            params![
                document.source.table(),
                source_id,
                document.kind.as_str(),
                document.title,
                document.body,
                document.period_start_ms,
                document.period_end_ms,
                document.confidence,
                generation_kind,
                generation_model,
                generated_at_ms,
                content_digest,
                document.source_updated_at_ms,
            ],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    let Some(id) = row else {
        return Ok(None);
    };
    Ok(normalized_evidence_equal(load_evidence(conn, id)?, &document.evidence).then_some(id))
}

fn normalized_evidence_equal(mut stored: Vec<EvidenceRef>, expected: &[EvidenceRef]) -> bool {
    let mut expected = expected.to_vec();
    stored.sort_by_key(evidence_sort_key);
    expected.sort_by_key(evidence_sort_key);
    stored == expected
}

fn evidence_sort_key(evidence: &EvidenceRef) -> (&'static str, String) {
    match evidence {
        EvidenceRef::Event { id, .. } => ("event", id.to_string()),
        EvidenceRef::Session { id, .. } => ("session", id.to_string()),
        EvidenceRef::TranscriptSegment { id, .. } => ("transcript_segment", id.to_string()),
        EvidenceRef::SessionNote { id, .. } => ("session_note", id.to_string()),
        EvidenceRef::MeetingRecap { id, .. } => ("meeting_recap", id.to_string()),
        EvidenceRef::Brief { date, .. } => ("brief", date.clone()),
        EvidenceRef::FeedbackEvent { id, .. } => ("feedback_event", id.to_string()),
    }
}

fn load_invalidation(
    conn: &Connection,
    source: &SourceRef,
) -> Result<Option<Invalidation>, SearchDocumentError> {
    Ok(conn
        .query_row(
            "SELECT invalidation_kind, required_owner_updated_after_ms
               FROM search_document_invalidations
              WHERE source_table = ?1 AND source_id = ?2",
            params![source.table(), source.id()],
            |row| {
                Ok(Invalidation {
                    kind: row.get(0)?,
                    required_owner_updated_after_ms: row.get(1)?,
                })
            },
        )
        .optional()?)
}

fn clear_invalidation(conn: &Connection, source: &SourceRef) -> Result<(), SearchDocumentError> {
    conn.execute(
        "DELETE FROM search_document_invalidations
          WHERE source_table = ?1 AND source_id = ?2",
        params![source.table(), source.id()],
    )?;
    Ok(())
}

/// Deterministically reconcile every V21 source. Canonical tables are never modified.
pub fn rebuild_all(
    conn: &mut Connection,
    now_ms: i64,
) -> Result<RebuildReport, SearchDocumentError> {
    ensure_projection_schema(conn)?;
    let mut sources = all_sources(conn)?;
    let mut invalidated = list_all_invalidated_sources(conn)?;
    sources.append(&mut invalidated);
    sources.sort_by(|left, right| (left.table(), left.id()).cmp(&(right.table(), right.id())));
    sources.dedup();
    let mut report = RebuildReport::default();
    for source in sources {
        report.visited += 1;
        match rebuild_source(conn, &source, now_ms)? {
            RebuildOutcome::Upserted => report.upserted += 1,
            RebuildOutcome::Unchanged => {}
            RebuildOutcome::Deleted => report.deleted += 1,
            RebuildOutcome::Deferred => report.deferred += 1,
            RebuildOutcome::Failed(_) => report.failed += 1,
            RebuildOutcome::UnchangedAbsent => {}
        }
    }
    rebuild_fts(conn)?;
    Ok(report)
}

fn list_all_invalidated_sources(conn: &Connection) -> Result<Vec<SourceRef>, SearchDocumentError> {
    let mut stmt = conn
        .prepare("SELECT source_table, source_id FROM search_document_invalidations ORDER BY id")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.map(|row| {
        let (table, id) = row?;
        SourceRef::parse(&table, &id)
    })
    .collect()
}

/// Drop only rebuildable projection storage, recreate it from version-aware DDL, and backfill.
/// Refinery history and every canonical row remain untouched.
pub fn recreate_projection_and_rebuild(
    conn: &mut Connection,
    now_ms: i64,
) -> Result<RebuildReport, SearchDocumentError> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS search_document_vec;
         DROP TABLE IF EXISTS search_document_embeddings;
         DROP TABLE IF EXISTS search_document_provenance;
         DROP TABLE IF EXISTS search_documents_fts;
         DROP TABLE IF EXISTS search_documents;",
    )?;
    ensure_projection_schema(conn)?;
    crate::document_vector::ensure_schema(conn)?;
    let report = rebuild_all(conn, now_ms)?;
    crate::period_summaries::reproject_all(conn, now_ms)?;
    Ok(report)
}

fn all_sources(conn: &Connection) -> Result<Vec<SourceRef>, SearchDocumentError> {
    let mut out = Vec::new();
    append_numeric_sources(
        conn,
        "SELECT id FROM people ORDER BY id",
        SourceRef::Person,
        &mut out,
    )?;
    append_numeric_sources(
        conn,
        "SELECT id FROM projects ORDER BY id",
        SourceRef::Project,
        &mut out,
    )?;
    append_numeric_sources(
        conn,
        "SELECT id FROM commitments ORDER BY id",
        SourceRef::Commitment,
        &mut out,
    )?;
    append_numeric_sources(
        conn,
        "SELECT id FROM open_loops ORDER BY id",
        SourceRef::OpenLoop,
        &mut out,
    )?;
    append_numeric_sources(
        conn,
        "SELECT id FROM threads WHERE summary IS NOT NULL AND trim(summary) <> '' ORDER BY id",
        SourceRef::Thread,
        &mut out,
    )?;
    append_numeric_sources(
        conn,
        "SELECT id FROM sessions WHERE summary IS NOT NULL AND trim(summary) <> '' ORDER BY id",
        SourceRef::Session,
        &mut out,
    )?;
    append_numeric_sources(
        conn,
        "SELECT id FROM meeting_recaps ORDER BY id",
        SourceRef::MeetingRecap,
        &mut out,
    )?;
    append_numeric_sources(
        conn,
        "SELECT id FROM session_notes WHERE trim(body) <> '' ORDER BY id",
        SourceRef::SessionNote,
        &mut out,
    )?;
    {
        let mut stmt = conn.prepare("SELECT date FROM briefs ORDER BY date")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            out.push(SourceRef::Brief(row?));
        }
    }
    append_numeric_sources(
        conn,
        "SELECT id FROM lessons WHERE active = 1 ORDER BY id",
        SourceRef::Lesson,
        &mut out,
    )?;
    Ok(out)
}

fn append_numeric_sources(
    conn: &Connection,
    sql: &str,
    wrap: fn(i64) -> SourceRef,
    out: &mut Vec<SourceRef>,
) -> Result<(), rusqlite::Error> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
    for row in rows {
        out.push(wrap(row?));
    }
    Ok(())
}

fn build_source(conn: &Connection, source: &SourceRef) -> Result<BuildResult, SearchDocumentError> {
    match source {
        SourceRef::Person(id) => build_person(conn, *id),
        SourceRef::Project(id) => build_project(conn, *id),
        SourceRef::Commitment(id) => build_commitment(conn, *id),
        SourceRef::OpenLoop(id) => build_open_loop(conn, *id),
        SourceRef::Thread(id) => build_thread(conn, *id),
        SourceRef::Session(id) => build_session(conn, *id),
        SourceRef::MeetingRecap(id) => build_recap(conn, *id),
        SourceRef::SessionNote(id) => build_note(conn, *id),
        SourceRef::Brief(date) => build_brief(conn, date),
        SourceRef::Lesson(id) => build_lesson(conn, *id),
        SourceRef::PeriodSummary(id) => build_period_summary(conn, *id),
    }
}

fn build_period_summary(
    conn: &Connection,
    id: i64,
) -> Result<BuildResult, SearchDocumentError> {
    let row = conn
        .query_row(
            "SELECT title, content, period_start_ms, period_end_ms, generation_mode,
                    generation_model, generated_at_ms, updated_at_ms, is_dirty
               FROM period_summaries WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)? != 0,
                ))
            },
        )
        .optional()?;
    let Some((title, body, start, end, mode, model, generated_at, updated_at, is_dirty)) = row
    else {
        return Ok(BuildResult::Absent);
    };
    if is_dirty {
        return Ok(BuildResult::Absent);
    }
    let generation = match (mode.as_str(), model) {
        ("extractive", None) => GenerationMeta::Extractive {
            generated_at_ms: Some(generated_at),
        },
        ("model", Some(model)) => GenerationMeta::Model {
            model,
            generated_at_ms: generated_at,
        },
        _ => return Ok(BuildResult::Failed("generation_metadata")),
    };
    let mut stmt = conn.prepare(
        "SELECT event_id, weight FROM period_summary_event_provenance
          WHERE summary_id = ?1 ORDER BY event_id",
    )?;
    let evidence = stmt
        .query_map([id], |row| {
            Ok(EvidenceRef::Event {
                id: row.get(0)?,
                weight: row.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(built(
        SourceRef::PeriodSummary(id),
        BuiltContent {
            title,
            body,
            period: Some((start, end)),
            confidence: 1.0,
            generation,
            source_updated_at_ms: updated_at,
            evidence,
        },
    ))
}

fn built(source: SourceRef, content: BuiltContent) -> BuildResult {
    if content.title.trim().is_empty() && content.body.trim().is_empty() {
        return BuildResult::Absent;
    }
    let (period_start_ms, period_end_ms) = content.period.map_or((None, None), |(start, end)| {
        (Some(start), Some(end.max(start.saturating_add(1))))
    });
    BuildResult::Document(BuiltDocument {
        kind: source.kind(),
        source,
        title: content.title,
        body: content.body,
        period_start_ms,
        period_end_ms,
        confidence: content.confidence,
        generation: content.generation,
        source_updated_at_ms: content.source_updated_at_ms,
        evidence: content.evidence,
    })
}

fn canonical(raw: &str) -> String {
    crate::sanitize::persist_body(raw).text.into_owned()
}

fn generated(raw: &str) -> Option<String> {
    crate::sanitize::persist_generated(raw).map(|body| body.text.into_owned())
}

fn join_nonempty(parts: impl IntoIterator<Item = String>) -> String {
    parts
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn state_evidence(
    conn: &Connection,
    table: &str,
    id: i64,
) -> Result<Option<Vec<EvidenceRef>>, rusqlite::Error> {
    let table = crate::state::StateTable::parse(table).ok_or(rusqlite::Error::InvalidQuery)?;
    Ok(
        crate::state::supported_provenance(conn, table, id, usize::MAX)?.map(|links| {
            links
                .into_iter()
                .map(|(id, weight)| EvidenceRef::Event { id, weight })
                .collect()
        }),
    )
}

fn build_person(conn: &Connection, id: i64) -> Result<BuildResult, SearchDocumentError> {
    let row = conn
        .query_row(
            &format!(
                "SELECT display_name, relationship_summary, pending_from_me, pending_from_them,
                        {}, updated_at
                   FROM people WHERE id = ?1",
                crate::entity_pages::listed_confidence_sql(crate::state::StateTable::People, "people"),
            ),
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, f64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            },
        )
        .optional()?;
    let Some((name, summary, mine, theirs, confidence, updated_at)) = row else {
        return Ok(BuildResult::Absent);
    };
    let Some(evidence) = state_evidence(conn, "people", id)? else {
        return Ok(BuildResult::Absent);
    };
    Ok(built(
        SourceRef::Person(id),
        BuiltContent {
            title: canonical(&name),
            body: join_nonempty(
                [summary, mine, theirs]
                    .into_iter()
                    .flatten()
                    .map(|v| canonical(&v)),
            ),
            period: None,
            confidence,
            generation: GenerationMeta::Canonical,
            source_updated_at_ms: updated_at,
            evidence,
        },
    ))
}

fn build_project(conn: &Connection, id: i64) -> Result<BuildResult, SearchDocumentError> {
    let row = conn
        .query_row(
            // Search still finds a hidden singleton (it is evidence); a listed project is a fact.
            &format!(
                "SELECT name, status, summary, {}, updated_at FROM projects WHERE id = ?1",
                crate::entity_pages::listed_confidence_sql(crate::state::StateTable::Projects, "projects"),
            ),
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, f64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((name, status, summary, confidence, updated_at)) = row else {
        return Ok(BuildResult::Absent);
    };
    let Some(evidence) = state_evidence(conn, "projects", id)? else {
        return Ok(BuildResult::Absent);
    };
    Ok(built(
        SourceRef::Project(id),
        BuiltContent {
            title: canonical(&name),
            body: join_nonempty(
                [Some(status), summary]
                    .into_iter()
                    .flatten()
                    .map(|v| canonical(&v)),
            ),
            period: None,
            confidence,
            generation: GenerationMeta::Canonical,
            source_updated_at_ms: updated_at,
            evidence,
        },
    ))
}

fn build_commitment(conn: &Connection, id: i64) -> Result<BuildResult, SearchDocumentError> {
    let row = conn
        .query_row(
            "SELECT description, status, confidence, updated_at FROM commitments WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((description, status, confidence, updated_at)) = row else {
        return Ok(BuildResult::Absent);
    };
    let Some(evidence) = state_evidence(conn, "commitments", id)? else {
        return Ok(BuildResult::Absent);
    };
    Ok(built(
        SourceRef::Commitment(id),
        BuiltContent {
            title: "Commitment".to_owned(),
            body: join_nonempty([canonical(&description), canonical(&status)]),
            period: None,
            confidence,
            generation: GenerationMeta::Canonical,
            source_updated_at_ms: updated_at,
            evidence,
        },
    ))
}

fn build_open_loop(conn: &Connection, id: i64) -> Result<BuildResult, SearchDocumentError> {
    let row = conn
        .query_row(
            "SELECT description, kind, status, confidence, updated_at FROM open_loops WHERE id = ?1",
            [id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, f64>(3)?, row.get::<_, i64>(4)?)),
        )
        .optional()?;
    let Some((description, kind, status, confidence, updated_at)) = row else {
        return Ok(BuildResult::Absent);
    };
    let Some(evidence) = state_evidence(conn, "open_loops", id)? else {
        return Ok(BuildResult::Absent);
    };
    Ok(built(
        SourceRef::OpenLoop(id),
        BuiltContent {
            title: "Open loop".to_owned(),
            body: join_nonempty([
                canonical(&description),
                canonical(&kind),
                canonical(&status),
            ]),
            period: None,
            confidence,
            generation: GenerationMeta::Canonical,
            source_updated_at_ms: updated_at,
            evidence,
        },
    ))
}

fn event_evidence_for(
    conn: &Connection,
    column: &str,
    value: &dyn ToSql,
) -> Result<Vec<EvidenceRef>, rusqlite::Error> {
    let sql = format!("SELECT id FROM event_log WHERE {column} = ?1 ORDER BY ts, id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([value], |row| {
        Ok(EvidenceRef::Event {
            id: row.get(0)?,
            weight: 1.0,
        })
    })?;
    rows.collect()
}

fn build_thread(conn: &Connection, id: i64) -> Result<BuildResult, SearchDocumentError> {
    let row = conn
        .query_row(
            "SELECT thread_key, title, summary, first_activity_at, last_activity_at,
                    confidence, updated_at FROM threads WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, f64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((thread_key, title, summary, start, end, confidence, updated_at)) = row else {
        return Ok(BuildResult::Absent);
    };
    let Some(summary) = summary.filter(|summary| !summary.trim().is_empty()) else {
        return Ok(BuildResult::Absent);
    };
    let Some(body) = generated(&summary) else {
        return Ok(BuildResult::Failed("generated_content_rejected"));
    };
    Ok(built(
        SourceRef::Thread(id),
        BuiltContent {
            title: title.map_or_else(|| "Thread summary".to_owned(), |title| canonical(&title)),
            body,
            period: Some((start, end)),
            confidence,
            generation: GenerationMeta::Extractive {
                generated_at_ms: Some(updated_at),
            },
            source_updated_at_ms: updated_at,
            evidence: event_evidence_for(conn, "thread_key", &thread_key)?,
        },
    ))
}

fn build_session(conn: &Connection, id: i64) -> Result<BuildResult, SearchDocumentError> {
    let row = conn
        .query_row(
            "SELECT title, summary, started_at, ended_at, confidence, updated_at
               FROM sessions WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, f64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            },
        )
        .optional()?;
    let Some((title, summary, start, end, confidence, updated_at)) = row else {
        return Ok(BuildResult::Absent);
    };
    let Some(summary) = summary.filter(|summary| !summary.trim().is_empty()) else {
        return Ok(BuildResult::Absent);
    };
    let Some(body) = generated(&summary) else {
        return Ok(BuildResult::Failed("generated_content_rejected"));
    };
    let mut evidence = vec![EvidenceRef::Session { id, weight: 1.0 }];
    evidence.extend(event_evidence_for(conn, "session_id", &id)?);
    Ok(built(
        SourceRef::Session(id),
        BuiltContent {
            title: title.map_or_else(|| "Session summary".to_owned(), |title| canonical(&title)),
            body,
            period: Some((start, end.unwrap_or(updated_at))),
            confidence,
            generation: GenerationMeta::Extractive {
                generated_at_ms: Some(updated_at),
            },
            source_updated_at_ms: updated_at,
            evidence,
        },
    ))
}

fn build_recap(conn: &Connection, id: i64) -> Result<BuildResult, SearchDocumentError> {
    let row = conn
        .query_row(
            "SELECT r.session_id, r.summary, r.decisions, r.next_actions, r.model, r.created_at,
                    s.title, s.started_at, s.ended_at, s.confidence
               FROM meeting_recaps r JOIN sessions s ON s.id = r.session_id
              WHERE r.id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                    row.get::<_, f64>(9)?,
                ))
            },
        )
        .optional()?;
    let Some((
        session_id,
        summary,
        decisions,
        actions,
        model,
        created_at,
        title,
        start,
        end,
        confidence,
    )) = row
    else {
        return Ok(BuildResult::Absent);
    };
    let decisions = match generated_json_lines(&decisions, false) {
        Ok(lines) => lines,
        Err(()) => return Ok(BuildResult::Failed("invalid_source_json")),
    };
    let actions = match generated_json_lines(&actions, true) {
        Ok(lines) => lines,
        Err(()) => return Ok(BuildResult::Failed("invalid_source_json")),
    };
    let mut body_parts = Vec::new();
    if let Some(summary) = generated(&summary) {
        body_parts.push(summary);
    }
    body_parts.extend(decisions);
    body_parts.extend(actions);
    let body = join_nonempty(body_parts);
    if body.is_empty() {
        return Ok(BuildResult::Failed("generated_content_rejected"));
    }
    let mut evidence = vec![
        EvidenceRef::Session {
            id: session_id,
            weight: 1.0,
        },
        EvidenceRef::MeetingRecap { id, weight: 1.0 },
    ];
    let note_id = conn
        .query_row(
            "SELECT id FROM session_notes WHERE session_id = ?1",
            [session_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    if let Some(note_id) = note_id {
        evidence.push(EvidenceRef::SessionNote {
            id: note_id,
            weight: 1.0,
        });
    }
    let mut stmt =
        conn.prepare("SELECT id FROM transcript_segments WHERE session_id = ?1 AND excluded = 0 ORDER BY ts, id")?;
    let rows = stmt.query_map([session_id], |row| row.get::<_, i64>(0))?;
    for row in rows {
        evidence.push(EvidenceRef::TranscriptSegment {
            id: row?,
            weight: 1.0,
        });
    }
    Ok(built(
        SourceRef::MeetingRecap(id),
        BuiltContent {
            title: title.map_or_else(|| "Meeting recap".to_owned(), |title| canonical(&title)),
            body,
            period: Some((start, end.unwrap_or(created_at))),
            confidence,
            generation: GenerationMeta::Model {
                model,
                generated_at_ms: created_at,
            },
            source_updated_at_ms: created_at,
            evidence,
        },
    ))
}

fn generated_json_lines(raw: &str, objects: bool) -> Result<Vec<String>, ()> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| ())?;
    let Some(items) = value.as_array() else {
        return Err(());
    };
    let mut lines = Vec::new();
    for item in items {
        let raw_line = if objects {
            item.get("text").and_then(serde_json::Value::as_str)
        } else {
            item.as_str()
        };
        if let Some(line) = raw_line.and_then(generated) {
            if !line.trim().is_empty() {
                lines.push(line);
            }
        }
    }
    Ok(lines)
}

fn build_note(conn: &Connection, id: i64) -> Result<BuildResult, SearchDocumentError> {
    let row = conn
        .query_row(
            "SELECT n.session_id, n.body, n.updated_at, s.title, s.started_at, s.ended_at,
                    s.confidence
               FROM session_notes n JOIN sessions s ON s.id = n.session_id
              WHERE n.id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, f64>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((session_id, body, updated_at, title, start, end, confidence)) = row else {
        return Ok(BuildResult::Absent);
    };
    let body = canonical(&body);
    if body.trim().is_empty() {
        return Ok(BuildResult::Absent);
    }
    Ok(built(
        SourceRef::SessionNote(id),
        BuiltContent {
            title: title.map_or_else(|| "Meeting note".to_owned(), |title| canonical(&title)),
            body,
            period: Some((start, end.unwrap_or(updated_at))),
            confidence,
            generation: GenerationMeta::Canonical,
            source_updated_at_ms: updated_at,
            evidence: vec![
                EvidenceRef::Session {
                    id: session_id,
                    weight: 1.0,
                },
                EvidenceRef::SessionNote { id, weight: 1.0 },
            ],
        },
    ))
}

fn build_brief(conn: &Connection, date: &str) -> Result<BuildResult, SearchDocumentError> {
    let row = conn
        .query_row(
            "SELECT payload, built_at FROM briefs WHERE date = ?1",
            [date],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    let Some((payload, built_at)) = row else {
        return Ok(BuildResult::Absent);
    };
    let payload: crate::briefs::BriefPayload = match serde_json::from_str(&payload) {
        Ok(payload) => payload,
        Err(_) => return Ok(BuildResult::Failed("invalid_source_json")),
    };
    let mut body = Vec::new();
    let mut evidence_ids = std::collections::BTreeSet::new();
    let mut confidence = 1.0_f64;
    for line in payload.commitments_due.iter().chain(&payload.open_loops) {
        if line.evidence_kind != Some(crate::state::EvidenceKind::EditedContext) {
            let Some(event_id) = line.provenance_event_id else {
                continue;
            };
            let exists = conn
                .query_row("SELECT 1 FROM event_log WHERE id = ?1", [event_id], |_| {
                    Ok(())
                })
                .optional()?
                .is_some();
            if !exists {
                continue;
            }
            evidence_ids.insert(event_id);
        }
        body.push(canonical(&line.text));
        confidence = confidence.min(if line.possibly { 0.5 } else { 0.8 });
    }
    let body = join_nonempty(body);
    if body.is_empty() {
        return Ok(BuildResult::Absent);
    }
    let mut evidence = vec![EvidenceRef::Brief {
        date: date.to_owned(),
        weight: 1.0,
    }];
    evidence.extend(
        evidence_ids
            .into_iter()
            .map(|id| EvidenceRef::Event { id, weight: 1.0 }),
    );
    Ok(built(
        SourceRef::Brief(date.to_owned()),
        BuiltContent {
            title: canonical(&format!("Brief {date}")),
            body,
            period: None,
            confidence,
            generation: GenerationMeta::Extractive {
                generated_at_ms: Some(built_at),
            },
            source_updated_at_ms: built_at,
            evidence,
        },
    ))
}

fn build_lesson(conn: &Connection, id: i64) -> Result<BuildResult, SearchDocumentError> {
    let row = conn
        .query_row(
            "SELECT instruction, confidence, active, updated_at FROM lessons WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, f64>(1)?,
                    row.get::<_, i64>(2)? != 0,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((instruction, confidence, active, updated_at)) = row else {
        return Ok(BuildResult::Absent);
    };
    if !active {
        return Ok(BuildResult::Absent);
    }
    let Some(body) = generated(&instruction) else {
        return Ok(BuildResult::Failed("generated_content_rejected"));
    };
    let mut stmt = conn.prepare(
        "SELECT feedback_event_id FROM lesson_provenance
          WHERE lesson_id = ?1 ORDER BY feedback_event_id",
    )?;
    let rows = stmt.query_map([id], |row| row.get::<_, i64>(0))?;
    let mut evidence = Vec::new();
    for row in rows {
        evidence.push(EvidenceRef::FeedbackEvent {
            id: row?,
            weight: 1.0,
        });
    }
    Ok(built(
        SourceRef::Lesson(id),
        BuiltContent {
            title: "Lesson".to_owned(),
            body,
            period: None,
            confidence,
            generation: GenerationMeta::Extractive {
                generated_at_ms: Some(updated_at),
            },
            source_updated_at_ms: updated_at,
            evidence,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_event(conn: &Connection, ts: i64, content: &str, content_hash: &str) -> i64 {
        crate::event_log::insert(
            conn,
            &crate::event_log::NewEvent {
                ts,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content,
                content_hash,
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap()
    }

    fn seed_all_kinds(conn: &Connection) -> (i64, i64, i64) {
        conn.execute(
            "INSERT INTO threads
               (thread_key, source, title, summary, first_activity_at, last_activity_at,
                event_count, salience, confidence, created_at, updated_at)
             VALUES ('thread-1', 'capture', 'Roadmap thread', 'Roadmap launch status',
                     10, 20, 1, 0.0, 0.9, 10, 20)",
            [],
        )
        .unwrap();
        let thread_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO sessions
               (kind, started_at, ended_at, title, summary, confidence, provenance,
                created_at, updated_at)
             VALUES ('meeting', 10, 30, 'Strategy sync', 'Strategy session summary',
                     0.9, '{}', 10, 30)",
            [],
        )
        .unwrap();
        let session_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO event_log
               (ts, source, kind, content, content_hash, last_seen_at, thread_key, session_id)
             VALUES (11, 'capture', 'text', 'Canonical source', 'search-doc-source', 11,
                     'thread-1', ?1)",
            [session_id],
        )
        .unwrap();
        let event_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO people
               (display_name, relationship_summary, confidence, created_at, updated_at,
                last_evidence_at, base_confidence)
             VALUES ('Alice', 'Quarterly owner', 0.9, 1, 2, 2, 0.9)",
            [],
        )
        .unwrap();
        let person_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO projects
               (name, status, summary, confidence, created_at, updated_at,
                last_evidence_at, base_confidence)
             VALUES ('Atlas', 'active', 'Atlas migration', 0.9, 1, 2, 2, 0.9)",
            [],
        )
        .unwrap();
        let project_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO commitments
               (direction, description, status, confidence, created_at, updated_at,
                last_evidence_at, base_confidence)
             VALUES ('mine', 'Send vendor invoice', 'open', 0.9, 1, 2, 2, 0.9)",
            [],
        )
        .unwrap();
        let commitment_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO open_loops
               (kind, description, opened_at, status, confidence, created_at, updated_at,
                last_evidence_at, base_confidence)
             VALUES ('review_pending', 'Legal review pending', 1, 'open', 0.9, 1, 2, 2, 0.9)",
            [],
        )
        .unwrap();
        let open_loop_id = conn.last_insert_rowid();
        for (table, id) in [
            ("people", person_id),
            ("projects", project_id),
            ("commitments", commitment_id),
            ("open_loops", open_loop_id),
        ] {
            conn.execute(
                "INSERT INTO state_provenance (state_table, state_id, event_id, weight)
                 VALUES (?1, ?2, ?3, 1.0)",
                params![table, id, event_id],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO session_notes (session_id, body, updated_at)
             VALUES (?1, 'Pineapple note', 31)",
            [session_id],
        )
        .unwrap();
        let note_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO transcript_segments
               (session_id, ts, text, origin, confidence, created_at)
             VALUES (?1, 12, 'Transcript evidence', 'asr', 0.9, 12)",
            [session_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO meeting_recaps
               (session_id, summary, decisions, next_actions, model, created_at)
             VALUES (?1, 'Budget recap', '[\"Approve budget\"]',
                     '[{\"text\":\"Book venue\"}]', 'model-a', 32)",
            [session_id],
        )
        .unwrap();
        let recap_id = conn.last_insert_rowid();
        let payload = serde_json::to_string(&crate::briefs::BriefPayload {
            date: "2026-08-23".to_owned(),
            today: Vec::new(),
            commitments_due: vec![crate::briefs::BriefLine {
                text: "Launch checklist".to_owned(),
                provenance_event_id: Some(event_id),
                evidence_kind: None,
                possibly: false,
            }],
            open_loops: Vec::new(),
            what_happened: vec!["Unproven narrative must stay out".to_owned()],
            suggested_actions: Vec::new(),
            charm_line: None,
            pattern_line: None,
            meetings: vec![],
        })
        .unwrap();
        conn.execute(
            "INSERT INTO briefs (date, payload, generated, built_at)
             VALUES ('2026-08-23', ?1, 0, 40)",
            [payload],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO feedback_events (ts_ms, kind, scope)
             VALUES (50, 'approve_unchanged', 'global')",
            [],
        )
        .unwrap();
        let feedback_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO lessons
               (kind, scope, instruction, confidence, evidence_count, active,
                created_at, updated_at, last_evidence_at)
             VALUES ('style', 'global', 'Keep updates concise', 0.7, 1, 1, 50, 50, 50)",
            [],
        )
        .unwrap();
        let lesson_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO lesson_provenance (lesson_id, feedback_event_id) VALUES (?1, ?2)",
            params![lesson_id, feedback_id],
        )
        .unwrap();
        (thread_id, event_id, recap_id.max(note_id))
    }

    #[test]
    fn rebuild_all_indexes_every_existing_v21_kind_and_excludes_unproven_brief_text() {
        let mut conn = crate::open_in_memory().unwrap();
        seed_all_kinds(&conn);

        let report = rebuild_all(&mut conn, 100).unwrap();
        assert_eq!(report.upserted, 10);

        let kinds: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT document_kind FROM search_documents ORDER BY document_kind")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(kinds.len(), 10);
        assert!(!kinds.iter().any(|kind| kind == "period_summary"));

        for term in [
            "Alice",
            "Atlas",
            "invoice",
            "Legal",
            "Roadmap",
            "Strategy",
            "Budget",
            "Pineapple",
            "Launch",
            "concise",
        ] {
            assert!(
                !search_documents_fts(&conn, term, 10).unwrap().is_empty(),
                "{term}"
            );
        }
        assert!(search_documents_fts(&conn, "Unproven narrative", 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn derived_search_matches_natural_question_without_verbatim_phrase() {
        let mut conn = crate::open_in_memory().unwrap();
        seed_all_kinds(&conn);
        rebuild_all(&mut conn, 100).unwrap();

        let results =
            search_documents_fts(&conn, "What was the roadmap launch status?", 10).unwrap();

        assert_eq!(
            results.first().map(|candidate| candidate.kind),
            Some(SearchDocumentKind::ThreadSummary),
            "document matching all topical terms should rank first"
        );
    }

    #[test]
    fn projection_sanitizes_note_copy_without_mutating_canonical_note() {
        let mut conn = crate::open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO sessions
               (kind, started_at, ended_at, title, confidence, provenance, created_at, updated_at)
             VALUES ('meeting', 1, 2, 'Private', 0.9, '{}', 1, 2)",
            [],
        )
        .unwrap();
        let session_id = conn.last_insert_rowid();
        let raw = "secret sk-ant-api03-abcdefghijklmnop\u{200b}";
        conn.execute(
            "INSERT INTO session_notes (session_id, body, updated_at) VALUES (?1, ?2, 3)",
            params![session_id, raw],
        )
        .unwrap();
        let note_id = conn.last_insert_rowid();

        assert_eq!(
            rebuild_source(&mut conn, &SourceRef::SessionNote(note_id), 4).unwrap(),
            RebuildOutcome::Upserted
        );
        let projected: String = conn
            .query_row("SELECT body FROM search_documents", [], |row| row.get(0))
            .unwrap();
        assert_eq!(projected, "secret [redacted]");
        let canonical: String = conn
            .query_row(
                "SELECT body FROM session_notes WHERE id = ?1",
                [note_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(canonical, raw);
    }

    #[test]
    fn evidence_and_source_delete_hide_documents_until_reconciled() {
        let mut conn = crate::open_in_memory().unwrap();
        let (thread_id, event_id, _) = seed_all_kinds(&conn);
        rebuild_source(&mut conn, &SourceRef::Thread(thread_id), 100).unwrap();
        let document_id: i64 = conn
            .query_row(
                "SELECT id FROM search_documents WHERE source_table = 'threads'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        conn.execute("DELETE FROM event_log WHERE id = ?1", [event_id])
            .unwrap_err();
        conn.execute(
            "DELETE FROM state_provenance WHERE event_id = ?1",
            [event_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE event_log SET session_id = NULL WHERE id = ?1",
            [event_id],
        )
        .unwrap();
        conn.execute("DELETE FROM event_log WHERE id = ?1", [event_id])
            .unwrap();
        let invalidated: i64 = conn
            .query_row(
                "SELECT count(*) FROM search_document_invalidations
                  WHERE source_table = 'threads' AND source_id = ?1",
                [thread_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(invalidated, 1);
        assert!(search_documents_fts(&conn, "Roadmap", 10)
            .unwrap()
            .is_empty());
        assert!(hydrate_documents(&conn, &[document_id]).unwrap().is_empty());
        assert!(list_dirty(&conn, None, 100)
            .unwrap()
            .sources
            .contains(&SourceRef::Thread(thread_id)));

        conn.execute("DELETE FROM threads WHERE id = ?1", [thread_id])
            .unwrap();
        assert_eq!(
            rebuild_source(&mut conn, &SourceRef::Thread(thread_id), 101).unwrap(),
            RebuildOutcome::Deleted
        );
        let remaining: i64 = conn
            .query_row(
                "SELECT count(*) FROM search_documents WHERE source_table = 'threads'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0);
        assert!(search_documents_fts(&conn, "Roadmap", 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn recreate_projection_rebuilds_without_changing_refinery_history() {
        let mut conn = crate::open_in_memory().unwrap();
        seed_all_kinds(&conn);
        rebuild_all(&mut conn, 100).unwrap();
        conn.execute(
            "UPDATE search_document_backfill_state
                SET last_source_rowid = 7, updated_at_ms = 101
              WHERE source_table = 'people'",
            [],
        )
        .unwrap();

        let report = recreate_projection_and_rebuild(&mut conn, 200).unwrap();

        assert_eq!(report.upserted, 10);
        assert_eq!(crate::schema_version(&conn).unwrap(), Some(crate::LATEST_SCHEMA_VERSION));
        assert_eq!(search_documents_fts(&conn, "Atlas", 10).unwrap().len(), 1);
        let cursor: i64 = conn
            .query_row(
                "SELECT last_source_rowid FROM search_document_backfill_state
                  WHERE source_table = 'people'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cursor, 7, "optional projection repair preserves durable backfill state");
    }

    #[test]
    fn invalid_recap_json_keeps_previous_document_dirty_and_content_intact() {
        let mut conn = crate::open_in_memory().unwrap();
        let (_, _, recap_or_note_id) = seed_all_kinds(&conn);
        let recap_id: i64 = conn
            .query_row("SELECT id FROM meeting_recaps", [], |row| row.get(0))
            .unwrap();
        rebuild_source(&mut conn, &SourceRef::MeetingRecap(recap_id), 100).unwrap();
        let original: String = conn
            .query_row(
                "SELECT body FROM search_documents WHERE source_table = 'meeting_recaps'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "UPDATE meeting_recaps SET decisions = '{', created_at = 101 WHERE id = ?1",
            [recap_id],
        )
        .unwrap();

        let outcome = rebuild_source(&mut conn, &SourceRef::MeetingRecap(recap_id), 102).unwrap();

        assert_eq!(outcome, RebuildOutcome::Failed("invalid_source_json"));
        let (body, dirty, failure): (String, i64, Option<String>) = conn
            .query_row(
                "SELECT body, is_dirty, last_failure_class FROM search_documents
                  WHERE source_table = 'meeting_recaps'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(body, original);
        assert_eq!(dirty, 1);
        assert_eq!(failure.as_deref(), Some("invalid_source_json"));
        assert!(recap_or_note_id > 0);
    }

    #[test]
    fn canonical_writes_survive_each_missing_optional_projection_object() {
        for broken in ["provenance", "fts", "documents"] {
            let mut conn = crate::open_in_memory().unwrap();
            // Authored rows remain projectable without source links; unsupported automatic
            // rows are intentionally absent regardless of projection schema health.
            conn.execute(
                "INSERT INTO people
                   (display_name, confidence, created_at, updated_at, last_evidence_at,
                    base_confidence, user_pinned)
                 VALUES ('Before repair', 0.8, 1, 1, 1, 0.8, 1)",
                [],
            )
            .unwrap();
            let id = conn.last_insert_rowid();
            rebuild_source(&mut conn, &SourceRef::Person(id), 2).unwrap();

            match broken {
                "provenance" => conn
                    .execute_batch("DROP TABLE search_document_provenance")
                    .unwrap(),
                "fts" => conn
                    .execute_batch("DROP TABLE search_documents_fts")
                    .unwrap(),
                "documents" => conn
                    .execute_batch(
                        "DROP TABLE search_document_provenance;
                         DROP TABLE search_documents_fts;
                         DROP TABLE search_documents;",
                    )
                    .unwrap(),
                _ => unreachable!(),
            }

            conn.execute(
                "UPDATE people SET display_name = 'After repair', updated_at = 3 WHERE id = ?1",
                [id],
            )
            .unwrap();
            conn.execute("DELETE FROM people WHERE id = ?1", [id])
                .unwrap();
            conn.execute(
                "INSERT INTO people
                   (display_name, confidence, created_at, updated_at, last_evidence_at,
                    base_confidence, user_pinned)
                 VALUES ('Converged', 0.8, 4, 4, 4, 0.8, 1)",
                [],
            )
            .unwrap();

            recreate_projection_and_rebuild(&mut conn, 5).unwrap();
            assert_eq!(
                search_documents_fts(&conn, "Converged", 10).unwrap().len(),
                1
            );
        }
    }

    #[test]
    fn dirty_only_document_update_does_not_touch_fts() {
        let mut conn = crate::open_in_memory().unwrap();
        seed_all_kinds(&conn);
        rebuild_source(&mut conn, &SourceRef::Person(1), 100).unwrap();
        conn.execute_batch("DROP TABLE search_documents_fts")
            .unwrap();

        conn.execute(
            "UPDATE search_documents SET is_dirty = 1 WHERE source_table = 'people'",
            [],
        )
        .unwrap();
    }

    #[test]
    fn lesson_provenance_key_update_invalidates_old_and_new_owners() {
        let mut conn = crate::open_in_memory().unwrap();
        seed_all_kinds(&conn);
        let old_lesson: i64 = conn
            .query_row("SELECT id FROM lessons", [], |row| row.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO lessons
               (kind, scope, instruction, confidence, evidence_count, active,
                created_at, updated_at, last_evidence_at)
             VALUES ('style', 'global', 'Use headings', 0.7, 1, 1, 50, 50, 50)",
            [],
        )
        .unwrap();
        let new_lesson = conn.last_insert_rowid();
        let feedback: i64 = conn
            .query_row("SELECT id FROM feedback_events", [], |row| row.get(0))
            .unwrap();
        rebuild_source(&mut conn, &SourceRef::Lesson(old_lesson), 100).unwrap();

        conn.execute(
            "UPDATE lesson_provenance SET lesson_id = ?1
              WHERE lesson_id = ?2 AND feedback_event_id = ?3",
            params![new_lesson, old_lesson, feedback],
        )
        .unwrap();

        let invalidated: i64 = conn
            .query_row(
                "SELECT count(*) FROM search_document_invalidations
                  WHERE source_table = 'lessons' AND source_id IN (?1, ?2)",
                params![old_lesson.to_string(), new_lesson.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(invalidated, 2);
        assert!(search_documents_fts(&conn, "concise", 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn evidence_delete_cannot_reexpose_unchanged_derived_phrase() {
        let mut conn = crate::open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO event_log
               (ts, source, kind, content, content_hash, last_seen_at)
             VALUES (100, 'capture', 'text', 'old support', 'old-support', 100)",
            [],
        )
        .unwrap();
        let old_event = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO event_log
               (ts, source, kind, content, content_hash, last_seen_at)
             VALUES (1000, 'capture', 'text', 'recent secret', 'recent-secret', 1000)",
            [],
        )
        .unwrap();
        let recent_event = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO people
               (display_name, relationship_summary, confidence, created_at, updated_at,
                last_evidence_at, base_confidence)
             VALUES ('Alice', 'DeletedPhrase belongs only to recent evidence', 0.8,
                     100, 100, 1000, 0.8)",
            [],
        )
        .unwrap();
        let person = conn.last_insert_rowid();
        for event in [old_event, recent_event] {
            conn.execute(
                "INSERT INTO state_provenance (state_table, state_id, event_id, weight)
                 VALUES ('people', ?1, ?2, 1.0)",
                params![person, event],
            )
            .unwrap();
        }
        assert_eq!(
            rebuild_source(&mut conn, &SourceRef::Person(person), 1100).unwrap(),
            RebuildOutcome::Upserted
        );
        assert_eq!(
            search_documents_fts(&conn, "DeletedPhrase", 10)
                .unwrap()
                .len(),
            1
        );

        crate::maintenance::delete_since(&mut conn, 500).unwrap();
        assert!(search_documents_fts(&conn, "DeletedPhrase", 10)
            .unwrap()
            .is_empty());
        assert_eq!(
            rebuild_source(&mut conn, &SourceRef::Person(person), 1200).unwrap(),
            RebuildOutcome::Deferred
        );
        assert!(search_documents_fts(&conn, "DeletedPhrase", 10)
            .unwrap()
            .is_empty());

        conn.execute(
            "UPDATE people
                SET relationship_summary = 'Scrubbed against surviving evidence', updated_at = 200
              WHERE id = ?1",
            [person],
        )
        .unwrap();
        assert_eq!(
            rebuild_source(&mut conn, &SourceRef::Person(person), 1300).unwrap(),
            RebuildOutcome::Upserted
        );
        assert!(search_documents_fts(&conn, "DeletedPhrase", 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn recap_evidence_deletes_require_canonical_rederivation() {
        let mut conn = crate::open_in_memory().unwrap();
        seed_all_kinds(&conn);
        let recap: i64 = conn
            .query_row("SELECT id FROM meeting_recaps", [], |row| row.get(0))
            .unwrap();
        let source = SourceRef::MeetingRecap(recap);
        assert_eq!(
            rebuild_source(&mut conn, &source, 100).unwrap(),
            RebuildOutcome::Upserted
        );
        let document_id: i64 = conn
            .query_row(
                "SELECT id FROM search_documents
                  WHERE source_table = 'meeting_recaps' AND source_id = ?1",
                [recap.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(search_documents_fts(&conn, "Budget", 10).unwrap().len(), 1);

        conn.execute("DELETE FROM transcript_segments", []).unwrap();
        assert!(search_documents_fts(&conn, "Budget", 10)
            .unwrap()
            .is_empty());
        assert!(hydrate_documents(&conn, &[document_id]).unwrap().is_empty());
        assert_eq!(
            rebuild_source(&mut conn, &source, 101).unwrap(),
            RebuildOutcome::Deferred
        );

        conn.execute(
            "UPDATE meeting_recaps SET summary = 'Re-derived recap', created_at = 40",
            [],
        )
        .unwrap();
        assert_eq!(
            rebuild_source(&mut conn, &source, 102).unwrap(),
            RebuildOutcome::Upserted
        );
        assert_eq!(hydrate_documents(&conn, &[document_id]).unwrap().len(), 1);

        conn.execute("DELETE FROM session_notes", []).unwrap();
        assert!(search_documents_fts(&conn, "Re-derived", 10)
            .unwrap()
            .is_empty());
        assert!(hydrate_documents(&conn, &[document_id]).unwrap().is_empty());
        assert_eq!(
            rebuild_source(&mut conn, &source, 103).unwrap(),
            RebuildOutcome::Deferred
        );
        conn.execute("UPDATE meeting_recaps SET created_at = 41", [])
            .unwrap();
        assert_eq!(
            rebuild_source(&mut conn, &source, 104).unwrap(),
            RebuildOutcome::Upserted
        );
        assert_eq!(hydrate_documents(&conn, &[document_id]).unwrap().len(), 1);
    }

    #[test]
    fn recap_evidence_inserts_require_canonical_rederivation() {
        for evidence_kind in ["transcript", "note"] {
            let mut conn = crate::open_in_memory().unwrap();
            seed_all_kinds(&conn);
            let (recap, session): (i64, i64) = conn
                .query_row(
                    "SELECT id, session_id FROM meeting_recaps",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            let source = SourceRef::MeetingRecap(recap);

            if evidence_kind == "transcript" {
                conn.execute("DELETE FROM transcript_segments", []).unwrap();
            } else {
                conn.execute("DELETE FROM session_notes", []).unwrap();
            }
            conn.execute("UPDATE meeting_recaps SET created_at = 40", [])
                .unwrap();
            assert_eq!(
                rebuild_source(&mut conn, &source, 100).unwrap(),
                RebuildOutcome::Upserted
            );
            let document_id: i64 = conn
                .query_row(
                    "SELECT id FROM search_documents
                      WHERE source_table = 'meeting_recaps' AND source_id = ?1",
                    [recap.to_string()],
                    |row| row.get(0),
                )
                .unwrap();

            if evidence_kind == "transcript" {
                conn.execute(
                    "INSERT INTO transcript_segments
                       (session_id, ts, text, origin, confidence, created_at)
                     VALUES (?1, 50, 'new transcript evidence', 'asr', 0.9, 50)",
                    [session],
                )
                .unwrap();
            } else {
                conn.execute(
                    "INSERT INTO session_notes (session_id, body, updated_at)
                     VALUES (?1, 'new note evidence', 50)",
                    [session],
                )
                .unwrap();
            }

            assert!(search_documents_fts(&conn, "Budget", 10)
                .unwrap()
                .is_empty());
            assert!(hydrate_documents(&conn, &[document_id]).unwrap().is_empty());
            assert_eq!(
                rebuild_source(&mut conn, &source, 101).unwrap(),
                RebuildOutcome::Deferred
            );

            conn.execute("UPDATE meeting_recaps SET created_at = 41", [])
                .unwrap();
            assert_eq!(
                rebuild_source(&mut conn, &source, 102).unwrap(),
                RebuildOutcome::Upserted
            );
            assert_eq!(search_documents_fts(&conn, "Budget", 10).unwrap().len(), 1);
            assert_eq!(hydrate_documents(&conn, &[document_id]).unwrap().len(), 1);
        }
    }

    #[test]
    fn repeated_backfill_preserves_identical_logical_rows() {
        let mut conn = crate::open_in_memory().unwrap();
        seed_all_kinds(&conn);
        rebuild_all(&mut conn, 100).unwrap();
        let before = crate::maintenance::export_json(&conn).unwrap();

        let report = rebuild_all(&mut conn, 200).unwrap();
        let after = crate::maintenance::export_json(&conn).unwrap();

        assert_eq!(report.upserted, 0, "unchanged canonical rows are a no-op");
        assert_eq!(after, before, "retry must preserve byte-identical logical projection rows");
    }

    #[test]
    fn hydration_revalidates_confidence_and_normalized_evidence() {
        let mut conn = crate::open_in_memory().unwrap();
        let (thread_id, event_id, _) = seed_all_kinds(&conn);
        let source = SourceRef::Thread(thread_id);
        rebuild_source(&mut conn, &source, 100).unwrap();
        let document_id: i64 = conn
            .query_row(
                "SELECT id FROM search_documents WHERE source_table = 'threads'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        conn.execute(
            "UPDATE threads SET confidence = 0.2 WHERE id = ?1",
            [thread_id],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM search_document_invalidations
              WHERE source_table = 'threads' AND source_id = ?1",
            [thread_id.to_string()],
        )
        .unwrap();
        assert!(hydrate_documents(&conn, &[document_id]).unwrap().is_empty());

        conn.execute(
            "UPDATE threads SET confidence = 0.9 WHERE id = ?1",
            [thread_id],
        )
        .unwrap();
        rebuild_source(&mut conn, &source, 101).unwrap();
        conn.execute(
            "DELETE FROM search_document_provenance
              WHERE document_id = ?1 AND event_id = ?2",
            params![document_id, event_id],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM search_document_invalidations
              WHERE source_table = 'threads' AND source_id = ?1",
            [thread_id.to_string()],
        )
        .unwrap();
        assert!(hydrate_documents(&conn, &[document_id]).unwrap().is_empty());
    }

    #[test]
    fn canonical_event_delete_dirties_then_scrubs_v21_evidence() {
        let mut conn = crate::open_in_memory().unwrap();
        let (thread_id, event_id, _) = seed_all_kinds(&conn);
        rebuild_source(&mut conn, &SourceRef::Thread(thread_id), 100).unwrap();

        delete_canonical_event(&mut conn, event_id).unwrap();

        let event_exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM event_log WHERE id = ?1",
                [event_id],
                |row| row.get(0),
            )
            .unwrap();
        let provenance_exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM search_document_provenance WHERE event_id = ?1",
                [event_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(event_exists, 0);
        assert_eq!(provenance_exists, 0);
        assert!(list_dirty(&conn, None, 100)
            .unwrap()
            .sources
            .contains(&SourceRef::Thread(thread_id)));
    }

    #[test]
    fn canonical_repository_boundaries_project_every_v21_kind_once() {
        let mut conn = crate::open_in_memory().unwrap();
        let event = crate::event_log::NewEvent {
            ts: 10,
            source: "capture",
            kind: "text",
            app_bundle_id: Some("com.example.task6"),
            window_title: Some("Task 6 lifecycle"),
            content: "canonical evidence",
            content_hash: "task6-event",
            dwell_ms: 0,
            display_id: None,
            window_bounds: None,
        };
        let event_id = crate::event_log::insert(&conn, &event).unwrap();
        let provenance = [crate::state::Provenance::new(event_id)];
        crate::state::insert_person(
            &mut conn,
            &crate::state::NewPerson {
                display_name: "Alice",
                confidence: 0.9,
                now: 20,
                ..Default::default()
            },
            &provenance,
        )
        .unwrap();
        crate::state::insert_project(
            &mut conn,
            &crate::state::NewProject {
                name: "Atlas",
                status: crate::state::ProjectStatus::Active,
                summary: Some("Project summary"),
                participants_json: None,
                sources_json: None,
                confidence: 0.9,
                now: 21,
            },
            &provenance,
        )
        .unwrap();
        crate::state::insert_commitment(
            &mut conn,
            &crate::state::NewCommitment {
                direction: crate::state::CommitmentDirection::Mine,
                counterparty_id: None,
                description: "Ship task six",
                due_at: None,
                status: crate::state::CommitmentStatus::Open,
                project_id: None,
                confidence: 0.8,
                now: 22,
            },
            &provenance,
        )
        .unwrap();
        crate::state::insert_open_loop(
            &mut conn,
            &crate::state::NewOpenLoop {
                kind: crate::state::OpenLoopKind::ReviewPending,
                description: "Review task six",
                counterparty_id: None,
                project_id: None,
                opened_at: 10,
                confidence: 0.8,
                now: 23,
            },
            &provenance,
        )
        .unwrap();

        let thread_id: i64 = conn
            .query_row("SELECT id FROM threads", [], |row| row.get(0))
            .unwrap();
        let session_id = crate::session::open(
            &conn,
            &crate::session::NewSession {
                kind: "meeting",
                started_at: 10,
                title: Some("Task 6 meeting"),
                app_bundle_id: None,
                calendar_occurrence_id: None,
                confidence: 0.9,
                provenance: "{}",
            },
        )
        .unwrap();
        crate::session::attach_event(&conn, session_id, event_id).unwrap();
        crate::session_notes::save(&conn, session_id, "Meeting note", 30).unwrap();
        crate::meeting_recaps::save(
            &conn,
            session_id,
            "Meeting recap",
            "[\"Decision\"]",
            "[{\"text\":\"Action\"}]",
            None,
            "fixture-model",
            31,
        )
        .unwrap();
        crate::session::set_summary(&conn, session_id, "Session summary", 32).unwrap();
        let thread_key: String = conn
            .query_row("SELECT thread_key FROM threads WHERE id = ?1", [thread_id], |row| {
                row.get(0)
            })
            .unwrap();
        crate::thread::set_summary(&conn, &thread_key, "Thread summary", 33).unwrap();

        let brief = crate::briefs::BriefPayload {
            date: "2026-08-23".to_owned(),
            today: Vec::new(),
            commitments_due: vec![crate::briefs::BriefLine {
                text: "Ship task six".to_owned(),
                provenance_event_id: Some(event_id),
                evidence_kind: None,
                possibly: false,
            }],
            open_loops: Vec::new(),
            what_happened: Vec::new(),
            suggested_actions: Vec::new(),
            charm_line: None,
            pattern_line: None,
            meetings: vec![],
        };
        crate::briefs::upsert_brief(
            &conn,
            "2026-08-23",
            &serde_json::to_string(&brief).unwrap(),
            false,
            34,
        )
        .unwrap();
        conn.execute(
            "INSERT INTO feedback_events (ts_ms, kind, scope)
             VALUES (35, 'approve_unchanged', 'global')",
            [],
        )
        .unwrap();
        let feedback_id = conn.last_insert_rowid();
        crate::lessons::upsert_lesson(
            &mut conn,
            &crate::lessons::LessonCandidate {
                kind: crate::lessons::LessonKind::Style,
                scope: crate::lessons::LessonScope::Global,
                scope_ref: None,
                instruction: "Keep updates concise".to_owned(),
                evidence: vec![feedback_id],
            },
            &[feedback_id],
            36,
        )
        .unwrap();

        let documents: i64 = conn
            .query_row("SELECT count(*) FROM search_documents WHERE is_dirty = 0", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(documents, 10);
    }

    #[test]
    fn canonical_write_survives_projection_failure_and_retry_converges() {
        let mut conn = crate::open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO event_log
               (ts, source, kind, content, content_hash, last_seen_at)
             VALUES (1, 'capture', 'text', 'evidence', 'failure-evidence', 1)",
            [],
        )
        .unwrap();
        let event_id = conn.last_insert_rowid();
        conn.execute_batch("DROP TABLE search_documents_fts").unwrap();

        let person_id = crate::state::insert_person(
            &mut conn,
            &crate::state::NewPerson {
                display_name: "Durable Alice",
                confidence: 0.9,
                now: 2,
                ..Default::default()
            },
            &[crate::state::Provenance::new(event_id)],
        )
        .unwrap();
        assert!(crate::state::get_person(&conn, person_id).unwrap().is_some());

        ensure_projection_schema(&conn).unwrap();
        reconcile_invalidated(&conn, 100, 3).unwrap();
        assert_eq!(search_documents_fts(&conn, "Durable Alice", 10).unwrap().len(), 1);
    }

    #[test]
    fn same_ms_source_update_reconciles_after_identity_attach() {
        let mut conn = crate::open_in_memory().unwrap();
        let event_id = crate::event_log::insert(
            &conn,
            &crate::event_log::NewEvent {
                ts: 10,
                source: "gmail",
                kind: "email",
                app_bundle_id: None,
                window_title: None,
                content: "alice@example.com",
                content_hash: "task6-same-ms",
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
        let observed = crate::identity::observe(
            &mut conn,
            &crate::identity::Identity::new(
                crate::identity::Channel::Email,
                "alice@example.com",
            ),
            Some("Alice"),
            event_id,
            20,
        )
        .unwrap();
        let crate::identity::Observed::New { person_id } = observed else {
            panic!("first identity observation must create a person");
        };
        assert!(list_dirty(&conn, None, 100)
            .unwrap()
            .sources
            .contains(&SourceRef::Person(person_id)));

        let report = reconcile_invalidated(&conn, 100, 20).unwrap();

        assert_eq!(
            (
                report.deferred,
                list_dirty(&conn, None, 100).unwrap().sources,
                search_documents_fts(&conn, "Alice", 10).unwrap().len(),
            ),
            (0, Vec::new(), 1),
        );
    }

    #[test]
    fn unrelated_event_delete_does_not_invalidate_or_hide_brief() {
        let mut conn = crate::open_in_memory().unwrap();
        let cited_event_id = insert_event(&conn, 100, "Cited brief evidence", "brief-cited");
        let unrelated_event_id = insert_event(&conn, 101, "Unrelated evidence", "brief-unrelated");
        let brief = crate::briefs::BriefPayload {
            date: "2026-08-24".to_owned(),
            today: Vec::new(),
            commitments_due: vec![crate::briefs::BriefLine {
                text: "Cited brief evidence".to_owned(),
                provenance_event_id: Some(cited_event_id),
                evidence_kind: None,
                possibly: false,
            }],
            open_loops: Vec::new(),
            what_happened: Vec::new(),
            suggested_actions: Vec::new(),
            charm_line: None,
            pattern_line: None,
            meetings: vec![],
        };
        crate::briefs::upsert_brief(
            &conn,
            "2026-08-24",
            &serde_json::to_string(&brief).unwrap(),
            false,
            102,
        )
        .unwrap();
        let document_id: i64 = conn
            .query_row(
                "SELECT id FROM search_documents WHERE source_table = 'briefs' AND source_id = '2026-08-24'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        delete_canonical_event(&mut conn, unrelated_event_id).unwrap();

        let invalidations: i64 = conn
            .query_row(
                "SELECT count(*) FROM search_document_invalidations
                  WHERE source_table = 'briefs' AND source_id = '2026-08-24'",
                [],
                |row| row.get(0),
        )
        .unwrap();
        assert_eq!(invalidations, 0);
        assert_eq!(
            search_documents_fts(&conn, "Cited brief evidence", 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(hydrate_documents(&conn, &[document_id]).unwrap().len(), 1);
    }

    #[test]
    fn cited_event_delete_hides_brief_until_canonical_regeneration() {
        let mut conn = crate::open_in_memory().unwrap();
        let cited_event_id = insert_event(&conn, 100, "Cited brief evidence", "brief-delete");
        let retained_event_id =
            insert_event(&conn, 101, "Retained brief evidence", "brief-retained");
        let brief = crate::briefs::BriefPayload {
            date: "2026-08-24".to_owned(),
            today: Vec::new(),
            commitments_due: vec![
                crate::briefs::BriefLine {
                    text: "Cited brief evidence".to_owned(),
                    provenance_event_id: Some(cited_event_id),
                    evidence_kind: None,
                    possibly: false,
                },
                crate::briefs::BriefLine {
                    text: "Retained brief evidence".to_owned(),
                    provenance_event_id: Some(retained_event_id),
                    evidence_kind: None,
                    possibly: false,
                },
            ],
            open_loops: Vec::new(),
            what_happened: Vec::new(),
            suggested_actions: Vec::new(),
            charm_line: None,
            pattern_line: None,
            meetings: vec![],
        };
        crate::briefs::upsert_brief(
            &conn,
            "2026-08-24",
            &serde_json::to_string(&brief).unwrap(),
            false,
            102,
        )
        .unwrap();
        let document_id: i64 = conn
            .query_row(
                "SELECT id FROM search_documents WHERE source_table = 'briefs' AND source_id = '2026-08-24'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        delete_canonical_event(&mut conn, cited_event_id).unwrap();

        let invalidation: (String, i64) = conn
            .query_row(
                "SELECT invalidation_kind, required_owner_updated_after_ms
                   FROM search_document_invalidations
                  WHERE source_table = 'briefs' AND source_id = '2026-08-24'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(invalidation, ("evidence_deleted".to_owned(), 102));
        assert!(search_documents_fts(&conn, "Cited brief evidence", 10)
            .unwrap()
            .is_empty());
        assert!(hydrate_documents(&conn, &[document_id]).unwrap().is_empty());
        assert_eq!(reconcile_invalidated(&conn, 100, 103).unwrap().deferred, 1);
        assert_eq!(rebuild_all(&mut conn, 104).unwrap().deferred, 1);

        let replacement_event_id =
            insert_event(&conn, 105, "Safe replacement evidence", "brief-safe");
        let regenerated = crate::briefs::BriefPayload {
            date: "2026-08-24".to_owned(),
            today: Vec::new(),
            commitments_due: Vec::new(),
            open_loops: vec![crate::briefs::BriefLine {
                text: "Safe regenerated brief".to_owned(),
                provenance_event_id: Some(replacement_event_id),
                evidence_kind: None,
                possibly: false,
            }],
            what_happened: Vec::new(),
            suggested_actions: Vec::new(),
            charm_line: None,
            pattern_line: None,
            meetings: vec![],
        };
        crate::briefs::upsert_brief(
            &conn,
            "2026-08-24",
            &serde_json::to_string(&regenerated).unwrap(),
            false,
            106,
        )
        .unwrap();

        let invalidations: i64 = conn
            .query_row(
                "SELECT count(*) FROM search_document_invalidations
                  WHERE source_table = 'briefs' AND source_id = '2026-08-24'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(invalidations, 0);
        let regenerated_document_id: i64 = conn
            .query_row(
                "SELECT id FROM search_documents WHERE source_table = 'briefs' AND source_id = '2026-08-24'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            search_documents_fts(&conn, "Safe regenerated brief", 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            hydrate_documents(&conn, &[regenerated_document_id])
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn canonical_event_delete_survives_missing_projection_tables() {
        for (missing, drop_sql) in [
            ("search_document_provenance", "DROP TABLE search_document_provenance"),
            (
                "search_documents",
                "DROP TABLE search_document_provenance; DROP TABLE search_documents",
            ),
        ] {
            let mut conn = crate::open_in_memory().unwrap();
            let event_id = insert_event(&conn, 100, "Deletion must survive", missing);
            conn.execute_batch(drop_sql).unwrap();

            delete_canonical_event(&mut conn, event_id).unwrap();

            let remaining: i64 = conn
                .query_row("SELECT count(*) FROM event_log WHERE id = ?1", [event_id], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(remaining, 0, "canonical deletion must not need {missing}");
        }
    }

    #[test]
    fn canonical_event_delete_tolerates_malformed_brief_payload() {
        let mut conn = crate::open_in_memory().unwrap();
        let event_id = insert_event(&conn, 100, "Deletion must survive", "brief-malformed");
        conn.execute(
            "INSERT INTO briefs (date, payload, generated, built_at) VALUES ('2026-08-26', '{', 0, 101)",
            [],
        )
        .unwrap();

        delete_canonical_event(&mut conn, event_id).unwrap();

        let remaining: i64 = conn
            .query_row("SELECT count(*) FROM event_log WHERE id = ?1", [event_id], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn canonical_event_delete_tolerates_wrong_brief_evidence_element_types() {
        for (date, payload) in [
            (
                "2026-08-27",
                r#"{"commitments_due":["not an evidence object"],"open_loops":[]}"#,
            ),
            (
                "2026-08-28",
                r#"{"commitments_due":[],"open_loops":["not an evidence object"]}"#,
            ),
        ] {
            let mut conn = crate::open_in_memory().unwrap();
            let event_id = insert_event(&conn, 100, "Deletion must survive", date);
            conn.execute(
                "INSERT INTO briefs (date, payload, generated, built_at) VALUES (?1, ?2, 0, 101)",
                params![date, payload],
            )
            .unwrap();

            delete_canonical_event(&mut conn, event_id).unwrap();

            let remaining: i64 = conn
                .query_row("SELECT count(*) FROM event_log WHERE id = ?1", [event_id], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(remaining, 0, "canonical deletion must survive {date}");
        }
    }

    #[test]
    fn sole_cited_event_delete_stays_hidden_through_rowid_reuse_until_regeneration() {
        let mut conn = crate::open_in_memory().unwrap();
        let cited_event_id = insert_event(&conn, 100, "Deleted brief evidence", "brief-sole");
        let brief = crate::briefs::BriefPayload {
            date: "2026-08-25".to_owned(),
            today: Vec::new(),
            commitments_due: vec![crate::briefs::BriefLine {
                text: "Deleted brief evidence".to_owned(),
                provenance_event_id: Some(cited_event_id),
                evidence_kind: None,
                possibly: false,
            }],
            open_loops: Vec::new(),
            what_happened: Vec::new(),
            suggested_actions: Vec::new(),
            charm_line: None,
            pattern_line: None,
            meetings: vec![],
        };
        crate::briefs::upsert_brief(
            &conn,
            "2026-08-25",
            &serde_json::to_string(&brief).unwrap(),
            false,
            102,
        )
        .unwrap();
        let document_id: i64 = conn
            .query_row(
                "SELECT id FROM search_documents WHERE source_table = 'briefs' AND source_id = '2026-08-25'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        delete_canonical_event(&mut conn, cited_event_id).unwrap();

        assert_eq!(reconcile_invalidated(&conn, 100, 103).unwrap().deferred, 1);
        assert_eq!(rebuild_all(&mut conn, 104).unwrap().deferred, 1);
        assert!(search_documents_fts(&conn, "Deleted brief evidence", 10)
            .unwrap()
            .is_empty());
        assert!(hydrate_documents(&conn, &[document_id]).unwrap().is_empty());

        let reused_event_id = insert_event(&conn, 105, "Unrelated replacement event", "brief-reused");
        assert_eq!(reused_event_id, cited_event_id, "SQLite reused the deleted rowid");
        assert_eq!(rebuild_all(&mut conn, 106).unwrap().deferred, 1);
        assert!(search_documents_fts(&conn, "Deleted brief evidence", 10)
            .unwrap()
            .is_empty());
        assert!(hydrate_documents(&conn, &[document_id]).unwrap().is_empty());

        let regenerated = crate::briefs::BriefPayload {
            date: "2026-08-25".to_owned(),
            today: Vec::new(),
            commitments_due: vec![crate::briefs::BriefLine {
                text: "Safe regenerated brief".to_owned(),
                provenance_event_id: Some(reused_event_id),
                evidence_kind: None,
                possibly: false,
            }],
            open_loops: Vec::new(),
            what_happened: Vec::new(),
            suggested_actions: Vec::new(),
            charm_line: None,
            pattern_line: None,
            meetings: vec![],
        };
        crate::briefs::upsert_brief(
            &conn,
            "2026-08-25",
            &serde_json::to_string(&regenerated).unwrap(),
            false,
            107,
        )
        .unwrap();

        assert_eq!(
            search_documents_fts(&conn, "Safe regenerated brief", 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn bounded_reconciliation_rotates_deferred_rows_without_starving_ready_owner() {
        let mut conn = crate::open_in_memory().unwrap();
        let event_id = crate::event_log::insert(
            &conn,
            &crate::event_log::NewEvent {
                ts: 10,
                source: "capture",
                kind: "text",
                app_bundle_id: None,
                window_title: None,
                content: "shared evidence",
                content_hash: "task6-fairness",
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            },
        )
        .unwrap();
        let provenance = [crate::state::Provenance::new(event_id)];
        for index in 0..101 {
            crate::state::insert_person(
                &mut conn,
                &crate::state::NewPerson {
                    display_name: &format!("Deferred person {index}"),
                    confidence: 0.8,
                    now: 20 + index,
                    ..Default::default()
                },
                &provenance,
            )
            .unwrap();
        }
        mark_dirty_for_evidence(
            &conn,
            &EvidenceRef::Event {
                id: event_id,
                weight: 1.0,
            },
            200,
        )
        .unwrap();
        let project_id = crate::state::insert_project(
            &mut conn,
            &crate::state::NewProject {
                name: "Initial project",
                status: crate::state::ProjectStatus::Active,
                summary: Some("Ready summary"),
                participants_json: None,
                sources_json: None,
                confidence: 0.9,
                now: 300,
            },
            &provenance,
        )
        .unwrap();
        conn.execute(
            "UPDATE projects SET name = 'Ready project', updated_at = 301 WHERE id = ?1",
            [project_id],
        )
        .unwrap();

        reconcile_invalidated(&conn, 100, 400).unwrap();
        reconcile_invalidated(&conn, 100, 401).unwrap();

        let pending: i64 = conn
            .query_row("SELECT count(*) FROM search_document_invalidations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            (search_documents_fts(&conn, "Ready project", 10).unwrap().len(), pending),
            (1, 101),
        );
    }
}
