//! Mid-run steering (issue #395) on every face: `actions.steer` over MCP (`tools/call`), REST
//! (`POST /v1/actions/steer`) and the CLI (`shogun actions steer`, over REST), plus the calls the
//! desktop runner makes around a steerable run.
//!
//! A run lives in the desktop (the Agent lane); a note can come from the Notch or from any API
//! face, which run in other processes. So the ledger (`shogun_agents::steer::SteerLedger`) is
//! persisted in `steer_runs.json` beside the database, under the same kind of cross-process lock,
//! 0600 atomic write and bounded size as the L3 approval store. The file holds note text only
//! while a note waits for its run's next turn: taking it, or the run ending, removes the text, so
//! a finished run leaves ids and timestamps only (the approval store's "body-free once resolved"
//! rule). Nothing here logs a note.
//!
//! The runner's side (the integration point for the tool loop):
//! 1. [`open_run`] when a steerable run starts (a chat turn, a Notch agent run);
//! 2. at every model turn boundary, [`take_for_turn`] and append the notes to the next turn's user
//!    half with `shogun_agents::steer::append_to_user_half` — a read of a small file only at a
//!    boundary, no timer and no polling;
//! 3. [`close_run`] when it ends, however it ends.
//!
//! [`SteeredTurnSource`] does all three around a tool-loop model source; the desktop's tool chat
//! uses it, so the calls cannot drift from this list.
//!
//! **Separate from the L3 approval queue on purpose**: nothing here reads or writes it, and a note
//! approves nothing. A send the run proposes after reading a note waits for the full preview and
//! the dedicated confirm like any other. The origin is set by the face, never read from the body.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use shogun_agents::clarify::{RunKind, RunRef};
use shogun_agents::steer::{
    append_to_user_half, check_note, RunEntry, SteerLedger, SteerNote, SteerOrigin, SteerRefusal,
    MAX_PENDING_PER_RUN, MAX_RUNS,
};

use crate::tool_loop::{ModelTurn, ModelTurnSource, ToolResult};

/// The store file, beside `memory.db`.
pub const STORE_FILE: &str = "steer_runs.json";
/// Overrides where the store lives (tests, a relocated daemon).
pub const STORE_ENV: &str = "SHOGUN_STEER_RUNS";
const SCHEMA: u8 = 1;
/// Far above what [`MAX_RUNS`] runs with [`MAX_PENDING_PER_RUN`] notes each can need; a file past
/// this is refused rather than parsed.
const MAX_STORE_BYTES: u64 = 1024 * 1024;

