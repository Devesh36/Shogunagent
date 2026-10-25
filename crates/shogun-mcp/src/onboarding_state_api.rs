//! Read-only projection of the desktop-owned onboarding file for MCP, REST and CLI.
//! This reports saved progress, not live permissions or billing entitlements. Unknown fields
//! never reach callers, and unsupported/corrupt files never become fabricated completed state.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const V1_STEPS: [&str; 6] = ["welcome", "reads", "permission", "plan", "connect", "ready"];
const V2_STEPS: [&str; 16] = [
    "intro",
    "welcome",
    "reads",
    "theme",
    "permission",
    "accessibility",
    "microphone",
    "screen_recording",
    "right_option",
    "scribe_demo",
    "dictation_demo",
    "privacy",
    "plan",
    "connect",
    "gate",
    "ready",
];

fn welcome() -> String {
    "welcome".to_owned()
}

/// Deliberate allowlist. Permission repair is a live desktop response hint, so it is omitted.
#[derive(Default, Deserialize, Serialize)]
struct SavedState {
    #[serde(default)]
    completed: bool,
    #[serde(default = "welcome")]
    step: String,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    intro_complete: bool,
    #[serde(default)]
    music_muted: bool,
    #[serde(default)]
    accessibility_skipped: bool,
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    trial_started_at: Option<i64>,
    #[serde(default)]
    first_launch_at: Option<i64>,
    #[serde(default)]
    first_answer_at: Option<i64>,
    #[serde(default)]
    restart_pending: Option<RestartPending>,
}

#[derive(Deserialize, Serialize)]
struct RestartPending {
    reason: String,
    bundle_id: String,
    step: String,
}

fn unknown(status: &str) -> Value {
    json!({"status": status, "state": null})
}

/// Project supported v2/v1 records and the legacy Accessibility-guide disposition, without
/// migrating or writing. Missing optional fields use exactly the desktop's stored-state defaults.
pub fn project(text: &str) -> Value {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return unknown("corrupt");
    };
    let Some(object) = value.as_object() else {
        return unknown("corrupt");
    };
    let Some(version) = object.get("version") else {
        if !object.is_empty()
            && !object.contains_key("completed")
            && !object.contains_key("skipped")
        {
            return unknown("corrupt");
        }
        let completed = object.get("completed").map_or(Some(false), Value::as_bool);
        let skipped = object.get("skipped").map_or(Some(false), Value::as_bool);
        let (Some(completed), Some(skipped)) = (completed, skipped) else {
            return unknown("corrupt");
        };
        let state = SavedState {
            completed: completed || skipped,
            step: if completed || skipped {
                "ready"
            } else {
                "welcome"
            }
            .to_owned(),
            accessibility_skipped: skipped,
            ..SavedState::default()
        };
        return json!({"status": "saved", "version": 0, "state": state});
    };
    let Some(version) = version.as_u64() else {
        return unknown("corrupt");
    };
    if version != 1 && version != 2 {
        return unknown("unsupported_schema");
    }
    let Some(raw_state) = object.get("state").filter(|state| state.is_object()) else {
        return unknown("corrupt");
    };
    let mut raw_state = raw_state.clone();
    if version == 1 {
        if let Some(fields) = raw_state.as_object_mut() {
            for key in [
                "revision",
                "intro_complete",
                "music_muted",
                "restart_pending",
            ] {
                fields.remove(key);
            }
        }
    }
    let Ok(state) = serde_json::from_value::<SavedState>(raw_state) else {
        return unknown("corrupt");
    };
    let steps = if version == 1 {
        V1_STEPS.as_slice()
    } else {
        V2_STEPS.as_slice()
    };
    if !steps.contains(&state.step.as_str()) {
        return unknown("corrupt");
    }
    if state
        .plan
        .as_deref()
        .is_some_and(|plan| plan != "standard" && plan != "pro")
    {
        return unknown("corrupt");
    }
    if state.restart_pending.as_ref().is_some_and(|pending| {
        pending.reason != "screen_recording" || !V2_STEPS.contains(&pending.step.as_str())
    }) {
        return unknown("corrupt");
    }
    json!({"status": "saved", "version": version, "state": state})
}

/// Re-read each call so a running daemon sees progress immediately. No paths or I/O details
/// leave this response. The desktop remains the sole writer and migration owner.
pub fn read(path: Option<&Path>) -> Value {
    let Some(path) = path else {
        return unknown("unavailable");
    };
    match std::fs::read_to_string(path) {
        Ok(text) => project(&text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => unknown("missing"),
        Err(_) => unknown("unavailable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_reader_reports_unknown_and_never_writes_or_caches_progress() {
        let root =
            std::env::temp_dir().join(format!("shogun-onboarding-reader-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("onboarding.json");
        assert_eq!(read(None)["status"], "unavailable");
        assert_eq!(read(Some(&path))["status"], "missing");
        let first =
            r#"{"version":2,"state":{"step":"accessibility","revision":7,"completed":false}}"#;
        std::fs::write(&path, first).unwrap();
        assert_eq!(read(Some(&path))["state"]["revision"], 7);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
        std::fs::write(
            &path,
            r#"{"version":2,"state":{"step":"ready","revision":8,"completed":true}}"#,
        )
        .unwrap();
        assert_eq!(read(Some(&path))["state"]["completed"], true);
        assert_eq!(read(Some(&path))["state"]["revision"], 8);
        std::fs::write(&path, "broken").unwrap();
        assert_eq!(read(Some(&path))["status"], "corrupt");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn supported_versions_project_only_saved_setup_fields() {
        for version in [1, 2] {
            let result = project(
                &json!({"version": version, "state": {
                    "step": "plan", "completed": false, "plan": "pro", "trial_started_at": 42,
                    "token": "private", "permissions_repair": true
                }})
                .to_string(),
            );
            assert_eq!(result["state"]["step"], "plan");
            assert_eq!(result["state"]["trial_started_at"], 42);
            assert!(result["state"].get("token").is_none());
            assert!(result["state"].get("permissions_repair").is_none());
        }
    }

    #[test]
    fn malformed_and_future_records_never_claim_completion() {
        for text in [
            "garbage",
            "[]",
            r#"{"version":2}"#,
            r#"{"version":2,"state":{"step":"future"}}"#,
            r#"{"version":2,"state":{"completed":"true"}}"#,
            r#"{"version":"2","state":{}}"#,
        ] {
            assert_eq!(project(text)["status"], "corrupt");
            assert!(project(text)["state"].is_null());
        }
        assert_eq!(
            project(r#"{"version":3,"state":{"completed":true}}"#)["status"],
            "unsupported_schema"
        );
    }

    #[test]
    fn legacy_disposition_uses_desktop_migration_without_starting_trial() {
        let result = project(r#"{"skipped":true}"#);
        assert_eq!(result["state"]["completed"], true);
        assert_eq!(result["state"]["step"], "ready");
        assert!(result["state"]["trial_started_at"].is_null());
    }
}
