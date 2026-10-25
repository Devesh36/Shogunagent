//! The allowlist for funnel events a webview may ask for (issues #6 / #82), and the pure clock
//! behind "time from first launch to the first real answer" (issue #6).
//!
//! A webview never names a PostHog event. It sends a short, fixed-grammar name to the desktop
//! shell (`onboarding_event`), and [`webview_event`] is the only thing that turns such a name into
//! an event: every event name is a `&'static str` literal below, and every property value is
//! either one of the fixed vocabularies below or a small bounded integer. An unknown name, an
//! unknown connector, or an out-of-range count maps to `None` and nothing is sent.
//!
//! What may ride along (CLAUDE.md: analytics carry feature counts only): a step id, a connector
//! KIND (`gmail`, `gcal`, …), an outcome, a connection count, a sample-question id, or a duration
//! bucket. Never an account, address, query, title, or any text the user wrote or read.
//!
//! Pure and cross-platform so the allowlist is tested on Linux CI; the macOS shell only forwards
//! the result to the #91 adapter, whose `opt_out` gate still decides whether anything is sent.

use serde_json::Value;

use super::Props;

/// The connector kinds a connection event may name — `Service::source_str` for every service
/// with a row on the connections screen. A kind, never an account.
pub const CONNECTORS: &[&str] = &["gmail", "gcal", "gdrive", "slack", "notion", "github", "linear", "granola"];

/// The sample questions offered right after a successful connection (#82, product design §3).
pub const SAMPLE_QUESTIONS: &[&str] = &["calendar", "calendar_mail", "mail"];

/// The largest connection count `connection_depth` reports — one per connector kind.
pub const MAX_CONNECTION_DEPTH: u64 = 8;

/// One allowlisted event: its PostHog name and its (content-free) properties.
#[derive(Debug, Clone, PartialEq)]
pub struct FunnelEvent {
    pub event: &'static str,
    pub props: Props,
}

impl FunnelEvent {
    fn bare(event: &'static str) -> Self {
        Self { event, props: Props::new() }
    }

    fn with(event: &'static str, key: &str, value: Value) -> Self {
        let mut props = Props::new();
        props.insert(key.into(), value);
        Self { event, props }
    }
}

/// The static spelling of `value` from `list`, so a property value is always one of ours and
/// never the caller's string.
fn member(list: &'static [&'static str], value: &str) -> Option<&'static str> {
    list.iter().copied().find(|v| *v == value)
}

