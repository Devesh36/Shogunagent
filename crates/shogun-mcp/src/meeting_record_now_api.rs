//! Cross-process `meeting.start` (invariant 6, FR-API-04).
//!
//! MCP / REST / CLI never open the microphone. They append a row to a durable ledger in app-data
//! and ping the desktop with a one-shot wakeup file. The desktop driver consumes a pending row
//! only while Idle and **offers** notes (L2). Capture starts only after the human confirms.
//!
//! The Offered **card** is the meeting 10s confirm (a product exception to the L3 10-minute
//! window). The **poll ceiling** for a stuck pending row is still FR-API-04: 10 minutes, then
//! `expired` — desktop absence and a never-decided offer share that status.
//!
//! Outcomes live in the ledger, not in per-process memory, so a CLI poll after an MCP queue
//! sees the desktop decision. Human Record now does not create a row.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use shogun_agents::approval::APPROVAL_TIMEOUT_MS;

use crate::plan_source::{meeting_record_now_path, meeting_start_approvals_path};

/// Desktop returned this when it claimed a row but could not show Offered (already busy).
pub const OFFER_BUSY: &str = "meeting already active";
/// Desktop lane lock was held. Transient — release the claim so the next Idle tick can retry.
pub const OFFER_LOCK_BUSY: &str = "meeting notes busy";
/// Settings toggle is off at offer time (queued while on, then switched off).
pub const OFFER_NOTES_OFF: &str = "meeting notes are off";

/// Claimed a row then could not show Offered. Busy is retryable; notes-off is a refusal.
pub fn settle_offer_error(id: u64, err: &str, decided_at_ms: i64) -> Result<(), RequestError> {
    Store::host().settle_offer_error(id, err, decided_at_ms)
}

/// Desktop Offered card: a tap is `rejected`; the 10s API grace running out is `expired`.
pub fn decline_outcome(offered_via_api: bool, elapsed_ms: i64, grace_ms: i64) -> Outcome {
    if offered_via_api && elapsed_ms >= grace_ms {
        Outcome::Expired
    } else {
        Outcome::Rejected
    }
}

#[cfg(unix)]
const LOCK_RETRIES: usize = 200;
#[cfg(unix)]
const LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(10);
const MAX_TERMINAL_ROWS: usize = 100;

static PROCESS_LOCK: Mutex<()> = Mutex::new(());
static SIDECAR_SEQ: AtomicU64 = AtomicU64::new(1);

/// Why a queued start was refused. Wire bodies stay stable; filesystem detail stays in logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestError {
    /// Meeting notes are off — the same Settings toggle the human UI requires (FR-MT-01).
    NotesOff,
    /// Settings or app-data cannot be read safely.
    Unavailable,
    /// Create/write/rename failed. Callers see [`RequestError::code`], not the OS text.
    Storage,
}

impl RequestError {
    pub fn code(self) -> &'static str {
        match self {
            Self::NotesOff => "meeting_notes_off",
            Self::Unavailable | Self::Storage => "meeting_start_unavailable",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::NotesOff => OFFER_NOTES_OFF,
            Self::Unavailable | Self::Storage => "meeting record-now unavailable",
        }
    }

    pub fn http_status(self) -> u16 {
        match self {
            Self::NotesOff => 409,
            Self::Unavailable | Self::Storage => 503,
        }
    }

    /// JSON-RPC: policy/caller refusal vs server failure (MCP `-32603` Internal error).
    pub fn mcp_code(self) -> i64 {
        match self {
            Self::NotesOff => -32602,
            Self::Unavailable | Self::Storage => -32603,
        }
    }
}

/// Terminal (or pending) outcome a caller can poll. Snake_case on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Pending,
    Approved,
    Rejected,
    Expired,
    Failed,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Expired => "expired",
            Self::Failed => "failed",
        }
    }

    fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Ledger {
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    records: Vec<Record>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            next_id: 1,
            records: Vec::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    id: u64,
    requested_at_ms: i64,
    #[serde(default)]
    offered_at_ms: Option<i64>,
    status: Outcome,
    #[serde(default)]
    resolved_at_ms: Option<i64>,
    /// Start for a room, not a call (issue #262 FR-MC-01). Absent in older ledgers = online.
    #[serde(default)]
    in_person: bool,
}

