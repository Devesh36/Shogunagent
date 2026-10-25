//! Translation / reply-draft hand-offs from the Memory API (issue #279, V66): the queue the API
//! faces write and the desktop drains.
//!
//! Every text field is masked for secrets before it is written (the DB-write rule every other
//! stored body follows). Rows are never a history: see `shogun_memory::translate_jobs`.

use super::*;
use shogun_memory::translate_jobs::{self, TranslateJob};

use crate::llm::AgentClient;
use crate::translate_job::{JobFailure, JobRequest, JobSettings};

fn mask(text: &str) -> String {
    shogun_memory::sanitize::persist_body(text)
        .text
        .into_owned()
}

/// Mask the free-text fields of a result before it is stored. Ids, tags and flags pass through.
fn mask_result(mut result: serde_json::Value) -> serde_json::Value {
    for key in ["text", "draft", "back_translation"] {
        if let Some(serde_json::Value::String(s)) = result.get_mut(key) {
            *s = mask(s);
        }
    }
    result
}

impl Db {
    /// Queue a validated request from `origin` (`api` | `mcp` | `cli`).
    pub fn translate_job_request(
        &self,
        request: &JobRequest,
        origin: &str,
    ) -> Result<TranslateJob, String> {
        let now = self.now_ms();
        let stored = request.clone().map_text(mask).to_json();
        let kind = request.kind();
        self.with_conn("translate.job_request", |c| {
            translate_jobs::request(c, kind, origin, &stored, now)
        })
        .map_err(|fault| format!("translate request failed: {}", fault.as_str()))
    }

    /// Read one request; a finished one is deleted as it is returned (it is being handed over).
    pub fn translate_job_take(&self, id: i64) -> Result<Option<TranslateJob>, String> {
        self.with_conn("translate.job_take", |c| {
            translate_jobs::take_terminal(c, id)
        })
        .map_err(|fault| format!("translate status failed: {}", fault.as_str()))
    }

    /// Drop a request nobody will read (the caller gave up before the desktop claimed it).
    pub fn translate_job_discard(&self, id: i64) -> bool {
        self.with_conn("translate.job_discard", |c| translate_jobs::discard(c, id))
            .unwrap_or(false)
    }

    /// Whether the desktop is working on a request right now.
    pub fn translate_jobs_any_running(&self) -> bool {
        let now = self.now_ms();
        self.with_conn("translate.any_running", |c| {
            translate_jobs::any_running(c, now)
        })
        .unwrap_or(false)
    }

    /// The desktop runner takes the next pending request.
    pub fn translate_job_claim_next(&self) -> Option<TranslateJob> {
        let now = self.now_ms();
        self.with_conn("translate.job_claim", |c| {
            translate_jobs::claim_next(c, now)
        })
        .ok()
        .flatten()
    }

    /// Record how a claimed request ended: a result (masked here) or a failure code.
    pub fn translate_job_finish(&self, id: i64, outcome: Result<serde_json::Value, &str>) -> bool {
        let now = self.now_ms();
        let stored = outcome.map(|v| mask_result(v).to_string());
        self.with_conn("translate.job_finish", |c| {
            translate_jobs::finish(c, id, stored.as_deref().map_err(|e| *e), now)
        })
        .unwrap_or(false)
    }

    /// Requests a crash left running fail rather than rerun (desktop start).
    pub fn translate_jobs_fail_interrupted(&self) -> usize {
        let now = self.now_ms();
        self.with_conn("translate.fail_interrupted", |c| {
            translate_jobs::fail_interrupted(c, now)
        })
        .unwrap_or(0)
    }