/// `steer_runs.json` for a daemon whose DB is at `db_path`: [`STORE_ENV`] when set, otherwise
/// beside the DB.
pub fn resolve_path(db_path: &str) -> PathBuf {
    if let Ok(path) = std::env::var(STORE_ENV) {
        if !path.trim().is_empty() {
            return PathBuf::from(path);
        }
    }
    Path::new(db_path)
        .parent()
        .map_or_else(|| PathBuf::from(STORE_FILE), |dir| dir.join(STORE_FILE))
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WireStore {
    schema: u8,
    runs: Vec<WireRun>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRun {
    kind: String,
    id: i64,
    opened_ms: u64,
    closed_ms: Option<u64>,
    delivered: u32,
    pending: Vec<WireNote>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WireNote {
    text: String,
    origin: String,
    at_ms: u64,
}

fn to_wire(ledger: &SteerLedger) -> WireStore {
    WireStore {
        schema: SCHEMA,
        runs: ledger
            .entries()
            .iter()
            .map(|e| WireRun {
                kind: e.run.kind.as_str().to_string(),
                id: e.run.id,
                opened_ms: e.opened_ms,
                closed_ms: e.closed_ms,
                delivered: e.delivered,
                pending: e
                    .pending
                    .iter()
                    .map(|n| WireNote { text: n.text.clone(), origin: n.origin.as_str().to_string(), at_ms: n.at_ms })
                    .collect(),
            })
            .collect(),
    }
}

/// Rebuild the ledger. A row this build cannot read (a run kind from a newer build) is skipped,
/// and a note that no longer passes [`check_note`] is dropped: neither is ever delivered.
fn from_wire(store: WireStore) -> SteerLedger {
    let runs = store
        .runs
        .into_iter()
        .filter_map(|row| {
            let kind = RunKind::parse(&row.kind)?;
            let pending = row
                .pending
                .into_iter()
                .filter_map(|n| {
                    let origin = SteerOrigin::parse(&n.origin)?;
                    let text = check_note(&n.text).ok()?;
                    Some(SteerNote { text, origin, at_ms: n.at_ms })
                })
                .take(MAX_PENDING_PER_RUN)
                .collect();
            Some(RunEntry {
                run: RunRef { kind, id: row.id },
                opened_ms: row.opened_ms,
                closed_ms: row.closed_ms,
                pending,
                delivered: row.delivered,
            })
        })
        .take(MAX_RUNS)
        .collect();
    SteerLedger::from_entries(runs)
}

/// The ledger on disk. A missing file is empty; an unreadable, oversized or corrupt one is an
/// error, never silently replaced.
fn load(path: &Path) -> Result<SteerLedger, String> {
    match fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_STORE_BYTES => return Err("steer store too large".into()),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(SteerLedger::default()),
        Err(_) => return Err("cannot read steer store".into()),
    }
    let bytes = fs::read(path).map_err(|_| "cannot read steer store".to_string())?;
    let store: WireStore =
        serde_json::from_slice(&bytes).map_err(|_| "steer store is unreadable".to_string())?;
    if store.schema != SCHEMA {
        return Err("steer store is from another version".into());
    }
    Ok(from_wire(store))
}

/// One locked read-modify-write. `f` returns the result and whether the ledger changed.
fn with_ledger<R>(path: &Path, f: impl FnOnce(&mut SteerLedger) -> (R, bool)) -> Result<R, String> {
    let _lock =
        crate::approval_store::acquire_lock(path).map_err(|_| "cannot lock steer store".to_string())?;
    let mut ledger = load(path)?;
    let (out, changed) = f(&mut ledger);
    if changed {
        let data = serde_json::to_vec(&to_wire(&ledger)).map_err(|_| "cannot encode steer store".to_string())?;
        crate::approval_store::save_bytes(path, &data).map_err(|_| "cannot write steer store".to_string())?;
    }
    Ok(out)
}

/// The runner started a steerable run. A skill run (one model call, no turn boundary) is not
/// steerable and is refused, so no face is ever told a note will be read when it cannot be.
pub fn open_run(path: &Path, run: RunRef, now_ms: u64) -> Result<(), String> {
    if !shogun_agents::steer::is_steerable_kind(run.kind) {
        return Err("run kind is not steerable".into());
    }
    with_ledger(path, |ledger| {
        ledger.open(run, now_ms);
        ((), true)
    })
}

/// The run ended. Returns how many notes it never read (their text is removed with it).
pub fn close_run(path: &Path, run: RunRef, now_ms: u64) -> Result<usize, String> {
    with_ledger(path, |ledger| {
        let dropped = ledger.close(run, now_ms);
        (dropped, true)
    })
}

/// The runner is at a turn boundary: the notes waiting for `run`, oldest first, removed from the
/// store. No file means no notes, and nothing is created.
pub fn take_for_turn(path: &Path, run: RunRef) -> Result<Vec<SteerNote>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    with_ledger(path, |ledger| {
        let notes = ledger.take_for_turn(run);
        let changed = !notes.is_empty();
        (notes, changed)
    })
}

/// A steerable tool-loop run (issue #395): the run's [`ModelTurnSource`], wrapped so the three
/// runner calls above cannot be forgotten or reordered.
///
/// - [`SteeredTurnSource::open`] registers the run — only when the kind is steerable **and** the
///   source can carry a note on its user half. A store fault leaves the run unsteerable (a note
///   for it gets `unknown_run`); it never fails the run the user asked for.
/// - Before every model turn it takes the notes waiting for this run and hands them, fenced by
///   [`append_to_user_half`], to the source's user half. The only read of the store is here, at a
///   turn boundary: no timer, no polling.
/// - [`SteeredTurnSource::close`] — or dropping it, however the run ended (answer, error, Stop) —
///   closes the run, so a late note gets `run_finished` and unread text is removed.
///
/// It wraps the source and nothing else: the runner, the gate and the proposal sink are the
/// loop's, untouched. A note is text the model reads; a send it asks for afterwards is classified
/// by the same catalog and gate and still ends as an L3 proposal.
pub struct SteeredTurnSource<S> {
    inner: S,
    store: PathBuf,
    run: RunRef,
    open: bool,
    delivered: usize,
}

impl<S: ModelTurnSource> SteeredTurnSource<S> {
    /// Wrap `inner` and open `run` in the store at `store`.
    pub fn open(inner: S, store: PathBuf, run: RunRef, now_ms: u64) -> Self {
        let open = inner.accepts_user_notes() && open_run(&store, run, now_ms).is_ok();
        Self { inner, store, run, open, delivered: 0 }
    }
}

impl<S> SteeredTurnSource<S> {
    /// Whether a note for this run can be taken right now (the run opened and has not closed).
    pub fn is_steerable(&self) -> bool {
        self.open
    }

    /// The wrapped source.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// How many notes this run has read so far.
    pub fn delivered(&self) -> usize {
        self.delivered
    }

    /// End the run. Returns how many notes arrived too late to be read (their text is removed
    /// with the run). Idempotent: a second call, or the drop after it, returns 0 and writes
    /// nothing.
    pub fn close(&mut self, now_ms: u64) -> usize {
        if !std::mem::replace(&mut self.open, false) {
            return 0;
        }
        close_run(&self.store, self.run, now_ms).unwrap_or(0)
    }
}

impl<S> Drop for SteeredTurnSource<S> {
    fn drop(&mut self) {
        self.close(crate::approval_store::now_ms());
    }
}

impl<S: ModelTurnSource> ModelTurnSource for SteeredTurnSource<S> {
    fn next_turn(&mut self, results: &[ToolResult]) -> Result<ModelTurn, String> {
        if self.open {
            // A store fault keeps the notes in the file; this turn goes ahead without them.
            if let Ok(notes) = take_for_turn(&self.store, self.run) {
                if !notes.is_empty() {
                    self.delivered += notes.len();
                    let block = append_to_user_half("", &notes);
                    self.inner.push_user_note(block.trim_start().to_string());
                }
            }
        }
        self.inner.next_turn(results)
    }

    fn supports_async_results(&self) -> bool {
        self.inner.supports_async_results()
    }

    // `accepts_user_notes` stays `false`: this wrapper is the one reader of the store for its
    // run, so a second steering wrapper around it could never take a note twice.
}

/// One `actions.steer` request, lifted off its wire. `origin` is the face's, not the caller's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteerOperation {
    pub run: RunRef,
    pub text: String,
    pub origin: SteerOrigin,
}