struct FileLock {
    file: File,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.file), libc::LOCK_UN);
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn timeout_ms() -> i64 {
    i64::try_from(APPROVAL_TIMEOUT_MS).unwrap_or(i64::MAX)
}

/// Where one ledger lives: the approvals ledger, the desktop wakeup file, and the meeting
/// settings whose toggle gates a request.
///
/// The free functions of this module ([`request`], [`take`], [`status`], …) always use
/// [`Store::host`] — the shared app-data files every face reads. A `Store` value exists so a
/// caller that must not touch those files (a unit test, an isolated server) can name its own
/// directory instead of rewriting process-global environment variables that tests running in
/// parallel also read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    ledger: Option<PathBuf>,
    wakeup: Option<PathBuf>,
    settings: Option<PathBuf>,
}

impl Store {
    /// The shared app-data files (debug env overrides first, then `$HOME`), resolved now.
    pub fn host() -> Self {
        Self {
            ledger: meeting_start_approvals_path(),
            wakeup: meeting_record_now_path(),
            settings: crate::plan_source::meeting_json_path(),
        }
    }

    /// All three files inside `dir`, under the names the host uses.
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            ledger: Some(dir.join("meeting_start_approvals.json")),
            wakeup: Some(dir.join("meeting_record_now.json")),
            settings: Some(dir.join("meeting.json")),
        }
    }

    /// The meeting settings file this store reads the notes toggle from.
    pub fn settings_path(&self) -> Option<&Path> {
        self.settings.as_deref()
    }

    fn notes_enabled(&self) -> Result<bool, RequestError> {
        let path = self.settings.as_deref().ok_or(RequestError::Unavailable)?;
        let json = match std::fs::read_to_string(path) {
            Ok(json) => json,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                eprintln!("[meeting] settings unavailable: {error}");
                return Err(RequestError::Unavailable);
            }
        };
        let value = serde_json::from_str::<serde_json::Value>(&json).map_err(|error| {
            eprintln!("[meeting] settings unavailable: {error}");
            RequestError::Unavailable
        })?;
        Ok(value
            .get("enabled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false))
    }
}

fn sidecar(path: &Path, kind: &str) -> PathBuf {
    let n = SIDECAR_SEQ.fetch_add(1, Ordering::Relaxed);
    path.with_extension(format!("json.{kind}-{}-{}", std::process::id(), n))
}

fn prepare_parent(path: &Path) -> Result<(), RequestError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            eprintln!("[meeting] start-approval store unavailable: {error}");
            RequestError::Storage
        })?;
    }
    Ok(())
}

fn acquire_file_lock(path: &Path) -> Result<FileLock, RequestError> {
    let lock_path = path.with_extension("json.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|error| {
            eprintln!("[meeting] start-approval store unavailable: {error}");
            RequestError::Storage
        })?;
    #[cfg(unix)]
    for _ in 0..LOCK_RETRIES {
        if unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&file),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        } == 0
        {
            return Ok(FileLock { file });
        }
        std::thread::sleep(LOCK_RETRY_DELAY);
    }
    #[cfg(not(unix))]
    return Ok(FileLock { file });
    #[cfg(unix)]
    {
        eprintln!("[meeting] start-approval store busy");
        Err(RequestError::Storage)
    }
}

fn repair_next_id(ledger: &mut Ledger) {
    let max_id = ledger
        .records
        .iter()
        .map(|record| record.id)
        .max()
        .unwrap_or(0);
    ledger.next_id = ledger.next_id.max(1).max(max_id.saturating_add(1));
}

