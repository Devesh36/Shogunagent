//! Read-only app update state for MCP `device.update.get`, REST `GET /v1/device/update` and
//! `shogun update` (issue #1296, invariant 6). Installing, restarting and the Automatic updates
//! switch stay in the app's Settings: an AI client can never relaunch the app.
//!
//! The desktop updater worker owns the state and mirrors it to `<app-data>/update_status.json`
//! (`{"version": "1.2.3", "staged": true, "next_check_ms": 1760000000000}`). The switch is read
//! from the desktop's own `automatic-updates` file (`on`/`off`; missing = on). Nothing here is
//! secret. A missing or unreadable mirror reads as "no update known".

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::plan_source::{automatic_updates_path, update_status_json_path};

/// What the desktop worker mirrors. `staged` = downloaded (or installed) and waiting for a
/// relaunch; otherwise `version` is only offered by the server.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mirror {
    pub version: Option<String>,
    pub staged: bool,
    pub next_check_ms: Option<u64>,
}

/// `{"automatic": bool, "version": string|null, "staged": bool, "next_check_ms": int|null}`.
pub fn get() -> String {
    render(
        automatic_updates_path().as_deref(),
        update_status_json_path().as_deref(),
    )
}

/// Desktop only: replace the mirror atomically (owner-only, not fsynced; it is rewritten on change).
pub fn write(mirror: &Mirror) -> Result<(), String> {
    let path = update_status_json_path().ok_or("update status unavailable")?;
    let bytes = serde_json::to_vec(mirror).map_err(|_| "update status unavailable")?;
    crate::settings_file::write_atomic(&path, &bytes, false)
}

fn render(switch: Option<&Path>, mirror: Option<&Path>) -> String {
    let automatic = automatic(switch);
    let mirror = mirror
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|json| parse(&json))
        .unwrap_or_default();
    serde_json::json!({
        "automatic": automatic,
        "version": mirror.version,
        "staged": mirror.staged,
        // Off means no scheduled check; the mirror can lag the switch by one worker tick.
        "next_check_ms": mirror.next_check_ms.filter(|_| automatic),
    })
    .to_string()
}

/// Same rule as the desktop's `automatic_enabled`: missing = on, anything but `on` = off.
fn automatic(switch: Option<&Path>) -> bool {
    let Some(path) = switch else {
        return true;
    };
    match std::fs::read_to_string(path) {
        Ok(value) => value == "on",
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

/// Garbage reads as "no update known"; a version that is not a plain version string is dropped,
/// and `staged` without a version is meaningless.
pub fn parse(json: &str) -> Mirror {
    let mut mirror: Mirror = serde_json::from_str(json).unwrap_or_default();
    mirror.version = mirror.version.filter(|v| {
        !v.is_empty()
            && v.len() <= 64
            && v.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
    });
    mirror.staged &= mirror.version.is_some();
    mirror
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_keeps_valid_state_and_drops_garbage() {
        let staged = parse(r#"{"version":"1.2.3-beta.1","staged":true,"next_check_ms":42}"#);
        assert_eq!(
            staged,
            Mirror {
                version: Some("1.2.3-beta.1".into()),
                staged: true,
                next_check_ms: Some(42)
            }
        );
        assert_eq!(parse("not json"), Mirror::default());
        assert_eq!(parse(r#"{"version":7}"#), Mirror::default());
        assert_eq!(
            parse(r#"{"version":"1.0 <script>","staged":true}"#),
            Mirror::default()
        );
        assert_eq!(parse(r#"{"version":"","staged":true}"#), Mirror::default());
        assert_eq!(
            parse(&format!(r#"{{"version":"{}"}}"#, "9".repeat(65))),
            Mirror::default()
        );
        let unknown = parse(r#"{"next_check_ms":5,"phase":"downloading","received":10}"#);
        assert_eq!(
            unknown,
            Mirror {
                next_check_ms: Some(5),
                ..Mirror::default()
            }
        );
    }

    #[test]
    fn render_reads_the_switch_and_hides_the_check_when_off() {
        let dir = std::env::temp_dir().join(format!("shogun-update-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let switch = dir.join("automatic-updates");
        let mirror = dir.join("update_status.json");
        let read = || -> serde_json::Value {
            serde_json::from_str(&render(Some(&switch), Some(&mirror))).unwrap()
        };

        let empty = read();
        assert_eq!(empty["automatic"], true, "missing switch = on");
        assert!(empty["version"].is_null() && empty["next_check_ms"].is_null());
        assert_eq!(empty["staged"], false);

        std::fs::write(
            &mirror,
            r#"{"version":"0.4.0","staged":false,"next_check_ms":99}"#,
        )
        .unwrap();
        let available = read();
        assert_eq!(
            (available["version"].as_str(), available["staged"].as_bool()),
            (Some("0.4.0"), Some(false))
        );
        assert_eq!(available["next_check_ms"], 99);

        std::fs::write(&switch, "off").unwrap();
        let off = read();
        assert_eq!(off["automatic"], false);
        assert!(
            off["next_check_ms"].is_null(),
            "no scheduled check while off"
        );
        assert_eq!(off["version"], "0.4.0");
        std::fs::write(&switch, "garbled").unwrap();
        assert_eq!(
            read()["automatic"],
            false,
            "a torn switch fails closed like the desktop"
        );
        std::fs::write(&switch, "on").unwrap();
        assert_eq!(read()["automatic"], true);

        // The rendered object carries exactly the read-only fields, never an action.
        let keys: Vec<String> = read().as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys.len(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