/// Map a webview funnel name onto an allowlisted event. `steps` is the onboarding flow's own step
/// list (owned by the desktop shell), so the step ids stay defined in one place.
///
/// Grammar:
/// - `shown`, a step id, and the permission-lane names — the onboarding funnel.
/// - `connect_skipped` — the connect step was skipped.
/// - `accessibility_skipped` — setup continued without Accessibility (degraded mode, #778).
/// - `connection.<connected|reconnected|connect_failed|disconnected>.<connector>`.
/// - `connection_depth.<n>` — how many services are connected when onboarding finishes.
/// - `gmail_consent.<shown|accepted|revoked>` — the third-party read consent.
/// - `sample_question.<shown|asked>.<id>`.
pub fn webview_event(name: &str, steps: &[&'static str]) -> Option<FunnelEvent> {
    let fixed = match name {
        "shown" => Some("onboarding_shown"),
        "accessibility_settings_opened" => Some("onboarding_accessibility_settings_opened"),
        "microphone_requested" => Some("onboarding_microphone_requested"),
        "screen_recording_requested" => Some("onboarding_screen_recording_requested"),
        "permission_app_drag_started" => Some("onboarding_permission_app_drag_started"),
        "all_permissions_granted" => Some("onboarding_all_permissions_granted"),
        "connect_skipped" => Some("onboarding_connect_skipped"),
        "accessibility_skipped" => Some("onboarding_accessibility_skipped"),
        "gmail_consent.shown" => Some("gmail_consent_shown"),
        "gmail_consent.accepted" => Some("gmail_consent_accepted"),
        "gmail_consent.revoked" => Some("gmail_consent_revoked"),
        _ => None,
    };
    if let Some(event) = fixed {
        return Some(FunnelEvent::bare(event));
    }
    if let Some(step) = steps.iter().copied().find(|s| *s == name) {
        return Some(FunnelEvent::with("onboarding_step_viewed", "step", Value::from(step)));
    }
    let mut parts = name.split('.');
    let head = parts.next()?;
    let a = parts.next();
    let b = parts.next();
    if parts.next().is_some() {
        return None;
    }
    match (head, a, b) {
        ("connection", Some(outcome), Some(connector)) => {
            let event = match outcome {
                "connected" => "service_connected",
                // Amber → connected again (the spec's `amber_recovered`).
                "reconnected" => "service_reconnected",
                "connect_failed" => "service_connect_failed",
                "disconnected" => "service_disconnected",
                _ => return None,
            };
            let connector = member(CONNECTORS, connector)?;
            Some(FunnelEvent::with(event, "connector", Value::from(connector)))
        }
        ("connection_depth", Some(n), None) => {
            // Digits only: `+3`, `03`, ` 3` and friends are not a count we minted.
            if n.is_empty() || n.len() > 1 || !n.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let n: u64 = n.parse().ok()?;
            (n <= MAX_CONNECTION_DEPTH)
                .then(|| FunnelEvent::with("connection_depth", "connected", Value::from(n)))
        }
        ("sample_question", Some(phase), Some(id)) => {
            let event = match phase {
                "shown" => "sample_question_shown",
                "asked" => "sample_question_asked",
                _ => return None,
            };
            let id = member(SAMPLE_QUESTIONS, id)?;
            Some(FunnelEvent::with(event, "question", Value::from(id)))
        }
        _ => None,
    }
}

// time to first real answer

/// When to stamp `first_launch_at`, the start of the time-to-first-answer clock.
///
/// Only a genuine first run starts it: no trial stamp yet AND setup not completed. A device that
/// already had a trial stamp (it launched before this clock existed) or that migrated from the
/// legacy guide as completed has no honest start time, so it is never measured — a made-up start
/// would put every upgraded user into the "answered within a minute" bucket.
pub fn first_launch_stamp(
    prev_first_launch: Option<i64>,
    prev_trial_started: Option<i64>,
    prev_completed: bool,
    now_unix: i64,
) -> Option<i64> {
    prev_first_launch.or_else(|| {
        (prev_trial_started.is_none() && !prev_completed).then_some(now_unix)
    })
}

/// Seconds from first launch to now, when THIS answer is the first real one to be measured:
/// the clock has a start and no first answer has been stamped yet. A clock that went backwards
/// clamps to zero rather than reporting a negative duration.
pub fn first_answer_elapsed(
    first_launch_at: Option<i64>,
    first_answer_at: Option<i64>,
    now_unix: i64,
) -> Option<u64> {
    if first_answer_at.is_some() {
        return None;
    }
    let start = first_launch_at?;
    Some(u64::try_from(now_unix.saturating_sub(start)).unwrap_or(0))
}

/// Coarse bucket for a duration in seconds. Buckets, not seconds, travel: an exact duration is
/// more precise than the question ("how long until SHOGUN is useful?") needs.
pub fn duration_bucket(secs: u64) -> &'static str {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    match secs {
        s if s < 2 * MIN => "under_2m",
        s if s < 10 * MIN => "2m_10m",
        s if s < 30 * MIN => "10m_30m",
        s if s < 2 * HOUR => "30m_2h",
        s if s < DAY => "2h_24h",
        s if s < 7 * DAY => "1d_7d",
        _ => "over_7d",
    }
}