fn load_at(path: &Path) -> Result<Ledger, RequestError> {
    match std::fs::read_to_string(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::default()),
        Err(error) => {
            eprintln!("[meeting] start-approval store unavailable: {error}");
            Err(RequestError::Storage)
        }
        Ok(json) => {
            if json.trim().is_empty() {
                return Ok(Ledger::default());
            }
            let mut ledger = serde_json::from_str::<Ledger>(&json).map_err(|error| {
                eprintln!("[meeting] start-approval store unavailable: {error}");
                RequestError::Storage
            })?;
            repair_next_id(&mut ledger);
            Ok(ledger)
        }
    }
}

fn write_at_locked(path: &Path, ledger: &Ledger) -> Result<(), RequestError> {
    let json = serde_json::to_string(ledger).map_err(|error| {
        eprintln!("[meeting] start-approval store unavailable: {error}");
        RequestError::Storage
    })?;
    let tmp = sidecar(path, "tmp");
    if let Err(error) = std::fs::write(&tmp, json) {
        eprintln!("[meeting] start-approval store unavailable: {error}");
        return Err(RequestError::Storage);
    }
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        eprintln!("[meeting] start-approval store unavailable: {error}");
        return Err(RequestError::Storage);
    }
    Ok(())
}

fn expire_due(ledger: &mut Ledger, now: i64) {
    let limit = timeout_ms();
    for record in &mut ledger.records {
        if record.status != Outcome::Pending {
            continue;
        }
        if now.saturating_sub(record.requested_at_ms) > limit {
            record.status = Outcome::Expired;
            record.resolved_at_ms = Some(now);
        }
    }
}

fn prune_terminal(ledger: &mut Ledger) {
    let terminal = ledger
        .records
        .iter()
        .filter(|record| record.status.is_terminal())
        .count();
    if terminal <= MAX_TERMINAL_ROWS {
        return;
    }
    let drop = terminal - MAX_TERMINAL_ROWS;
    let mut dropped = 0;
    ledger.records.retain(|record| {
        if record.status.is_terminal() && dropped < drop {
            dropped += 1;
            false
        } else {
            true
        }
    });
}

impl Store {
    fn with_ledger<T>(
        &self,
        now: i64,
        patch: impl FnOnce(&mut Ledger) -> Result<T, RequestError>,
    ) -> Result<T, RequestError> {
        let path = self.ledger.as_deref().ok_or(RequestError::Unavailable)?;
        let _process = PROCESS_LOCK.lock().map_err(|_| {
            eprintln!("[meeting] start-approval store busy");
            RequestError::Storage
        })?;
        prepare_parent(path)?;
        let _file = acquire_file_lock(path)?;
        let mut ledger = load_at(path)?;
        expire_due(&mut ledger, now);
        let value = patch(&mut ledger)?;
        prune_terminal(&mut ledger);
        write_at_locked(path, &ledger)?;
        Ok(value)
    }

