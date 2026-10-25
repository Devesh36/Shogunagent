//! Which port `shogun` dials, and which requests carry the token (FR-API-01, issue #776).
//!
//! `SHOGUN_API_PORT` wins when set (developers and scripts that started the server on a port of
//! their choosing). Otherwise the port a running `shogun-api` recorded next to its database
//! ([`shogun_mcp::api_port`]), found from `SHOGUN_DB_PATH` or the app's data folder. With neither
//! there is no port to dial: the CLI stops and says the server is not running (audit 2026-10-20).
//! It never guesses the default 7464: the server moves to an ephemeral port when 7464 is taken,
//! and a guess would hand the bearer token to whatever process holds it.

use std::path::Path;

use shogun_mcp::api_port;

/// The port `shogun-api` tries first. The CLI never dials it unrecorded (see the module docs).
pub const DEFAULT_PORT: u16 = 7464;

/// What `shogun` says when no running server recorded its port.
pub const NOT_RUNNING: &str = "SHOGUN's Memory API server isn't running: no `shogun-api` has \
recorded its port. Start it (with the Memory API on in ShogunAI Settings) and try again.";

/// The port to dial, or `None` when there is none to trust. `env_port` is `SHOGUN_API_PORT`,
/// `db_path` is `SHOGUN_DB_PATH`, and `app_data_dir` is the app's data folder (where the shipped
/// server keeps its database).
pub fn resolve(
    env_port: Option<&str>,
    db_path: Option<&Path>,
    app_data_dir: Option<&Path>,
) -> Option<u16> {
    if let Some(port) = env_port
        .and_then(|port| port.trim().parse::<u16>().ok())
        .filter(|port| *port != 0)
    {
        return Some(port);
    }
    match db_path {
        Some(db) => api_port::read(&api_port::port_file_path(db)),
        None => app_data_dir.and_then(|dir| api_port::read(&dir.join(api_port::PORT_FILE))),
    }
}

/// Whether a request should carry the bearer token. Discovery and health answer without one, so
/// the token is not sent there, where a process that is not SHOGUN could be listening.
/// Whether `path` is an operator route (E15 / E18): deciding an approval, changing a grant,
/// teams. The CLI sends `SHOGUN_ADMIN_TOKEN` there (unless `--token` is given), never the agent
/// token, so an agent's key cannot approve or grant.
pub fn needs_admin(path: &str) -> bool {
    let path = path.split('?').next().unwrap_or(path);
    path == "/v1/acl/grant"
        || path == "/v1/acl/revoke"
        || path == "/v1/teams"
        || (path.starts_with("/v1/approvals/") && path.ends_with("/decide"))
}

pub fn needs_token(path: &str) -> bool {
    let path = path.split('?').next().unwrap_or(path);
    !matches!(path, "/v1/status" | "/v1/metrics")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shogun-cli-port-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn an_explicit_port_wins() {
        let dir = scratch("explicit");
        api_port::write(&dir.join(api_port::PORT_FILE), 53_447).expect("record");
        let db = dir.join("memory.db");
        assert_eq!(resolve(Some("9000"), Some(&db), None), Some(9_000));
        // Garbage in the variable is ignored rather than dialling port 0.
        assert_eq!(resolve(Some("nope"), Some(&db), None), Some(53_447));
        assert_eq!(resolve(Some("0"), Some(&db), None), Some(53_447));
        // The variable alone is enough: a developer's server on a port of their choosing.
        assert_eq!(resolve(Some("9000"), None, None), Some(9_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_recorded_port_is_found_next_to_the_database() {
        let dir = scratch("recorded");
        api_port::write(&dir.join(api_port::PORT_FILE), 53_447).expect("record");
        assert_eq!(resolve(None, Some(&dir.join("memory.db")), None), Some(53_447));
        assert_eq!(resolve(None, None, Some(&dir)), Some(53_447));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_a_record_no_port_is_guessed() {
        // Audit 2026-10-20: the default port is never dialled unrecorded, so the token cannot go
        // to whatever process holds 7464 while SHOGUN's server is not running.
        let dir = scratch("default");
        assert_eq!(resolve(None, Some(&dir.join("memory.db")), None), None);
        assert_eq!(resolve(None, None, Some(&dir)), None);
        assert_eq!(resolve(None, None, None), None);
        assert_eq!(resolve(Some("nope"), None, None), None);
        assert!(NOT_RUNNING.contains("isn't running"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discovery_and_health_go_without_the_token() {
        assert!(!needs_token("/v1/status"));
        assert!(!needs_token("/v1/metrics"));
        assert!(!needs_token("/v1/status?x=1"));
        assert!(needs_token("/v1/state/people"));
        assert!(needs_token("/v1/memory/notes"));
        assert!(needs_token("/v1/statuses"));
    }
}
