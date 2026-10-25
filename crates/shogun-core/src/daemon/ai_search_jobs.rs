//! AI memory search hand-offs from the Memory API (issue #940 B2b, V72): the queue the API faces
//! write and the desktop drains, plus the one JSON shape every face returns for `mode: "ai"`.
//!
//! The faces cannot run the AI step themselves: it needs the user's Agent-lane credential, which
//! only the desktop holds (the `translate_jobs` boundary). They queue the question; the desktop
//! runs [`Db::ai_search_job_execute`] — the same keyword phase, plan gate, egress filter and
//! refine the Full UI uses — and stores the answer for the face to hand over. When nothing claims
//! the request, or the answer is late, the face answers with keyword results and a reason
//! ([`Db::ai_search_keyword_fallback_json`]), so a caller always gets results.
//!
//! Every stored text field is masked for secrets first, and rows are never a history (see
//! `shogun_memory::ai_search_jobs`).

use serde_json::{json, Value};
use shogun_agents::entitlement::Entitlements;
use shogun_memory::ai_search_jobs::{self, AiSearchJob};
use shogun_memory::search::FederatedHit;

use super::*;
use crate::ai_search::{
    self, AiSearchBounds, AiSearchResponse, AiSearchStatus, FallbackReason, SearchModel,
};
use crate::capture::exclusion::ExclusionPolicy;

fn mask(text: &str) -> String {
    shogun_memory::sanitize::persist_body(text).text.into_owned()
}

/// What a face queues: the question, masked.
#[derive(serde::Serialize, serde::Deserialize)]
struct JobRequest {
    question: String,
}

/// The `memory.search` answer for `mode: "ai"`, on every face: `memory.search`'s result fields
/// plus where each result sits in the AI order, and how that order was produced (`status`,
/// `reason`). The expanded queries never left the device; they are the caller's to show.
pub fn ai_search_api_json(response: &AiSearchResponse<FederatedHit>) -> Value {
    let (status, reason) = super::status_parts(response.status);
    let results: Vec<Value> = response
        .hits
        .iter()
        .map(|ranked| {
            let hit = &ranked.hit.hit;
            json!({
                "key": ranked.hit.key,
                "text": hit.excerpt,
                "source_kind": crate::db_backend::source_kind(hit),
                "source_id": crate::db_backend::source_id(hit),
                "provenance": hit.evidence_ids.iter().map(crate::db_backend::evidence_id).collect::<Vec<_>>(),
                "title": hit.title,
                "excerpt": hit.excerpt,
                "confidence": hit.confidence,
                "possibly": hit.confidence < 0.8,
                "score": hit.fused_score,
                "ai_rank": ranked.ai_rank,
                "keyword_rank": ranked.keyword_rank,
                "exact_match": ranked.exact_match,
            })
        })
        .collect();
    json!({
        "tool": "memory.search",
        "mode": "ai",
        "status": status,
        "reason": reason,
        "expanded_queries": response.expanded_queries,
        "model_calls": response.model_calls,
        "elapsed_ms": response.elapsed_ms,
        "results": results,
        "answer": response.answer,
    })
}

/// Mask the free-text fields of an answer before it is stored. Ids, ranks and codes pass through.
fn mask_answer(mut answer: Value) -> Value {
    if let Some(Value::Array(parts)) = answer.get_mut("answer") {
        for part in parts {
            if let Some(Value::String(text)) = part.get_mut("text") {
                *text = mask(text);
            }
        }
    }
    if let Some(Value::Array(results)) = answer.get_mut("results") {
        for result in results {
            for key in ["text", "title", "excerpt"] {
                if let Some(Value::String(s)) = result.get_mut(key) {
                    *s = mask(s);
                }
            }
        }
    }
    if let Some(Value::Array(queries)) = answer.get_mut("expanded_queries") {
        for query in queries {
            if let Value::String(s) = query {
                *s = mask(s);
            }
        }
    }
    answer
}

impl Db {
    /// Queue a question from `origin` (`api` | `mcp` | `cli`).
    pub fn ai_search_job_request(&self, question: &str, origin: &str) -> Result<AiSearchJob, String> {
        let now = self.now_ms();
        let stored = serde_json::to_string(&JobRequest { question: mask(question) })
            .map_err(|_| "ai search request failed: encode".to_string())?;
        self.with_conn("ai_search.job_request", |c| {
            ai_search_jobs::request(c, origin, &stored, now)
        })
        .map_err(|fault| format!("ai search request failed: {}", fault.as_str()))
    }

    /// Read one request; a finished one is deleted as it is returned (it is being handed over).
    pub fn ai_search_job_take(&self, id: i64) -> Result<Option<AiSearchJob>, String> {
        self.with_conn("ai_search.job_take", |c| ai_search_jobs::take_terminal(c, id))
            .map_err(|fault| format!("ai search status failed: {}", fault.as_str()))
    }

    /// Drop a request nobody will read (the face stopped waiting).
    pub fn ai_search_job_discard(&self, id: i64) -> bool {
        self.with_conn("ai_search.job_discard", |c| ai_search_jobs::discard(c, id))
            .unwrap_or(false)
    }

