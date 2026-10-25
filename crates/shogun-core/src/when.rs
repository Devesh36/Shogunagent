//! The shared `when` value on Memory API rows: unix ms plus the Mac's local wall-clock time.

use chrono::{Local, SecondsFormat, TimeZone};
use serde_json::{json, Value};

/// `{"ms": <unix ms>, "local": "2026-10-14T09:12:05+05:30"}`: RFC 3339, seconds precision, with
/// the local offset. `local` is `null` for a timestamp the local zone cannot place.
pub fn when_json(ms: i64) -> Value {
    let local = Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, false));
    json!({ "ms": ms, "local": local })
}
