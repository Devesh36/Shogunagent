//! The Memory API data backend seam (§6.11). The routing/auth/confidence policy lives in
//! [`crate::rest`] and [`crate::dispatch`]; the *data* (search hits, state rows) comes from a
//! [`MemoryBackend`]. Dependency is inverted: this trait is defined here, and the daemon
//! (shogun-core, over its `Db`) implements it — so the API layer never depends on the core.
//!
//! Read items carry their `confidence` so the server applies the single confidence rule
//! ([`crate::memory_api::read_inclusion`], FR-API-06) uniformly — the backend supplies rows, it does
//! not decide inclusion.

use crate::memory_api::Tool;

/// Default result cap used when a search caller omits `limit`.
pub const DEFAULT_SEARCH_LIMIT: usize = 20;
/// Hard boundary cap for returned search hits.
pub const MAX_SEARCH_LIMIT: usize = 50;
/// Maximum Unicode scalar count accepted for one search question.
pub const MAX_SEARCH_QUERY_CHARS: usize = 4_096;

/// Keys shared by Wiki UI/API/CLI. Only the currently stored entity types are accepted.
pub fn parse_knowledge_key(key: &str) -> Option<(&str, i64)> {
    if key.len() > 64 { return None; }
    let (kind, raw_id) = key.split_once('-')?;
    if !matches!(kind, "person" | "project" | "knowledge") || !raw_id.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let id = raw_id.parse::<i64>().ok()?;
    (id > 0).then_some((kind, id))
}

/// Requested semantic reach. Lexical widening is reported separately.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchDepthV1 {
    #[default]
    Warm,
    Auto,
    All,
}

/// Half-open UTC epoch-millisecond bounds: `[from_ms, to_ms)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchTimeRangeV1 {
    pub from_ms: i64,
    pub to_ms: i64,
}

/// One time bound as supplied by a wire face. Invalid values stay present until shared policy
/// validation so a malformed pair cannot silently become an omitted range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchBoundV1 {
    Absent,
    Integer(i64),
    Invalid,
}

/// Depth control as supplied by a wire face. A present non-string must not become an omitted
/// default because all faces reject invalid depth values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchDepthWireV1<'a> {
    Absent,
    String(&'a str),
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySourceFamilyV1 {
    Meeting,
    State,
    Brief,
    Person,
    Project,
    Thread,
    Period,
}

impl MemorySourceFamilyV1 {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Meeting => "meeting",
            Self::State => "state",
            Self::Brief => "brief",
            Self::Person => "person",
            Self::Project => "project",
            Self::Thread => "thread",
            Self::Period => "period",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchSourceWireV1<'a> {
    Absent,
    String(&'a str),
    Invalid,
}

/// Canonical search request shared by every Memory API face.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SearchRequestV1 {
    pub query: String,
    pub limit: usize,
    pub depth: SearchDepthV1,
    pub range: Option<SearchTimeRangeV1>,
    pub source_kind: Option<MemorySourceFamilyV1>,
    /// E2-T5: also return rows in the agents' `agent_scratch:*` namespaces. Off by default: a
    /// search sees the shared world only.
    pub include_scratch: bool,
}

impl SearchRequestV1 {
    /// Apply a caller's `limit` (MCP `limit`, REST `?limit=`, CLI `--limit`): absent keeps the
    /// default, otherwise it is clamped to `1..=MAX_SEARCH_LIMIT`.
    pub fn with_limit(mut self, limit: Option<usize>) -> Self {
        if let Some(limit) = limit {
            self.limit = limit.clamp(1, MAX_SEARCH_LIMIT);
        }
        self
    }
}

impl Default for SearchRequestV1 {
    fn default() -> Self {
        Self {
            query: String::new(),
            limit: DEFAULT_SEARCH_LIMIT,
            depth: SearchDepthV1::Warm,
            range: None,
            source_kind: None,
            include_scratch: false,
        }
    }
}

/// Stable policy rejection classes. No query or result content is carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchPolicyErrorV1 {
    InvalidRange,
    RangeRequired,
    ColdDepthRequiresExplicitAutoOrAll,
    ColdRangeTooBroad,
    InvalidSourceKind,
    QueryTooLong,
}

impl SearchPolicyErrorV1 {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRange => "invalid_search_range",
            Self::RangeRequired => "search_range_required",
            Self::ColdDepthRequiresExplicitAutoOrAll => "cold_depth_requires_explicit_auto_or_all",
            Self::ColdRangeTooBroad => "cold_range_too_broad",
            Self::InvalidSourceKind => "invalid_search_source_kind",
            Self::QueryTooLong => "search_query_too_long",
        }
    }
}

/// Shared resolved request. Policy belongs here so REST/MCP/CLI cannot drift.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedSearchRequestV1 {
    pub limit: usize,
    pub requested_depth: SearchDepthV1,
    pub effective_depth: SearchDepthV1,
    pub requested_range: Option<SearchTimeRangeV1>,
    pub resolved_range: SearchTimeRangeV1,
    pub explicit_deep: bool,
}

