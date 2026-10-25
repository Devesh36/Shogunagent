//! The L1/L2 execution engine (§6.6.2) — the gate between a proposed [`Action`] and its
//! effect.
//!
//! The engine routes by permission level ([`Action::required_level`]):
//! - **L1** runs immediately (auto-execute) and reports `action.executed`.
//! - **L2** is queued awaiting a one-tap confirm; [`ExecutionEngine::confirm`] runs it (unless it
//!   has expired), [`ExecutionEngine::cancel`] drops it.
//! - **L3** (external sends) is **rejected here** — v1 has no L3 execution path; it opens in M4.
//!   Because a send is rejected rather than "queued as auto", invariant 4 holds at the engine
//!   boundary too: no code path in this engine can auto-run a send.
//!
//! - **Clarify** (issue #394): a run may pause on one focused question
//!   ([`ExecutionEngine::ask`] → [`Disposition::NeedsInput`]). It is not an action: it runs
//!   nothing, sits in its own queue (never the pending-confirm queue), and an answer only hands
//!   text back to the run ([`ExecutionEngine::answer`] → [`AnswerOutcome::Resumed`]). Whatever the
//!   run does next is submitted here like any other action, at its own level — an answer cannot
//!   confirm an L2 action or approve a send. Past its deadline a question expires
//!   ([`ExecutionEngine::expire_questions_due`]); the run keeps its partial result as a local draft.
//!
//! Pure and platform-independent: OS effects go through [`LocalEffector`] and reporting
//! (event-log write + bus publish) through [`ExecutionObserver`], both injected by the daemon.
//! `now_ms` is a parameter, never a clock read, so queueing/expiry is deterministic under test.

use crate::approval::ApprovalOrigin;
use crate::clarify::{self, AnswerRefusal, ClarifyAnswer, Question, QuestionState, RunRef};
use crate::entitlement::Entitlements;
use crate::permission::{Action, Level};

/// A handle for a submitted action (for confirm / cancel).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActionId(pub u64);

/// Why an action was refused at submit time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// An external send (L3). v1 has no L3 execution path (opens in M4).
    ExternalSendNotAvailable,
    /// The current plan does not include agent execution (issue #97): an expired, unpaid trial.
    /// Enforced here in the Rust core — the webview only displays the state.
    PlanNotEntitled,
    /// Clarify (issue #394): this run already has a question waiting. One at a time.
    QuestionAlreadyOpen,
}

/// What happened when an action was submitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// L1: executed immediately.
    AutoRan,
    /// L1: the effector ran and reported an error. Distinct from [`Disposition::AutoRan`] so a
    /// caller keying off the return value (a UI writing "done") cannot mistake a failure for a
    /// success; the observer's `on_failed` carries the error detail.
    Failed,
    /// L2: queued, awaiting a one-tap confirm.
    AwaitingConfirm,
    /// Clarify (issue #394): the run asked one focused question and may carry on with other
    /// sub-steps meanwhile. Answer with [`ExecutionEngine::answer`] (the [`Submitted::id`]); past
    /// `deadline_ms` it expires. Nothing was executed and nothing is queued for confirmation.
    NeedsInput {
        question: String,
        options: Vec<String>,
        free_text: bool,
        deadline_ms: u64,
    },
    /// Refused (see [`RejectReason`]).
    Rejected(RejectReason),
}

/// The result of submitting an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submitted {
    pub id: ActionId,
    pub disposition: Disposition,
}

/// The terminal outcome of a queued (L2) action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Ran successfully.
    Executed,
    /// The user cancelled it before confirming.
    Cancelled,
    /// The confirm window elapsed.
    Expired,
    /// The effector failed; carries the error string (never capture content).
    Failed(String),
    /// No pending action with that id.
    Unknown,
    /// The plan in force at the tap no longer covers it (an ended trial, #940): it was dropped
    /// without running. Callers answer [`crate::entitlement::PLAN_REQUIRED`].
    PlanRequired,
}

/// An answer handed back to the run that asked (issue #394). The run resumes under the same
/// [`RunRef`] and appends [`Resume::answer`] to its untrusted user half
/// ([`clarify::append_to_user_half`]). `origin` is carried for the record only: the engine treats
/// a UI, REST/CLI and MCP answer identically (invariant 6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resume {
    pub id: ActionId,
    pub run: RunRef,
    pub question: Question,
    pub answer: ClarifyAnswer,
    pub origin: ApprovalOrigin,
}

/// The result of answering a question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerOutcome {
    /// Accepted: the run resumes with this answer.
    Resumed(Resume),
    /// The deadline passed first ([`Outcome::Expired`] for the question): the answer is dropped,
    /// the run keeps its partial result as a draft.
    Expired,
    /// Refused; the question is unchanged (still open for a retry when the refusal is not final).
    Refused(AnswerRefusal),
}

