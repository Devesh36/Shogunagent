//! L2 Clarify (issue #394) on the Memory API faces: `actions.questions` and `actions.answer`.
//!
//! An agent run in the desktop may pause on one focused question. Every face lists the questions
//! waiting (`awaiting_input`) and answers one — MCP (`tools/call`), REST (`GET
//! /v1/actions/questions[/{id}]`, `POST /v1/actions/answer`) and the CLI (`shogun actions
//! questions|answer`, over REST). Each face lifts its own wire shape into one typed
//! [`ClarifyOperation`] and the daemon answers it from the shared store
//! ([`crate::backend::MemoryBackend::clarify`]), so the Notch and every API caller hit the same
//! rules (`shogun_agents::clarify`).
//!
//! **Separate from the L3 approval queue on purpose.** Nothing here reads or writes the L3
//! queue or its durable store, a question id is never an `approval_id`, and an answer is
//! text for the run to read — it confirms no action and approves no send (a source guard in the
//! tests pins that). `actions.status` stays the L3 poller.
//!
//! The origin ([`ApprovalOrigin`]) is set by the face (`Mcp` / `Api`), never read from the body,
//! so a caller cannot pass itself off as the Notch. The engine treats every origin alike; it is
//! kept for the record.

use serde_json::{json, Value};
use shogun_agents::approval::ApprovalOrigin;
use shogun_agents::clarify::{Question, QuestionState, RunRef};

use crate::memory_api::Tool;

/// One Clarify request, already lifted off its wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClarifyOperation {
    /// The questions waiting for an answer, newest first.
    List,
    /// One question and where it stands.
    Get { id: i64 },
    /// Answer one question. `origin` is the face's, not the caller's claim.
    Answer { id: i64, answer: String, origin: ApprovalOrigin },
}

/// Whether `tool` is answered by [`crate::backend::MemoryBackend::clarify`].
pub fn is_clarify_tool(tool: Tool) -> bool {
    matches!(tool, Tool::ActionsQuestions | Tool::ActionsAnswer)
}

/// Build the operation for `tool` from a face's pieces: the request body (MCP arguments / REST
/// POST body, JSON), the id (MCP `id` argument / REST path segment) and the face's own origin.
/// `Err` is a fixed machine code for a malformed request.
pub fn operation(
    tool: Tool,
    body: &str,
    id: Option<i64>,
    origin: ApprovalOrigin,
) -> Result<ClarifyOperation, &'static str> {
    match tool {
        Tool::ActionsQuestions => match id {
            None => Ok(ClarifyOperation::List),
            Some(id) if id > 0 => Ok(ClarifyOperation::Get { id }),
            Some(_) => Err("invalid_id"),
        },
        Tool::ActionsAnswer => {
            let value: Value = serde_json::from_str(body).map_err(|_| "invalid_request")?;
            let id = value
                .get("id")
                .and_then(Value::as_i64)
                .filter(|id| *id > 0)
                .ok_or("missing_id")?;
            let answer = value
                .get("answer")
                .and_then(Value::as_str)
                .ok_or("missing_answer")?
                .to_string();
            Ok(ClarifyOperation::Answer { id, answer, origin })
        }
        _ => Err("not_a_clarify_tool"),
    }
}

/// The HTTP status for a Clarify JSON answer: a closed question is a conflict (409), an unknown
/// one 404, any other refusal 400.
pub fn http_status(json: &str) -> u16 {
    let Ok(value) = serde_json::from_str::<Value>(json) else {
        return 500;
    };
    match value.get("error").and_then(Value::as_str) {
        None => 200,
        Some("not_found" | "unknown_question") => 404,
        Some("already_answered" | "expired" | "cancelled") => 409,
        Some("unavailable") => 503,
        Some(_) => 400,
    }
}

/// A refusal with its machine code (never the text that was refused).
pub fn refusal_json(code: &str, id: Option<i64>) -> String {
    match id {
        Some(id) => json!({ "error": code, "question_id": id }).to_string(),
        None => json!({ "error": code }).to_string(),
    }
}