/// Resolve bounded search policy against the daemon clock.
pub fn resolve_search_request(
    request: &SearchRequestV1,
    now_ms: i64,
) -> Result<ResolvedSearchRequestV1, SearchPolicyErrorV1> {
    validate_search_query(&request.query)?;
    let warm_cutoff = now_ms
        .saturating_sub(shogun_memory::search::WARM_WINDOW_MS)
        .max(0);
    if let Some(range) = request.range {
        if range.from_ms < 0 || range.from_ms >= range.to_ms || range.to_ms > now_ms {
            return Err(SearchPolicyErrorV1::InvalidRange);
        }
    }
    // Without a range, `auto` is the default lookup (event text over all history, vectors Warm);
    // only `all` would scan Cold vectors, so only it needs bounds.
    if request.depth == SearchDepthV1::All && request.range.is_none() {
        return Err(SearchPolicyErrorV1::RangeRequired);
    }
    if request.depth == SearchDepthV1::Warm
        && request
            .range
            .is_some_and(|range| range.from_ms < warm_cutoff)
    {
        return Err(SearchPolicyErrorV1::ColdDepthRequiresExplicitAutoOrAll);
    }

    let resolved_range = request.range.unwrap_or(SearchTimeRangeV1 {
        from_ms: warm_cutoff,
        to_ms: now_ms,
    });
    let effective_depth = match request.depth {
        SearchDepthV1::Warm => SearchDepthV1::Warm,
        SearchDepthV1::Auto if resolved_range.from_ms >= warm_cutoff => SearchDepthV1::Warm,
        SearchDepthV1::Auto | SearchDepthV1::All => SearchDepthV1::All,
    };
    Ok(ResolvedSearchRequestV1 {
        limit: request.limit.min(MAX_SEARCH_LIMIT),
        requested_depth: request.depth,
        effective_depth,
        requested_range: request.range,
        resolved_range,
        explicit_deep: effective_depth == SearchDepthV1::All,
    })
}

/// Build one request from wire controls. Incomplete bounds are rejected before backend work.
pub fn search_request_from_wire(
    query: String,
    depth: SearchDepthWireV1<'_>,
    from_ms: SearchBoundV1,
    to_ms: SearchBoundV1,
    source_kind: SearchSourceWireV1<'_>,
) -> Result<SearchRequestV1, SearchPolicyErrorV1> {
    validate_search_query(&query)?;
    let depth = match match depth {
        SearchDepthWireV1::Absent => "warm",
        SearchDepthWireV1::String(depth) => depth,
        SearchDepthWireV1::Invalid => return Err(SearchPolicyErrorV1::InvalidRange),
    } {
        "warm" => SearchDepthV1::Warm,
        "auto" => SearchDepthV1::Auto,
        "all" => SearchDepthV1::All,
        _ => return Err(SearchPolicyErrorV1::InvalidRange),
    };
    let range = match (from_ms, to_ms) {
        (SearchBoundV1::Absent, SearchBoundV1::Absent) => None,
        (SearchBoundV1::Integer(from_ms), SearchBoundV1::Integer(to_ms)) => {
            Some(SearchTimeRangeV1 { from_ms, to_ms })
        }
        _ => return Err(SearchPolicyErrorV1::InvalidRange),
    };
    let source_kind = match source_kind {
        SearchSourceWireV1::Absent => None,
        SearchSourceWireV1::String(value) => Some(match value {
            "meeting" => MemorySourceFamilyV1::Meeting,
            "state" => MemorySourceFamilyV1::State,
            "brief" => MemorySourceFamilyV1::Brief,
            "person" => MemorySourceFamilyV1::Person,
            "project" => MemorySourceFamilyV1::Project,
            "thread" => MemorySourceFamilyV1::Thread,
            "period" => MemorySourceFamilyV1::Period,
            _ => return Err(SearchPolicyErrorV1::InvalidSourceKind),
        }),
        SearchSourceWireV1::Invalid => return Err(SearchPolicyErrorV1::InvalidSourceKind),
    };
    Ok(SearchRequestV1 {
        query,
        depth,
        range,
        source_kind,
        ..SearchRequestV1::default()
    })
}

/// Reject an overlong query before parsing, embedding, or transport encoding.
pub fn validate_search_query(query: &str) -> Result<(), SearchPolicyErrorV1> {
    if query.chars().take(MAX_SEARCH_QUERY_CHARS + 1).count() > MAX_SEARCH_QUERY_CHARS {
        Err(SearchPolicyErrorV1::QueryTooLong)
    } else {
        Ok(())
    }
}