    /// Whether the desktop is working on a request right now.
    pub fn ai_search_jobs_any_running(&self) -> bool {
        let now = self.now_ms();
        self.with_conn("ai_search.any_running", |c| ai_search_jobs::any_running(c, now))
            .unwrap_or(false)
    }

    /// The desktop runner takes the next pending request.
    pub fn ai_search_job_claim_next(&self) -> Option<AiSearchJob> {
        let now = self.now_ms();
        self.with_conn("ai_search.job_claim", |c| ai_search_jobs::claim_next(c, now))
            .ok()
            .flatten()
    }

    /// Requests a crash left running fail rather than rerun (desktop start).
    pub fn ai_search_jobs_fail_interrupted(&self) -> usize {
        let now = self.now_ms();
        self.with_conn("ai_search.fail_interrupted", |c| {
            ai_search_jobs::fail_interrupted(c, now)
        })
        .unwrap_or(0)
    }

    fn ai_search_job_finish(&self, id: i64, outcome: Result<Value, &str>) -> bool {
        let now = self.now_ms();
        let stored = outcome.map(|v| mask_answer(v).to_string());
        self.with_conn("ai_search.job_finish", |c| {
            ai_search_jobs::finish(c, id, stored.as_deref().map_err(|e| *e), now)
        })
        .unwrap_or(false)
    }