/// One question as every face reports it.
#[derive(Debug, Clone)]
pub struct QuestionView<'a> {
    pub id: i64,
    pub run: RunRef,
    pub question: &'a Question,
    pub state: QuestionState,
    pub answered_via: Option<ApprovalOrigin>,
    pub choice: Option<i64>,
    pub resolved_at_ms: Option<i64>,
}

/// The shared JSON for one question. The question and options are agent-written (they can quote
/// captured text): data to show, never instructions. The answer text itself is not echoed.
pub fn question_json(view: &QuestionView<'_>) -> Value {
    json!({
        "question_id": view.id,
        "status": view.state.as_str(),
        "run": { "kind": view.run.kind.as_str(), "id": view.run.id },
        "question": view.question.text(),
        "options": view.question.options(),
        "free_text": view.question.free_text(),
        "asked_at_ms": view.question.asked_at_ms(),
        "deadline_ms": view.question.deadline_ms(),
        "answered_via": view.answered_via.map(ApprovalOrigin::as_str),
        "choice": view.choice,
        "resolved_at_ms": view.resolved_at_ms,
    })
}

/// `actions.questions` without an id.
pub fn list_json(views: &[QuestionView<'_>]) -> String {
    let questions: Vec<Value> = views.iter().map(question_json).collect();
    json!({ "count": questions.len(), "questions": questions }).to_string()
}

/// `actions.questions` with an id.
pub fn get_json(view: &QuestionView<'_>) -> String {
    json!({ "question": question_json(view) }).to_string()
}

/// A recorded answer: the run resumes. `level` says what the call was (L1) — it approved nothing.
pub fn answered_json(id: i64, run: RunRef, origin: ApprovalOrigin, choice: Option<usize>) -> String {
    json!({
        "question_id": id,
        "status": QuestionState::Answered.as_str(),
        "run": { "kind": run.kind.as_str(), "id": run.id },
        "answered_via": origin.as_str(),
        "choice": choice,
        "level": "L1",
    })
    .to_string()
}

/// An in-memory Clarify store for face tests (MCP / REST): the same rules as the daemon's
/// (`shogun_agents::clarify::resolve`), the same JSON.
#[cfg(test)]
pub(crate) mod test_store {
    use std::sync::Mutex;

    use shogun_agents::approval::ApprovalOrigin;
    use shogun_agents::clarify::{self, Question, QuestionState, RunRef};

    use super::*;

    pub(crate) struct Entry {
        pub id: i64,
        pub run: RunRef,
        pub question: Question,
        pub state: QuestionState,
        pub answer: Option<String>,
        pub origin: Option<ApprovalOrigin>,
        pub choice: Option<i64>,
    }

    #[derive(Default)]
    pub(crate) struct MemClarify {
        pub entries: Mutex<Vec<Entry>>,
        pub now_ms: u64,
    }

    impl MemClarify {
        pub fn ask(&self, run: RunRef, question: Question) -> i64 {
            let mut entries = self.entries.lock().unwrap();
            let id = entries.len() as i64 + 1;
            entries.push(Entry {
                id,
                run,
                question,
                state: QuestionState::AwaitingInput,
                answer: None,
                origin: None,
                choice: None,
            });
            id
        }

        fn view(e: &Entry) -> QuestionView<'_> {
            QuestionView {
                id: e.id,
                run: e.run,
                question: &e.question,
                state: e.state,
                answered_via: e.origin,
                choice: e.choice,
                resolved_at_ms: None,
            }
        }

        pub fn apply(&self, op: ClarifyOperation) -> String {
            let mut entries = self.entries.lock().unwrap();
            for e in entries.iter_mut() {
                if e.state == QuestionState::AwaitingInput && !e.question.is_open_at(self.now_ms) {
                    e.state = QuestionState::Expired;
                }
            }
            match op {
                ClarifyOperation::List => {
                    let open: Vec<QuestionView<'_>> = entries
                        .iter()
                        .filter(|e| e.state == QuestionState::AwaitingInput)
                        .map(Self::view)
                        .collect();
                    list_json(&open)
                }
                ClarifyOperation::Get { id } => match entries.iter().find(|e| e.id == id) {
                    Some(e) => get_json(&Self::view(e)),
                    None => refusal_json("not_found", None),
                },
                ClarifyOperation::Answer { id, answer, origin } => {
                    let Some(e) = entries.iter_mut().find(|e| e.id == id) else {
                        return refusal_json(clarify::AnswerRefusal::Unknown.as_str(), Some(id));
                    };
                    match clarify::resolve(e.state, &e.question, &answer, self.now_ms) {
                        Ok(ok) => {
                            e.state = QuestionState::Answered;
                            e.answer = Some(ok.text().to_string());
                            e.origin = Some(origin);
                            e.choice = ok.choice().map(|c| c as i64);
                            answered_json(id, e.run, origin, ok.choice())
                        }
                        Err(refusal) => refusal_json(refusal.as_str(), Some(id)),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faces_lift_their_wire_into_one_operation_with_their_own_origin() {
        assert_eq!(operation(Tool::ActionsQuestions, "", None, ApprovalOrigin::Api), Ok(ClarifyOperation::List));
        assert_eq!(
            operation(Tool::ActionsQuestions, "", Some(4), ApprovalOrigin::Api),
            Ok(ClarifyOperation::Get { id: 4 })
        );
        assert_eq!(operation(Tool::ActionsQuestions, "", Some(0), ApprovalOrigin::Api), Err("invalid_id"));
        // A caller cannot claim to be the Notch: the body's origin is never read.
        assert_eq!(
            operation(Tool::ActionsAnswer, r#"{"id":3,"answer":"Sam Lee","origin":"ui"}"#, None, ApprovalOrigin::Mcp),
            Ok(ClarifyOperation::Answer { id: 3, answer: "Sam Lee".into(), origin: ApprovalOrigin::Mcp })
        );
        assert_eq!(operation(Tool::ActionsAnswer, r#"{"answer":"x"}"#, None, ApprovalOrigin::Api), Err("missing_id"));
        assert_eq!(operation(Tool::ActionsAnswer, r#"{"id":-1,"answer":"x"}"#, None, ApprovalOrigin::Api), Err("missing_id"));
        assert_eq!(operation(Tool::ActionsAnswer, r#"{"id":1}"#, None, ApprovalOrigin::Api), Err("missing_answer"));
        assert_eq!(operation(Tool::ActionsAnswer, "not json", None, ApprovalOrigin::Api), Err("invalid_request"));
        assert_eq!(operation(Tool::ActionsStatus, "{}", None, ApprovalOrigin::Api), Err("not_a_clarify_tool"));
    }

    #[test]
    fn statuses_follow_the_refusal_class() {
        assert_eq!(http_status(r#"{"count":0,"questions":[]}"#), 200);
        assert_eq!(http_status(&refusal_json("unknown_question", Some(1))), 404);
        assert_eq!(http_status(&refusal_json("not_found", None)), 404);
        for closed in ["already_answered", "expired", "cancelled"] {
            assert_eq!(http_status(&refusal_json(closed, Some(1))), 409);
        }
        assert_eq!(http_status(&refusal_json("not_an_option", Some(1))), 400);
        assert_eq!(http_status("garbage"), 500);
    }

    /// The L3 separation, pinned at the source: this face never touches the approval queue or
    /// its store, so `actions.answer` cannot approve a send whatever the caller writes.
    #[test]
    fn clarify_faces_never_touch_the_approval_queue() {
        let src = include_str!("clarify_api.rs");
        let body = src.split("#[cfg(test)]").next().unwrap_or(src);
        for forbidden in [
            concat!("approval", "_store::"),
            concat!("Approval", "Queue"),
            concat!("Confirm", "Intent"),
            concat!("poll", "_approval"),
            concat!("rest::", "act("),
        ] {
            assert!(!body.contains(forbidden), "clarify_api.rs must not use {forbidden}");
        }
    }
}