/// Search metadata exposed without discarding provenance or confidence.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MemorySearchResponse {
    /// Backward-compatible shared read-envelope tool name.
    pub tool: String,
    pub results: Vec<MemorySearchResult>,
    pub answerability: AnswerabilityV1,
    pub requested_depth: SearchDepthV1,
    pub effective_depth: SearchDepthV1,
    pub requested_range: Option<SearchTimeRangeV1>,
    pub resolved_range: SearchTimeRangeV1,
    pub lexical_scope: String,
    pub complete: bool,
    pub cold_partitions_visited: usize,
    pub cold_vectors_scanned: usize,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerabilityReasonV1 {
    EmptyQuery,
    NoEligibleMatch,
    MissingAnchor,
    InsufficientSignal,
    LowQuality,
    Answerable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct AnswerabilityV1 {
    pub answerable: bool,
    pub reason: AnswerabilityReasonV1,
}

/// Stable, serializable federated result shape for all Memory API faces.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MemorySearchResult {
    /// Backward-compatible result text. Federated identity fields provide richer attribution.
    pub text: String,
    pub source_kind: String,
    pub source_id: String,
    pub provenance: Vec<String>,
    pub title: String,
    pub excerpt: String,
    pub confidence: f64,
    pub possibly: bool,
    pub score: f64,
    /// `{"ms", "local"}`: when the newest event behind the hit was captured (else its period).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when: Option<serde_json::Value>,
    /// `{"app", "window_title"}` of that event; null keys omitted.
    #[serde(rename = "where", skip_serializing_if = "Option::is_none")]
    pub place: Option<serde_json::Value>,
    /// At most 240 chars centred on the first query-term match, `…` at cut ends.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// `{"tool": "memory.get_events", "ids": [...]}` for event-backed hits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open: Option<serde_json::Value>,
}

/// Search failure returned at the Memory API boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySearchError {
    Policy(SearchPolicyErrorV1),
    Unavailable,
}

impl MemorySearchError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Policy(error) => error.as_str(),
            Self::Unavailable => "search_unavailable",
        }
    }
}
use crate::voice_dictionary_api::{VoiceDictionaryOperation, VoiceDictionaryResult};

/// One read result: a display label and its confidence (FR-ST-02). The server filters/flags these.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadItem {
    pub label: String,
    pub confidence: f64,
    /// Stable lookup key when this is an addressable entity; absent on ordinary text hits.
    pub key: Option<String>,
    /// The state row this item is (`table`, `id`), when it is one: what `state get`, `state
    /// edit|delete <table> <id>` and `state split` take (issue #779). Absent on text hits.
    pub row: Option<(&'static str, i64)>,
    /// Commitments and open loops the user snoozed: back on the list at this Unix-ms instant.
    pub deferred_until: Option<i64>,
    /// People and projects read one at a time: where their evidence comes from, most-cited first
    /// (issue #1362). Absent on lists and text hits.
    pub sources: Option<Vec<ReadItemSource>>,
}

/// One source of a read item's evidence: the store's row, re-exported so the render layer and
/// the faces name one type.
pub type ReadItemSource = shogun_memory::state::EvidenceSource;

impl ReadItem {
    pub fn new(label: impl Into<String>, confidence: f64) -> Self {
        Self {
            label: label.into(),
            confidence,
            key: None,
            row: None,
            deferred_until: None,
            sources: None,
        }
    }

    /// Where this person's or project's evidence comes from.
    #[must_use]
    pub fn with_sources(mut self, sources: Vec<ReadItemSource>) -> Self {
        self.sources = Some(sources);
        self
    }

    /// Snoozed until `until` (Unix ms); `None` leaves the item unflagged.
    #[must_use]
    pub fn with_deferred(mut self, until: Option<i64>) -> Self {
        self.deferred_until = until;
        self
    }

    /// This item is row `id` of state table `table`.
    #[must_use]
    pub fn with_row(mut self, table: &'static str, id: i64) -> Self {
        self.row = Some((table, id));
        self
    }
}

/// Whether a host-owned, preassembled current-context cache can be read without collecting again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurrentContextAvailability {
    Fresh,
    Empty,
    Stale,
    Unavailable,
}

/// Read-only snapshot supplied by a host that owns live current context.
#[derive(Debug, Clone, PartialEq)]
pub struct CurrentContextSnapshot {
    pub availability: CurrentContextAvailability,
    pub items: Vec<ReadItem>,
}

impl CurrentContextSnapshot {
    pub fn unavailable() -> Self {
        Self {
            availability: CurrentContextAvailability::Unavailable,
            items: Vec::new(),
        }
    }
}

/// Optional host seam for RAM-only, already-preassembled current context.
pub trait CurrentContextProvider: Send + Sync {
    fn current_context(&self) -> CurrentContextSnapshot;
}