    /// Run one claimed request to the end and record the answer: the whole desktop side except
    /// building the clients and reading the plan. `model` is `None` when the user has no usable
    /// Agent-lane credential (the answer is then `needs_provider` with keyword results).
    ///
    /// Returns how the search ended, or the refusal code (`plan_required`, `empty_query`, a store
    /// fault), so the caller can latch or clear a rejected key as every Agent-lane caller does.
    pub fn ai_search_job_execute<M: SearchModel + ?Sized>(
        &self,
        job: &AiSearchJob,
        entitlements: &Entitlements,
        exclusions: &ExclusionPolicy,
        model: Option<&M>,
    ) -> Result<AiSearchStatus, &'static str> {
        let bounds = AiSearchBounds::default();
        let outcome = match serde_json::from_str::<JobRequest>(&job.request) {
            Err(_) => Err("invalid_request"),
            Ok(request) => {
                let searcher = self.ai_keyword_searcher(exclusions);
                self.ai_search_keyword_phase(entitlements, &searcher, &request.question, &bounds)
                    .map(|phase| self.ai_search_refine(entitlements, &searcher, model, phase, &bounds))
                    .map_err(|error| error.code())
            }
        };
        let status = outcome.as_ref().map(|response| response.status).map_err(|code| *code);
        self.ai_search_job_finish(job.id, outcome.map(|response| ai_search_api_json(&response)));
        status
    }

    /// The face's own answer when the desktop did not answer in time (or `reason` says AI may not
    /// run for this caller): the plain keyword results for `question`, labelled `keyword_fallback`
    /// with `reason`. Local only — no provider is called from the API process.
    ///
    /// The face has already passed the Memory API plan gate (memory access included) before it
    /// gets here, so the keyword phase runs with that decision.
    pub fn ai_search_keyword_fallback_json(
        &self,
        question: &str,
        reason: FallbackReason,
    ) -> Result<Value, &'static str> {
        let bounds = AiSearchBounds::default();
        let policy = ExclusionPolicy::new();
        let searcher = self.ai_keyword_searcher(&policy);
        let phase = ai_search::keyword_phase(Ok(()), &searcher, question, &bounds)
            .map_err(|error| error.code())?;
        let response = phase.into_response(AiSearchStatus::KeywordFallback(reason), &bounds);
        Ok(ai_search_api_json(&response))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use shogun_agents::entitlement::{entitlements, Plan};
    use shogun_memory::event_log::NewEvent;

    use super::*;
    use crate::ai_search::ModelStep;
    use crate::llm::LlmError;

    const NOW: i64 = 1_791_201_600_000; // 2026-10-05T12:00:00Z

    struct Ranks(&'static str);

    impl SearchModel for Ranks {
        fn complete(
            &self,
            step: ModelStep,
            _system: &str,
            user: &str,
            _deadline: Instant,
        ) -> Result<String, LlmError> {
            Ok(match step {
                ModelStep::Expand => r#"{"queries": ["postpone release"]}"#.to_string(),
                ModelStep::Rerank => {
                    let ids: Vec<String> = user
                        .lines()
                        .filter(|l| l.contains(self.0))
                        .filter_map(|l| Some(format!("\"{}\"", &l[1..l.find(']')?])))
                        .collect();
                    let sources = ids.first().cloned().unwrap_or_default();
                    format!(r#"{{"ranked": [{}], "answer": [{{"text":"The saved decision postpones Orchid to November, key sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAA", "sources":[{}]}}]}}"#, ids.join(","), sources)
                }
            })
        }
    }

    fn db() -> Db {
        let db = Db::open_in_memory(Arc::new(|| NOW)).unwrap();
        for (i, content) in [
            "Launch party catering confirmed",
            "Agreed: postpone the Orchid release to November, key sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ]
        .iter()
        .enumerate()
        {
            let hash = format!("ai-search-job-{i}");
            db.capture(&NewEvent {
                ts: NOW - (i as i64 + 1) * 86_400_000,
                source: "capture",
                kind: "text",
                app_bundle_id: Some("com.apple.Notes"),
                window_title: Some("Notes"),
                content,
                content_hash: &hash,
                dwell_ms: 0,
                display_id: None,
                window_bounds: None,
            })
            .unwrap();
        }
        db
    }

    fn active() -> Entitlements {
        entitlements(Plan::Trial { started_at_ms: Some(NOW as u64) }, NOW as u64)
    }

    const QUESTION: &str = "when did we decide to delay the launch";

    #[test]
    fn the_desktop_answers_a_queued_question_once_and_masked() {
        let db = db();
        let job = db.ai_search_job_request(QUESTION, "mcp").unwrap();
        let claimed = db.ai_search_job_claim_next().unwrap();
        assert_eq!(claimed.id, job.id);
        assert!(db.ai_search_jobs_any_running());

        let status = db.ai_search_job_execute(&claimed, &active(), &ExclusionPolicy::new(), Some(&Ranks("Orchid")));
        assert_eq!(status, Ok(AiSearchStatus::AiReranked));

        let done = db.ai_search_job_take(job.id).unwrap().unwrap();
        assert_eq!(done.status, "done");
        let answer: Value = serde_json::from_str(done.result.as_deref().unwrap()).unwrap();
        assert_eq!(answer["status"], "ai_reranked");
        assert_eq!(answer["reason"], Value::Null);
        assert_eq!(answer["mode"], "ai");
        assert_eq!(answer["model_calls"], 2);
        let top = &answer["results"][0];
        assert_eq!(top["ai_rank"], 1);
        assert_eq!(answer["answer"][0]["sources"][0], top["key"]);
        assert!(answer["answer"][0]["text"].as_str().unwrap().contains("November"));
        assert_eq!(top["source_kind"], "event");
        assert!(top["excerpt"].as_str().unwrap().contains("Orchid"));
        assert!(
            !done.result.as_deref().unwrap().contains("sk-ant-api03"),
            "a secret in a matched excerpt is masked before it is stored"
        );
        assert!(db.ai_search_job_take(job.id).unwrap().is_none(), "handed over once");
    }

    #[test]
    fn generated_answer_text_is_masked_at_the_persistence_boundary() {
        let masked = mask_answer(json!({"answer":[{"text":"key sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAA", "sources":["event:1"]}]}));
        assert!(!masked.to_string().contains("sk-ant-api03"));
        assert_eq!(masked["answer"][0]["sources"][0], "event:1");
    }

    #[test]
    fn no_provider_still_answers_with_keyword_results() {
        let db = db();
        let job = db.ai_search_job_request("Orchid release", "api").unwrap();
        let claimed = db.ai_search_job_claim_next().unwrap();
        let status = db.ai_search_job_execute::<Ranks>(&claimed, &active(), &ExclusionPolicy::new(), None);
        assert_eq!(status, Ok(AiSearchStatus::NeedsProvider));
        let answer: Value =
            serde_json::from_str(db.ai_search_job_take(job.id).unwrap().unwrap().result.as_deref().unwrap()).unwrap();
        assert_eq!(answer["status"], "needs_provider");
        assert!(answer["results"].as_array().is_some_and(|r| !r.is_empty()));
    }

    #[test]
    fn an_expired_plan_fails_the_request_without_searching() {
        let db = db();
        let job = db.ai_search_job_request(QUESTION, "cli").unwrap();
        let claimed = db.ai_search_job_claim_next().unwrap();
        let status = db.ai_search_job_execute(
            &claimed,
            &Entitlements::trial_not_started(),
            &ExclusionPolicy::new(),
            Some(&Ranks("Orchid")),
        );
        assert_eq!(status, Err("plan_required"));
        let failed = db.ai_search_job_take(job.id).unwrap().unwrap();
        assert_eq!((failed.status.as_str(), failed.error.as_deref()), ("failed", Some("plan_required")));
        assert_eq!(failed.result, None);
    }

    #[test]
    fn the_faces_fallback_is_keyword_results_labelled_with_the_reason() {
        let db = db();
        let answer = db
            .ai_search_keyword_fallback_json("Orchid release", FallbackReason::AppNotRunning)
            .unwrap();
        assert_eq!(answer["status"], "keyword_fallback");
        assert_eq!(answer["reason"], "app_not_running");
        assert_eq!(answer["model_calls"], 0);
        assert_eq!(answer["results"][0]["keyword_rank"], 1);
        assert_eq!(answer["results"][0]["ai_rank"], Value::Null);
        assert_eq!(
            db.ai_search_keyword_fallback_json("  ", FallbackReason::ReadOnlyToken),
            Err("empty_query")
        );
    }
}
