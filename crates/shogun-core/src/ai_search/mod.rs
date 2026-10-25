//! AI memory search: question expansion plus evidence rerank over the local keyword index
//! (issue #940: B1 core logic and evaluation; B2 wires the desktop Full UI and, through the
//! desktop, `memory.search` `mode: "ai"` on MCP / REST / CLI — `cancel` holds the
//! deadline / newer-search stop it uses).
//!
//! Owner decision 2026-10-05: no embeddings (the #237 decision stands). "Meaning-aware" retrieval
//! is two Agent-lane calls on the user's own credentials around the existing FTS5 trigram search:
//!
//! 1. [`keyword_phase`] — the plan gate, then the ordinary keyword search for the question. Its
//!    hits are what a surface shows immediately.
//! 2. [`refine`] — the model rewrites the question into a bounded set of FTS queries (synonyms,
//!    paraphrases, the other of English/Japanese, a resolved date range); each runs through the
//!    same keyword search; the union is deduplicated and fused; the top
//!    [`AiSearchBounds::max_candidates`] excerpts go back to the model, which returns the ids that
//!    answer the question, best first, plus short answer paragraphs citing those ids.
//!    Citations are only drawn from sent candidates and must resolve to returned hits.
//!
//! Every failure degrades to keyword results with a labelled reason ([`AiSearchStatus`]); a
//! missing credential is its own state ([`AiSearchStatus::NeedsProvider`]) so a surface can show
//! setup instead of an error. A question that is only a link, identifier or quoted phrase skips
//! the model ([`FallbackReason::ExactQuery`]), and in a mixed question the hits that contain such
//! an exact token stay first in keyword order whatever the rerank says.
//!
//! Boundaries (CLAUDE.md invariants 3 and 5):
//! - the model is reached only through [`SearchModel`]; production wraps the Agent-lane
//!   [`AgentClient`] (BYOK or subscription delegation), whose clients write one content-free
//!   traceability row per request ([`TRACE_PURPOSE`]). The Select KK key cannot reach this path:
//!   no Batch type appears in it.
//! - only the question and the bounded candidate excerpts leave the device, redacted for egress;
//!   a hit the searcher marks `cloud_eligible = false` (an exclusion added after capture) is shown
//!   locally and never sent.
//! - nothing here logs user text.
//!
//! Pure: no database, no network, no clock. The daemon adapter (`daemon::ai_search`, `db`
//! feature) supplies the [`KeywordSearcher`] over the real search pipeline and the plan gate.

mod cancel;
pub mod dates;
pub mod eval;
mod exact;
mod prompt;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::llm::{AgentClient, LlmError};

pub use cancel::{CancellableModel, CancellableSearchModel, LatestSearch, SurfaceSearches, CANCELLED};
pub use exact::{contains_exact, exact_tokens, is_exact_only_query, ExactToken};
pub use prompt::{
    candidate_id, expansion_system_prompt, expansion_user_prompt, parse_expansion, parse_rerank,
    rerank_system_prompt, rerank_user_prompt, sanitize_line, sanitize_question,
    summary_system_prompt, Expansion, ParseError, RerankCandidate, RerankOrder,
};

/// The traceability `purpose` an AI-search request is recorded under (the client's
/// `with_purpose`). One row per request, digest and length only.
pub const TRACE_PURPOSE: &str = "memory_search";

/// Reciprocal-rank-fusion constant, the same one the memory crate fuses its lanes with.
const RRF_K: f64 = 60.0;

/// Every bound on the work and the egress of one AI search. The defaults are the product's.
#[derive(Debug, Clone, PartialEq)]
pub struct AiSearchBounds {
    /// Question characters sent to the model (the keyword phase uses the whole question).
    pub max_question_chars: usize,
    /// Extra FTS queries taken from the expansion.
    pub max_expanded_queries: usize,
    /// Characters per expanded query.
    pub max_query_chars: usize,
    /// Hits read from the keyword phase (the original question).
    pub keyword_limit: usize,
    /// Hits read per expanded query.
    pub per_query_limit: usize,
    /// Excerpts sent to the rerank step.
    pub max_candidates: usize,
    /// Characters per excerpt in the rerank prompt.
    pub max_snippet_chars: usize,
    /// Characters per title in the rerank prompt.
    pub max_title_chars: usize,
    /// Characters of the whole candidate block in the rerank prompt.
    pub max_rerank_input_chars: usize,
    /// Characters of a model answer the parsers will look at.
    pub max_model_output_chars: usize,
    /// Results returned.
    pub final_limit: usize,
    /// Wall-clock budget for the whole refine step; later answers are discarded.
    pub timeout: Duration,
    /// Whether the rerank also writes answer paragraphs. The desktop turns it off and streams
    /// its answer as a third call ([`summary_request`]); the Memory API keeps it.
    pub rerank_answer: bool,
}

