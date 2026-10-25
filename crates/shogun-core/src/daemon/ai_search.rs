//! The daemon side of AI memory search (issue #940 B1): the [`KeywordSearcher`] over the
//! canonical search pipeline ([`Db::search_request`]), the egress filter for capture exclusions,
//! and the plan gate.
//!
//! Every entry point here takes the caller's [`Entitlements`] and checks
//! `require_memory_access` before the store is read, so a locked plan gets neither local results
//! nor a provider call (#940 part A). The desktop, MCP, REST and CLI surfaces (B2) call these
//! methods rather than [`crate::ai_search`] directly.

use std::collections::{HashMap, HashSet};

use shogun_agents::entitlement::Entitlements;
use shogun_memory::search::{CanonicalEvidenceId, FederatedHit, FederatedSourceId};

use super::*;
use crate::ai_search::{
    self, AiSearchBounds, AiSearchError, AiSearchResponse, AiSearchStatus, FallbackReason,
    KeywordHit, KeywordPhase, KeywordSearcher, SearchClock, SearchModel, TimeRange,
};
use crate::capture::exclusion::ExclusionPolicy;

/// Keyword search over the real pipeline, marking hits whose evidence came from an app or window
/// the user's capture exclusions now cover as local-only.
///
/// Excluded apps are never captured, so in the normal case every hit is eligible. The check is
/// for rows captured *before* an exclusion was added (or before a default joined the list, as the
/// terminals did): those stay searchable on the device, but an exclusion is the user saying "not
/// this app", and sending its text to a provider would ignore that.
pub struct DbKeywordSearcher<'a> {
    db: &'a Db,
    exclusions: &'a ExclusionPolicy,
}

/// A stable identity for deduplicating one result across several queries.
fn hit_key(hit: &FederatedHit) -> String {
    match &hit.source_id {
        FederatedSourceId::UserNote(id) => format!("user_note:{id}"),
        FederatedSourceId::Event(id) => format!("event:{id}"),
        FederatedSourceId::Derived(source) => format!("{}:{}", source.table(), source.id()),
        FederatedSourceId::Meeting(id) => format!("meeting:{id}"),
        FederatedSourceId::Imported(id) => format!("imported:{id}"),
    }
}

/// Every event row a hit rests on.
fn event_ids(hit: &FederatedHit) -> impl Iterator<Item = i64> + '_ {
    let primary = match hit.source_id {
        FederatedSourceId::Event(id) => Some(id),
        _ => None,
    };
    primary.into_iter().chain(hit.evidence_ids.iter().filter_map(|id| match id {
        CanonicalEvidenceId::Event(id) => Some(*id),
        _ => None,
    }))
}

impl DbKeywordSearcher<'_> {
    /// Event ids whose app or window title the current policy excludes. `Err` when the lookup
    /// failed, which the caller treats as "every event-backed hit stays local" (fail closed).
    fn excluded_events(&self, hits: &[FederatedHit]) -> Result<HashSet<i64>, ()> {
        let ids: Vec<i64> = hits
            .iter()
            .flat_map(event_ids)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        if ids.is_empty() {
            return Ok(HashSet::new());
        }
        let rows = self
            .db
            .with_conn("ai_search.exclusions", |conn| -> Result<_, rusqlite::Error> {
                let mut out = Vec::with_capacity(ids.len());
                // Bounded by the per-query hit limit times evidence per hit; chunked to stay far
                // below SQLite's bind-variable limit.
                for chunk in ids.chunks(500) {
                    let marks = vec!["?"; chunk.len()].join(",");
                    let mut stmt = conn.prepare(&format!(
                        "SELECT id, app_bundle_id, window_title FROM event_log WHERE id IN ({marks})"
                    ))?;
                    let found = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    })?;
                    for row in found {
                        out.push(row?);
                    }
                }
                Ok(out)
            })
            .map_err(|_| ())?;
        Ok(rows
            .into_iter()
            // A row with no app (a meeting, a connector read) was never captured from a window,
            // so an app exclusion cannot apply to it. (`is_excluded("")` would read as SHOGUN's
            // own app and exclude every such row.)
            .filter(|(_, bundle, title)| {
                bundle.as_deref().is_some_and(|bundle| {
                    !bundle.is_empty() && self.exclusions.is_excluded(bundle, title.as_deref()).is_some()
                })
            })
            .map(|(id, _, _)| id)
            .collect())
    }
}