/// The `mail.triage.list` / `mail.history.list` controls (FR-TG-30), as the wire carries them:
/// plain strings the backend parses against its own enums, so an unknown value is refused there
/// with a reason rather than silently dropped by a face. All optional; `Default` is "everything
/// unhandled, not archived, newest first".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailListQuery {
    /// `important` | `other`.
    pub bucket: Option<String>,
    /// `needs_reply` | `waiting_on_them` | `fyi` | `none`.
    pub attention: Option<String>,
    /// A label key (`marketing`, `custom:vendors`, …).
    pub label: Option<String>,
    /// Only rows updated at or after this unix-ms timestamp.
    pub since_ms: Option<i64>,
    /// Row cap (the backend bounds it).
    pub limit: Option<usize>,
    /// Include rows the user already marked handled.
    pub include_handled: bool,
    /// The FR-API-06 opt-in for Low-band rows — the same `include_low` every read honours; the
    /// faces copy it here because structured reads carry their own gate per row.
    pub include_low: bool,
}

/// Parameters a read tool may need: a `get`-by-id, or a `search` query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadParams {
    /// Set by authenticated API faces for Full scope only, never from wire arguments.
    pub include_account_labels: bool,
    /// Explicit opt-in for tentative records on structured review surfaces.
    pub include_low: bool,
    /// The id for a `get` variant (`/state/people/42`).
    pub id: Option<i64>,
    /// The string key for a `get` variant whose identity is not an integer — a mail thread key
    /// (`gmail:<id>`) or a label key (`custom:vendors`).
    pub key: Option<String>,
    /// Mail triage list controls (FR-TG-30).
    pub mail: MailListQuery,
    /// The query for `search` (`?q=...`).
    pub query: Option<String>,
    /// Optional visual-recall time window start (unix ms).
    pub from_ms: Option<i64>,
    /// Optional visual-recall time window end (unix ms).
    pub to_ms: Option<i64>,
    /// Exclusive visual-recall timeline cursor timestamp (unix ms).
    pub cursor_ts: Option<i64>,
    /// Exclusive visual-recall timeline cursor frame id. Must accompany `cursor_ts`.
    pub cursor_id: Option<i64>,
    /// `older` (default) or `newer` from the exclusive visual-recall cursor.
    pub cursor_direction: Option<String>,
    /// Visual-recall page size. Omitted keeps the existing default.
    pub page_size: Option<usize>,
    /// Generation lookup (issue #104): active Global lessons, plus any scope ids below.
    /// Off = management list (Settings / `lessons list`) including sleeping and person rows.
    pub for_generation: bool,
    pub app_bundle_id: Option<String>,
    pub person_id: Option<String>,
    pub project_id: Option<String>,
    /// E2-T5: also return rows in the agents' `agent_scratch:*` namespaces (`memory.get_context_pack`,
    /// `state.*`). Off by default: reads see the shared world only.
    pub include_scratch: bool,
}

/// The result of a write tool (append_note = L1, meeting.record_now = L2). `Some(id)` when a row was
/// written (a note), `None` when the write is accepted but not persisted here (a proposal awaits
/// Notch confirmation).
pub type WriteResult = Result<Option<i64>, String>;

/// The data source behind the Memory API. Implemented by the daemon over its DB.
pub trait MemoryBackend: Send + Sync {
    /// Rows for a read tool, given its parameters. Rows are unfiltered — the server applies the
    /// confidence gate (FR-API-06).
    fn read(&self, tool: Tool, params: &ReadParams) -> Vec<ReadItem>;

    /// Typed federated memory search. This keeps query policy and execution metadata out of the
    /// lossy generic `ReadItem` channel used by older read tools.
    fn search(
        &self,
        _request: &SearchRequestV1,
    ) -> Result<MemorySearchResponse, MemorySearchError> {
        Err(MemorySearchError::Unavailable)
    }

    /// Live context is host-owned. Standalone backends report unavailable instead of pretending a
    /// DB-derived snapshot is current.
    fn current_context(&self) -> CurrentContextSnapshot {
        CurrentContextSnapshot::unavailable()
    }

    /// Perform a write tool with the request `body` (e.g. the note text). The default is a no-op
    /// "accepted" so a read-only backend still compiles.
    fn write(&self, _tool: Tool, _body: &str) -> WriteResult {
        Ok(None)
    }

    /// Structured JSON for tools that do not use [`ReadItem`] / the confidence gate (visual recall).
    /// `None` → fall back to [`Self::read`].
    fn read_structured(&self, _tool: Tool, _params: &ReadParams) -> Option<String> {
        None
    }

    /// Classify a `capture.set_paused` body against the pause in force (issue #897), without
    /// writing anything. `Ok(Some(pause))`: the body is `{"paused": false}` and capture is paused
    /// (`pause` is that pause), so the face queues it for the user's in-app approval instead; the
    /// request carries `pause`, and confirming ends only that one. `Ok(None)`: anything else,
    /// which the face writes directly. `Err`: invalid or unavailable; the face then lets
    /// [`Self::write`] answer, and the write refuses on its own anything that would start
    /// capture again sooner, so an unclassifiable request can never resume capture.
    fn capture_unpause_requested(
        &self,
        _body: &str,
    ) -> Result<Option<shogun_agents::permission::PausedCapture>, String> {
        Err("capture control unavailable".to_string())
    }