/// A question that expired unanswered: the run it belonged to stops the step that needed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpiredQuestion {
    pub id: ActionId,
    pub run: RunRef,
}

/// The OS-effect seam. The desktop app runs the real effect (open app, reveal file, save draft,
/// update state, …); tests inject a double. Only [`crate::permission::LocalAction`]s reach here —
/// sends never do — so an effector cannot be asked to send off-device.
pub trait LocalEffector {
    /// Run a local action. Returns an error string on failure (must not contain captured text).
    fn run(&self, action: &Action) -> Result<(), String>;
}

/// The reporting seam: the daemon wires this to the `action.executed` bus event + the event-log
/// write (FR-MEM-10). The engine calls it on every terminal transition.
pub trait ExecutionObserver {
    fn on_executed(&self, id: ActionId, action: &Action);
    fn on_rejected(&self, id: ActionId, action: &Action, reason: &RejectReason);
    fn on_cancelled(&self, id: ActionId, action: &Action);
    fn on_expired(&self, id: ActionId, action: &Action);
    fn on_failed(&self, id: ActionId, action: &Action, error: &str);
    /// Clarify (issue #394): a run asked a question. Ids only — never the question text.
    fn on_question_asked(&self, _id: ActionId, _run: RunRef) {}
    /// A question was answered. Ids and origin only — never the answer.
    fn on_question_answered(&self, _id: ActionId, _run: RunRef, _origin: ApprovalOrigin) {}
    /// A question expired unanswered.
    fn on_question_expired(&self, _id: ActionId, _run: RunRef) {}
}

/// A queued L2 action awaiting confirmation.
#[derive(Debug, Clone)]
struct Pending {
    id: ActionId,
    action: Action,
    submitted_at_ms: u64,
}

/// A question waiting for an answer. Deliberately holds no [`Action`]: answering it can only
/// hand text back to the run.
#[derive(Debug, Clone)]
struct OpenQuestion {
    id: ActionId,
    run: RunRef,
    question: Question,
}

/// How many closed questions the engine remembers, so a second answer is refused as
/// "already answered" rather than "unknown".
const CLOSED_QUESTION_MEMORY: usize = 64;

/// The execution engine. Owns the pending-confirm queue and the confirm timeout, and — kept
/// apart from it — the open Clarify questions.
pub struct ExecutionEngine<E: LocalEffector, O: ExecutionObserver> {
    effector: E,
    observer: O,
    next_id: u64,
    pending: Vec<Pending>,
    confirm_timeout_ms: u64,
    questions: Vec<OpenQuestion>,
    closed_questions: Vec<(ActionId, QuestionState)>,
}

impl<E: LocalEffector, O: ExecutionObserver> ExecutionEngine<E, O> {
    /// Build an engine with a confirm timeout (how long an L2 action waits for a tap).
    pub fn new(effector: E, observer: O, confirm_timeout_ms: u64) -> Self {
        Self {
            effector,
            observer,
            next_id: 1,
            pending: Vec::new(),
            confirm_timeout_ms,
            questions: Vec::new(),
            closed_questions: Vec::new(),
        }
    }

    fn alloc_id(&mut self) -> ActionId {
        let id = ActionId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        id
    }

    /// Submit an action. L1 runs now; L2 is queued; L3 (send) is rejected. The caller passes the
    /// current [`Entitlements`] (resolved by the desktop layer per decision, issue #97): without
    /// `agent_execution` — an expired trial — *nothing* is executed or queued,
    /// regardless of level. The plan gate runs before the level gate so a non-entitled send is
    /// reported as a plan rejection, but invariant 4 still holds behind it (an entitled send is
    /// still rejected as L3).
    pub fn submit(&mut self, action: Action, now_ms: u64, ent: &Entitlements) -> Submitted {
        let id = self.alloc_id();
        if !ent.agent_execution {
            let reason = RejectReason::PlanNotEntitled;
            self.observer.on_rejected(id, &action, &reason);
            return Submitted { id, disposition: Disposition::Rejected(reason) };
        }
        match action.required_level() {
            Level::L1 => {
                let disposition = self.run_now(id, &action);
                Submitted { id, disposition }
            }
            Level::L2 => {
                self.pending.push(Pending { id, action, submitted_at_ms: now_ms });
                Submitted { id, disposition: Disposition::AwaitingConfirm }
            }
            Level::L3 => {
                let reason = RejectReason::ExternalSendNotAvailable;
                self.observer.on_rejected(id, &action, &reason);
                Submitted { id, disposition: Disposition::Rejected(reason) }
            }
        }
    }

    /// Run an action through the effector now, reporting the outcome. Returns [`Disposition::AutoRan`]
    /// on success — used for the L1 path (and internally by confirm).
    fn run_now(&self, id: ActionId, action: &Action) -> Disposition {
        match self.effector.run(action) {
            Ok(()) => {
                self.observer.on_executed(id, action);
                Disposition::AutoRan
            }
            Err(e) => {
                self.observer.on_failed(id, action, &e);
                Disposition::Failed
            }
        }
    }

