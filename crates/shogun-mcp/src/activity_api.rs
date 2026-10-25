//! `memory.get_events` and `memory.read_session`: open the captured rows behind a hit, or read
//! what happened in one app / window over a span. MCP passes its `arguments`, REST its POST body
//! (the same JSON), and the CLI posts that body, so all three faces share one parser here and
//! one backend call ([`crate::backend::MemoryBackend::activity`]). Both are reads.

use serde_json::Value;

use crate::memory_api::Tool;

pub const MAX_EVENT_IDS: usize = 20;
pub const MIN_EVENT_CHARS: usize = 200;
pub const MAX_EVENT_CHARS: usize = 8_000;
pub const DEFAULT_EVENT_CHARS: usize = 8_000;
/// The longest span one `memory.read_session` call reads (12 hours: a whole working day).
pub const MAX_SESSION_SPAN_MS: i64 = 12 * 3_600_000;
pub const MAX_SESSION_LIMIT: usize = 50;
pub const DEFAULT_SESSION_LIMIT: usize = 20;

/// One request, already lifted off its wire and range-checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityRequest {
    /// Event ids, deduplicated in request order, and the per-event text cap in chars.
    Events { ids: Vec<i64>, max_chars: usize },
    /// `[from_ms, to_ms)`, an optional case-insensitive app filter, and a session cap.
    Session { from_ms: i64, to_ms: i64, app: Option<String>, limit: usize },
}

pub fn is_activity_tool(tool: Tool) -> bool {
    matches!(tool, Tool::MemoryGetEvents | Tool::MemoryReadSession)
}

/// Parse `args` for `tool`. `Err` is `<code>: <sentence>`, the caller's to fix (MCP -32602,
/// REST 400).
pub fn request(tool: Tool, args: &Value) -> Result<ActivityRequest, String> {
    let int = |key: &str| -> Result<Option<i64>, String> {
        match args.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_i64()
                .map(Some)
                .ok_or_else(|| format!("invalid_{key}: {key} must be an integer.")),
        }
    };
    match tool {
        Tool::MemoryGetEvents => {
            let Some(raw) = args.get("ids").and_then(Value::as_array) else {
                return Err("missing_ids: pass ids, an array of event ids.".into());
            };
            if raw.is_empty() || raw.len() > MAX_EVENT_IDS {
                return Err(format!("invalid_ids: pass 1 to {MAX_EVENT_IDS} event ids."));
            }
            let mut ids = Vec::with_capacity(raw.len());
            for v in raw {
                let id = v
                    .as_i64()
                    .filter(|id| *id >= 1)
                    .ok_or("invalid_ids: each id must be a positive integer.")?;
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            let max_chars = match int("max_chars")? {
                None => DEFAULT_EVENT_CHARS,
                Some(n) if (MIN_EVENT_CHARS as i64..=MAX_EVENT_CHARS as i64).contains(&n) => n as usize,
                Some(_) => {
                    return Err(format!(
                        "invalid_max_chars: max_chars must be {MIN_EVENT_CHARS} to {MAX_EVENT_CHARS}."
                    ))
                }
            };
            Ok(ActivityRequest::Events { ids, max_chars })
        }
        Tool::MemoryReadSession => {
            let (Some(from_ms), Some(to_ms)) = (int("from_ms")?, int("to_ms")?) else {
                return Err("missing_range: pass from_ms and to_ms (unix ms).".into());
            };
            if to_ms <= from_ms || to_ms.saturating_sub(from_ms) > MAX_SESSION_SPAN_MS {
                return Err("invalid_range: to_ms must be after from_ms, at most 12 hours later.".into());
            }
            let app = match args.get("app") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) if !s.trim().is_empty() && s.chars().count() <= 200 => {
                    Some(s.trim().to_string())
                }
                Some(_) => return Err("invalid_app: app must be a non-empty string (name or bundle id).".into()),
            };
            let limit = match int("limit")? {
                None => DEFAULT_SESSION_LIMIT,
                Some(n) if (1..=MAX_SESSION_LIMIT as i64).contains(&n) => n as usize,
                Some(_) => return Err(format!("invalid_limit: limit must be 1 to {MAX_SESSION_LIMIT}.")),
            };
            Ok(ActivityRequest::Session { from_ms, to_ms, app, limit })
        }
        _ => Err("unknown_tool: not an activity tool.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn get_events_bounds_and_dedupes_ids() {
        let ok = request(Tool::MemoryGetEvents, &json!({"ids": [3, 1, 3]})).unwrap();
        assert_eq!(ok, ActivityRequest::Events { ids: vec![3, 1], max_chars: DEFAULT_EVENT_CHARS });
        for bad in [
            json!({}),
            json!({"ids": []}),
            json!({"ids": [0]}),
            json!({"ids": ["7"]}),
            json!({"ids": (1..=21).collect::<Vec<_>>()}),
            json!({"ids": [1], "max_chars": 199}),
            json!({"ids": [1], "max_chars": 8001}),
            json!({"ids": [1], "max_chars": "big"}),
        ] {
            let err = request(Tool::MemoryGetEvents, &bad).unwrap_err();
            assert!(err.contains(": "), "{err}");
        }
    }

    #[test]
    fn read_session_needs_a_bounded_forward_span() {
        let ok = request(Tool::MemoryReadSession, &json!({"from_ms": 0, "to_ms": 1000, "app": " Chrome "})).unwrap();
        assert_eq!(ok, ActivityRequest::Session { from_ms: 0, to_ms: 1000, app: Some("Chrome".into()), limit: 20 });
        let day = 12 * 3_600_000;
        assert!(request(Tool::MemoryReadSession, &json!({"from_ms": 0, "to_ms": day})).is_ok(), "a whole working day");
        for bad in [
            json!({"from_ms": 0}),
            json!({"from_ms": 10, "to_ms": 10}),
            json!({"from_ms": 0, "to_ms": MAX_SESSION_SPAN_MS + 1}),
            json!({"from_ms": 0, "to_ms": 1, "limit": 0}),
            json!({"from_ms": 0, "to_ms": 1, "limit": 51}),
            json!({"from_ms": 0, "to_ms": 1, "app": ""}),
            json!({"from_ms": "0", "to_ms": 1}),
        ] {
            assert!(request(Tool::MemoryReadSession, &bad).is_err(), "{bad}");
        }
    }
}