/// Lift `{kind, id, text}` (MCP arguments / REST body). `Err` is a fixed machine code.
pub fn operation(body: &str, origin: SteerOrigin) -> Result<SteerOperation, &'static str> {
    let value: Value = serde_json::from_str(body).map_err(|_| "invalid_request")?;
    let kind = value.get("kind").and_then(Value::as_str).ok_or("missing_kind")?;
    let kind = RunKind::parse(kind).ok_or("unknown_kind")?;
    let id = value.get("id").and_then(Value::as_i64).filter(|id| *id > 0).ok_or("missing_id")?;
    let text = value.get("text").and_then(Value::as_str).ok_or("missing_text")?.to_string();
    Ok(SteerOperation { run: RunRef { kind, id }, text, origin })
}

fn run_json(run: RunRef) -> Value {
    json!({ "kind": run.kind.as_str(), "id": run.id })
}

/// Add the note. `Ok` is the JSON every face renders: an acceptance, or an `error` payload with a
/// fixed code (the note is never echoed). An `unknown_run` refusal lists the runs that can take a
/// note right now (kind, id and start time only), so a caller can find the right one. `Err` is a
/// store fault.
pub fn apply(path: &Path, op: &SteerOperation, now_ms: u64) -> Result<String, String> {
    with_ledger(path, |ledger| match ledger.steer(op.run, &op.text, op.origin, now_ms) {
        Ok(receipt) => (
            json!({
                "accepted": true,
                "run": run_json(receipt.run),
                "pending": receipt.pending,
                "delivery": receipt.delivery.as_str(),
            })
            .to_string(),
            true,
        ),
        Err(SteerRefusal::UnknownRun) => {
            let open: Vec<Value> = ledger
                .open_runs()
                .iter()
                .map(|e| json!({ "kind": e.run.kind.as_str(), "id": e.run.id, "started_at_ms": e.opened_ms }))
                .collect();
            (json!({ "error": SteerRefusal::UnknownRun.as_str(), "run": run_json(op.run), "open_runs": open }).to_string(), false)
        }
        Err(refusal) => (json!({ "error": refusal.as_str(), "run": run_json(op.run) }).to_string(), false),
    })
}

