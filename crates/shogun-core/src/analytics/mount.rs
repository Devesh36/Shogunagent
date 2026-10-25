//! `memory_api_mounted`: one counts-only anonymous event each time another app starts SHOGUN's
//! stdio memory server and it accepts the app's token (issue #400; owner decision 2026-09-26).
//!
//! It answers "is the Memory API used from other apps, and read-only or read-and-change", and
//! nothing else. What rides along, and nothing else (CLAUDE.md: analytics carry feature counts
//! only):
//! - `scope` — the scope the session runs under, one of [`SCOPES`] (`read` / `full`).
//!
//! Not the app that started it (the owner ruled out client identity: the server cannot know it
//! reliably and a guess is a fingerprint), not a token id or name, not a path, not a count of
//! tools. The value sent is our literal from [`SCOPES`], never the caller's string.
//!
//! The same opt-out as every other event ([`consented_distinct_id`]): the desktop's
//! `analytics.json`. A device that opted out, or whose file is missing or unreadable, sends
//! nothing; the stdio server never creates that file or an id of its own. The event goes out
//! through this crate's analytics transport (FR-TR-03), once, off the serving thread.

use serde_json::Value;

use super::{Event, Props};

/// The PostHog event name.
pub const EVENT: &str = "memory_api_mounted";

/// The scopes a session can run under (`TokenScope::as_str`). Anything else sends nothing.
pub const SCOPES: &[&str] = &["read", "full"];

/// The event for a session running under `scope`, or `None` when `scope` is not one of
/// [`SCOPES`].
pub fn event(scope: &str) -> Option<Event> {
    let scope = SCOPES.iter().copied().find(|s| *s == scope)?;
    let mut props = Props::new();
    props.insert("scope".into(), Value::from(scope));
    Some(Event { name: EVENT.to_string(), props })
}

/// The anonymous id to send under, read from the desktop's `analytics.json` text, or `None` —
/// send nothing — when the user opted out or the file is missing its id or does not parse. The
/// same file and field the Settings switch writes (`opt_out`), so one switch covers this too.
pub fn consented_distinct_id(analytics_json: Option<&str>) -> Option<String> {
    let v: Value = serde_json::from_str(analytics_json?).ok()?;
    if v.get("opt_out").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    // `opt_out` present but not a bool: not a state we wrote, so not consent either.
    if v.get("opt_out").is_some_and(|o| !o.is_boolean()) {
        return None;
    }
    let id = v.get("distinct_id")?.as_str()?.trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// Send the event for `scope` once, on a background thread, if a key is set, the user has not
/// opted out and `scope` is known. Never blocks the caller; a failure is dropped silently.
#[cfg(feature = "net")]
pub fn send_in_background(
    scope: &str,
    analytics_json: Option<String>,
    api_key: Option<String>,
    host: &str,
) {
    let Some(event) = event(scope) else { return };
    let Some(distinct_id) = consented_distinct_id(analytics_json.as_deref()) else { return };
    let Some(api_key) = api_key else { return };
    let host = host.to_string();
    let _ = std::thread::Builder::new().name("shogun-analytics-mount".into()).spawn(move || {
        use super::Transport;
        let config = super::AnalyticsConfig { api_key, distinct_id };
        if let Some(transport) = super::reqwest_transport::ReqwestTransport::new(&host) {
            let _ = transport.send_batch(super::build_batch_payload(&config, &[event]));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_scope_rides_along() {
        for scope in SCOPES {
            let e = event(scope).unwrap();
            assert_eq!(e.name, EVENT);
            let keys: Vec<&str> = e.props.keys().map(String::as_str).collect();
            assert_eq!(keys, ["scope"]);
            assert_eq!(e.props["scope"], *scope);
        }
    }

    #[test]
    fn an_unknown_scope_sends_nothing() {
        for scope in ["", "Read", "read ", "admin", "claude-code", "shogun_0123456789abcdef0123"] {
            assert!(event(scope).is_none(), "{scope:?}");
        }
    }

    #[test]
    fn the_sent_scope_is_our_literal_not_the_callers_string() {
        let caller = String::from("read");
        let e = event(&caller).unwrap();
        let body = super::super::build_batch_payload(
            &super::super::AnalyticsConfig { api_key: "phc_x".into(), distinct_id: "anon".into() },
            &[e],
        );
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["batch"][0]["event"], EVENT);
        assert_eq!(v["batch"][0]["distinct_id"], "anon");
        assert_eq!(v["batch"][0]["properties"], serde_json::json!({ "scope": "read" }));
    }

    #[test]
    fn opt_out_and_missing_state_send_nothing() {
        let id = |json: Option<&str>| consented_distinct_id(json);
        assert_eq!(id(Some(r#"{"distinct_id":"abc","opt_out":false}"#)), Some("abc".into()));
        // Written before opt_out existed: the default is on, like the desktop reads it.
        assert_eq!(id(Some(r#"{"distinct_id":"abc"}"#)), Some("abc".into()));
        assert_eq!(id(Some(r#"{"distinct_id":"abc","opt_out":true}"#)), None);
        assert_eq!(id(Some(r#"{"distinct_id":"abc","opt_out":"no"}"#)), None);
        assert_eq!(id(Some(r#"{"distinct_id":"  ","opt_out":false}"#)), None);
        assert_eq!(id(Some(r#"{"opt_out":false}"#)), None);
        assert_eq!(id(Some("not json")), None);
        assert_eq!(id(None), None);
    }
}