impl Default for AiSearchBounds {
    fn default() -> Self {
        Self {
            max_question_chars: 500,
            max_expanded_queries: 6,
            max_query_chars: 120,
            keyword_limit: 20,
            per_query_limit: 20,
            max_candidates: 30,
            max_snippet_chars: 280,
            max_title_chars: 80,
            max_rerank_input_chars: 12_000,
            max_model_output_chars: 8_000,
            final_limit: 20,
            timeout: Duration::from_secs(20),
            rerank_answer: true,
        }
    }
}

/// "Now", as the prompts and the date parser need it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchClock {
    pub now_ms: i64,
    /// The local UTC offset in force at `now_ms`, in minutes east of UTC.
    pub utc_offset_minutes: i32,
}

/// A half-open `[from_ms, to_ms)` window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct TimeRange {
    pub from_ms: i64,
    pub to_ms: i64,
}

/// One keyword-search result as the AI pipeline sees it. `hit` carries the searcher's own
/// provenance-preserving record (the daemon's `FederatedHit`) through untouched.
#[derive(Debug, Clone, PartialEq)]
pub struct KeywordHit<H> {
    /// Stable identity used to deduplicate across queries (`event:12`, `projects:3`).
    pub key: String,
    pub title: String,
    /// The retained evidence excerpt.
    pub text: String,
    pub date_ms: Option<i64>,
    /// False when the hit may be shown locally but must never be sent to a provider.
    pub cloud_eligible: bool,
    /// Suggestion-only or unresolved evidence cannot support a generated answer.
    pub answer_eligible: bool,
    /// The retained claim is below the existing High-confidence band.
    pub tentative: bool,
    pub hit: H,
}

/// The keyword search the AI step widens. Implemented over the daemon's search pipeline; tests
/// use an in-memory corpus.
pub trait KeywordSearcher {
    type Hit: Clone;
    /// Up to `limit` hits for `query`, optionally inside `range`. The error is a content-free code.
    fn search(
        &self,
        query: &str,
        range: Option<TimeRange>,
        limit: usize,
    ) -> Result<Vec<KeywordHit<Self::Hit>>, &'static str>;
}

/// Which of the two model calls is being made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelStep {
    Expand,
    Rerank,
}

/// The one seam to the model. `system` is ours; `user` is the untrusted half. `deadline` is when
/// the whole search stops waiting: an implementation that can cancel should stop then, and an
/// answer that arrives later is discarded either way.
pub trait SearchModel {
    fn complete(
        &self,
        step: ModelStep,
        system: &str,
        user: &str,
        deadline: Instant,
    ) -> Result<String, LlmError>;
}

/// The production [`SearchModel`]: Agent-lane clients (BYOK or subscription delegation), one per
/// step so usage rows carry the step's prompt tag (`MEMORY_SEARCH_EXPAND` /
/// `MEMORY_SEARCH_RERANK`). Each client records its own traceability row per request.
///
/// [`AgentClient`] has no cancellation, so the deadline is enforced by discarding a late answer;
/// a surface that holds a cancellable client (the desktop's `complete_split_cancellable`) should
/// implement [`SearchModel`] itself and stop at `deadline`.
pub struct AgentSearchModel<A> {
    pub expand: A,
    pub rerank: A,
}

impl<A: AgentClient> SearchModel for AgentSearchModel<A> {
    fn complete(
        &self,
        step: ModelStep,
        system: &str,
        user: &str,
        _deadline: Instant,
    ) -> Result<String, LlmError> {
        match step {
            ModelStep::Expand => self.expand.complete_split(system, user),
            ModelStep::Rerank => self.rerank.complete_split(system, user),
        }
    }
}

