//! L2 Clarify (issue #394): the one implementation every face calls — the desktop skill runner
//! that asks and resumes, the Notch card (desktop `clarify_card.rs`), and `actions.questions` /
//! `actions.answer` on MCP, REST and CLI (invariant 6).
//!
//! The rules are `shogun_agents::clarify` (question shape, answer check, the transition); the
//! rows are `shogun_memory::clarify_questions`; the JSON is `shogun_mcp::clarify_api`, so the
//! faces cannot drift. Nothing here touches the L3 approval queue: an answer is text the run
//! reads on its untrusted user half, and it grants nothing. Nothing here logs the question or the
//! answer.

use shogun_agents::approval::ApprovalOrigin;
use shogun_agents::clarify::{
    self, AnswerRefusal, ClarifyAnswer, Question, QuestionState, RunKind, RunRef,
};
use shogun_mcp::clarify_api::{self, ClarifyOperation, QuestionView};
use shogun_memory::clarify_questions::{self as store, AskError, ClarifyRow};

use super::*;

/// Why a run's question was not recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClarifyAskError {
    /// The run already has a question waiting (one at a time).
    AlreadyOpen,
    /// The question or an option reads as an instruction to a model; it was not stored.
    Withheld,
    /// The store failed.
    Store,
}

/// A row read back as the pure types. `None` for a row this build cannot read (a run kind from a
/// newer build, a malformed question) — it is treated as unknown, never answered.
/// Where one question stands, for the run that asked ([`Db::clarify_poll`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClarifyPoll {
    pub run: RunRef,
    pub state: QuestionState,
    /// Once answered: the question and the answer to append to the run's untrusted user half
    /// ([`clarify::append_to_user_half`]).
    pub resumed: Option<(Question, ClarifyAnswer)>,
}

fn parts(row: &ClarifyRow) -> Option<(RunRef, Question, QuestionState)> {
    let kind = RunKind::parse(&row.run_kind)?;
    let question = Question::from_parts(
        &row.question,
        row.options.clone(),
        row.free_text,
        u64::try_from(row.created_at).ok()?,
        u64::try_from(row.deadline_at).ok()?,
    )
    .ok()?;
    let state = QuestionState::parse(&row.status)?;
    Some((RunRef { kind, id: row.run_id }, question, state))
}

fn refusal_after_race(row: Option<ClarifyRow>) -> AnswerRefusal {
    match row.and_then(|r| QuestionState::parse(&r.status)) {
        Some(QuestionState::Answered) => AnswerRefusal::AlreadyAnswered,
        Some(QuestionState::Expired) => AnswerRefusal::Expired,
        Some(QuestionState::Cancelled) => AnswerRefusal::Cancelled,
        // Still open after a refused write means the deadline passed between the check and the
        // write — the same outcome as an expired question.
        Some(QuestionState::AwaitingInput) => AnswerRefusal::Expired,
        None => AnswerRefusal::Unknown,
    }
}

impl Db {
    /// A run asks one focused question (the desktop runner). `question` was built with
    /// [`Question::new`] at [`Db::now_ms`]. Returns the question id the faces answer by.
    pub fn clarify_ask(&self, run: RunRef, question: &Question) -> Result<i64, ClarifyAskError> {
        let now = self.now_ms();
        let asked = i64::try_from(question.asked_at_ms()).map_err(|_| ClarifyAskError::Store)?;
        let deadline = i64::try_from(question.deadline_ms()).map_err(|_| ClarifyAskError::Store)?;
        let result = self.with_conn("clarify.ask", |c| {
            store::expire_due(c, now)?;
            Ok::<_, rusqlite::Error>(store::ask(
                c,
                run.kind.as_str(),
                run.id,
                question.text(),
                question.options(),
                question.free_text(),
                asked,
                deadline,
            ))
        });
        match result {
            Ok(Ok(row)) => Ok(row.id),
            Ok(Err(AskError::AlreadyOpen)) => Err(ClarifyAskError::AlreadyOpen),
            Ok(Err(AskError::Withheld)) => Err(ClarifyAskError::Withheld),
            Ok(Err(AskError::Db(_))) | Err(_) => Err(ClarifyAskError::Store),
        }
    }

    /// Close every question past its deadline. The runner calls this on its tick; every read and
    /// answer also sweeps first, so a late question is never reported open without it.
    pub fn clarify_expire_due(&self) -> usize {
        let now = self.now_ms();
        self.with_conn("clarify.expire", |c| store::expire_due(c, now)).unwrap_or(0)
    }

