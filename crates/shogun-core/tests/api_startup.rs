//! The `shogun-api` binary (the REST face the CLI talks to) at start-up (issue #744): a refusal
//! names the settings file it checked, and a locked plan is said once on stderr and named in every
//! 403 `plan_required` body, after authentication.
//!
//! Debug builds only (the `SHOGUN_*_JSON` path overrides live there) and not on macOS (there the
//! binary reads the token list from the real login Keychain; elsewhere it registers
//! `SHOGUN_API_TOKEN`, which this test sets).
#![cfg(all(feature = "daemon-server", debug_assertions, not(target_os = "macos")))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "shogun-api-startup-{tag}-{}-{nanos}",
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

/// Kills the server when the test ends, pass or fail.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn server(dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_shogun-api"));
    for var in ["SHOGUN_API_TOKEN", "SHOGUN_MEMORY_API_SETTINGS", "SHOGUN_POSTHOG_KEY"] {
        cmd.env_remove(var);
    }
    cmd.env("SHOGUN_DB_PATH", dir.join("memory.db"))
        .env("SHOGUN_ONBOARDING_JSON", dir.join("onboarding.json"))
        .env("SHOGUN_BILLING_JSON", dir.join("billing.json"))
        .env("SHOGUN_TRIAL_RELAY_JSON", dir.join("trial_relay.json"))
        .env("SHOGUN_ANALYTICS_JSON", dir.join("analytics.json"))
        .env("SHOGUN_DB_KEY", "00".repeat(32))
        // Port 0: the OS picks a free port, and the server prints where it listens.
        .env("SHOGUN_API_PORT", "0")
        .current_dir(dir);
    cmd
}

fn get(port: u16, path: &str, token: Option<&str>) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\n{auth}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

#[test]
fn a_refusal_names_the_settings_file_it_checked() {
    let dir = Dir::new("refuse");
    std::fs::write(dir.0.join("memory_api.json"), r#"{"enabled":false}"#).unwrap();
    let out = server(&dir.0).stdin(Stdio::null()).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.starts_with("shogun-api: Memory API is disabled.") && err.contains("memory_api.json"),
        "{err}"
    );
}

#[test]
fn a_locked_plan_is_named_in_the_403_after_auth() {
    let dir = Dir::new("locked");
    std::fs::write(dir.0.join("memory_api.json"), r#"{"enabled":true}"#).unwrap();
    let expired = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 30 * 86_400;
    std::fs::write(
        dir.0.join("onboarding.json"),
        format!(r#"{{"version":1,"state":{{"completed":true,"step":"ready","trial_started_at":{expired}}}}}"#),
    )
    .unwrap();
    // The licence API said the trial is over (no token, its end passed), as the desktop records
    // it: a stamp alone would read as "not started" (issue #1031).
    let ended = expired + 7 * 86_400;
    std::fs::write(
        dir.0.join("trial_relay.json"),
        format!(r#"{{"version":1,"device_id":"dev-test","token":null,"expires_at":null,"checked_at":{ended},"trial_ends_at":{ended}}}"#),
    )
    .unwrap();
    // A current store, as the app leaves it: the server opens it without migrating (#800).
    drop(shogun_memory::open_encrypted(dir.0.join("memory.db"), &shogun_memory::DbKey::new([0; 32])).unwrap());
    let mut child = server(&dir.0)
        .env("SHOGUN_API_TOKEN", "shogun_0123456789abcdef0123")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let _server = Server(child);

    // "shogun-memory-api listening on http://127.0.0.1:PORT  (db: …)"
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).unwrap();
    let port: u16 = line
        .split("http://127.0.0.1:")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("no listening line: {line:?}"));

    let locked = get(port, "/v1/state/people", Some("shogun_0123456789abcdef0123"));
    assert!(locked.starts_with("HTTP/1.1 403"), "{locked}");
    assert!(
        locked.contains(r#""error":"plan_required""#)
            && locked.contains(r#""reason":"trial_ended""#),
        "{locked}"
    );
    for token in [None, Some("shogun_ffffffffffffffffffff")] {
        let refused = get(port, "/v1/state/people", token);
        assert!(
            refused.starts_with("HTTP/1.1 401") && !refused.contains("trial_ended"),
            "no plan before auth: {refused}"
        );
    }

    drop(_server);
    let mut err = String::new();
    BufReader::new(stderr).read_to_string(&mut err).unwrap();
    assert!(
        err.contains("shogun-api: the Memory API is locked on this Mac (trial_ended)"),
        "{err}"
    );
}