/// Why a search came back as keyword results. Stable snake_case codes; none carries content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReason {
    /// Only a link, identifier or quoted phrase: keyword search is the right tool.
    ExactQuery,
    /// The plan allows memory search but not Agent-lane execution.
    AiNotInPlan,
    /// No answer within [`AiSearchBounds::timeout`].
    ProviderTimeout,
    /// The provider rejected the user's credential (HTTP 401/403).
    CredentialRejected,
    /// The delegate's plan quota is used up.
    QuotaExhausted,
    /// HTTP 429.
    RateLimited,
    /// The delegate CLI is too old for SHOGUN's flags.
    ProviderNeedsUpdate,
    /// The provider stopped the request under its safety policy.
    ProviderPolicyStop,
    /// Any other provider or transport failure.
    ProviderError,
    /// The expansion answer was not the contracted JSON.
    MalformedExpansion,
    /// The rerank answer was not the contracted JSON.
    MalformedRerank,
    /// A Memory API request: the SHOGUN app, which holds the user's model access, is not running
    /// (nothing claimed the request).
    AppNotRunning,
    /// A Memory API request from a read-only token: AI search spends the user's model access, which
    /// only a token that may write may do (the rule `translate.text` follows).
    ReadOnlyToken,
    /// A Memory API request from a connection without meeting notes and screen recall (owner
    /// decision 2026-10-20): the desktop's AI step reads the whole memory, so it does not run;
    /// the keyword results leave both out.
    SourcesWithheld,
}

impl FallbackReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExactQuery => "exact_query",
            Self::AiNotInPlan => "ai_not_in_plan",
            Self::ProviderTimeout => "provider_timeout",
            Self::CredentialRejected => "credential_rejected",
            Self::QuotaExhausted => "quota_exhausted",
            Self::RateLimited => "rate_limited",
            Self::ProviderNeedsUpdate => "provider_needs_update",
            Self::ProviderPolicyStop => "provider_policy_stop",
            Self::ProviderError => "provider_error",
            Self::MalformedExpansion => "malformed_expansion",
            Self::MalformedRerank => "malformed_rerank",
            Self::AppNotRunning => "app_not_running",
            Self::ReadOnlyToken => "read_only_token",
            Self::SourcesWithheld => "sources_withheld",
        }
    }
}

/// How the returned order was produced. Serialized as
/// `{"status":"ai_reranked"}`, `{"status":"keyword_fallback","reason":"provider_timeout"}` or
/// `{"status":"needs_provider"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "status", content = "reason", rename_all = "snake_case")]
pub enum AiSearchStatus {
    /// Expansion and rerank both ran and their answers were used.
    AiReranked,
    /// Keyword results (possibly widened by a successful expansion); the reason says why.
    KeywordFallback(FallbackReason),
    /// No AI provider is configured. Keyword results are attached; the surface should offer
    /// provider setup (error code [`NEEDS_PROVIDER`]).
    NeedsProvider,
}

/// The actionable code for [`AiSearchStatus::NeedsProvider`].
pub const NEEDS_PROVIDER: &str = "needs_provider";

impl AiSearchStatus {
    /// `ai_reranked`, `needs_provider`, or the fallback reason code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::AiReranked => "ai_reranked",
            Self::KeywordFallback(reason) => reason.as_str(),
            Self::NeedsProvider => NEEDS_PROVIDER,
        }
    }
}

/// Search refused before any work. Content-free codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiSearchError {
    /// The plan gate refused (`plan_required` after the trial ends). Nothing was searched and no
    /// provider was called.
    Locked(&'static str),
    /// The question is empty.
    EmptyQuery,
    /// The keyword phase itself failed (a store fault); there is nothing to fall back to.
    Search(&'static str),
}

impl AiSearchError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Locked(code) | Self::Search(code) => code,
            Self::EmptyQuery => "empty_query",
        }
    }
}

/// One result in the final order.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedHit<H> {
    pub hit: KeywordHit<H>,
    /// Contains a link, identifier or quoted phrase from the question (pinned first).
    pub exact_match: bool,
    /// 1-based position in the model's ranking, when it judged this hit relevant.
    pub ai_rank: Option<usize>,
    /// 1-based position in the original keyword results, when the question alone found it.
    pub keyword_rank: Option<usize>,
    /// How many of the run's queries (original included) retrieved it.
    pub matched_queries: usize,
    /// Whether its excerpt was part of the rerank request.
    pub sent_to_model: bool,
}

/// A generated paragraph and the stable keys of its supporting returned memories.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MemoryAnswerPart {
    pub text: String,
    pub sources: Vec<String>,
}