impl KeywordSearcher for DbKeywordSearcher<'_> {
    type Hit = FederatedHit;

    fn search(
        &self,
        query: &str,
        range: Option<TimeRange>,
        limit: usize,
    ) -> Result<Vec<KeywordHit<FederatedHit>>, &'static str> {
        let now = self.db.now_ms();
        let range = range
            .map(|r| SearchTimeRangeV1 { from_ms: r.from_ms.max(0), to_ms: r.to_ms.min(now) })
            .filter(|r| r.from_ms < r.to_ms);
        let request = SearchRequestV1 {
            query: query.to_string(),
            limit,
            // An explicit window may reach past Warm; `Auto` lets the canonical policy decide the
            // depth instead of rejecting an older month.
            depth: if range.is_some() { SearchDepthV1::Auto } else { SearchDepthV1::Warm },
            range,
            source_kind: None,
        };
        let response = self.db.search_request(&request).map_err(|e| e.as_str())?;
        let excluded = self.excluded_events(&response.federated_hits);
        Ok(response
            .federated_hits
            .into_iter()
            .map(|hit| {
                let cloud_eligible = match &excluded {
                    Ok(excluded) => !event_ids(&hit).any(|id| excluded.contains(&id)),
                    Err(()) => event_ids(&hit).next().is_none(),
                };
                KeywordHit {
                    key: hit_key(&hit),
                    title: hit.title.clone(),
                    text: hit.excerpt.clone(),
                    date_ms: hit.period_end_ms.or(hit.period_start_ms),
                    cloud_eligible,
                    answer_eligible: hit.answer_eligible,
                    tentative: hit.confidence < 0.8,
                    hit,
                }
            })
            .collect())
    }
}

/// One AI-search result as the desktop draws it (issue #940 B2). Bounded like the keyword
/// search box: the excerpt the search already centred, where it came from, and when. An event
/// hit's title is its window title, which the panel never shows (no usernames or paths), so it
/// is `None` and the app's bundle id is given instead.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AiSearchHitView {
    /// Stable identity (`event:12`, `projects:3`), unique within one response.
    pub key: String,
    /// `event`, `meeting`, `derived:<kind>` or `imported:<platform>`.
    pub source_kind: String,
    pub title: Option<String>,
    pub excerpt: String,
    /// When the evidence happened (unix ms), when known.
    pub ts: Option<i64>,
    /// The app bundle id an event was captured from.
    pub app: Option<String>,
    /// Retained, visible screen destination, if this event still has one.
    pub screen_ts: Option<i64>,
    /// Resolve recap/note identities to their owning meeting, never assume equal ids.
    pub meeting_session_id: Option<i64>,
    pub can_open_source: bool,
    /// Contains a link, identifier or quoted phrase from the question (kept first).
    pub exact_match: bool,
    /// 1-based position in the model's ranking, when it judged this hit relevant.
    pub ai_rank: Option<usize>,
    /// 1-based position in the plain keyword results, when the question alone found it.
    pub keyword_rank: Option<usize>,
    /// Below the High-confidence band: an answer built on it attributes rather than asserts.
    pub tentative: bool,
}

/// An AI-search response for the desktop. `status` is `keyword` for the instant keyword phase,
/// then `ai_reranked`, `keyword_fallback` (with `reason`) or `needs_provider`. Includes the
/// bounded generated answer and its supporting hit keys.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AiSearchView {
    pub status: &'static str,
    pub reason: Option<&'static str>,
    pub hits: Vec<AiSearchHitView>,
    pub answer: Vec<ai_search::MemoryAnswerPart>,
    pub expanded_queries: Vec<String>,
    pub model_calls: usize,
    pub elapsed_ms: u64,
}