    /// The run ended another way: close its open question, if any.
    pub fn clarify_cancel_run(&self, run: RunRef) -> bool {
        let now = self.now_ms();
        self.with_conn("clarify.cancel", |c| store::cancel_for_run(c, run.kind.as_str(), run.id, now))
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    /// Where one question stands, for the run that asked. `None` for an unknown id.
    pub fn clarify_poll(&self, id: i64) -> Option<ClarifyPoll> {
        let now = self.now_ms();
        let row = self
            .with_conn("clarify.poll", |c| {
                store::expire_due(c, now)?;
                store::get(c, id)
            })
            .ok()
            .flatten()?;
        let (run, question, state) = parts(&row)?;
        let resumed = match (state, row.answer.as_deref()) {
            (QuestionState::Answered, Some(text)) => {
                question.check_answer(text).ok().map(|answer| (question, answer))
            }
            _ => None,
        };
        Some(ClarifyPoll { run, state, resumed })
    }

    /// Answer one question from any face. Returns the JSON the face renders: the answered record,
    /// or a refusal with its machine code. `Err` is a store fault only.
    pub fn clarify_answer(&self, id: i64, raw: &str, origin: ApprovalOrigin) -> Result<String, String> {
        let now = self.now_ms();
        let now_u = u64::try_from(now).unwrap_or(0);
        self.with_conn("clarify.answer", |c| {
            store::expire_due(c, now)?;
            let parsed = store::get(c, id)?.and_then(|row| parts(&row));
            let Some((run, question, state)) = parsed else {
                return Ok(clarify_api::refusal_json(AnswerRefusal::Unknown.as_str(), Some(id)));
            };
            let answer = match clarify::resolve(state, &question, raw, now_u) {
                Ok(answer) => answer,
                Err(refusal) => return Ok(clarify_api::refusal_json(refusal.as_str(), Some(id))),
            };
            let choice = answer.choice().and_then(|c| i64::try_from(c).ok());
            if store::record_answer(c, id, answer.text(), choice, origin.as_str(), now)? {
                Ok::<_, rusqlite::Error>(clarify_api::answered_json(id, run, origin, answer.choice()))
            } else {
                let refusal = refusal_after_race(store::get(c, id)?);
                Ok(clarify_api::refusal_json(refusal.as_str(), Some(id)))
            }
        })
        .map_err(|fault| format!("clarify answer failed: {}", fault.as_str()))
    }

    /// `actions.questions` (list / one) and `actions.answer` for every API face.
    pub fn clarify_api(&self, operation: ClarifyOperation) -> Result<String, String> {
        let now = self.now_ms();
        match operation {
            ClarifyOperation::Answer { id, answer, origin } => self.clarify_answer(id, &answer, origin),
            ClarifyOperation::List => {
                let rows = self
                    .with_conn("clarify.list", |c| {
                        store::expire_due(c, now)?;
                        store::open(c)
                    })
                    .map_err(|fault| format!("clarify list failed: {}", fault.as_str()))?;
                let parsed: Vec<(ClarifyRow, RunRef, Question, QuestionState)> = rows
                    .into_iter()
                    .filter_map(|row| parts(&row).map(|(run, q, state)| (row, run, q, state)))
                    .collect();
                let views: Vec<QuestionView<'_>> = parsed.iter().map(|(row, run, q, state)| view(row, *run, q, *state)).collect();
                Ok(clarify_api::list_json(&views))
            }
            ClarifyOperation::Get { id } => {
                let row = self
                    .with_conn("clarify.get", |c| {
                        store::expire_due(c, now)?;
                        store::get(c, id)
                    })
                    .map_err(|fault| format!("clarify get failed: {}", fault.as_str()))?;
                Ok(match row.as_ref().and_then(|row| parts(row).map(|p| (row, p))) {
                    Some((row, (run, question, state))) => {
                        clarify_api::get_json(&view(row, run, &question, state))
                    }
                    None => clarify_api::refusal_json("not_found", None),
                })
            }
        }
    }
}

fn view<'a>(row: &ClarifyRow, run: RunRef, question: &'a Question, state: QuestionState) -> QuestionView<'a> {
    QuestionView {
        id: row.id,
        run,
        question,
        state,
        answered_via: row.answer_origin.as_deref().and_then(ApprovalOrigin::parse),
        choice: row.answer_choice,
        resolved_at_ms: row.resolved_at,
    }
}