    /// Classify a `capture_exclusions.set` body against the exclusion lists in force (issue #657),
    /// without writing anything. `Ok(None)`: the request only adds exclusions (or changes
    /// nothing), so the face writes it directly. `Ok(Some(change))`: it would stop excluding
    /// something, so the face queues it for the user's in-app approval instead. `Err`: the body is
    /// invalid or the lists are unavailable; the face then lets [`Self::write`] answer, and the
    /// write refuses any removal on its own, so an unclassifiable request can never widen capture.
    fn capture_exclusions_change(
        &self,
        _body: &str,
    ) -> Result<Option<shogun_agents::permission::CaptureExclusionChange>, String> {
        Err("capture exclusions unavailable".to_string())
    }

    /// Classify an `agent.source.set` body against the Agent lane settings in force (issue #110),
    /// without writing anything. `Ok(None)`: it only narrows egress (an API-key provider, a model
    /// change, withdrawing the subscription opt-in), so the face writes it directly.
    /// `Ok(Some(request))`: it selects a subscription the user has not opted into, so the face
    /// queues it for the user's in-app approval instead. `Err`: invalid or unavailable; the face
    /// then lets [`Self::write`] answer, and the write refuses any subscription selection on its
    /// own, so an unclassifiable request can never grant the opt-in.
    fn agent_source_change(&self, _body: &str) -> Result<Option<AgentSubscriptionRequest>, String> {
        Err("agent source unavailable".to_string())
    }

    /// Classify a `visual_recall.set_enabled` / `visual_recall.set_retention` body against the
    /// Visual recall settings in force (issue #782), without writing anything. `Ok(None)`: it
    /// keeps no more of the screen (turning it off, an already-on enable, an equal or shorter
    /// retention), so the face writes it directly. `Ok(Some(change))`: it turns Visual recall on
    /// or keeps saved screens longer, so the face queues it for the user's in-app approval
    /// instead. `Err`: invalid or unavailable; the face then lets [`Self::write`] answer, and the
    /// write refuses any widening on its own, so an unclassifiable request can never widen it.
    fn visual_recall_change(
        &self,
        _tool: Tool,
        _body: &str,
    ) -> Result<Option<shogun_agents::permission::VisualRecallWidening>, String> {
        Err("visual recall unavailable".to_string())
    }

    /// Classify a `mail.triage.archive` / `mail.triage.unarchive` body (issue #779) against the
    /// thread as it is now, without writing anything. `Ok(None)`: the backend's direct write may
    /// record it — the thread's label carries the user's standing `auto_archive` approval
    /// (FR-TG-21), or the request changes nothing on Gmail (already requested, or withdrawing an
    /// archive that has not run). `Ok(Some(request))`: it would change Gmail without any
    /// approval from the user, so the face queues it for the user's one-tap confirm instead.
    /// `Err`: invalid, unknown thread or unavailable; the face then lets [`Self::write`] answer
    /// with its own reason, and the write refuses anything that needs approval on its own.
    fn mail_archive_change(
        &self,
        _tool: Tool,
        _body: &str,
    ) -> Result<Option<crate::mail_archive_api::MailArchiveRequest>, String> {
        Err("mail triage unavailable".to_string())
    }

    /// Private vocabulary uses a typed contract, never the generic text write channel.
    fn manage_voice_dictionary(
        &self,
        _operation: VoiceDictionaryOperation,
    ) -> Result<VoiceDictionaryResult, String> {
        Err("voice dictionary unavailable".to_string())
    }

    /// Memory import (issues #384 / #391): preview / commit / list / delete. `Ok` is the JSON a
    /// face renders — a caller mistake is an `error` payload with a fixed code; `Err` is a store
    /// fault. Local only.
    fn memory_import(
        &self,
        _operation: crate::memory_import_api::MemoryImportOperation,
    ) -> Result<String, String> {
        Err("memory import unavailable".to_string())
    }

    /// Live desktop form session. API apply asks for human review and never confirms a fill.
    fn forms(
        &self,
        _operation: crate::forms_api::FormOperation,
        _auth: crate::forms_api::FormsAuth,
    ) -> Result<String, String> {
        Ok(crate::forms_api::refusal_json("form_unavailable"))
    }

    /// L2 Clarify (issue #394): list the questions agent runs are waiting on, read one, or answer
    /// one. `Ok` is the JSON a face renders — a refusal is an `error` payload with a fixed code
    /// (`unknown_question`, `already_answered`, `expired`, …); `Err` is a store fault. Never
    /// touches the L3 approval queue.
    fn clarify(&self, _operation: crate::clarify_api::ClarifyOperation) -> Result<String, String> {
        Err("clarify unavailable".to_string())
    }