/// The outcome of a search.
#[derive(Debug, Clone, PartialEq)]
pub struct AiSearchResponse<H> {
    pub status: AiSearchStatus,
    pub hits: Vec<RankedHit<H>>,
    pub answer: Vec<MemoryAnswerPart>,
    /// The expansion's queries, when it succeeded (run locally only).
    pub expanded_queries: Vec<String>,
    /// The expansion's resolved date range, when it gave one.
    pub date_range: Option<TimeRange>,
    /// Excerpts included in the rerank request.
    pub candidates_sent: usize,
    /// Rerank ids discarded as unknown or duplicate.
    pub dropped_ids: usize,
    /// Model requests made (each one traceability row).
    pub model_calls: usize,
    pub elapsed_ms: u64,
}

/// The keyword results for a question, produced only after the plan gate passed. [`refine`]
/// takes this, so the AI step cannot run on a locked plan.
#[derive(Debug, Clone)]
pub struct KeywordPhase<H> {
    question: String,
    hits: Vec<KeywordHit<H>>,
}

impl<H: Clone> KeywordPhase<H> {
    pub fn question(&self) -> &str {
        &self.question
    }

    /// The keyword hits in keyword order — what a surface shows before the rerank arrives.
    pub fn hits(&self) -> &[KeywordHit<H>] {
        &self.hits
    }

    /// Finish without the model: the keyword hits under `status`.
    pub fn into_response(self, status: AiSearchStatus, bounds: &AiSearchBounds) -> AiSearchResponse<H> {
        let fused = fuse(vec![self.hits]);
        finish(status, fused, None, &[], Run::default(), Instant::now(), bounds)
    }
}

/// Step 1: the plan gate, then keyword search for the question.
///
/// `access` is the caller's plan decision (`Entitlements::require_memory_access`). It is checked
/// before the store is touched, so a locked plan gets neither results nor a provider call.
pub fn keyword_phase<S: KeywordSearcher>(
    access: Result<(), &'static str>,
    searcher: &S,
    question: &str,
    bounds: &AiSearchBounds,
) -> Result<KeywordPhase<S::Hit>, AiSearchError> {
    access.map_err(AiSearchError::Locked)?;
    if question.trim().is_empty() {
        return Err(AiSearchError::EmptyQuery);
    }
    let hits = searcher
        .search(question, None, bounds.keyword_limit)
        .map_err(AiSearchError::Search)?;
    Ok(KeywordPhase {
        question: question.to_string(),
        hits,
    })
}

/// Map a provider error to the state the user sees. A missing credential is not a failure of AI
/// search but a setup step, so it gets its own status.
fn provider_status(error: &LlmError) -> AiSearchStatus {
    let reason = match error {
        LlmError::NotConfigured => return AiSearchStatus::NeedsProvider,
        LlmError::Unauthorized(..) => FallbackReason::CredentialRejected,
        LlmError::QuotaExhausted(_) => FallbackReason::QuotaExhausted,
        LlmError::RateLimited(..) => FallbackReason::RateLimited,
        LlmError::NeedsUpdate(_) => FallbackReason::ProviderNeedsUpdate,
        LlmError::PolicyStopped => FallbackReason::ProviderPolicyStop,
        LlmError::Provider(_)
        | LlmError::InvalidRequest
        | LlmError::Transport(_)
        | LlmError::Parse(_)
        | LlmError::TraceUnrecorded => FallbackReason::ProviderError,
    };
    AiSearchStatus::KeywordFallback(reason)
}

#[derive(Default)]
struct Run {
    answer: Vec<MemoryAnswerPart>,
    expanded_queries: Vec<String>,
    date_range: Option<TimeRange>,
    candidates_sent: usize,
    dropped_ids: usize,
    model_calls: usize,
}

/// A deduplicated hit with its fusion bookkeeping.
struct Fused<H> {
    hit: KeywordHit<H>,
    score: f64,
    keyword_rank: Option<usize>,
    matched_queries: usize,
    first_seen: usize,
}

