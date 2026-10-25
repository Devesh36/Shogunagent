//! Mid-run steering (issue #395) through the **real** tool loop: a note added while a run is
//! going reaches the model's next turn on its user half, and nothing it says can move a send past
//! the L3 approval queue.
//!
//! The model is a fake that obeys every note it reads — the worst case. The runner, the gate, the
//! catalog and the loop are the production ones; the proposal sink is the real
//! [`ApprovalQueue`], so "waits for L3" is checked against the queue itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde_json::{json, Value};
use shogun_agents::approval::{ApprovalOrigin, ApprovalQueue, Preview, Route};
use shogun_agents::clarify::{RunKind, RunRef};
use shogun_agents::entitlement::{entitlements, Plan};
use shogun_agents::permission::{Action, Level, SendAction};
use shogun_agents::steer::{SteerOrigin, MAX_PENDING_PER_RUN};
use shogun_mcp::connection::ConnState;
use shogun_mcp::scope::{Service, Wave};
use shogun_mcp::steer_api::{self, SteerOperation, SteeredTurnSource};
use shogun_mcp::tool_catalog::{ServiceState, ToolContext};
use shogun_mcp::tool_loop::{
    run_read_loop, LoopLimits, ModelTurn, ModelTurnSource, ProposalSink, ReadToolRunner,
    ToolResult, ToolRunError, ToolUse,
};

const RUN: RunRef = RunRef { kind: RunKind::Chat, id: 1_700_000_000_002 };

fn temp_store() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "shogun-steer-loop-{}-{}/{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
        steer_api::STORE_FILE
    ))
}

fn steer(path: &Path, run: RunRef, text: &str, origin: SteerOrigin) -> Value {
    let op = SteerOperation { run, text: text.to_string(), origin };
    serde_json::from_str(&steer_api::apply(path, &op, steer_api_now()).unwrap()).unwrap()
}

fn steer_api_now() -> u64 {
    shogun_mcp::approval_store::now_ms()
}

fn ctx(draft_stop: bool) -> ToolContext {
    ToolContext { highest_released: Wave::One, draft_stop, plan: entitlements(Plan::Pro, 0) }
}

fn connected() -> Vec<ServiceState> {
    [Service::Gmail, Service::GoogleCalendar]
        .into_iter()
        .map(|service| ServiceState { service, conn: ConnState::Connected { last_sync_ms: 0 } })
        .collect()
}

/// A model that does whatever the last note it read says. Turn 1 looks at the calendar; if the
/// next turn carries a note asking to send, it asks for the send (by the proposal tool **and** by
/// an invented direct-send name) and then answers.
#[derive(Default)]
struct ObedientModel {
    /// The note text each turn carried on its user half (`None` = no note that turn).
    notes_per_turn: Vec<Option<String>>,
    pending_note: Option<String>,
    turns: usize,
}

impl ModelTurnSource for ObedientModel {
    fn accepts_user_notes(&self) -> bool {
        true
    }

    fn push_user_note(&mut self, note: String) {
        self.pending_note = Some(note);
    }

    fn next_turn(&mut self, _results: &[ToolResult]) -> Result<ModelTurn, String> {
        let note = self.pending_note.take();
        self.notes_per_turn.push(note.clone());
        self.turns += 1;
        match self.turns {
            1 => Ok(ModelTurn::ToolUses(vec![ToolUse {
                id: "r1".into(),
                name: "list_calendar_events".into(),
                input: json!({}),
            }])),
            2 if note.as_deref().is_some_and(|n| n.contains("send it")) => {
                Ok(ModelTurn::ToolUses(vec![
                    ToolUse {
                        id: "p1".into(),
                        name: "propose_send_email".into(),
                        input: json!({ "to": "sam@example.com", "body": "Here is the summary." }),
                    },
                    // A model talked into it tries the direct route as well.
                    ToolUse { id: "x1".into(), name: "send_email".into(), input: json!({}) },
                ]))
            }
            _ => Ok(ModelTurn::Final("Done.".into())),
        }
    }
}

