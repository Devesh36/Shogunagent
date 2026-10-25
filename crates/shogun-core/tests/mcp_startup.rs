//! The `shogun-mcp` binary as an MCP client launches it (issue #744): each start-up refusal says
//! which file it checked and why, and a locked plan is explained on stderr, in the handshake and in
//! every `plan_required` answer. Runs the real binary.
//!
//! Debug builds only: the `SHOGUN_*_JSON` path overrides that keep the plan and analytics files in
//! a temp folder exist only there. Not on macOS: there the binary reads the token list from the
//! real login Keychain, which a test must not touch; elsewhere it takes `SHOGUN_API_TOKEN`, which
//! the harness removes. CI runs this on Linux (`cargo test -p shogun-core --features daemon-server`, a superset of `db`).
#![cfg(all(feature = "db", debug_assertions, not(target_os = "macos")))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as URL_SAFE};
use base64::Engine as _;
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
        Self(path)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The token the harness issues (off macOS the binary registers `SHOGUN_API_TOKEN` as issued and
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
    ] {
        cmd.env_remove(var);
    }
    cmd.env("SHOGUN_ONBOARDING_JSON", dir.join("onboarding.json"))
        .env("SHOGUN_BILLING_JSON", dir.join("billing.json"))
        .env("SHOGUN_TRIAL_RELAY_JSON", dir.join("trial_relay.json"))
        // Debug builds trust this key instead of the embedded one: the trial token below is
        // signed with it.
        .env("SHOGUN_LICENSE_PUBKEY", B64.encode(signing().verifying_key().to_bytes()))
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

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// The test's licence API key.
fn signing() -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[31u8; 32])
}

/// A trial that started at `trial_started_at` on this Mac: the stamp in `onboarding.json` and the
/// licence API's signed trial token in `trial_relay.json` (issue #1031), issued at the start.
/// Off macOS there is no hardware id, so the id recorded beside the token decides.
fn stamp(dir: &Path, trial_started_at: i64) {
    use ed25519_dalek::Signer as _;
    std::fs::write(
        dir.join("onboarding.json"),
        format!(r#"{{"version":1,"state":{{"completed":true,"step":"ready","trial_started_at":{trial_started_at}}}}}"#),
    )
    .unwrap();
    let (iat, end) = (trial_started_at, trial_started_at + 7 * 86_400);
    let body = serde_json::json!({
        "v": 1, "lic": "trial_dev-test", "plan": "trial", "status": "trialing", "device": "dev-test",
        "iat": iat, "exp": iat + 24 * 3600, "period_end": end,
        "cancel_at_period_end": true, "grace_days": 0,
    })
    .to_string();
    let token = format!(
        "v1.{}.{}",
        URL_SAFE.encode(body.as_bytes()),
        URL_SAFE.encode(signing().sign(body.as_bytes()).to_bytes())
    );
    std::fs::write(
        dir.join("trial_relay.json"),
        serde_json::json!({
            "version": 1, "device_id": "dev-test", "token": token, "expires_at": iat + 24 * 3600,
            "checked_at": iat, "trial_ends_at": end,
        })
        .to_string(),
    )
    .unwrap();
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
fn an_ended_trial_is_explained_on_stderr_in_the_handshake_and_in_every_refusal() {
    let dir = Dir::new("locked");
    std::fs::write(dir.0.join("memory_api.json"), r#"{"enabled":true}"#).unwrap();
    stamp(&dir.0, now_secs() - 30 * 86_400);
    let mut cmd = server(&dir.0);
    cmd.env("SHOGUN_DB_PATH", seed_store(&dir.0));
    let out = run(cmd, &[INITIALIZE, SEARCH]);
    let err = stderr(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "the server still starts and serves: {err}"
    );
    assert!(err.contains("locked on this Mac (trial_ended)"), "{err}");
    // Audit 2026-10-20 M5: no process-trust note; the session runs on the harness token.
    assert!(!err.contains("trusted as the local user"), "{err}");

    let replies = responses(&out);
    assert_eq!(replies.len(), 2, "{replies:?}");
    let instructions = replies[0]["result"]["instructions"]
        .as_str()
        .unwrap_or_default();
    assert!(
        instructions.contains("(trial_ended)") && instructions.contains("Plan & billing"),
        "{instructions}"
    );
    assert_eq!(replies[1]["error"]["code"], -32003);
    assert_eq!(
        replies[1]["error"]["message"],
        "plan_required: the Memory API needs Pro (or an active trial)"
    );
    assert_eq!(replies[1]["error"]["data"]["reason"], "trial_ended");
}

#[test]
fn a_running_trial_starts_quietly_and_answers() {
    let dir = Dir::new("running");
    std::fs::write(dir.0.join("memory_api.json"), r#"{"enabled":true}"#).unwrap();
    stamp(&dir.0, now_secs() - 86_400);
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
        "an open plan answers the call: {:?}",
        replies[1]
    );
}

#[test]
fn a_store_the_app_has_not_upgraded_is_refused_per_call_and_left_unmigrated() {
    // #800: only the app migrates. The server still starts and lists tools, but every tool call
    // answers `memory_update_pending`, and the store's history is exactly as it was.
    let dir = Dir::new("pending");
    std::fs::write(dir.0.join("memory_api.json"), r#"{"enabled":true}"#).unwrap();
    stamp(&dir.0, now_secs() - 86_400);
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
    assert_eq!(
        shogun_memory::schema_version(&conn).unwrap(),
        Some(shogun_memory::LATEST_SCHEMA_VERSION - 1),
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