/// The `status` of the instant keyword phase.
pub const KEYWORD_PHASE_STATUS: &str = "keyword";

/// `(status, reason)` as the views carry them.
pub fn status_parts(status: AiSearchStatus) -> (&'static str, Option<&'static str>) {
    match status {
        AiSearchStatus::AiReranked => ("ai_reranked", None),
        AiSearchStatus::KeywordFallback(reason) => ("keyword_fallback", Some(reason.as_str())),
        AiSearchStatus::NeedsProvider => (ai_search::NEEDS_PROVIDER, None),
    }
}

impl Db {
    /// Best-effort destinations; failure leaves results readable without an Open action.
    fn event_destinations(&self, ids: &[i64]) -> HashMap<i64, (Option<String>, Option<i64>, bool)> {
        if ids.is_empty() {
            return HashMap::new();
        }
        self.with_conn("ai_search.destinations", |conn| -> Result<_, rusqlite::Error> {
            let frames = shogun_memory::screen_frames::frame_ids_for_events(conn, ids)?;
            let mut out = HashMap::with_capacity(ids.len());
            for chunk in ids.chunks(500) {
                let marks = vec!["?"; chunk.len()].join(",");
                let mut stmt = conn.prepare(&format!(
                    "SELECT id, app_bundle_id, source FROM event_log WHERE id IN ({marks})"
                ))?;
                let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, String>(2)?))
                })?;
                for row in rows {
                    let (id, app, source) = row?;
                    let screen_ts = frames.get(&id).and_then(|frame| {
                        conn.query_row("SELECT created_at_ms FROM screen_frames WHERE id = ?1", [frame], |row| row.get::<_, i64>(0)).ok()
                    });
                    let openable = screen_ts.is_some() || match source.as_str() {
                        "capture" | "screen_ocr" | "screen_text" => app.as_ref().is_some_and(|id| !id.is_empty()),
                        "gmail" | "gcal" | "slack" | "notion" | "github" | "linear" => true,
                        _ => false,
                    };
                    out.insert(id, (app, screen_ts, openable));
                }
            }
            Ok(out)
        }).unwrap_or_default()
    }

    fn search_meeting_destination(&self, source: &FederatedSourceId) -> Option<i64> {
        use shogun_memory::search_documents::SourceRef;
        let (table, id) = match source {
            FederatedSourceId::Meeting(id) => return Some(*id),
            FederatedSourceId::Derived(SourceRef::MeetingRecap(id)) => ("meeting_recaps", id),
            FederatedSourceId::Derived(SourceRef::SessionNote(id)) => ("session_notes", id),
            _ => return None,
        };
        self.with_conn("ai_search.meeting_destination", |conn| {
            conn.query_row(&format!("SELECT session_id FROM {table} WHERE id = ?1"), [id], |row| row.get::<_, i64>(0))
        }).ok()
    }

    fn hit_views<'h>(
        &self,
        hits: impl Iterator<Item = (&'h KeywordHit<FederatedHit>, bool, Option<usize>, Option<usize>)>,
    ) -> Vec<AiSearchHitView> {
        let hits: Vec<_> = hits.collect();
        let ids: Vec<i64> = hits
            .iter()
            .filter_map(|(hit, ..)| match hit.hit.source_id {
                FederatedSourceId::Event(id) => Some(id),
                _ => None,
            })
            .collect();
        let destinations = self.event_destinations(&ids);
        hits.into_iter()
            .map(|(hit, exact_match, ai_rank, keyword_rank)| {
                let event = match hit.hit.source_id {
                    FederatedSourceId::Event(id) => Some(id),
                    _ => None,
                };
                let destination = event.and_then(|id| destinations.get(&id));
                AiSearchHitView {
                    key: hit.key.clone(),
                    source_kind: crate::db_backend::source_kind(&hit.hit),
                    title: if event.is_some() { None } else { Some(hit.title.clone()) },
                    excerpt: hit.text.clone(),
                    ts: hit.date_ms,
                    app: destination.and_then(|(app, ..)| app.clone()),
                    screen_ts: destination.and_then(|(_, ts, _)| *ts),
                    meeting_session_id: self.search_meeting_destination(&hit.hit.source_id),
                    can_open_source: destination.is_some_and(|(_, _, openable)| *openable),
                    exact_match,
                    ai_rank,
                    keyword_rank,
                    tentative: hit.tentative,
                }
            })
            .collect()
    }

    /// The instant keyword phase, projected for the desktop.
    pub fn ai_search_keyword_view(&self, phase: &KeywordPhase<FederatedHit>) -> AiSearchView {
        let exact = ai_search::exact_tokens(phase.question());
        let hits = self.hit_views(phase.hits().iter().enumerate().map(|(i, hit)| {
            let exact_match = exact.iter().any(|token| {
                ai_search::contains_exact(&hit.title, token) || ai_search::contains_exact(&hit.text, token)
            });
            (hit, exact_match, None, Some(i + 1))
        }));
        AiSearchView {
            status: KEYWORD_PHASE_STATUS,
            answer: Vec::new(),
            reason: None,
            hits,
            expanded_queries: Vec::new(),
            model_calls: 0,
            elapsed_ms: 0,
        }
    }

    /// A finished search, projected for the desktop.
    pub fn ai_search_view(&self, response: &AiSearchResponse<FederatedHit>) -> AiSearchView {
        let (status, reason) = status_parts(response.status);
        let hits = self.hit_views(
            response
                .hits
                .iter()
                .map(|r| (&r.hit, r.exact_match, r.ai_rank, r.keyword_rank)),
        );
        AiSearchView {
            status,
            answer: response.answer.clone(),
            reason,
            hits,
            expanded_queries: response.expanded_queries.clone(),
            model_calls: response.model_calls,
            elapsed_ms: response.elapsed_ms,
        }
    }
}