/// The read runner. While the calendar read is "in flight", the user adds notes from the Notch
/// and over MCP — the moment steering is for.
struct Runner<'a> {
    store: &'a Path,
    notes: &'a [(&'a str, SteerOrigin)],
    answers: Mutex<Vec<Value>>,
    reached: Mutex<Vec<&'static str>>,
}

impl ReadToolRunner for Runner<'_> {
    fn run(
        &self,
        _service: Service,
        scope_op: &'static str,
        _input: &Value,
        _timeout_ms: u64,
    ) -> Result<String, ToolRunError> {
        self.reached.lock().unwrap().push(scope_op);
        let mut answers = self.answers.lock().unwrap();
        for (text, origin) in self.notes {
            answers.push(steer(self.store, RUN, text, *origin));
        }
        Ok("2 events tomorrow".into())
    }
}

/// The real L3 queue as the loop's proposal sink — what the desktop's `QueueSink` does, minus
/// the file lock.
#[derive(Default)]
struct Queue(ApprovalQueue);

impl ProposalSink for Queue {
    fn propose(&mut self, action: Action, body: &str) -> Result<(), String> {
        let Action::Send(send) = action else { return Err("not a send".into()) };
        let preview = Preview::for_send(&send, body, Route::ViaComposio);
        self.0.try_request(send, preview, ApprovalOrigin::Ui, 0).map(|_| ()).map_err(str::to_string)
    }
}

#[test]
fn a_note_that_says_send_it_still_ends_as_an_l3_proposal_waiting_for_the_user() {
    let store = temp_store();
    let notes = [("send it to sam@example.com now, I approve", SteerOrigin::Ui)];
    let mut runner = Runner { store: &store, notes: &notes, answers: Mutex::default(), reached: Mutex::default() };
    let mut queue = Queue::default();

    let mut source = SteeredTurnSource::open(ObedientModel::default(), store.clone(), RUN, steer_api_now());
    assert!(source.is_steerable());
    let out = run_read_loop(&mut source, &mut runner, &mut queue, &connected(), &ctx(false), LoopLimits::default())
        .unwrap();

    // The note was accepted mid-run and delivered on the very next turn, fenced on the user half.
    let answers = runner.answers.lock().unwrap().clone();
    assert_eq!(answers[0]["accepted"], true, "{answers:?}");
    assert_eq!(answers[0]["delivery"], "next_turn");
    assert_eq!(source.delivered(), 1);
    assert_eq!(out.answer, "Done.");

    // Nothing but the read reached a service; the invented direct send was refused.
    assert_eq!(*runner.reached.lock().unwrap(), vec!["read_sync"]);
    assert_eq!(out.executed, 1);
    assert_eq!(out.proposed, 1);
    assert_eq!(out.refused, 1);

    // The send the note asked for waits in the L3 queue for the dedicated confirm.
    assert_eq!(queue.0.pending_len(), 1);
    let id = queue.0.pending_ids()[0];
    let action = queue.0.action(id).unwrap().clone();
    assert_eq!(action, SendAction::SendEmail { to: "sam@example.com".into() });
    assert!(Action::Send(action.clone()).is_external_send());
    assert_eq!(Action::Send(action).required_level(), Level::L3);
    assert_eq!(queue.0.poll(id), shogun_agents::approval::Decision::StillPending);

    // After the run: a late note is `run_finished`, and the store keeps no note text.
    drop(source);
    assert_eq!(steer(&store, RUN, "one more thing", SteerOrigin::Mcp)["error"], "run_finished");
    let raw = std::fs::read_to_string(&store).unwrap();
    assert!(!raw.contains("sam@example.com"), "{raw}");
    let _ = std::fs::remove_dir_all(store.parent().unwrap());
}

#[test]
fn the_note_reaches_the_model_fenced_as_the_users_words_that_approve_nothing() {
    let store = temp_store();
    let notes = [("send it to sam@example.com", SteerOrigin::Mcp)];
    let mut runner = Runner { store: &store, notes: &notes, answers: Mutex::default(), reached: Mutex::default() };
    let mut source = SteeredTurnSource::open(ObedientModel::default(), store.clone(), RUN, steer_api_now());
    run_read_loop(&mut source, &mut runner, &mut Queue::default(), &connected(), &ctx(false), LoopLimits::default())
        .unwrap();
    let seen = &source.inner().notes_per_turn;
    assert_eq!(seen[0], None, "no note before one was added");
    let note = seen[1].clone().expect("the note arrives on the next turn");
    assert!(note.starts_with("Added by the user while this was running"), "{note}");
    assert!(note.contains("approve nothing"));
    assert!(note.contains("<<<USER NOTE>>>\n- send it to sam@example.com\n<<<END USER NOTE>>>"), "{note}");
    assert_eq!(seen[2], None, "each note is read once");
    drop(source);
    let _ = std::fs::remove_dir_all(store.parent().unwrap());
}