    /// Mid-run steering (issue #395): add one more instruction to a run that is still going.
    /// `Ok` is the JSON a face renders — an acceptance (`delivery: next_turn`) or an `error`
    /// payload with a fixed code (`unknown_run`, `run_finished`, …); `Err` is a store fault.
    /// Never touches the L3 approval queue, and the note is never echoed.
    /// Translation and reply drafts (issue #279): queue a request for the desktop and wait for
    /// its answer, or read one the face stopped waiting for. `Ok` is the JSON a face renders — a
    /// result, a `pending` hand-back with a job id, or an `error` payload with a fixed code
    /// (`crate::translate_api`); `Err` is a store fault. The default answers `unavailable`, so a
    /// backend without the queue never pretends to translate.
    /// `memory.search` with `mode: "ai"` (issue #940 B2b): queue the question for the desktop,
    /// which runs the AI step on the user's own model access, and wait for its answer. `ai` is
    /// the face's decision ([`crate::ai_search_api::ai_gate`]): `Err(reason)` answers with keyword
    /// results labelled with that reason, without queueing. `Ok` is the JSON a face renders (an
    /// answer, or an `error` payload from [`crate::ai_search_api::refusal_json`]); `Err` is a
    /// store fault. The default answers `search_unavailable`.
    fn ai_search(
        &self,
        _query: &str,
        _origin: &'static str,
        _ai: Result<(), &'static str>,
    ) -> Result<String, String> {
        Ok(crate::ai_search_api::refusal_json("search_unavailable"))
    }

    fn translate(&self, _operation: crate::translate_api::TranslateOperation) -> Result<String, String> {
        Ok(crate::translate_api::refusal_json("unavailable", None))
    }

    fn steer(&self, _operation: crate::steer_api::SteerOperation) -> Result<String, String> {
        Err("steering unavailable".to_string())
    }

    /// `memory.get_events` / `memory.read_session`: `Ok` is the flat result JSON (without the
    /// `tool` key, which the face adds); `Err` is a store fault.
    fn activity(&self, _request: &crate::activity_api::ActivityRequest) -> Result<String, String> {
        Err("memory_unavailable".to_string())
    }

    /// `memory.ingest` (E4-T3): append the items to the event log through the sanitize / redact /
    /// dedup append path under the request's idempotency key, replaying a repeated key. `Err` is
    /// a store fault (the face answers `ingest_unavailable`).
    fn ingest(&self, _request: &crate::ingest_api::IngestRequest) -> Result<crate::ingest_api::IngestOutcome, String> {
        Err("ingest_unavailable".to_string())
    }

    /// `memory.recall` (E4-T3): the context-pack assembly cut to the request's token budget.
    fn recall(&self, _request: &crate::ingest_api::RecallRequest) -> Result<crate::ingest_api::RecallOutcome, String> {
        Err("recall_unavailable".to_string())
    }

    /// `memory.fs` (E6-T2): one memory-tool command over `/memories`. `Err` is a store fault (the
    /// face answers `memory_fs_unavailable`); a contract error (missing path, duplicate
    /// `old_str`, …) is an `Ok` reply with `is_error`.
    fn memory_fs(&self, _command: &crate::memfs_tool::FsCommand) -> Result<crate::memfs_tool::FsReply, String> {
        Err("memory_fs_unavailable".to_string())
    }

    /// `state.export` in export format v1 (E7): the JSON Lines export, full, from a time, or
    /// differential from the stored cursor (which it then advances). `Err` is a store fault.
    fn export_v1(&self, _request: &crate::export_api::ExportRequest) -> Result<crate::export_api::ExportOutput, String> {
        Err("export_unavailable".to_string())
    }

    /// Append one audit record (E18) to this principal's store. A backend without a store has
    /// nothing to record into and answers `Ok`; a store fault is `Err` (the write it describes
    /// already happened, so the face reports it and carries on).
    fn audit_append(&self, _record: &shogun_memory::audit::AuditRecord) -> Result<(), String> {
        Ok(())
    }

    /// `audit.list` (E18): the store's audit records, oldest first.
    fn audit_list(&self, _query: &crate::governance_api::AuditQuery) -> Result<serde_json::Value, String> {
        Err("audit_unavailable".to_string())
    }

    /// `acl.list`, `acl.grant`, `acl.revoke` (E18) on this principal's store, `by` recording who
    /// granted. The faces decide who may change grants; this only applies the change.
    fn acl(&self, _operation: &crate::governance_api::AclOperation, _by: &str) -> Result<serde_json::Value, String> {
        Err("acl_unavailable".to_string())
    }