    fn take_pending(&mut self, id: ActionId) -> Option<Pending> {
        let idx = self.pending.iter().position(|p| p.id == id)?;
        Some(self.pending.remove(idx))
    }

    /// Confirm a queued L2 action. Expired ones do not run.
    pub fn confirm(&mut self, id: ActionId, now_ms: u64) -> Outcome {
        let Some(p) = self.take_pending(id) else {
            return Outcome::Unknown;
        };
        if now_ms.saturating_sub(p.submitted_at_ms) > self.confirm_timeout_ms {
            self.observer.on_expired(id, &p.action);
            return Outcome::Expired;
        }
        match self.effector.run(&p.action) {
            Ok(()) => {
                self.observer.on_executed(id, &p.action);
                Outcome::Executed
            }
            Err(e) => {
                self.observer.on_failed(id, &p.action, &e);
                Outcome::Failed(e)
            }
        }
    }

    /// [`Self::confirm`] behind the plan gate, with the [`Entitlements`] resolved at the tap. The
    /// plan was checked at submit, but it can change inside the confirm window (a trial ending
    /// between the offer and the tap), and an L2 action reads saved memory and runs as agent
    /// execution, so both must still hold (#940). Otherwise the action is dropped unexecuted,
    /// reported as a plan rejection, and the answer is [`Outcome::PlanRequired`].
    pub fn confirm_entitled(&mut self, id: ActionId, now_ms: u64, ent: &Entitlements) -> Outcome {
        if ent.agent_execution && ent.memory_access {
            return self.confirm(id, now_ms);
        }
        let Some(p) = self.take_pending(id) else {
            return Outcome::Unknown;
        };
        self.observer.on_rejected(id, &p.action, &RejectReason::PlanNotEntitled);
        Outcome::PlanRequired
    }

    /// Cancel a queued L2 action before it is confirmed.
    pub fn cancel(&mut self, id: ActionId) -> Outcome {
        match self.take_pending(id) {
            Some(p) => {
                self.observer.on_cancelled(id, &p.action);
                Outcome::Cancelled
            }
            None => Outcome::Unknown,
        }
    }

    /// Expire every queued action past the confirm timeout. Returns the expired ids. The daemon
    /// calls this on a timer tick.
    pub fn expire_due(&mut self, now_ms: u64) -> Vec<ActionId> {
        let timeout = self.confirm_timeout_ms;
        let (expired, live): (Vec<Pending>, Vec<Pending>) = self
            .pending
            .drain(..)
            .partition(|p| now_ms.saturating_sub(p.submitted_at_ms) > timeout);
        self.pending = live;
        for p in &expired {
            self.observer.on_expired(p.id, &p.action);
        }
        expired.iter().map(|p| p.id).collect()
    }

    /// How many actions are queued awaiting confirmation.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    // Clarify (issue #394)

    /// A run asks one focused question. Non-blocking: this returns at once with
    /// [`Disposition::NeedsInput`] and the run may carry on with other sub-steps. Refused without
    /// agent execution (like [`Self::submit`]) and when the run already has a question waiting.
    pub fn ask(&mut self, run: RunRef, question: Question, ent: &Entitlements) -> Submitted {
        let id = self.alloc_id();
        if !ent.agent_execution {
            return Submitted { id, disposition: Disposition::Rejected(RejectReason::PlanNotEntitled) };
        }
        if self.questions.iter().any(|q| q.run == run) {
            return Submitted {
                id,
                disposition: Disposition::Rejected(RejectReason::QuestionAlreadyOpen),
            };
        }
        let disposition = Disposition::NeedsInput {
            question: question.text().to_string(),
            options: question.options().to_vec(),
            free_text: question.free_text(),
            deadline_ms: question.deadline_ms(),
        };
        self.questions.push(OpenQuestion { id, run, question });
        self.observer.on_question_asked(id, run);
        Submitted { id, disposition }
    }

    fn close_question(&mut self, id: ActionId, state: QuestionState) {
        self.closed_questions.push((id, state));
        if self.closed_questions.len() > CLOSED_QUESTION_MEMORY {
            let excess = self.closed_questions.len() - CLOSED_QUESTION_MEMORY;
            self.closed_questions.drain(..excess);
        }
    }