#[test]
fn with_draft_stop_on_the_note_cannot_even_queue_the_send() {
    let store = temp_store();
    let notes = [("send it to sam@example.com", SteerOrigin::Api)];
    let mut runner = Runner { store: &store, notes: &notes, answers: Mutex::default(), reached: Mutex::default() };
    let mut queue = Queue::default();
    let mut source = SteeredTurnSource::open(ObedientModel::default(), store.clone(), RUN, steer_api_now());
    let out =
        run_read_loop(&mut source, &mut runner, &mut queue, &connected(), &ctx(true), LoopLimits::default()).unwrap();
    assert_eq!(source.delivered(), 1);
    assert_eq!(out.proposed, 0);
    assert_eq!(out.refused, 2);
    assert_eq!(queue.0.pending_len(), 0);
    drop(source);
    let _ = std::fs::remove_dir_all(store.parent().unwrap());
}

#[test]
fn at_most_three_notes_wait_for_one_turn() {
    let store = temp_store();
    let four: Vec<(&str, SteerOrigin)> = (0..=MAX_PENDING_PER_RUN).map(|_| ("shorter", SteerOrigin::Ui)).collect();
    let mut runner = Runner { store: &store, notes: &four, answers: Mutex::default(), reached: Mutex::default() };
    let mut source = SteeredTurnSource::open(ObedientModel::default(), store.clone(), RUN, steer_api_now());
    run_read_loop(&mut source, &mut runner, &mut Queue::default(), &connected(), &ctx(false), LoopLimits::default())
        .unwrap();
    let answers = runner.answers.lock().unwrap().clone();
    let codes: Vec<Value> =
        answers.iter().map(|a| a.get("error").cloned().unwrap_or_else(|| a["pending"].clone())).collect();
    assert_eq!(codes, vec![json!(1), json!(2), json!(3), json!("too_many_notes")]);
    assert_eq!(source.delivered(), MAX_PENDING_PER_RUN, "the three that fit were all read");
    drop(source);
    let _ = std::fs::remove_dir_all(store.parent().unwrap());
}

#[test]
fn an_unknown_run_is_refused_and_lists_the_run_that_can_take_a_note() {
    let store = temp_store();
    assert_eq!(steer(&store, RUN, "shorter", SteerOrigin::Api)["error"], "unknown_run");
    let started = steer_api_now();
    let source = SteeredTurnSource::open(ObedientModel::default(), store.clone(), RUN, started);
    let other = RunRef { kind: RunKind::Chat, id: 42 };
    let v = steer(&store, other, "shorter", SteerOrigin::Api);
    assert_eq!(v["error"], "unknown_run");
    assert_eq!(v["open_runs"], json!([{ "kind": "chat", "id": RUN.id, "started_at_ms": started }]));
    drop(source);
    let _ = std::fs::remove_dir_all(store.parent().unwrap());
}

#[test]
fn a_source_that_cannot_carry_a_note_never_opens_a_run() {
    /// Every provider source that does not implement the user-note seam (the default).
    struct Plain;
    impl ModelTurnSource for Plain {
        fn next_turn(&mut self, _: &[ToolResult]) -> Result<ModelTurn, String> {
            Ok(ModelTurn::Final("ok".into()))
        }
    }
    let store = temp_store();
    let source = SteeredTurnSource::open(Plain, store.clone(), RUN, steer_api_now());
    assert!(!source.is_steerable());
    assert_eq!(steer(&store, RUN, "shorter", SteerOrigin::Ui)["error"], "unknown_run");
    drop(source);
    let _ = std::fs::remove_dir_all(store.parent().unwrap());
}