    fn write_wakeup(&self, id: u64, requested_at_ms: i64) {
        let Some(path) = self.wakeup.as_deref() else {
            return;
        };
        if prepare_parent(path).is_err() {
            return;
        }
        let body = serde_json::json!({ "id": id, "requested_at_ms": requested_at_ms }).to_string();
        let tmp = sidecar(path, "tmp");
        if std::fs::write(&tmp, body).is_err() {
            return;
        }
        if std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    fn consume_wakeup(&self) {
        let Some(path) = self.wakeup.as_deref() else {
            return;
        };
        let claimed = sidecar(path, "claimed");
        if std::fs::rename(path, &claimed).is_ok() {
            let _ = std::fs::remove_file(&claimed);
        }
    }

    /// [`request`] against this store.
    pub fn request(&self) -> Result<String, RequestError> {
        self.request_with(false)
    }

    /// [`request_with`] against this store.
    pub fn request_with(&self, in_person: bool) -> Result<String, RequestError> {
        self.request_at(now_ms(), in_person)
    }

    fn request_at(&self, now: i64, in_person: bool) -> Result<String, RequestError> {
        if !self.notes_enabled()? {
            return Err(RequestError::NotesOff);
        }
        let id = self.with_ledger(now, |ledger| {
            let id = ledger.next_id.max(1);
            ledger.next_id = id.saturating_add(1);
            ledger.records.push(Record {
                id,
                requested_at_ms: now,
                offered_at_ms: None,
                status: Outcome::Pending,
                resolved_at_ms: None,
                in_person,
            });
            Ok(id)
        })?;
        self.write_wakeup(id, now);
        Ok(serde_json::json!({
            "queued": true,
            "level": "L2",
            "approval_id": id,
            "status": "pending",
        })
        .to_string())
    }

    /// [`take`] against this store.
    pub fn take(&self) -> Option<OfferedStart> {
        self.take_at(now_ms())
    }

    fn take_at(&self, now: i64) -> Option<OfferedStart> {
        self.consume_wakeup();
        self.with_ledger(now, |ledger| {
            let Some(record) = ledger
                .records
                .iter_mut()
                .find(|record| record.status == Outcome::Pending && record.offered_at_ms.is_none())
            else {
                return Ok(None);
            };
            record.offered_at_ms = Some(now);
            Ok(Some(OfferedStart {
                id: record.id,
                expires_at_ms: record.requested_at_ms.saturating_add(timeout_ms()),
                in_person: record.in_person,
            }))
        })
        .ok()
        .flatten()
    }

    /// [`release_claim`] against this store.
    pub fn release_claim(&self, id: u64) -> Result<(), RequestError> {
        self.with_ledger(now_ms(), |ledger| {
            if let Some(record) = ledger.records.iter_mut().find(|record| record.id == id) {
                if record.status == Outcome::Pending {
                    record.offered_at_ms = None;
                }
            }
            Ok(())
        })
    }

    /// [`resolve`] against this store.
    pub fn resolve(&self, id: u64, outcome: Outcome) -> Result<(), RequestError> {
        self.resolve_at(id, outcome, now_ms())
    }

    /// [`resolve_at`] against this store.
    pub fn resolve_at(
        &self,
        id: u64,
        outcome: Outcome,
        decided_at_ms: i64,
    ) -> Result<(), RequestError> {
        if !outcome.is_terminal() {
            return Ok(());
        }
        self.with_ledger(now_ms(), |ledger| {
            if let Some(record) = ledger.records.iter_mut().find(|record| record.id == id) {
                let timely_decision = record.offered_at_ms.is_some_and(|offered| {
                    decided_at_ms >= offered
                        && decided_at_ms <= record.requested_at_ms.saturating_add(timeout_ms())
                });
                if record.status == Outcome::Pending
                    || (record.status == Outcome::Expired && timely_decision)
                {
                    record.status = outcome;
                    record.resolved_at_ms = Some(decided_at_ms);
                }
            }
            Ok(())
        })
    }

    /// [`settle_offer_error`] against this store.
    pub fn settle_offer_error(
        &self,
        id: u64,
        err: &str,
        decided_at_ms: i64,
    ) -> Result<(), RequestError> {
        if err == OFFER_BUSY || err == OFFER_LOCK_BUSY {
            return self.release_claim(id);
        }
        if err == OFFER_NOTES_OFF {
            return self.resolve_at(id, Outcome::Rejected, decided_at_ms);
        }
        self.resolve_at(id, Outcome::Failed, decided_at_ms)
    }

    /// [`status`] against this store.
    pub fn status(&self, id: u64) -> Result<Option<String>, RequestError> {
        self.status_at(id, now_ms())
    }

    fn status_at(&self, id: u64, now: i64) -> Result<Option<String>, RequestError> {
        self.with_ledger(now, |ledger| {
            Ok(ledger
                .records
                .iter()
                .find(|record| record.id == id)
                .map(|record| status_json(record.id, record.status)))
        })
    }
}

fn status_json(id: u64, status: Outcome) -> String {
    serde_json::json!({ "approval_id": id, "status": status.as_str() }).to_string()
}

/// Queue a manual start. Fails when meeting notes are off so an agent cannot listen
/// without the same Settings toggle the human UI requires (FR-MT-01).
pub fn request() -> Result<String, RequestError> {
    Store::host().request()
}

/// [`request`] for a room (issue #262 FR-MC-01): the desktop offers an in-person start — no
/// system tap, diarized microphone — under the same L2 confirm. Symmetric with the human
/// "In person" button (invariant 6).
pub fn request_with(in_person: bool) -> Result<String, RequestError> {
    Store::host().request_with(in_person)
}

/// Read `in_person` from a request body (`{"in_person": true}`); anything else is online.
pub fn in_person_from_body(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("in_person")?.as_bool())
        .unwrap_or(false)
}

