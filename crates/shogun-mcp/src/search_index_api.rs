//! `search_index.status` / `search_index.compact.start` / `search_index.compact.stop` (#714 step 3,
//! invariant 6): how a start or stop reaches the job, and how its progress reaches every face.
//!
//! The compaction job runs in the desktop app, the long-lived process that owns capture, so it can
//! pace itself around capture and keep running after an MCP client disconnects. The API faces run
//! in short-lived processes, so they never run it themselves. They use the same two-file pattern
//! as the Connections screen (issue #82, [`crate::connections_api`]):
//!
//! - [`REQUESTS_FILE`] holds the latest request (`start` or `stop`) from any face, Settings
//!   included. The desktop takes it within a couple of seconds. The latest request wins, so a stop
//!   sent before the app picked up a start cancels it. A start older than [`REQUEST_TTL_MS`] is
//!   dropped: a request someone made while the app was closed does not start a rewrite days later.
//! - [`STATUS_FILE`] holds the job's last published status, rewritten about once a second while it
//!   runs. Counts, sizes and state names only: never a query, a row or indexed text.
//!
//! Both files sit beside the database. Writes take the shared file lock and replace the file
//! atomically ([`crate::settings_file`]).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The job's published status, beside the DB.
pub const STATUS_FILE: &str = "search_index_status.json";
/// The latest start / stop request, beside the DB.
pub const REQUESTS_FILE: &str = "search_index_requests.json";
/// How long a start request waits for the app: the same 10 minutes an API approval waits
/// (FR-API-04). Older ones are dropped, never run.
pub const REQUEST_TTL_MS: u64 = 10 * 60 * 1000;
/// A `running` status older than this was left by an app that quit or crashed mid-run: the job is
/// reported as interrupted (a new start resumes it).
pub const RUNNING_STALE_MS: u64 = 30_000;

const BUSY: &str = "search index requests are busy";

/// Where the two shared files are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchIndexPaths {
    /// [`STATUS_FILE`].
    pub status: PathBuf,
    /// [`REQUESTS_FILE`].
    pub requests: PathBuf,
}

impl SearchIndexPaths {
    /// Beside the database, where the desktop and the daemons both find them.
    pub fn for_db(db_path: impl AsRef<Path>) -> Self {
        let dir = db_path.as_ref().parent().map(Path::to_path_buf).unwrap_or_default();
        Self { status: dir.join(STATUS_FILE), requests: dir.join(REQUESTS_FILE) }
    }
}

/// What a face asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Start,
    Stop,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Start => "start",
            Action::Stop => "stop",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Request {
    action: Action,
    requested_at_ms: u64,
}

fn read_request(path: &Path) -> Option<Request> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Whether a request is still due: a stop always is, a start only within [`REQUEST_TTL_MS`].
fn fresh(request: Request, now_ms: u64) -> bool {
    match request.action {
        Action::Stop => true,
        Action::Start => now_ms.saturating_sub(request.requested_at_ms) <= REQUEST_TTL_MS,
    }
}

/// Record `action` as the latest request (replacing any earlier one).
pub fn request(path: &Path, action: Action, now_ms: u64) -> Result<(), String> {
    let _lock = crate::settings_file::lock(path, BUSY)?;
    let bytes = serde_json::to_vec(&Request { action, requested_at_ms: now_ms })
        .map_err(|e| e.to_string())?;
    crate::settings_file::write_atomic(path, &bytes, true)
}

/// The request waiting for the app, if it is still due. Read-only.
pub fn pending(path: &Path, now_ms: u64) -> Option<Action> {
    read_request(path).filter(|r| fresh(*r, now_ms)).map(|r| r.action)
}

/// Take the waiting request (the desktop's side): returns it if still due and removes the file
/// either way. A missing file costs one failed `stat`, so the idle poll is cheap.
pub fn take(path: &Path, now_ms: u64) -> Option<Action> {
    if std::fs::metadata(path).is_err() {
        return None;
    }
    let _lock = crate::settings_file::lock(path, BUSY).ok()?;
    let request = read_request(path);
    let _ = std::fs::remove_file(path);
    request.filter(|r| fresh(*r, now_ms)).map(|r| r.action)
}

/// Publish the job status (the desktop's side). Not fsynced: it is a progress snapshot that is
/// rewritten every second while the job runs.
pub fn write_status(path: &Path, status: &serde_json::Value) -> Result<(), String> {
    let bytes = serde_json::to_vec(status).map_err(|e| e.to_string())?;
    crate::settings_file::write_atomic(path, &bytes, false)
}

/// The last published status, if any. Missing or unreadable is `None`: the job never ran here.
pub fn read_status(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// A start or stop takes no arguments. An empty body, `{}` and JSON `null` are accepted; anything
/// else is refused so a caller does not believe an option it sent was applied.
pub fn parse_empty_body(body: &str) -> Result<(), String> {
    let trimmed = body.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(());
    }
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(serde_json::Value::Object(map)) if map.is_empty() => Ok(()),
        _ => Err("invalid request: this action takes no arguments".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("shogun_search_index_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_latest_request_wins_and_is_taken_once() {
        let paths = SearchIndexPaths::for_db(dir("latest").join("memory.db"));
        assert_eq!(take(&paths.requests, 1_000), None);
        request(&paths.requests, Action::Start, 1_000).unwrap();
        assert_eq!(pending(&paths.requests, 1_500), Some(Action::Start));
        request(&paths.requests, Action::Stop, 2_000).unwrap();
        assert_eq!(pending(&paths.requests, 2_000), Some(Action::Stop));
        assert_eq!(take(&paths.requests, 2_000), Some(Action::Stop));
        assert_eq!(take(&paths.requests, 2_000), None, "taken once");
        assert_eq!(pending(&paths.requests, 2_000), None);
    }

    #[test]
    fn a_start_nobody_picked_up_expires() {
        let paths = SearchIndexPaths::for_db(dir("ttl").join("memory.db"));
        request(&paths.requests, Action::Start, 1_000).unwrap();
        let late = 1_000 + REQUEST_TTL_MS + 1;
        assert_eq!(pending(&paths.requests, late), None);
        assert_eq!(take(&paths.requests, late), None);
        assert!(!paths.requests.exists(), "an expired request is cleared, not kept");
    }

    #[test]
    fn status_round_trips_and_garbage_reads_as_none() {
        let paths = SearchIndexPaths::for_db(dir("status").join("memory.db"));
        assert_eq!(read_status(&paths.status), None);
        let status = serde_json::json!({ "state": "running", "steps": 3 });
        write_status(&paths.status, &status).unwrap();
        assert_eq!(read_status(&paths.status), Some(status));
        std::fs::write(&paths.status, b"{not json").unwrap();
        assert_eq!(read_status(&paths.status), None);
        std::fs::write(&paths.requests, b"[]").unwrap();
        assert_eq!(pending(&paths.requests, 0), None);
    }

    #[test]
    fn start_and_stop_take_no_arguments() {
        for ok in ["", "  ", "{}", "null", " { } "] {
            assert!(parse_empty_body(ok).is_ok(), "{ok:?}");
        }
        for bad in ["{\"force\":true}", "[]", "1", "nope"] {
            assert!(parse_empty_body(bad).is_err(), "{bad:?}");
        }
    }
}