/// Reciprocal-rank fusion of the per-query lists, deduplicated by key. The first list is the
/// original question's; its ranks are kept as `keyword_rank`. The first-seen copy of a hit is
/// kept (the original question's excerpt when it found it).
fn fuse<H: Clone>(lists: Vec<Vec<KeywordHit<H>>>) -> Vec<Fused<H>> {
    let mut by_key: HashMap<String, usize> = HashMap::new();
    let mut fused: Vec<Fused<H>> = Vec::new();
    for (list_index, list) in lists.into_iter().enumerate() {
        for (rank, hit) in list.into_iter().enumerate() {
            let contribution = 1.0 / (RRF_K + rank as f64 + 1.0);
            match by_key.get(&hit.key) {
                Some(&at) => {
                    let entry = &mut fused[at];
                    entry.score += contribution;
                    entry.matched_queries += 1;
                    entry.hit.answer_eligible &= hit.answer_eligible;
                    entry.hit.tentative |= hit.tentative;
                    if list_index == 0 && entry.keyword_rank.is_none() {
                        entry.keyword_rank = Some(rank + 1);
                    }
                }
                None => {
                    by_key.insert(hit.key.clone(), fused.len());
                    let first_seen = fused.len();
                    fused.push(Fused {
                        hit,
                        score: contribution,
                        keyword_rank: (list_index == 0).then_some(rank + 1),
                        matched_queries: 1,
                        first_seen,
                    });
                }
            }
        }
    }
    fused.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(
                a.keyword_rank
                    .unwrap_or(usize::MAX)
                    .cmp(&b.keyword_rank.unwrap_or(usize::MAX)),
            )
            .then(a.first_seen.cmp(&b.first_seen))
    });
    fused
}

/// The final order.
///
/// - When nothing reordered the keyword results (`ai_order` is `None` and only the question was
///   searched), the keyword order is returned unchanged: a keyword fallback is exactly keyword
///   search.
/// - Otherwise hits containing an exact token from the question come first, in keyword order
///   (stable: the model cannot move them), then the model's ranking, then everything else in
///   fused order.
fn finish<H: Clone>(
    status: AiSearchStatus,
    fused: Vec<Fused<H>>,
    ai_order: Option<&[usize]>,
    exact: &[ExactToken],
    run: Run,
    started: Instant,
    bounds: &AiSearchBounds,
) -> AiSearchResponse<H> {
    let reordered =
        ai_order.is_some() || !run.expanded_queries.is_empty() || run.date_range.is_some();
    let sent = run.candidates_sent;
    let mut ai_rank: Vec<Option<usize>> = vec![None; fused.len()];
    if let Some(order) = ai_order {
        for (position, &index) in order.iter().enumerate() {
            if let Some(slot) = ai_rank.get_mut(index) {
                *slot = Some(position + 1);
            }
        }
    }
    let exact_flags: Vec<bool> = fused
        .iter()
        .map(|f| {
            exact.iter().any(|token| {
                contains_exact(&f.hit.title, token) || contains_exact(&f.hit.text, token)
            })
        })
        .collect();

    let mut indexes: Vec<usize> = (0..fused.len()).collect();
    if reordered {
        indexes.sort_by_key(|&i| {
            let group = if exact_flags[i] {
                0
            } else if ai_rank[i].is_some() {
                1
            } else {
                2
            };
            let within = match group {
                0 => fused[i].keyword_rank.unwrap_or(usize::MAX),
                1 => ai_rank[i].unwrap_or(usize::MAX),
                _ => 0,
            };
            (group, within, i)
        });
    }

    // `sent_to_model` refers to the positions in the candidate list, which is the eligible prefix
    // of the fused order; recompute it from the same rule `refine` used.
    let mut sent_flags = vec![false; fused.len()];
    let mut counted = 0;
    for (i, f) in fused.iter().enumerate() {
        if counted >= sent {
            break;
        }
        if f.hit.cloud_eligible {
            sent_flags[i] = true;
            counted += 1;
        }
    }

    let mut slots: Vec<Option<Fused<H>>> = fused.into_iter().map(Some).collect();
    let hits: Vec<RankedHit<H>> = indexes
        .into_iter()
        .take(bounds.final_limit)
        .filter_map(|i| {
            let f = slots[i].take()?;
            Some(RankedHit {
                exact_match: exact_flags[i],
                ai_rank: ai_rank[i],
                keyword_rank: f.keyword_rank,
                matched_queries: f.matched_queries,
                sent_to_model: sent_flags[i],
                hit: f.hit,
            })
        })
        .collect();
    // A citation must resolve to a visible result, even when exact matches displace it at the cap.
    let answer = run
        .answer
        .into_iter()
        .filter(|part| {
            part.sources
                .iter()
                .all(|key| hits.iter().any(|hit| hit.hit.key == *key))
        })
        .collect();
    AiSearchResponse {
        status,
        answer,
        hits,
        expanded_queries: run.expanded_queries,
        date_range: run.date_range,
        candidates_sent: run.candidates_sent,
        dropped_ids: run.dropped_ids,
        model_calls: run.model_calls,
        elapsed_ms: started.elapsed().as_millis() as u64,
    }
}