/// Properties of `onboarding_first_answer`: the duration bucket and nothing else.
pub fn first_answer_props(secs: u64) -> Props {
    let mut props = Props::new();
    props.insert("duration_bucket".into(), Value::from(duration_bucket(secs)));
    props
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const STEPS: [&str; 6] = ["welcome", "reads", "permission", "plan", "connect", "ready"];

    fn ev(name: &str) -> Option<FunnelEvent> {
        webview_event(name, &STEPS)
    }

    #[test]
    fn the_existing_onboarding_names_keep_their_events() {
        assert_eq!(ev("shown").unwrap().event, "onboarding_shown");
        for (name, event) in [
            ("accessibility_settings_opened", "onboarding_accessibility_settings_opened"),
            ("microphone_requested", "onboarding_microphone_requested"),
            ("screen_recording_requested", "onboarding_screen_recording_requested"),
            ("permission_app_drag_started", "onboarding_permission_app_drag_started"),
            ("all_permissions_granted", "onboarding_all_permissions_granted"),
        ] {
            let e = ev(name).unwrap();
            assert_eq!(e.event, event);
            assert!(e.props.is_empty(), "{name} carries nothing");
        }
        for step in STEPS {
            let e = ev(step).unwrap();
            assert_eq!(e.event, "onboarding_step_viewed");
            assert_eq!(e.props["step"], step);
            assert_eq!(e.props.len(), 1);
        }
    }

    #[test]
    fn connection_events_carry_the_connector_kind_only() {
        for (outcome, event) in [
            ("connected", "service_connected"),
            ("reconnected", "service_reconnected"),
            ("connect_failed", "service_connect_failed"),
            ("disconnected", "service_disconnected"),
        ] {
            for connector in CONNECTORS {
                let e = ev(&format!("connection.{outcome}.{connector}")).unwrap();
                assert_eq!(e.event, event);
                assert_eq!(e.props.len(), 1, "least data wins");
                assert_eq!(e.props["connector"], *connector);
            }
        }
    }

    #[test]
    fn connection_events_refuse_anything_outside_the_vocabulary() {
        for name in [
            "connection.connected.alice@example.com",
            "connection.connected.web",
            "connection.connected",
            "connection.connected.gmail.extra",
            "connection.sent.gmail",
            "connection..gmail",
            "connection.connected.GMAIL",
            "service_connected",
            "",
            "ready.extra",
        ] {
            assert_eq!(ev(name), None, "{name:?} must not become an event");
        }
    }

    #[test]
    fn connection_depth_is_a_bounded_count() {
        for n in 0..=MAX_CONNECTION_DEPTH {
            let e = ev(&format!("connection_depth.{n}")).unwrap();
            assert_eq!(e.event, "connection_depth");
            assert_eq!(e.props["connected"], n);
            assert_eq!(e.props.len(), 1);
        }
        for bad in ["connection_depth.9", "connection_depth.-1", "connection_depth.03",
            "connection_depth.+3", "connection_depth.", "connection_depth.three",
            "connection_depth.99999999999999999999"]
        {
            assert_eq!(ev(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn gmail_consent_and_skip_events_are_bare() {
        for (name, event) in [
            ("gmail_consent.shown", "gmail_consent_shown"),
            ("gmail_consent.accepted", "gmail_consent_accepted"),
            ("gmail_consent.revoked", "gmail_consent_revoked"),
            ("connect_skipped", "onboarding_connect_skipped"),
            ("accessibility_skipped", "onboarding_accessibility_skipped"),
        ] {
            let e = ev(name).unwrap();
            assert_eq!(e.event, event);
            assert!(e.props.is_empty());
        }
        assert_eq!(ev("gmail_consent.declined_by_bob"), None);
    }

    #[test]
    fn sample_question_events_carry_a_question_id_not_text() {
        for id in SAMPLE_QUESTIONS {
            for (phase, event) in [("shown", "sample_question_shown"), ("asked", "sample_question_asked")] {
                let e = ev(&format!("sample_question.{phase}.{id}")).unwrap();
                assert_eq!(e.event, event);
                assert_eq!(e.props["question"], *id);
                assert_eq!(e.props.len(), 1);
            }
        }
        assert_eq!(ev("sample_question.asked.What's on my calendar tomorrow?"), None);
        assert_eq!(ev("sample_question.clicked.mail"), None);
    }

    #[test]
    fn every_property_value_comes_from_our_own_vocabulary() {
        // Whatever the caller sends, a string property is one of our literals — so no caller
        // string can ride along, even one that happens to match a vocabulary word.
        let vocab: Vec<&str> = CONNECTORS
            .iter()
            .chain(SAMPLE_QUESTIONS)
            .chain(STEPS.iter())
            .copied()
            .collect();
        let names = ["connection.connected.gmail", "sample_question.asked.mail", "ready"];
        for name in names {
            for value in ev(name).unwrap().props.values() {
                if let Some(s) = value.as_str() {
                    assert!(vocab.contains(&s), "{s}");
                }
            }
        }
    }

    #[test]
    fn only_a_genuine_first_run_starts_the_clock() {
        assert_eq!(first_launch_stamp(None, None, false, 100), Some(100));
        // Already stamped: never moves.
        assert_eq!(first_launch_stamp(Some(50), None, false, 100), Some(50));
        assert_eq!(first_launch_stamp(Some(50), Some(10), true, 100), Some(50));
        // Launched before this clock existed (trial already running): no honest start.
        assert_eq!(first_launch_stamp(None, Some(10), false, 100), None);
        // Migrated from the legacy guide as completed: no honest start either.
        assert_eq!(first_launch_stamp(None, None, true, 100), None);
    }

    #[test]
    fn the_first_answer_is_measured_once() {
        assert_eq!(first_answer_elapsed(Some(100), None, 400), Some(300));
        assert_eq!(first_answer_elapsed(Some(100), Some(400), 900), None, "already stamped");
        assert_eq!(first_answer_elapsed(None, None, 900), None, "no start, no measurement");
        assert_eq!(first_answer_elapsed(Some(500), None, 400), Some(0), "clock skew clamps");
    }

    #[test]
    fn durations_fall_into_coarse_buckets() {
        for (secs, bucket) in [
            (0, "under_2m"),
            (119, "under_2m"),
            (120, "2m_10m"),
            (599, "2m_10m"),
            (600, "10m_30m"),
            (1_800, "30m_2h"),
            (7_200, "2h_24h"),
            (86_399, "2h_24h"),
            (86_400, "1d_7d"),
            (604_799, "1d_7d"),
            (604_800, "over_7d"),
            (u64::MAX, "over_7d"),
        ] {
            assert_eq!(duration_bucket(secs), bucket, "{secs}");
        }
    }

    #[test]
    fn first_answer_props_carry_the_bucket_only() {
        let p = first_answer_props(90);
        assert_eq!(p.len(), 1, "no exact duration, no content");
        assert_eq!(p["duration_bucket"], "under_2m");
    }
}