/// The HTTP status for an `actions.steer` answer: unknown run 404, finished run or a full turn
/// 409, a store fault 503, a bad note 400.
pub fn http_status(json: &str) -> u16 {
    let Ok(value) = serde_json::from_str::<Value>(json) else {
        return 500;
    };
    match value.get("error").and_then(Value::as_str) {
        None => 200,
        Some("unknown_run") => 404,
        Some("run_finished" | "too_many_notes") => 409,
        Some("unavailable") => 503,
        Some(_) => 400,
    }
}

/// A refusal before the store is reached (a malformed request).
pub fn refusal_json(code: &str) -> String {
    json!({ "error": code }).to_string()
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn temp_store() -> PathBuf {
        std::env::temp_dir().join(format!(
            "shogun-steer-{}-{}/{STORE_FILE}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::temp_store;
    use super::*;

    const CHAT: RunRef = RunRef { kind: RunKind::Chat, id: 3 };

    fn body(kind: &str, id: i64, text: &str) -> String {
        json!({ "kind": kind, "id": id, "text": text }).to_string()
    }

    fn steer(path: &Path, text: &str, origin: SteerOrigin, now: u64) -> Value {
        let op = operation(&body("chat", 3, text), origin).unwrap();
        serde_json::from_str(&apply(path, &op, now).unwrap()).unwrap()
    }

    #[test]
    fn a_note_crosses_processes_and_is_read_once_at_the_next_turn() {
        let path = temp_store();
        assert!(take_for_turn(&path, CHAT).unwrap().is_empty());
        assert!(!path.exists(), "a take never creates the store");
        // Before the run starts: unknown, and nothing listed.
        let v = steer(&path, "shorter", SteerOrigin::Mcp, 1);
        assert_eq!(v["error"], "unknown_run");
        assert_eq!(v["open_runs"], json!([]));

        open_run(&path, CHAT, 10).unwrap();
        let v = steer(&path, "make it shorter", SteerOrigin::Mcp, 11);
        assert_eq!(v, json!({ "accepted": true, "run": { "kind": "chat", "id": 3 }, "pending": 1, "delivery": "next_turn" }));
        assert!(!v.to_string().contains("shorter"), "the note is never echoed");
        // Another process (the runner) reads it back from the file.
        let notes = take_for_turn(&path, CHAT).unwrap();
        assert_eq!((notes[0].text.as_str(), notes[0].origin), ("make it shorter", SteerOrigin::Mcp));
        assert!(take_for_turn(&path, CHAT).unwrap().is_empty());
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("make it shorter"), "a read note leaves no text: {raw}");

        // A wrong id lists the run that can take a note.
        let op = operation(&body("chat", 99, "x"), SteerOrigin::Api).unwrap();
        let v: Value = serde_json::from_str(&apply(&path, &op, 12).unwrap()).unwrap();
        assert_eq!(v["error"], "unknown_run");
        assert_eq!(v["open_runs"], json!([{ "kind": "chat", "id": 3, "started_at_ms": 10 }]));
        assert_eq!(http_status(&v.to_string()), 404);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn after_the_run_ends_a_note_is_a_clear_error_and_unread_text_is_gone() {
        let path = temp_store();
        open_run(&path, CHAT, 0).unwrap();
        assert_eq!(steer(&path, "unread", SteerOrigin::Ui, 1)["accepted"], true);
        assert_eq!(close_run(&path, CHAT, 2).unwrap(), 1);
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("unread"), "{raw}");
        let v = steer(&path, "too late", SteerOrigin::Api, 3);
        assert_eq!(v["error"], "run_finished");
        assert_eq!(http_status(&v.to_string()), 409);
        assert!(take_for_turn(&path, CHAT).unwrap().is_empty());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn requests_are_checked_before_the_store_and_limits_are_codes() {
        assert_eq!(operation("nope", SteerOrigin::Api), Err("invalid_request"));
        assert_eq!(operation(r#"{"id":1,"text":"x"}"#, SteerOrigin::Api), Err("missing_kind"));
        assert_eq!(operation(r#"{"kind":"batch","id":1,"text":"x"}"#, SteerOrigin::Api), Err("unknown_kind"));
        assert_eq!(operation(r#"{"kind":"chat","id":0,"text":"x"}"#, SteerOrigin::Api), Err("missing_id"));
        assert_eq!(operation(r#"{"kind":"chat","id":1}"#, SteerOrigin::Api), Err("missing_text"));
        // The origin is the face's: a body cannot claim to be the Notch.
        let op = operation(r#"{"kind":"chat","id":1,"text":"x","origin":"ui"}"#, SteerOrigin::Mcp).unwrap();
        assert_eq!(op.origin, SteerOrigin::Mcp);

        let path = temp_store();
        open_run(&path, CHAT, 0).unwrap();
        assert_eq!(steer(&path, "  ", SteerOrigin::Api, 1)["error"], "empty_note");
        assert_eq!(http_status(&steer(&path, "  ", SteerOrigin::Api, 1).to_string()), 400);
        for i in 0..MAX_PENDING_PER_RUN {
            assert_eq!(steer(&path, &format!("n{i}"), SteerOrigin::Api, 2)["accepted"], true);
        }
        assert_eq!(steer(&path, "more", SteerOrigin::Api, 3)["error"], "too_many_notes");
        // A skill run is one call with no turn to read a note at: never opened.
        assert!(open_run(&path, RunRef { kind: RunKind::SkillRun, id: 1 }, 0).is_err());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_corrupt_store_is_an_error_and_is_never_overwritten() {
        let path = temp_store();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{not json").unwrap();
        assert!(open_run(&path, CHAT, 0).is_err());
        assert!(take_for_turn(&path, CHAT).is_err());
        let op = operation(&body("chat", 3, "x"), SteerOrigin::Api).unwrap();
        assert!(apply(&path, &op, 0).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{not json");
        // A row from a newer build is skipped, not fatal.
        fs::write(
            &path,
            r#"{"schema":1,"runs":[{"kind":"future","id":1,"opened_ms":0,"closed_ms":null,"delivered":0,"pending":[]}]}"#,
        )
        .unwrap();
        assert!(open_run(&path, CHAT, 1).is_ok());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn the_store_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_store();
        open_run(&path, CHAT, 0).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn steering_is_separate_from_the_approval_queue() {
        let src = include_str!("steer_api.rs");
        let body = src.split("#[cfg(test)]").next().unwrap_or(src);
        for forbidden in [
            concat!("Approval", "Queue"),
            concat!("with_", "queue"),
            concat!("load_", "queue"),
            concat!("Send", "Action"),
            concat!("Confirm", "Intent"),
        ] {
            assert!(!body.contains(forbidden), "steer_api.rs must not use {forbidden}");
        }
    }
}
