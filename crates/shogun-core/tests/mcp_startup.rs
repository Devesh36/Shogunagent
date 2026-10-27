//! The non-desktop `shogun-mcp` binary as an MCP client launches it: settings, authentication
//! and store-version gates remain enforced, while OSS reads need no desktop licence/trial.
//! Debug-only path overrides keep the tests' analytics/settings isolated.
#![cfg(all(feature = "db", debug_assertions, not(feature = "desktop")))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// A throwaway folder standing in for the app-data directory.
struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "shogun-mcp-startup-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        // SQLite's NOFOLLOW upgrade path also rejects macOS's /var → /private/var
        // alias. Use the real temp path without weakening the store's symlink guard.
        Self(path.canonicalize().unwrap())
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The token the harness issues (the non-desktop binary registers `SHOGUN_API_TOKEN` as issued and
/// checks the same value). Audit 2026-10-20 M5: there is no process trust, so a session needs it.
const HARNESS_TOKEN: &str = "shogun_harness_token";

/// The binary with nothing inherited that could steer it: no toolset filter, no real plan or
/// analytics files, a fixed database key instead of the Keychain, and the harness token.
fn server(dir: &Path) -> Command {
    let mut cmd = bare_server(dir);
    cmd.env("SHOGUN_API_TOKEN", HARNESS_TOKEN);
    cmd
}

/// [`server`] without any token.
fn bare_server(dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_shogun-mcp"));
    for var in [
        "SHOGUN_DB_PATH",
        "SHOGUN_API_TOKEN",
        "SHOGUN_MCP_TOOLSETS",
        "SHOGUN_MEMORY_API_SETTINGS",
        "SHOGUN_POSTHOG_KEY",
        "SHOGUN_LICENSE_PUBKEY_NEXT",
        "SHOGUN_LICENSE_PUBKEY",
    ] {
        cmd.env_remove(var);
    }
    cmd.env("SHOGUN_ONBOARDING_JSON", dir.join("onboarding.json"))
        .env("SHOGUN_BILLING_JSON", dir.join("billing.json"))
        .env("SHOGUN_TRIAL_RELAY_JSON", dir.join("trial_relay.json"))
        .env("SHOGUN_ANALYTICS_JSON", dir.join("analytics.json"))
        .env("SHOGUN_DB_KEY", "00".repeat(32))
        .current_dir(dir);
    cmd
}

fn run(mut cmd: Command, stdin_lines: &[&str]) -> Output {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn shogun-mcp");
    {
        let mut stdin = child.stdin.take().unwrap();
        for line in stdin_lines {
            // The server may already have exited on a refusal; a closed pipe is fine then.
            let _ = writeln!(stdin, "{line}");
        }
    }
    child.wait_with_output().expect("wait for shogun-mcp")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn responses(out: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("one JSON-RPC response per line"))
        .collect()
}

/// A current encrypted store under the harness's `SHOGUN_DB_KEY`, as the app leaves it: the
/// server opens it without migrating (#800) and never creates one.
fn seed_store(dir: &Path) -> PathBuf {
    let path = dir.join("memory.db");
    drop(shogun_memory::open_encrypted(&path, &shogun_memory::DbKey::new([0; 32])).unwrap());
    path
}

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
const SEARCH: &str = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"memory.search","arguments":{"query":"roadmap"}}}"#;

#[test]
fn without_shogun_db_path_it_says_where_it_looked() {
    let dir = Dir::new("unset");
    let out = run(server(&dir.0), &[]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("no Memory API settings at") && err.contains("memory_api.json"),
        "{err}"
    );
    assert!(err.contains("SHOGUN_DB_PATH is not set"), "{err}");
    assert!(out.stdout.is_empty(), "nothing on the protocol channel");
}

#[test]
fn a_launcher_pointing_into_the_legacy_folder_is_called_out() {
    let dir = Dir::new("legacy");
    let legacy = dir.0.join("dev.shogun.spike");
    std::fs::create_dir_all(&legacy).unwrap();
    let mut cmd = server(&dir.0);
    cmd.env("SHOGUN_DB_PATH", legacy.join("memory.db"));
    let out = run(cmd, &[]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("old dev.shogun.spike folder") && err.contains("Add to an app"),
        "{err}"
    );
    assert!(
        !legacy.join("memory.db").exists(),
        "a refused start never creates a database"
    );
}