    /// Answer an open question. Touches only the question queue: never the effector, never the
    /// pending-confirm queue. The same call serves the Notch and the API faces; `origin` is only
    /// recorded. A refusal that is not final (an empty answer, not one of the options) leaves the
    /// question open.
    pub fn answer(
        &mut self,
        id: ActionId,
        raw: &str,
        origin: ApprovalOrigin,
        now_ms: u64,
    ) -> AnswerOutcome {
        let Some(idx) = self.questions.iter().position(|q| q.id == id) else {
            let refusal = match self.closed_questions.iter().rev().find(|(c, _)| *c == id) {
                Some((_, QuestionState::Answered)) => AnswerRefusal::AlreadyAnswered,
                Some((_, QuestionState::Expired)) => AnswerRefusal::Expired,
                Some((_, QuestionState::Cancelled)) => AnswerRefusal::Cancelled,
                Some((_, QuestionState::AwaitingInput)) | None => AnswerRefusal::Unknown,
            };
            return AnswerOutcome::Refused(refusal);
        };
        let open = &self.questions[idx];
        match clarify::resolve(QuestionState::AwaitingInput, &open.question, raw, now_ms) {
            Ok(answer) => {
                let open = self.questions.remove(idx);
                self.close_question(id, QuestionState::Answered);
                self.observer.on_question_answered(id, open.run, origin);
                AnswerOutcome::Resumed(Resume {
                    id,
                    run: open.run,
                    question: open.question,
                    answer,
                    origin,
                })
            }
            Err(AnswerRefusal::Expired) => {
                let open = self.questions.remove(idx);
                self.close_question(id, QuestionState::Expired);
                self.observer.on_question_expired(id, open.run);
                AnswerOutcome::Expired
            }
            Err(refusal) => AnswerOutcome::Refused(refusal),
        }
    }

    /// The run ended before its question was answered. Returns whether one was open.
    pub fn cancel_question(&mut self, id: ActionId) -> bool {
        let Some(idx) = self.questions.iter().position(|q| q.id == id) else {
            return false;
        };
        self.questions.remove(idx);
        self.close_question(id, QuestionState::Cancelled);
        true
    }

    /// Expire every question past its deadline. Each returned run stops the step that needed the
    /// answer and keeps what it produced so far as a local draft — nothing is executed here. The
    /// daemon calls this on the same tick as [`Self::expire_due`].
    pub fn expire_questions_due(&mut self, now_ms: u64) -> Vec<ExpiredQuestion> {
        let (expired, live): (Vec<OpenQuestion>, Vec<OpenQuestion>) =
            self.questions.drain(..).partition(|q| !q.question.is_open_at(now_ms));
        self.questions = live;
        let mut out = Vec::with_capacity(expired.len());
        for q in expired {
            self.close_question(q.id, QuestionState::Expired);
            self.observer.on_question_expired(q.id, q.run);
            out.push(ExpiredQuestion { id: q.id, run: q.run });
        }
        out
    }

    /// The question a run is waiting on, if any.
    pub fn open_question(&self, run: RunRef) -> Option<(ActionId, &Question)> {
        self.questions.iter().find(|q| q.run == run).map(|q| (q.id, &q.question))
    }