    /// Run one claimed request to the end and record the outcome: the whole desktop side except
    /// building the client and deciding the plan. `agent` is `None` when the user has no usable
    /// Agent-lane credential; `allowed` is the plan / setup gate the desktop decided.
    ///
    /// Returns the failure code, or `None` on success, so the caller can latch or clear a
    /// rejected key the way every other Agent-lane caller does.
    pub fn translate_job_execute<A: AgentClient + ?Sized>(
        &self,
        job: &TranslateJob,
        allowed: bool,
        settings: &JobSettings,
        agent: Option<&A>,
    ) -> Option<&'static str> {
        let outcome: Result<serde_json::Value, &'static str> = if !allowed {
            Err("plan_required")
        } else {
            match (JobRequest::parse(&job.kind, &job.request), agent) {
                (Err(e), _) => Err(JobFailure::Rejected(e).code()),
                (Ok(_), None) => Err("no_agent"),
                (Ok(request), Some(agent)) => {
                    crate::translate_job::run(&request, settings, agent).map_err(|e| e.code())
                }
            }
        };
        let failure = outcome.as_ref().err().copied();
        if let Some(code) = failure {
            // A code only — never the request text or provider detail.
            crate::elog!("[translate-api] request={} failed: {code}", job.id);
        }
        self.translate_job_finish(job.id, outcome);
        failure
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::llm::LlmError;
    use crate::translate::LanguagePair;
    use crate::translate_job::KIND_TRANSLATE;

    struct Echo(Mutex<Vec<String>>);
    impl AgentClient for Echo {
        fn complete(&self, prompt: &str) -> Result<String, LlmError> {
            self.0.lock().unwrap().push(prompt.to_string());
            Ok("こんにちは".to_string())
        }
        fn complete_split(&self, _system: &str, user: &str) -> Result<String, LlmError> {
            self.complete(user)
        }
    }

    fn db() -> Db {
        Db::open_in_memory(Arc::new(|| 1_000)).unwrap()
    }

    #[test]
    fn a_request_is_masked_run_on_the_given_client_and_handed_over_once() {
        let db = db();
        let secret = "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789";
        let body = serde_json::json!({ "text": format!("hello, my key is {secret}") }).to_string();
        let request = JobRequest::parse(KIND_TRANSLATE, &body).unwrap();
        let job = db.translate_job_request(&request, "mcp").unwrap();
        assert!(
            !job.request.contains(secret),
            "stored masked: {}",
            job.request
        );

        let claimed = db.translate_job_claim_next().unwrap();
        assert!(db.translate_jobs_any_running());
        let settings = JobSettings {
            pair: LanguagePair::from_locale("ja"),
            ..JobSettings::default()
        };
        let agent = Echo(Mutex::new(Vec::new()));
        assert_eq!(
            db.translate_job_execute(&claimed, true, &settings, Some(&agent)),
            None
        );
        // The provider saw the masked text too: a key in the text never reaches it.
        assert!(!agent.0.lock().unwrap()[0].contains(secret));

        let done = db.translate_job_take(job.id).unwrap().unwrap();
        assert_eq!(done.status, "done");
        let result: serde_json::Value =
            serde_json::from_str(done.result.as_deref().unwrap()).unwrap();
        assert_eq!(result["text"], "こんにちは");
        assert!(
            db.translate_job_take(job.id).unwrap().is_none(),
            "handed over once"
        );
    }

    #[test]
    fn no_plan_or_no_credential_fails_with_a_code_and_calls_nothing() {
        let db = db();
        let request = JobRequest::parse(KIND_TRANSLATE, r#"{"text":"hello"}"#).unwrap();
        let settings = JobSettings::default();
        let agent = Echo(Mutex::new(Vec::new()));

        let job = db.translate_job_request(&request, "api").unwrap();
        let claimed = db.translate_job_claim_next().unwrap();
        assert_eq!(
            db.translate_job_execute(&claimed, false, &settings, Some(&agent)),
            Some("plan_required")
        );
        let failed = db.translate_job_take(job.id).unwrap().unwrap();
        assert_eq!(failed.error.as_deref(), Some("plan_required"));

        let job = db.translate_job_request(&request, "cli").unwrap();
        let claimed = db.translate_job_claim_next().unwrap();
        assert_eq!(
            db.translate_job_execute::<Echo>(&claimed, true, &settings, None),
            Some("no_agent")
        );
        assert_eq!(
            db.translate_job_take(job.id)
                .unwrap()
                .unwrap()
                .error
                .as_deref(),
            Some("no_agent")
        );
        assert!(
            agent.0.lock().unwrap().is_empty(),
            "no call without the plan or a credential"
        );
    }
}