    /// The principal's durable approval ledger (V103): spent decision nonces and decisions. A
    /// backend without a store cannot remember a decision across a restart, so it answers `Err`
    /// and a decision is refused (fail closed) rather than made without a durable record.
    fn approval_ledger(&self, _op: crate::remote_approvals::LedgerOp<'_>) -> Result<crate::remote_approvals::LedgerReply, String> {
        Err("approval_ledger_unavailable".to_string())
    }

    /// Team (E18): whether `grantee_principal` holds a `world_read` grant on this store.
    fn principal_may_read(&self, _grantee_principal: &str) -> bool {
        false
    }
}

/// A shared backend is a backend (E4-T1): the HTTP transport's `McpServer` and the REST state
/// hold the same `Arc<dyn MemoryBackend>`, so one store answers both faces. Every method
/// forwards, defaults included, so the shared backend behaves exactly like the owned one.
impl<T: MemoryBackend + ?Sized> MemoryBackend for std::sync::Arc<T> {
    fn read(&self, tool: Tool, params: &ReadParams) -> Vec<ReadItem> {
        (**self).read(tool, params)
    }
    fn search(&self, request: &SearchRequestV1) -> Result<MemorySearchResponse, MemorySearchError> {
        (**self).search(request)
    }
    fn current_context(&self) -> CurrentContextSnapshot {
        (**self).current_context()
    }
    fn write(&self, tool: Tool, body: &str) -> WriteResult {
        (**self).write(tool, body)
    }
    fn read_structured(&self, tool: Tool, params: &ReadParams) -> Option<String> {
        (**self).read_structured(tool, params)
    }
    fn capture_unpause_requested(&self, body: &str) -> Result<Option<shogun_agents::permission::PausedCapture>, String> {
        (**self).capture_unpause_requested(body)
    }
    fn capture_exclusions_change(&self, body: &str) -> Result<Option<shogun_agents::permission::CaptureExclusionChange>, String> {
        (**self).capture_exclusions_change(body)
    }
    fn agent_source_change(&self, body: &str) -> Result<Option<AgentSubscriptionRequest>, String> {
        (**self).agent_source_change(body)
    }
    fn visual_recall_change(&self, tool: Tool, body: &str) -> Result<Option<shogun_agents::permission::VisualRecallWidening>, String> {
        (**self).visual_recall_change(tool, body)
    }
    fn mail_archive_change(&self, tool: Tool, body: &str) -> Result<Option<crate::mail_archive_api::MailArchiveRequest>, String> {
        (**self).mail_archive_change(tool, body)
    }
    fn manage_voice_dictionary(&self, operation: VoiceDictionaryOperation) -> Result<VoiceDictionaryResult, String> {
        (**self).manage_voice_dictionary(operation)
    }
    fn memory_import(&self, operation: crate::memory_import_api::MemoryImportOperation) -> Result<String, String> {
        (**self).memory_import(operation)
    }
    fn forms(&self, operation: crate::forms_api::FormOperation, auth: crate::forms_api::FormsAuth) -> Result<String, String> {
        (**self).forms(operation, auth)
    }
    fn clarify(&self, operation: crate::clarify_api::ClarifyOperation) -> Result<String, String> {
        (**self).clarify(operation)
    }
    fn ai_search(&self, query: &str, origin: &'static str, ai: Result<(), &'static str>) -> Result<String, String> {
        (**self).ai_search(query, origin, ai)
    }
    fn translate(&self, operation: crate::translate_api::TranslateOperation) -> Result<String, String> {
        (**self).translate(operation)
    }
    fn steer(&self, operation: crate::steer_api::SteerOperation) -> Result<String, String> {
        (**self).steer(operation)
    }
    fn activity(&self, request: &crate::activity_api::ActivityRequest) -> Result<String, String> {
        (**self).activity(request)
    }
    fn ingest(&self, request: &crate::ingest_api::IngestRequest) -> Result<crate::ingest_api::IngestOutcome, String> {
        (**self).ingest(request)
    }
    fn recall(&self, request: &crate::ingest_api::RecallRequest) -> Result<crate::ingest_api::RecallOutcome, String> {
        (**self).recall(request)
    }
    fn memory_fs(&self, command: &crate::memfs_tool::FsCommand) -> Result<crate::memfs_tool::FsReply, String> {
        (**self).memory_fs(command)
    }
    fn export_v1(&self, request: &crate::export_api::ExportRequest) -> Result<crate::export_api::ExportOutput, String> {
        (**self).export_v1(request)
    }
    fn audit_append(&self, record: &shogun_memory::audit::AuditRecord) -> Result<(), String> {
        (**self).audit_append(record)
    }
    fn audit_list(&self, query: &crate::governance_api::AuditQuery) -> Result<serde_json::Value, String> {
        (**self).audit_list(query)
    }
    fn acl(&self, operation: &crate::governance_api::AclOperation, by: &str) -> Result<serde_json::Value, String> {
        (**self).acl(operation, by)
    }
    fn approval_ledger(&self, op: crate::remote_approvals::LedgerOp<'_>) -> Result<crate::remote_approvals::LedgerReply, String> {
        (**self).approval_ledger(op)
    }
    fn principal_may_read(&self, grantee_principal: &str) -> bool {
        (**self).principal_may_read(grantee_principal)
    }
}