/// A desktop claim carries the same absolute deadline enforced by caller polling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OfferedStart {
    pub id: u64,
    pub expires_at_ms: i64,
    /// The caller asked for a room start (issue #262 FR-MC-01).
    pub in_person: bool,
}

/// Consume the oldest pending row that the desktop can offer. `None` when the ledger is empty,
/// unreadable, or every pending row has already been offered / expired.
pub fn take() -> Option<OfferedStart> {
    Store::host().take()
}

/// Desktop claimed a row then could not show Offered (another offer already on screen).
/// The row stays pending so the next Idle tick can offer it.
pub fn release_claim(id: u64) -> Result<(), RequestError> {
    Store::host().release_claim(id)
}

/// Record a decision made now. Storage failure must be retried by the desktop.
pub fn resolve(id: u64, outcome: Outcome) -> Result<(), RequestError> {
    Store::host().resolve(id, outcome)
}

/// Retry with the original decision time, never the retry time. A timeout written by a
/// concurrent poll cannot erase a human decision made while this claimed offer was valid.
pub fn resolve_at(id: u64, outcome: Outcome, decided_at_ms: i64) -> Result<(), RequestError> {
    Store::host().resolve_at(id, outcome, decided_at_ms)
}

/// Poll one approval. `Ok(None)` when the id was never issued.
pub fn status(id: u64) -> Result<Option<String>, RequestError> {
    Store::host().status(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

    /// A store in its own temp directory. Named explicitly rather than through the
    /// `SHOGUN_MEETING_*` env overrides: those are process-global, and other tests in this binary
    /// (the MCP `tools/call` sweep) run in parallel with these ones.
    fn isolated() -> Store {
        let dir = std::env::temp_dir().join(format!(
            "shogun-record-now-{}-{}-{}",
            std::process::id(),
            now_ms(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::create_dir_all(&dir);
        Store::in_dir(&dir)
    }

    fn write_settings(store: &Store, body: &str) {
        std::fs::write(store.settings_path().unwrap(), body).unwrap();
    }

    fn enable_notes(store: &Store) {
        write_settings(store, r#"{"enabled":true}"#);
    }

    fn ledger_path(store: &Store) -> PathBuf {
        store.ledger.clone().unwrap()
    }

    fn queued_id(body: &str) -> u64 {
        serde_json::from_str::<serde_json::Value>(body)
            .unwrap()
            .get("approval_id")
            .and_then(serde_json::Value::as_u64)
            .unwrap()
    }

    #[test]
    fn request_refuses_when_notes_are_off() {
        let store = isolated();
        write_settings(&store, r#"{"enabled":false}"#);
        assert_eq!(store.request().unwrap_err(), RequestError::NotesOff);
        assert_eq!(RequestError::NotesOff.http_status(), 409);
        assert!(store.take().is_none());
        assert!(
            store.status(1).unwrap().is_none(),
            "a refused request must not mint an id"
        );
    }

    #[test]
    fn malformed_settings_are_unavailable_not_notes_off() {
        let store = isolated();
        write_settings(&store, "{broken");

        assert_eq!(store.request().unwrap_err(), RequestError::Unavailable);
    }

    #[test]
    fn request_then_take_when_notes_are_on() {
        let store = isolated();
        enable_notes(&store);
        let body = store.request().unwrap();
        assert!(body.contains("queued"));
        assert!(body.contains("L2"));
        assert!(body.contains("\"status\":\"pending\""));
        let id = queued_id(&body);
        assert!(id >= 1);
        assert_eq!(store.take().map(|offer| offer.id), Some(id));
        assert!(store.take().is_none(), "a consumed request must not fire twice");
        let polled = store.status(id).unwrap().unwrap();
        assert!(polled.contains("pending"));
        assert!(polled.contains(&id.to_string()));
    }

    #[test]
    fn queue_then_desktop_decision_is_visible_on_poll() {
        let store = isolated();
        enable_notes(&store);
        let id = queued_id(&store.request().unwrap());
        assert_eq!(store.take().map(|offer| offer.id), Some(id));
        store.resolve(id, Outcome::Approved).unwrap();
        let body = store.status(id).unwrap().unwrap();
        assert!(body.contains("\"status\":\"approved\""));
        store.resolve(id, Outcome::Rejected).unwrap();
        assert!(
            store.status(id).unwrap().unwrap().contains("approved"),
            "a terminal row must not be overwritten"
        );
    }

    #[test]
    fn fifo_offers_oldest_pending_row() {
        let store = isolated();
        enable_notes(&store);
        let first = queued_id(&store.request().unwrap());
        let second = queued_id(&store.request().unwrap());
        assert_eq!(store.take().map(|offer| offer.id), Some(first));
        assert_eq!(store.take().map(|offer| offer.id), Some(second));
    }

    #[test]
    fn release_claim_lets_the_next_tick_offer_again() {
        let store = isolated();
        enable_notes(&store);
        let id = queued_id(&store.request().unwrap());
        assert_eq!(store.take().map(|offer| offer.id), Some(id));
        store.release_claim(id).unwrap();
        assert_eq!(store.take().map(|offer| offer.id), Some(id));
    }

    #[test]
    fn unconsumed_row_expires_after_fr_api_04_timeout() {
        let store = isolated();
        enable_notes(&store);
        let requested = 1_000;
        let id = queued_id(&store.request_at(requested, false).unwrap());
        assert!(store.status_at(id, requested + timeout_ms())
            .unwrap()
            .unwrap()
            .contains("pending"));
        let expired = store.status_at(id, requested + timeout_ms() + 1)
            .unwrap()
            .unwrap();
        assert!(expired.contains("expired"));
        assert!(store.take_at(requested + timeout_ms() + 1).is_none());
    }

    #[test]
    fn unknown_id_has_no_status() {
        let store = isolated();
        enable_notes(&store);
        let _ = store.request().unwrap();
        assert!(store.status(9_999_999).unwrap().is_none());
    }

    #[test]
    fn policy_and_storage_errors_do_not_share_a_code() {
        assert_ne!(RequestError::NotesOff.code(), RequestError::Storage.code());
        assert_eq!(RequestError::Storage.http_status(), 503);
        assert_eq!(RequestError::Storage.mcp_code(), -32603);
        assert!(!RequestError::Storage.message().contains('\\'));
    }

    #[test]
    fn queue_response_is_not_l3_actions_status() {
        let store = isolated();
        enable_notes(&store);
        let body = store.request().unwrap();
        assert!(!body.contains("timed_out"));
        assert!(!body.contains("/v1/actions/status"));
    }

    #[test]
    fn offered_pending_row_still_hits_the_poll_ceiling() {
        let store = isolated();
        enable_notes(&store);
        let requested = 1_000;
        let id = queued_id(&store.request_at(requested, false).unwrap());
        assert_eq!(store.take_at(requested + 1).map(|offer| offer.id), Some(id));
        let expired = store.status_at(id, requested + timeout_ms() + 1)
            .unwrap()
            .unwrap();
        assert!(expired.contains("expired"));
    }

    #[test]
    fn corrupt_ledger_is_unavailable_not_wiped() {
        let store = isolated();
        enable_notes(&store);
        let path = ledger_path(&store);
        std::fs::write(&path, "{broken").unwrap();
        assert_eq!(store.request().unwrap_err(), RequestError::Storage);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{broken");
        assert_eq!(store.status(1).unwrap_err(), RequestError::Storage);
    }

    #[test]
    fn next_id_skips_existing_rows_after_a_partial_write() {
        let store = isolated();
        enable_notes(&store);
        let path = ledger_path(&store);
        std::fs::write(
            &path,
            r#"{"next_id":1,"records":[{"id":9,"requested_at_ms":1,"status":"pending"}]}"#,
        )
        .unwrap();
        let id = queued_id(&store.request().unwrap());
        assert_eq!(id, 10);
    }

    #[test]
    fn decline_outcome_expires_only_after_the_api_card_grace() {
        assert_eq!(decline_outcome(true, 9_999, 10_000), Outcome::Rejected);
        assert_eq!(decline_outcome(true, 10_000, 10_000), Outcome::Expired);
        assert_eq!(decline_outcome(false, 10_000, 10_000), Outcome::Rejected);
    }

    #[test]
    fn settle_offer_error_retries_busy_and_refuses_notes_off() {
        let store = isolated();
        enable_notes(&store);
        let id = queued_id(&store.request().unwrap());
        assert_eq!(store.take().map(|offer| offer.id), Some(id));
        store.settle_offer_error(id, OFFER_BUSY, now_ms()).unwrap();
        assert_eq!(store.take().map(|offer| offer.id), Some(id));
        store.settle_offer_error(id, OFFER_LOCK_BUSY, now_ms()).unwrap();
        assert_eq!(store.take().map(|offer| offer.id), Some(id));
        store.settle_offer_error(id, OFFER_NOTES_OFF, now_ms()).unwrap();
        assert!(store.status(id).unwrap().unwrap().contains("rejected"));
        let other = queued_id(&store.request().unwrap());
        assert_eq!(store.take().map(|offer| offer.id), Some(other));
        store.settle_offer_error(other, "could not start meeting notes", now_ms()).unwrap();
        assert!(store.status(other).unwrap().unwrap().contains("failed"));
    }
    #[test]
    fn decision_retry_survives_storage_error_and_a_concurrent_timeout() {
        let store = isolated();
        enable_notes(&store);
        let id = queued_id(&store.request().unwrap());
        let offer = store.take().unwrap();
        assert_eq!(offer.id, id);
        let decided_at = now_ms();
        // Inject a transient read failure without changing the ledger's contents.
        let path = ledger_path(&store);
        let saved = path.with_extension("saved");
        std::fs::rename(&path, &saved).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            store.resolve_at(id, Outcome::Approved, decided_at),
            Err(RequestError::Storage)
        );
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(&saved, &path).unwrap();
        assert!(store.status_at(id, offer.expires_at_ms + 1)
            .unwrap()
            .unwrap()
            .contains("expired"));
        store.resolve_at(id, Outcome::Approved, decided_at).unwrap();
        assert!(store.status(id).unwrap().unwrap().contains("approved"));
        store.resolve_at(id, Outcome::Rejected, decided_at).unwrap();
        assert!(store.status(id).unwrap().unwrap().contains("approved"));
    }

    #[test]
    fn decision_after_deadline_cannot_revive_expired_offer() {
        let store = isolated();
        enable_notes(&store);
        let id = queued_id(&store.request_at(1_000, false).unwrap());
        let offer = store.take_at(1_001).unwrap();
        assert_eq!(offer.expires_at_ms, 1_000 + timeout_ms());
        store.resolve_at(id, Outcome::Approved, offer.expires_at_ms + 1).unwrap();
        assert!(store.status(id).unwrap().unwrap().contains("expired"));
    }
}