impl Db {
    /// The AI-search keyword searcher, honouring `exclusions` at egress.
    pub fn ai_keyword_searcher<'a>(&'a self, exclusions: &'a ExclusionPolicy) -> DbKeywordSearcher<'a> {
        DbKeywordSearcher { db: self, exclusions }
    }

    /// "Now" and the local UTC offset, for the prompts' date resolution.
    pub fn ai_search_clock(&self) -> SearchClock {
        let now_ms = self.now_ms();
        SearchClock {
            now_ms,
            utc_offset_minutes: os_gmt_offset_secs(now_ms) / 60,
        }
    }

    /// Step 1 behind the plan gate: the keyword results a surface shows immediately.
    pub fn ai_search_keyword_phase(
        &self,
        entitlements: &Entitlements,
        searcher: &DbKeywordSearcher<'_>,
        question: &str,
        bounds: &AiSearchBounds,
    ) -> Result<KeywordPhase<FederatedHit>, AiSearchError> {
        ai_search::keyword_phase(entitlements.require_memory_access(), searcher, question, bounds)
    }

    /// Step 2: expansion and rerank on the user's own Agent-lane credentials. `model` is `None`
    /// when no provider is configured. A plan without Agent-lane execution keeps the keyword
    /// results (`ai_not_in_plan`) and makes no call.
    pub fn ai_search_refine<M: SearchModel + ?Sized>(
        &self,
        entitlements: &Entitlements,
        searcher: &DbKeywordSearcher<'_>,
        model: Option<&M>,
        phase: KeywordPhase<FederatedHit>,
        bounds: &AiSearchBounds,
    ) -> AiSearchResponse<FederatedHit> {
        if !entitlements.agent_execution {
            return phase.into_response(
                AiSearchStatus::KeywordFallback(FallbackReason::AiNotInPlan),
                bounds,
            );
        }
        ai_search::refine(searcher, model, phase, self.ai_search_clock(), bounds)
    }