/// Step 2: expansion, widened keyword search, rerank. Infallible by design — every failure is a
/// labelled keyword fallback carrying the best results available at that point.
///
/// `model` is `None` when no provider is configured ([`AiSearchStatus::NeedsProvider`]).
pub fn refine<S, M>(
    searcher: &S,
    model: Option<&M>,
    phase: KeywordPhase<S::Hit>,
    clock: SearchClock,
    bounds: &AiSearchBounds,
) -> AiSearchResponse<S::Hit>
where
    S: KeywordSearcher,
    M: SearchModel + ?Sized,
{
    let started = Instant::now();
    let deadline = started + bounds.timeout;
    let KeywordPhase { question, hits } = phase;
    let exact = exact_tokens(&question);
    let mut run = Run::default();
    let mut lists = vec![hits];

    if is_exact_only_query(&question, &exact) {
        let fused = fuse(lists);
        let status = AiSearchStatus::KeywordFallback(FallbackReason::ExactQuery);
        return finish(status, fused, None, &exact, run, started, bounds);
    }
    let Some(model) = model else {
        let fused = fuse(lists);
        return finish(AiSearchStatus::NeedsProvider, fused, None, &exact, run, started, bounds);
    };

    let sent_question = sanitize_question(&question, bounds.max_question_chars);
    let raw = model.complete(
        ModelStep::Expand,
        &expansion_system_prompt(clock, bounds),
        &expansion_user_prompt(&sent_question),
        deadline,
    );
    run.model_calls += 1;
    let fail_keyword = |status: AiSearchStatus, lists: Vec<Vec<KeywordHit<S::Hit>>>, run: Run| {
        let fused = fuse(lists);
        finish(status, fused, None, &exact, run, started, bounds)
    };
    let raw = match raw {
        Ok(raw) if Instant::now() <= deadline => raw,
        Ok(_) => {
            let status = AiSearchStatus::KeywordFallback(FallbackReason::ProviderTimeout);
            return fail_keyword(status, lists, run);
        }
        Err(error) => return fail_keyword(provider_status(&error), lists, run),
    };
    let expansion = match parse_expansion(&raw, &sent_question, clock, bounds) {
        Ok(expansion) => expansion,
        Err(_) => {
            let status = AiSearchStatus::KeywordFallback(FallbackReason::MalformedExpansion);
            return fail_keyword(status, lists, run);
        }
    };

    // widened keyword search (local only)
    if let Some(range) = expansion.date_range {
        // The question itself, inside the resolved window: "last month's invoice" should find the
        // invoice from last month even when older invoices outrank it on words alone.
        if let Ok(found) = searcher.search(&question, Some(range), bounds.per_query_limit) {
            lists.push(found);
        }
    }
    for query in &expansion.queries {
        // A failing expanded query costs that query, not the search.
        if let Ok(found) = searcher.search(query, expansion.date_range, bounds.per_query_limit) {
            lists.push(found);
        }
    }
    run.expanded_queries = expansion.queries;
    run.date_range = expansion.date_range;
    let fused = fuse(lists);

    let eligible: Vec<usize> = fused
        .iter()
        .enumerate()
        .filter(|(_, f)| f.hit.cloud_eligible)
        .map(|(i, _)| i)
        .take(bounds.max_candidates)
        .collect();
    if eligible.is_empty() {
        // Nothing to judge: the expansion was used and found nothing sendable.
        return finish(AiSearchStatus::AiReranked, fused, Some(&[]), &exact, run, started, bounds);
    }
    let offered: Vec<RerankCandidate<'_>> = eligible
        .iter()
        .map(|&i| RerankCandidate {
            title: &fused[i].hit.title,
            text: &fused[i].hit.text,
            date_ms: fused[i].hit.date_ms,
            answer_eligible: fused[i].hit.answer_eligible,
            tentative: fused[i].hit.tentative,
        })
        .collect();
    let (user, included) = rerank_user_prompt(&sent_question, &offered, clock, bounds);
    run.candidates_sent = included;
    if Instant::now() >= deadline {
        let status = AiSearchStatus::KeywordFallback(FallbackReason::ProviderTimeout);
        run.candidates_sent = 0;
        return finish(status, fused, None, &exact, run, started, bounds);
    }
    let raw = model.complete(
        ModelStep::Rerank,
        &rerank_system_prompt(clock, bounds.rerank_answer),
        &user,
        deadline,
    );
    run.model_calls += 1;
    let raw = match raw {
        Ok(raw) if Instant::now() <= deadline => raw,
        Ok(_) => {
            let status = AiSearchStatus::KeywordFallback(FallbackReason::ProviderTimeout);
            return finish(status, fused, None, &exact, run, started, bounds);
        }
        Err(error) => return finish(provider_status(&error), fused, None, &exact, run, started, bounds),
    };
    match parse_rerank(&raw, included, bounds) {
        Ok(order) => {
            run.dropped_ids = order.dropped_ids;
            run.answer = order
                .answer
                .into_iter()
                .filter(|part| part.sources.iter().all(|&c| fused[eligible[c]].hit.answer_eligible))
                .map(|part| MemoryAnswerPart {
                    text: part.text,
                    sources: part
                        .sources
                        .into_iter()
                        .map(|c| fused[eligible[c]].hit.key.clone())
                        .collect(),
                })
                .collect();
            // Candidate positions back to fused indexes.
            let ai_order: Vec<usize> = order.order.iter().map(|&c| eligible[c]).collect();
            finish(AiSearchStatus::AiReranked, fused, Some(&ai_order), &exact, run, started, bounds)
        }
        Err(_) => {
            let status = AiSearchStatus::KeywordFallback(FallbackReason::MalformedRerank);
            finish(status, fused, None, &exact, run, started, bounds)
        }
    }
}