    /// How many questions are waiting for an answer.
    pub fn questions_len(&self) -> usize {
        self.questions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entitlement::{entitlements, Plan, TRIAL_DURATION_MS};
    use crate::permission::{LocalAction, SendAction};
    use std::cell::RefCell;

    /// An entitled caller (active trial) — the default for the level-gating tests.
    fn ent() -> Entitlements {
        crate::entitlement::entitlements(crate::entitlement::Plan::Pro, 0)
    }

    /// Records every observer callback and controls whether the effector succeeds.
    #[derive(Default)]
    struct Spy {
        events: RefCell<Vec<String>>,
        fail_with: Option<String>,
    }

    impl Spy {
        fn log(&self, s: String) {
            self.events.borrow_mut().push(s);
        }
        fn events(&self) -> Vec<String> {
            self.events.borrow().clone()
        }
    }

    // The engine takes effector + observer by value; use a shared &Spy via references implementing
    // both traits.
    impl LocalEffector for &Spy {
        fn run(&self, action: &Action) -> Result<(), String> {
            if let Some(e) = &self.fail_with {
                return Err(e.clone());
            }
            self.log(format!("run:{action:?}"));
            Ok(())
        }
    }
    impl ExecutionObserver for &Spy {
        fn on_executed(&self, id: ActionId, _a: &Action) {
            self.log(format!("executed:{}", id.0));
        }
        fn on_rejected(&self, id: ActionId, _a: &Action, r: &RejectReason) {
            self.log(format!("rejected:{}:{r:?}", id.0));
        }
        fn on_cancelled(&self, id: ActionId, _a: &Action) {
            self.log(format!("cancelled:{}", id.0));
        }
        fn on_expired(&self, id: ActionId, _a: &Action) {
            self.log(format!("expired:{}", id.0));
        }
        fn on_failed(&self, id: ActionId, _a: &Action, e: &str) {
            self.log(format!("failed:{}:{e}", id.0));
        }
    }

    fn l1() -> Action {
        Action::Local(LocalAction::LocalSearch { query: "budget".into() })
    }
    fn l2() -> Action {
        Action::Local(LocalAction::UpdateState { table: "people", state_id: 1 })
    }
    fn l3() -> Action {
        Action::Send(SendAction::SendEmail { to: "a@b.com".into() })
    }

    #[test]
    fn l1_auto_runs_and_reports() {
        let spy = Spy::default();
        let mut engine = ExecutionEngine::new(&spy, &spy, 5000);
        let r = engine.submit(l1(), 0, &ent());
        assert_eq!(r.disposition, Disposition::AutoRan);
        assert_eq!(engine.pending_len(), 0);
        assert!(spy.events().iter().any(|e| e.starts_with("executed:")));
    }

    #[test]
    fn l1_effector_failure_is_not_reported_as_auto_ran() {
        let spy = Spy { fail_with: Some("boom".into()), ..Spy::default() };
        let mut engine = ExecutionEngine::new(&spy, &spy, 5000);
        let r = engine.submit(l1(), 0, &ent());
        assert_eq!(r.disposition, Disposition::Failed, "a failed run must not read as success");
        assert!(spy.events().iter().any(|e| e.starts_with("failed:")));
        assert!(!spy.events().iter().any(|e| e.starts_with("executed:")));
    }

    #[test]
    fn l2_awaits_then_confirm_executes() {
        let spy = Spy::default();
        let mut engine = ExecutionEngine::new(&spy, &spy, 5000);
        let r = engine.submit(l2(), 1000, &ent());
        assert_eq!(r.disposition, Disposition::AwaitingConfirm);
        assert_eq!(engine.pending_len(), 1);
        assert!(!spy.events().iter().any(|e| e.starts_with("executed:")));
        let out = engine.confirm(r.id, 2000);
        assert_eq!(out, Outcome::Executed);
        assert_eq!(engine.pending_len(), 0);
    }

    /// #1005: an L2 action offered one second before the trial ended and tapped after it must not
    /// run — the plan is re-read at the tap, not trusted from the offer.
    #[test]
    fn l2_confirm_after_the_trial_ended_is_refused_with_plan_required() {
        let spy = Spy::default();
        let mut engine = ExecutionEngine::new(&spy, &spy, 8_000);
        let start = 1_000_000;
        let trial = Plan::Trial { started_at_ms: Some(start) };
        let offered_at = start + TRIAL_DURATION_MS - 1_000;
        let r = engine.submit(l2(), offered_at, &entitlements(trial, offered_at));
        assert_eq!(r.disposition, Disposition::AwaitingConfirm);

        let tapped_at = offered_at + 2_000; // inside the confirm window, after the trial
        let out = engine.confirm_entitled(r.id, tapped_at, &entitlements(trial, tapped_at));
        assert_eq!(out, Outcome::PlanRequired);
        assert_eq!(engine.pending_len(), 0, "a refused action is not left to be confirmed later");
        assert!(!spy.events().iter().any(|e| e.starts_with("run:") || e.starts_with("executed:")));
        assert!(spy.events().contains(&format!("rejected:{}:PlanNotEntitled", r.id.0)));
        // A second tap finds nothing to run.
        assert_eq!(engine.confirm_entitled(r.id, tapped_at, &ent()), Outcome::Unknown);
    }

    #[test]
    fn l2_confirm_entitled_runs_under_a_covering_plan() {
        let spy = Spy::default();
        let mut engine = ExecutionEngine::new(&spy, &spy, 5000);
        let r = engine.submit(l2(), 0, &ent());
        assert_eq!(engine.confirm_entitled(r.id, 1000, &entitlements(Plan::Standard, 1000)), Outcome::Executed);
        // Expiry is still enforced behind the plan gate.
        let r = engine.submit(l2(), 0, &ent());
        assert_eq!(engine.confirm_entitled(r.id, 6000, &ent()), Outcome::Expired);
    }

    #[test]
    fn l2_confirm_after_timeout_is_expired_not_run() {
        let spy = Spy::default();
        let mut engine = ExecutionEngine::new(&spy, &spy, 5000);
        let r = engine.submit(l2(), 0, &ent());
        let out = engine.confirm(r.id, 6000); // 6000 > 5000 timeout
        assert_eq!(out, Outcome::Expired);
        assert!(spy.events().iter().any(|e| e.starts_with("expired:")));
        assert!(!spy.events().iter().any(|e| e.starts_with("executed:")));
    }

    #[test]
    fn l2_cancel_drops_without_running() {
        let spy = Spy::default();
        let mut engine = ExecutionEngine::new(&spy, &spy, 5000);
        let r = engine.submit(l2(), 0, &ent());
        let out = engine.cancel(r.id);
        assert_eq!(out, Outcome::Cancelled);
        assert_eq!(engine.pending_len(), 0);
        assert!(spy.events().iter().any(|e| e.starts_with("cancelled:")));
        // A confirm after cancel is rejected and nothing runs.
        assert_eq!(engine.confirm(r.id, 1), Outcome::Unknown);
        assert!(!spy.events().iter().any(|e| e.starts_with("executed:")));
    }

    #[test]
    fn l3_send_is_rejected_never_run() {
        let spy = Spy::default();
        let mut engine = ExecutionEngine::new(&spy, &spy, 5000);
        let r = engine.submit(l3(), 0, &ent());
        assert_eq!(
            r.disposition,
            Disposition::Rejected(RejectReason::ExternalSendNotAvailable)
        );
        assert_eq!(engine.pending_len(), 0);
        // never handed to the effector
        assert!(!spy.events().iter().any(|e| e.starts_with("run:")));
        assert!(spy.events().iter().any(|e| e.starts_with("rejected:")));
    }

    #[test]
    fn effector_failure_is_reported() {
        let spy = Spy { fail_with: Some("no such app".into()), ..Default::default() };
        let mut engine = ExecutionEngine::new(&spy, &spy, 5000);
        engine.submit(l1(), 0, &ent());
        assert!(spy.events().iter().any(|e| e == "failed:1:no such app"));
    }

    #[test]
    fn expire_due_sweeps_only_stale_pending() {
        let spy = Spy::default();
        let mut engine = ExecutionEngine::new(&spy, &spy, 5000);
        let a = engine.submit(l2(), 0, &ent()); // submitted at 0
        let b = engine.submit(l2(), 4000, &ent()); // submitted at 4000
        // at t=6000: a is 6000ms old (>5000, stale); b is 2000ms old (live)
        let expired = engine.expire_due(6000);
        assert_eq!(expired, vec![a.id]);
        assert_eq!(engine.pending_len(), 1);
        // b still confirmable
        assert_eq!(engine.confirm(b.id, 6500), Outcome::Executed);
    }

    #[test]
    fn standard_plan_runs_the_engine_like_pro() {
        // L1 runs, L2 queues, L3 is still the level rejection (invariant 4), never a plan
        // rejection.
        let spy = Spy::default();
        let mut engine = ExecutionEngine::new(&spy, &spy, 5000);
        let standard = entitlements(Plan::Standard, 0);
        assert_eq!(engine.submit(l1(), 0, &standard).disposition, Disposition::AutoRan);
        assert_eq!(engine.submit(l2(), 0, &standard).disposition, Disposition::AwaitingConfirm);
        assert_eq!(engine.pending_len(), 1);
        let r = engine.submit(l3(), 0, &standard);
        assert!(
            !matches!(r.disposition, Disposition::Rejected(RejectReason::PlanNotEntitled)),
            "a paid plan is never plan-rejected"
        );
        assert!(spy.events().iter().any(|e| e.starts_with("run:")), "L1 reached the effector");
    }

    #[test]
    fn an_expired_trial_rejects_every_level_without_running() {
        // The one status without agent execution: L1 does not run, L2 is not queued.
        let spy = Spy::default();
        let mut engine = ExecutionEngine::new(&spy, &spy, 5000);
        let expired = entitlements(Plan::Trial { started_at_ms: Some(0) }, TRIAL_DURATION_MS);
        for action in [l1(), l2(), l3()] {
            let r = engine.submit(action, 0, &expired);
            assert_eq!(r.disposition, Disposition::Rejected(RejectReason::PlanNotEntitled));
        }
        assert_eq!(engine.pending_len(), 0);
        assert!(!spy.events().iter().any(|e| e.starts_with("run:")), "effector never touched");
        assert_eq!(spy.events().iter().filter(|e| e.starts_with("rejected:")).count(), 3);
    }

    #[test]
    fn expired_trial_rejects_but_active_trial_runs() {
        let spy = Spy::default();
        let mut engine = ExecutionEngine::new(&spy, &spy, 5000);
        let plan = Plan::Trial { started_at_ms: Some(0) };
        // active trial: L1 auto-runs
        let active = entitlements(plan, TRIAL_DURATION_MS - 1);
        assert_eq!(engine.submit(l1(), 0, &active).disposition, Disposition::AutoRan);
        // expired trial: locked
        let expired = entitlements(plan, TRIAL_DURATION_MS);
        assert_eq!(
            engine.submit(l1(), 0, &expired).disposition,
            Disposition::Rejected(RejectReason::PlanNotEntitled)
        );
    }

    // Clarify (issue #394)

    use crate::clarify::{RunKind, DEFAULT_TTL_MS};

    fn run(id: i64) -> RunRef {
        RunRef { kind: RunKind::SkillRun, id }
    }

    fn question(options: &[&str], free_text: bool) -> Question {
        Question::new("Which Sam — Sam Lee or Sam Ortiz?", options, free_text, 1_000, None).unwrap()
    }

    #[test]
    fn ask_needs_input_then_answer_resumes_the_same_run() {
        let spy = Spy::default();
        let mut engine = ExecutionEngine::new(&spy, &spy, 5000);
        let r = engine.ask(run(7), question(&["Sam Lee", "Sam Ortiz"], true), &ent());
        match &r.disposition {
            Disposition::NeedsInput { question, options, free_text, deadline_ms } => {
                assert!(question.starts_with("Which Sam"));
                assert_eq!(options, &["Sam Lee", "Sam Ortiz"]);
                assert!(*free_text);
                assert_eq!(*deadline_ms, 1_000 + DEFAULT_TTL_MS);
            }
            other => panic!("expected NeedsInput, got {other:?}"),
        }
        // Non-blocking: nothing ran, nothing is waiting for a confirm, the run can keep going.
        assert_eq!(engine.pending_len(), 0);
        assert_eq!(engine.questions_len(), 1);
        assert_eq!(engine.open_question(run(7)).map(|(id, _)| id), Some(r.id));
        assert!(!spy.events().iter().any(|e| e.starts_with("run:")));

        match engine.answer(r.id, "sam ortiz", ApprovalOrigin::Ui, 2_000) {
            AnswerOutcome::Resumed(resume) => {
                assert_eq!(resume.run, run(7), "the same run resumes");
                assert_eq!(resume.id, r.id);
                assert_eq!((resume.answer.text(), resume.answer.choice()), ("Sam Ortiz", Some(1)));
                let user = clarify::append_to_user_half("ctx", &resume.question, &resume.answer);
                assert!(user.ends_with("Answer: Sam Ortiz"));
            }
            other => panic!("expected Resumed, got {other:?}"),
        }
        assert_eq!(engine.questions_len(), 0);
        assert!(engine.open_question(run(7)).is_none());
    }

    #[test]
    fn a_second_answer_is_refused_and_the_first_stands() {
        let spy = Spy::default();
        let mut engine = ExecutionEngine::new(&spy, &spy, 5000);
        let r = engine.ask(run(1), question(&[], true), &ent());
        assert!(matches!(engine.answer(r.id, "Sam Lee", ApprovalOrigin::Mcp, 2_000), AnswerOutcome::Resumed(_)));
        assert_eq!(
            engine.answer(r.id, "Sam Ortiz", ApprovalOrigin::Ui, 2_001),
            AnswerOutcome::Refused(AnswerRefusal::AlreadyAnswered)
        );
    }

    #[test]
    fn an_unknown_id_or_a_bad_answer_is_refused() {
        let spy = Spy::default();
        let mut engine = ExecutionEngine::new(&spy, &spy, 5000);
        assert_eq!(
            engine.answer(ActionId(999), "x", ApprovalOrigin::Api, 0),
            AnswerOutcome::Refused(AnswerRefusal::Unknown)
        );
        let r = engine.ask(run(1), question(&["Yes", "No"], false), &ent());
        assert_eq!(
            engine.answer(r.id, "Maybe", ApprovalOrigin::Api, 2_000),
            AnswerOutcome::Refused(AnswerRefusal::NotAnOption)
        );
        assert_eq!(engine.answer(r.id, " ", ApprovalOrigin::Api, 2_000), AnswerOutcome::Refused(AnswerRefusal::Empty));
        // A retryable refusal leaves the question open.
        assert_eq!(engine.questions_len(), 1);
        assert!(matches!(engine.answer(r.id, "no", ApprovalOrigin::Api, 2_000), AnswerOutcome::Resumed(_)));
    }

    #[test]
    fn unanswered_by_the_deadline_expires_without_running_anything() {
        let spy = Spy::default();
        let mut engine = ExecutionEngine::new(&spy, &spy, 5000);
        let a = engine.ask(run(1), question(&[], true), &ent());
        let q2 = Question::new("Which folder?", &[], true, 1_000, Some(20 * 60 * 1000)).unwrap();
        let b = engine.ask(run(2), q2, &ent());
        let deadline = 1_000 + DEFAULT_TTL_MS;
        assert!(engine.expire_questions_due(deadline - 1).is_empty(), "not yet");
        let expired = engine.expire_questions_due(deadline);
        assert_eq!(expired, vec![ExpiredQuestion { id: a.id, run: run(1) }]);
        assert_eq!(engine.questions_len(), 1, "the longer question is still open");
        assert!(engine.open_question(run(2)).is_some());
        // An answer after expiry is refused; nothing ever reached the effector.
        assert_eq!(
            engine.answer(a.id, "Sam Lee", ApprovalOrigin::Ui, deadline),
            AnswerOutcome::Refused(AnswerRefusal::Expired)
        );
        assert!(!spy.events().iter().any(|e| e.starts_with("run:") || e.starts_with("executed:")));
        // Answering late (before the sweep ran) is the same Expired outcome.
        let late = 1_000 + 20 * 60 * 1000;
        assert_eq!(engine.answer(b.id, "Docs", ApprovalOrigin::Ui, late), AnswerOutcome::Expired);
        assert_eq!(engine.questions_len(), 0);
    }

    #[test]
    fn one_question_per_run_and_the_plan_gate_applies() {
        let spy = Spy::default();
        let mut engine = ExecutionEngine::new(&spy, &spy, 5000);
        let first = engine.ask(run(1), question(&[], true), &ent());
        assert!(matches!(first.disposition, Disposition::NeedsInput { .. }));
        let second = engine.ask(run(1), question(&[], true), &ent());
        assert_eq!(second.disposition, Disposition::Rejected(RejectReason::QuestionAlreadyOpen));
        // Another run may ask its own.
        assert!(matches!(engine.ask(run(2), question(&[], true), &ent()).disposition, Disposition::NeedsInput { .. }));
        let expired = entitlements(Plan::Trial { started_at_ms: Some(0) }, TRIAL_DURATION_MS);
        assert_eq!(
            engine.ask(run(3), question(&[], true), &expired).disposition,
            Disposition::Rejected(RejectReason::PlanNotEntitled)
        );
        // A cancelled run's question is closed.
        assert!(engine.cancel_question(first.id));
        assert_eq!(
            engine.answer(first.id, "x", ApprovalOrigin::Ui, 2_000),
            AnswerOutcome::Refused(AnswerRefusal::Cancelled)
        );
    }

    /// Issue #394 acceptance: "permission level unchanged by any answer". A question is never an
    /// approval: answering "send it" confirms no L2 action, approves no send, and the run's next
    /// send is still rejected at L3 exactly as before the question.
    #[test]
    fn answering_never_confirms_l2_or_approves_l3() {
        let spy = Spy::default();
        let mut engine = ExecutionEngine::new(&spy, &spy, 5000);
        let l2_pending = engine.submit(l2(), 1_000, &ent());
        assert_eq!(l2_pending.disposition, Disposition::AwaitingConfirm);
        let q = engine.ask(run(1), question(&["Send it", "Hold"], true), &ent());

        // The answer names a send; the engine hands back text and does nothing else.
        let AnswerOutcome::Resumed(resume) = engine.answer(q.id, "Send it", ApprovalOrigin::Mcp, 1_500) else {
            panic!("expected Resumed");
        };
        assert_eq!(resume.answer.text(), "Send it");
        assert_eq!(engine.pending_len(), 1, "the L2 action still waits for its own tap");
        assert!(!spy.events().iter().any(|e| e.starts_with("run:") || e.starts_with("executed:")));

        // The id spaces do not cross: a question id is not confirmable, an action id is not
        // answerable.
        assert_eq!(engine.confirm(q.id, 1_600), Outcome::Unknown);
        assert_eq!(
            engine.answer(l2_pending.id, "yes", ApprovalOrigin::Ui, 1_600),
            AnswerOutcome::Refused(AnswerRefusal::Unknown)
        );
        assert_eq!(engine.pending_len(), 1);

        // The run's next step is gated at its own level, unchanged by the answer.
        assert_eq!(
            engine.submit(l3(), 1_700, &ent()).disposition,
            Disposition::Rejected(RejectReason::ExternalSendNotAvailable)
        );
        assert_eq!(engine.submit(l2(), 1_700, &ent()).disposition, Disposition::AwaitingConfirm);
    }

    #[test]
    fn ui_and_api_answers_are_indistinguishable_to_the_run() {
        let mut out = Vec::new();
        for origin in [ApprovalOrigin::Ui, ApprovalOrigin::Api, ApprovalOrigin::Mcp] {
            let spy = Spy::default();
            let mut engine = ExecutionEngine::new(&spy, &spy, 5000);
            let q = engine.ask(run(1), question(&["Sam Lee"], true), &ent());
            let AnswerOutcome::Resumed(resume) = engine.answer(q.id, "Sam Lee", origin, 2_000) else {
                panic!("expected Resumed");
            };
            assert_eq!(resume.origin, origin, "carried for the record");
            let prompt = clarify::append_to_user_half("u", &resume.question, &resume.answer);
            out.push((resume.run, resume.answer, prompt));
        }
        assert!(out.windows(2).all(|w| w[0] == w[1]), "same run, same answer, same prompt text");
    }

    #[test]
    fn confirm_or_cancel_unknown_id_is_unknown() {
        let spy = Spy::default();
        let mut engine = ExecutionEngine::new(&spy, &spy, 5000);
        assert_eq!(engine.confirm(ActionId(999), 0), Outcome::Unknown);
        assert_eq!(engine.cancel(ActionId(999)), Outcome::Unknown);
    }
}