    /// Both steps in one call.
    pub fn ai_search<M: SearchModel + ?Sized>(
        &self,
        entitlements: &Entitlements,
        exclusions: &ExclusionPolicy,
        model: Option<&M>,
        question: &str,
        bounds: &AiSearchBounds,
    ) -> Result<AiSearchResponse<FederatedHit>, AiSearchError> {
        let searcher = self.ai_keyword_searcher(exclusions);
        let phase = self.ai_search_keyword_phase(entitlements, &searcher, question, bounds)?;
        Ok(self.ai_search_refine(entitlements, &searcher, model, phase, bounds))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use shogun_agents::entitlement::{entitlements, Plan};

    use super::*;
    use crate::ai_search::ModelStep;
    use crate::llm::LlmError;

    const NOW: i64 = 1_791_201_600_000; // 2026-10-05T12:00:00Z

    /// Expands to "postpone release" and ranks whichever candidate mentions `answer`.
    struct Fake {
        answer: &'static str,
        sent: Mutex<Vec<(ModelStep, String)>>,
    }

    impl Fake {
        fn new(answer: &'static str) -> Self {
            Self { answer, sent: Mutex::new(Vec::new()) }
        }
        fn sent(&self) -> Vec<(ModelStep, String)> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl SearchModel for Fake {
        fn complete(
            &self,
            step: ModelStep,
            _system: &str,
            user: &str,
            _deadline: Instant,
        ) -> Result<String, LlmError> {
            self.sent.lock().unwrap().push((step, user.to_string()));
            Ok(match step {
                ModelStep::Expand => r#"{"queries": ["postpone release"]}"#.to_string(),
                ModelStep::Rerank => {
                    let ids: Vec<String> = user
                        .lines()
                        .filter(|l| l.contains(self.answer))
                        .filter_map(|l| Some(format!("\"{}\"", &l[1..l.find(']')?])))
                        .collect();
                    format!(r#"{{"ranked": [{}]}}"#, ids.join(","))
                }
            })
        }
    }

    fn db_with(events: &[(&str, Option<&str>, i64)]) -> (Db, Vec<i64>) {
        let db = Db::open_in_memory(Arc::new(|| NOW)).unwrap();
        let ids = events
            .iter()
            .enumerate()
            .map(|(i, (content, app, ts))| {
                let hash = format!("ai-search-test-{i}");
                db.capture(&NewEvent {
                    ts: *ts,
                    source: "capture",
                    kind: "text",
                    app_bundle_id: *app,
                    window_title: Some("Notes"),
                    content,
                    content_hash: &hash,
                    dwell_ms: 0,
                    display_id: None,
                    window_bounds: None,
                })
                .unwrap()
                .0
            })
            .collect();
        (db, ids)
    }

    fn active() -> Entitlements {
        entitlements(Plan::Trial { started_at_ms: Some(NOW as u64) }, NOW as u64)
    }

    struct TopicAnswer;

    impl SearchModel for TopicAnswer {
        fn complete(&self, step: ModelStep, _system: &str, user: &str, _deadline: Instant) -> Result<String, LlmError> {
            match step {
                ModelStep::Expand => Ok(r#"{"queries":["DTU application"]}"#.into()),
                ModelStep::Rerank => {
                    let id = user.lines().find(|line| line.contains("DTU application is a draft"))
                        .and_then(|line| line.strip_prefix('[')?.split_once(']').map(|(id, _)| id))
                        .ok_or(LlmError::Parse("fixture source missing".into()))?;
                    Ok(serde_json::json!({"ranked":[id], "answer":[{"text":"Your saved DTU application is still a draft.","sources":[id]}]}).to_string())
                }
            }
        }
    }

    #[test]
    fn a_dtu_topic_answer_reaches_desktop_and_api_with_the_same_source() {
        let (db, ids) = db_with(&[("DTU application is a draft", Some("com.apple.Notes"), NOW - 86_400_000)]);
        let response = db.ai_search(&active(), &ExclusionPolicy::new(), Some(&TopicAnswer), "DTU", &AiSearchBounds::default()).unwrap();
        let view = db.ai_search_view(&response);
        let api = super::super::ai_search_api_json(&response);
        let key = format!("event:{}", ids[0]);
        assert_eq!(view.answer[0].text, "Your saved DTU application is still a draft.");
        assert_eq!(view.answer[0].sources, vec![key.clone()]);
        assert_eq!(view.hits[0].key, key);
        assert_eq!(api["answer"], serde_json::to_value(&view.answer).unwrap());
        assert_eq!(api["results"][0]["key"], key);
        assert_eq!(view.model_calls, 2);
    }

    #[test]
    fn imported_claims_remain_matches_but_cannot_support_an_answer() {
        let (db, _) = db_with(&[]);
        db.memory_import_commit(r#"{"package":{"format":"shogun-memory-import/v1","source":{"platform":"computer_history"},"profile":{"goals":[{"value":"DTU application is a draft","confidence":"high","evidence":"imported claim","status":"active"}]}},"select":"default"}"#, shogun_memory::memory_import::Via::Ui).unwrap();
        let response = db.ai_search(&active(), &ExclusionPolicy::new(), Some(&TopicAnswer), "DTU", &AiSearchBounds::default()).unwrap();
        assert_eq!(response.status, AiSearchStatus::AiReranked);
        assert!(!response.hits.is_empty());
        assert!(response.hits[0].ai_rank.is_some(), "imported suggestions can still be ranked");
        assert!(response.hits.iter().all(|hit| !hit.hit.answer_eligible));
        assert!(response.answer.is_empty(), "the model cannot promote an imported claim to an answer");
    }

    const QUESTION: &str = "when did we decide to delay the launch";

    #[test]
    fn the_desktop_view_hides_window_titles_and_names_the_app() {
        let (db, ids) = db_with(&[
            ("Launch party catering confirmed", Some("com.apple.mail"), NOW - 3 * 86_400_000),
            ("Agreed: postpone the Orchid release to November", Some("com.apple.Notes"), NOW - 9 * 86_400_000),
        ]);
        let policy = ExclusionPolicy::new();
        let searcher = db.ai_keyword_searcher(&policy);
        let bounds = AiSearchBounds::default();
        let phase = db.ai_search_keyword_phase(&active(), &searcher, QUESTION, &bounds).unwrap();

        let instant = db.ai_search_keyword_view(&phase);
        assert_eq!(instant.status, KEYWORD_PHASE_STATUS);
        assert_eq!(instant.model_calls, 0);
        for (i, hit) in instant.hits.iter().enumerate() {
            assert_eq!(hit.keyword_rank, Some(i + 1));
            assert_eq!(hit.ai_rank, None);
        }

        let model = Fake::new("postpone the Orchid release");
        let response = db.ai_search_refine(&active(), &searcher, Some(&model), phase, &bounds);
        let view = db.ai_search_view(&response);
        assert_eq!((view.status, view.reason), ("ai_reranked", None));
        assert_eq!(view.expanded_queries, vec!["postpone release".to_string()]);
        assert_eq!(view.model_calls, 2);
        let top = &view.hits[0];
        assert_eq!(top.key, format!("event:{}", ids[1]));
        assert_eq!(top.source_kind, "event");
        assert_eq!(top.title, None, "an event's title is its window title");
        assert_eq!(top.app.as_deref(), Some("com.apple.Notes"));
        assert_eq!(top.ai_rank, Some(1));
        assert_eq!(top.ts, Some(NOW - 9 * 86_400_000));
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("\"Notes\""), "the window title never reaches the webview");
    }

    #[test]
    fn view_status_parts_name_every_outcome() {
        assert_eq!(status_parts(AiSearchStatus::AiReranked), ("ai_reranked", None));
        assert_eq!(status_parts(AiSearchStatus::NeedsProvider), ("needs_provider", None));
        assert_eq!(
            status_parts(AiSearchStatus::KeywordFallback(FallbackReason::ProviderTimeout)),
            ("keyword_fallback", Some("provider_timeout"))
        );
    }

    #[test]
    fn an_expired_trial_is_refused_before_search_or_provider() {
        let (db, _) = db_with(&[("Orchid launch delayed to November", None, NOW - 1_000)]);
        let model = Fake::new("Orchid");
        let locked = Entitlements::trial_not_started();
        let result = db.ai_search(&locked, &ExclusionPolicy::new(), Some(&model), QUESTION, &AiSearchBounds::default());
        assert_eq!(result.unwrap_err(), AiSearchError::Locked("plan_required"));
        assert!(model.sent().is_empty(), "no provider call");
        let policy = ExclusionPolicy::new();
        let searcher = db.ai_keyword_searcher(&policy);
        let phase = db.ai_search_keyword_phase(&locked, &searcher, QUESTION, &AiSearchBounds::default());
        assert_eq!(phase.unwrap_err().code(), "plan_required", "no local results either");
    }

    #[test]
    fn the_real_pipeline_finds_the_paraphrase_with_provenance() {
        let (db, ids) = db_with(&[
            ("Launch party catering confirmed", None, NOW - 3 * 86_400_000),
            ("Agreed: postpone the Orchid release to November", None, NOW - 9 * 86_400_000),
        ]);
        let model = Fake::new("postpone the Orchid release");
        let response = db
            .ai_search(&active(), &ExclusionPolicy::new(), Some(&model), QUESTION, &AiSearchBounds::default())
            .unwrap();
        assert_eq!(response.status, AiSearchStatus::AiReranked);
        let top = &response.hits[0];
        assert_eq!(top.hit.key, format!("event:{}", ids[1]));
        assert!(top.hit.hit.evidence_ids.contains(&CanonicalEvidenceId::Event(ids[1])));
        assert_eq!(top.hit.date_ms, Some(NOW - 9 * 86_400_000));
        assert_eq!(model.sent().len(), 2);
    }

    #[test]
    fn excluded_apps_are_searchable_locally_but_never_sent() {
        let (db, ids) = db_with(&[
            ("Vault note: postpone the release, codename Falcon", Some("com.example.vault"), NOW - 2_000),
            ("Terminal scrollback: postpone release script", Some("com.apple.Terminal"), NOW - 3_000),
            ("Agreed: postpone the Orchid release to November", Some("com.apple.Notes"), NOW - 4_000),
        ]);
        let mut policy = ExclusionPolicy::new();
        policy.add_app("com.example.vault");
        policy.add_app("com.apple.Terminal");
        let model = Fake::new("Orchid");
        let response = db
            .ai_search(&active(), &policy, Some(&model), QUESTION, &AiSearchBounds::default())
            .unwrap();
        let rerank_user = &model.sent()[1].1;
        assert!(!rerank_user.contains("Falcon"), "user exclusion honoured at egress");
        assert!(!rerank_user.contains("scrollback"), "a user-excluded terminal honoured at egress");
        assert!(rerank_user.contains("Orchid"));
        for (id, eligible) in [(ids[0], false), (ids[1], false), (ids[2], true)] {
            let hit = response.hits.iter().find(|h| h.hit.key == format!("event:{id}")).unwrap();
            assert_eq!(hit.hit.cloud_eligible, eligible);
            assert_eq!(hit.sent_to_model, eligible);
        }
    }

    #[test]
    fn a_plan_without_agent_execution_keeps_keyword_results_without_a_call() {
        let (db, _) = db_with(&[("Orchid launch delayed to November", None, NOW - 1_000)]);
        let model = Fake::new("Orchid");
        let no_agent = Entitlements { agent_execution: false, ..active() };
        let response = db
            .ai_search(&no_agent, &ExclusionPolicy::new(), Some(&model), QUESTION, &AiSearchBounds::default())
            .unwrap();
        assert_eq!(response.status, AiSearchStatus::KeywordFallback(FallbackReason::AiNotInPlan));
        assert!(!response.hits.is_empty());
        assert!(model.sent().is_empty());
    }

    #[test]
    fn the_clock_reports_the_local_offset() {
        let (db, _) = db_with(&[]);
        let clock = db.ai_search_clock();
        assert_eq!(clock.now_ms, NOW);
        assert!((-12 * 60..=14 * 60).contains(&clock.utc_offset_minutes));
    }
}