/// Excerpts the streamed answer reads: the best of what the rerank kept.
const SUMMARY_SOURCES: usize = 8;

/// The desktop's streamed answer request: our instructions, the untrusted user turn, and the key
/// behind each `[cN]` id (`sources[N - 1]`).
#[derive(Debug, Clone, PartialEq)]
pub struct SummaryRequest {
    pub system: String,
    pub user: String,
    pub sources: Vec<String>,
}

/// The answer request over a finished rerank, or `None` when there is nothing to answer from (the
/// rerank did not run or kept nothing). Only excerpts that already went to the model and may
/// support an answer are offered — never a suggestion-only or local-only (excluded) hit — in the
/// final order: exact matches, then the model's ranking.
pub fn summary_request<H>(
    response: &AiSearchResponse<H>,
    question: &str,
    clock: SearchClock,
    bounds: &AiSearchBounds,
) -> Option<SummaryRequest> {
    if response.status != AiSearchStatus::AiReranked {
        return None;
    }
    let picked: Vec<&KeywordHit<H>> = response
        .hits
        .iter()
        .filter(|r| r.sent_to_model && r.hit.cloud_eligible && r.hit.answer_eligible)
        .filter(|r| r.ai_rank.is_some() || r.exact_match)
        .map(|r| &r.hit)
        .take(SUMMARY_SOURCES)
        .collect();
    let offered: Vec<RerankCandidate<'_>> = picked
        .iter()
        .map(|hit| RerankCandidate {
            title: &hit.title,
            text: &hit.text,
            date_ms: hit.date_ms,
            answer_eligible: true,
            tentative: hit.tentative,
        })
        .collect();
    let sent_question = sanitize_question(question, bounds.max_question_chars);
    let (user, included) = rerank_user_prompt(&sent_question, &offered, clock, bounds);
    if included == 0 {
        return None;
    }
    Some(SummaryRequest {
        system: summary_system_prompt(clock),
        user,
        sources: picked.iter().take(included).map(|hit| hit.key.clone()).collect(),
    })
}

/// [`keyword_phase`] then [`refine`] in one call, for callers that do not show keyword results
/// first (the evaluation harness, a CLI).
pub fn run<S, M>(
    access: Result<(), &'static str>,
    searcher: &S,
    model: Option<&M>,
    question: &str,
    clock: SearchClock,
    bounds: &AiSearchBounds,
) -> Result<AiSearchResponse<S::Hit>, AiSearchError>
where
    S: KeywordSearcher,
    M: SearchModel + ?Sized,
{
    let phase = keyword_phase(access, searcher, question, bounds)?;
    Ok(refine(searcher, model, phase, clock, bounds))
}