#[test]
fn the_switch_off_and_a_broken_settings_file_read_differently() {
    let dir = Dir::new("switch");
    let mut cmd = server(&dir.0);
    cmd.env("SHOGUN_DB_PATH", dir.0.join("memory.db"));
    std::fs::write(dir.0.join("memory_api.json"), r#"{"enabled":false}"#).unwrap();
    let out = run(cmd, &[]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("Memory API is disabled. Enable it in ShogunAI Settings > Memory API."),
        "{err}"
    );

    std::fs::write(dir.0.join("memory_api.json"), "{not json").unwrap();
    let mut cmd = server(&dir.0);
    cmd.env("SHOGUN_DB_PATH", dir.0.join("memory.db"));
    let out = run(cmd, &[]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("are not valid"), "{err}");
}

#[test]
fn desktop_plan_metadata_cannot_lock_the_oss_stdio_server() {
    let dir = Dir::new("locked");
    std::fs::write(dir.0.join("memory_api.json"), r#"{"enabled":true}"#).unwrap();
    // Stale/corrupt desktop metadata does not select server policy at runtime.
    std::fs::write(dir.0.join("onboarding.json"), r#"{"version":1,"state":{"trial_started_at":1}}"#).unwrap();
    std::fs::write(dir.0.join("billing.json"), "not a signed licence").unwrap();
    let mut cmd = server(&dir.0);
    cmd.env("SHOGUN_DB_PATH", seed_store(&dir.0));
    let out = run(cmd, &[INITIALIZE, SEARCH]);
    let err = stderr(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "the server still starts and serves: {err}"
    );
    assert!(!err.contains("locked on this Mac"), "{err}");
    // Audit 2026-10-20 M5: no process-trust note; the session runs on the harness token.
    assert!(!err.contains("trusted as the local user"), "{err}");

    let replies = responses(&out);
    assert_eq!(replies.len(), 2, "{replies:?}");
    let instructions = replies[0]["result"]["instructions"]
        .as_str()
        .unwrap_or_default();
    assert!(
        !instructions.contains("plan_required"),
        "{instructions}"
    );
    assert!(replies[1].get("error").is_none(), "{replies:?}");
}

#[test]
fn no_desktop_plan_files_are_needed_for_authenticated_reads() {
    let dir = Dir::new("running");
    std::fs::write(dir.0.join("memory_api.json"), r#"{"enabled":true}"#).unwrap();
    let mut cmd = server(&dir.0);
    cmd.env("SHOGUN_DB_PATH", seed_store(&dir.0));
    let out = run(cmd, &[INITIALIZE, SEARCH]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(!err.contains("locked"), "{err}");
    let replies = responses(&out);
    let guide = replies[0]["result"]["instructions"].as_str().unwrap_or_default();
    assert!(
        guide.contains("Routing:") && !guide.contains("plan_required"),
        "routing guide only, no lock text: {:?}",
        replies[0]
    );
    assert!(
        replies[1].get("error").is_none(),
        "an authenticated OSS read answers the call: {:?}",
        replies[1]
    );
}

#[test]
fn a_store_the_app_has_not_upgraded_is_refused_per_call_and_left_unmigrated() {
    // #800: only the app migrates. The server still starts and lists tools, but every tool call
    // answers `memory_update_pending`, and the store's history is exactly as it was.
    let dir = Dir::new("pending");
    std::fs::write(dir.0.join("memory_api.json"), r#"{"enabled":true}"#).unwrap();
    let path = seed_store(&dir.0);
    let key = shogun_memory::DbKey::new([0; 32]);
    let latest = i64::from(shogun_memory::LATEST_SCHEMA_VERSION);
    shogun_memory::open_encrypted_existing_for_upgrade(&path, &key)
        .unwrap()
        .execute("DELETE FROM refinery_schema_history WHERE version = ?1", [latest])
        .unwrap();
    let mut cmd = server(&dir.0);
    cmd.env("SHOGUN_DB_PATH", &path);
    let out = run(cmd, &[INITIALIZE, SEARCH]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("memory_update_pending: ShogunAI needs to finish updating"), "{err}");
    let replies = responses(&out);
    assert_eq!(replies[1]["error"]["code"], -32005, "{replies:?}");
    assert_eq!(replies[1]["error"]["data"]["reason"], "memory_update_pending");
    let conn = shogun_memory::open_encrypted_existing_for_upgrade(&path, &key).unwrap();
    // Below the latest (the previous embedded version; not LATEST - 1 while a version number is
    // taken on another branch and merged later).
    assert!(
        shogun_memory::schema_version(&conn).unwrap().is_some_and(|v| v < shogun_memory::LATEST_SCHEMA_VERSION),
        "the server ran no migration"
    );
}

#[test]
fn a_missing_store_is_refused_at_start_and_never_created() {
    let dir = Dir::new("nostore");
    std::fs::write(dir.0.join("memory_api.json"), r#"{"enabled":true}"#).unwrap();
    let mut cmd = server(&dir.0);
    cmd.env("SHOGUN_DB_PATH", dir.0.join("memory.db"));
    let out = run(cmd, &[INITIALIZE]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("memory_update_pending"), "{err}");
    assert!(!dir.0.join("memory.db").exists());
}

/// Audit 2026-10-20 M5: with no app connected and no token issued, an entry that names no app and
/// passes no token is refused at start.
#[test]
fn an_entry_with_neither_a_connection_nor_a_token_is_refused() {
    let dir = Dir::new("notoken");
    std::fs::write(dir.0.join("memory_api.json"), r#"{"enabled":true}"#).unwrap();
    let mut cmd = bare_server(&dir.0);
    cmd.env("SHOGUN_DB_PATH", seed_store(&dir.0));
    let out = run(cmd, &[INITIALIZE]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("names no app and passes no token"), "{err}");
    assert!(responses(&out).is_empty(), "nothing answered: {err}");
}