/// An `agent.source.set` that selects a subscription CLI (issue #110), as the backend classified
/// it: the delegate's stable id plus the product and plan names its approval card shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSubscriptionRequest {
    pub delegate: String,
    pub label: String,
    pub plan: String,
}

/// A backend that returns nothing — the server is runnable/testable before the DB is wired.
pub struct StubBackend;

impl MemoryBackend for StubBackend {
    fn read(&self, _tool: Tool, _params: &ReadParams) -> Vec<ReadItem> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knowledge_keys_accept_only_positive_stored_entity_ids() {
        assert_eq!(parse_knowledge_key("person-42"), Some(("person", 42)));
        assert_eq!(parse_knowledge_key("project-7"), Some(("project", 7)));
        for key in ["person-0", "person--1", "person-+1", "topic-1", "person-", "project-1/sql", "person-999999999999999999999999"] {
            assert!(parse_knowledge_key(key).is_none());
        }
    }

    #[test]
    fn stub_returns_empty() {
        assert!(StubBackend
            .read(Tool::StatePeopleList, &ReadParams::default())
            .is_empty());
    }

    #[test]
    fn search_controls_share_complete_half_open_range_policy() {
        let request = search_request_from_wire(
            "q".to_string(),
            SearchDepthWireV1::String("auto"),
            SearchBoundV1::Integer(10),
            SearchBoundV1::Integer(20),
            SearchSourceWireV1::Absent,
        )
        .unwrap();
        let resolved = resolve_search_request(&request, 3_000_000_000).unwrap();
        assert_eq!(resolved.effective_depth, SearchDepthV1::All);
        assert_eq!(
            resolved.resolved_range,
            SearchTimeRangeV1 {
                from_ms: 10,
                to_ms: 20
            }
        );
        assert_eq!(
            search_request_from_wire(
                "q".to_string(),
                SearchDepthWireV1::String("all"),
                SearchBoundV1::Integer(10),
                SearchBoundV1::Absent,
                SearchSourceWireV1::Absent,
            ),
            Err(SearchPolicyErrorV1::InvalidRange)
        );
    }

    #[test]
    fn auto_without_a_range_is_the_default_lookup() {
        let request = SearchRequestV1 { depth: SearchDepthV1::Auto, ..SearchRequestV1::default() };
        let resolved = resolve_search_request(&request, 100).unwrap();
        assert_eq!(resolved.effective_depth, SearchDepthV1::Warm);
        assert!(resolved.requested_range.is_none() && !resolved.explicit_deep);
        let all = SearchRequestV1 { depth: SearchDepthV1::All, ..SearchRequestV1::default() };
        assert_eq!(resolve_search_request(&all, 100).unwrap_err(), SearchPolicyErrorV1::RangeRequired);
    }

    #[test]
    fn default_search_stays_bounded_without_cold_policy() {
        let resolved = resolve_search_request(&SearchRequestV1::default(), 100).unwrap();
        assert_eq!(resolved.requested_depth, SearchDepthV1::Warm);
        assert_eq!(resolved.effective_depth, SearchDepthV1::Warm);
        assert!(!resolved.explicit_deep);
    }

    #[test]
    fn source_kind_wire_control_is_optional_and_rejects_exact_invalid_value() {
        let omitted = search_request_from_wire(
            "q".to_string(),
            SearchDepthWireV1::Absent,
            SearchBoundV1::Absent,
            SearchBoundV1::Absent,
            SearchSourceWireV1::Absent,
        )
        .unwrap();
        assert_eq!(omitted.source_kind, None);

        let invalid = search_request_from_wire(
            "q".to_string(),
            SearchDepthWireV1::Absent,
            SearchBoundV1::Absent,
            SearchBoundV1::Absent,
            SearchSourceWireV1::String("Brief"),
        );
        assert_eq!(invalid, Err(SearchPolicyErrorV1::InvalidSourceKind));
    }

    #[test]
    fn overlong_unicode_search_query_is_rejected_stably() {
        let error = search_request_from_wire(
            "界".repeat(4_097),
            SearchDepthWireV1::Absent,
            SearchBoundV1::Absent,
            SearchBoundV1::Absent,
            SearchSourceWireV1::Absent,
        )
        .unwrap_err();

        assert_eq!(error.as_str(), "search_query_too_long");
    }

    #[test]
    fn old_json_request_omission_and_unknown_fields_remain_compatible() {
        let request: SearchRequestV1 =
            serde_json::from_str(r#"{"query":"q","limit":4,"unknown_future_field":true}"#).unwrap();

        assert_eq!(request.source_kind, None);
    }
}
