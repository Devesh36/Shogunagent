//! Normalize an MCP `tools/call` result into [`FetchedItem`]s for the ingest pipeline.
//!
//! Two layers, both pure:
//! 1. **Envelope** (MCP spec, stable): a `CallToolResult` is `{ content: [...], isError?, ...}`.
//!    An `isError: true` result becomes an `Err`. Records are taken from `structuredContent` when
//!    present, else parsed out of the text content blocks (Google's servers return JSON there).
//! 2. **Field mapping** (tolerant): each record is reduced to (external_id, title, body, ts_ms) by
//!    trying a list of candidate keys. The candidate lists come from the Google server docs and
//!    are **PROVISIONAL for Calendar / Drive — not yet confirmed against live responses** (issue
//!    #80 does that confirmation; Gmail's fields ARE live-verified, on the Composio path in
//!    shogun-core `composio_read.rs`). The tolerant match means a schema tweak (e.g. `snippet` vs
//!    `preview`) does not break ingest — a record only drops if it yields neither a body nor a
//!    title to stand in for one ([`shogun_mcp::sync`] then drops anything still empty-bodied).
//!
//! No item content is ever logged from here; errors carry only a short reason.

use serde_json::Value;
use shogun_mcp::sync::FetchedItem;

const ID_KEYS: &[&str] = &["external_id", "id", "threadId", "messageId", "eventId", "fileId", "name"];
const TITLE_KEYS: &[&str] = &["title", "subject", "summary", "name", "filename"];
const BODY_KEYS: &[&str] = &["body", "snippet", "description", "text", "content", "preview"];
const TS_KEYS: &[&str] =
    &["ts_ms", "internalDate", "updatedMs", "start_ms", "createdTimeMs", "modifiedTimeMs"];
/// RFC3339 string timestamps, tried when no numeric one is present. Calendar events carry
/// `updated` / `created` this way; reading them as 0 would put every event at 1970.
const TS_RFC3339_KEYS: &[&str] = &["updated", "modifiedTime", "created", "createdTime", "updated_at", "created_at", "updatedAt", "createdAt"];

/// Parse a `tools/call` result value into normalized items. `result` is the JSON-RPC `result`
/// object (already unwrapped from the JSON-RPC envelope by the transport).
pub fn parse_items(result: &Value) -> Result<Vec<FetchedItem>, String> {
    if let Some(reason) = tool_error_reason(result) {
        return Err(reason.to_string());
    }
    let records = collect_records(result)?;
    Ok(records.iter().filter_map(record_to_item).collect())
}

/// GitHub search snapshots include status and provider update time as evidence. The generic
/// body-only mapping loses a close/reopen when the issue prose stays identical: ingest would
/// merely touch the old event. These stable provider fields distinguish revisions while an
/// exact replay still deduplicates. An absent search result is not a deletion instruction.
pub fn parse_github_issues(result: &Value) -> Result<Vec<FetchedItem>, String> {
    if let Some(reason) = tool_error_reason(result) {
        return Err(reason.to_string());
    }
    Ok(collect_records(result)?
        .iter()
        .filter_map(|record| {
            let mut item = record_to_item(record)?;
            let kind = if record.get("pull_request").is_some_and(Value::is_object) {
                "GitHub pull request"
            } else {
                "GitHub issue"
            };
            let mut lines = vec![match record.get("number").and_then(Value::as_u64) {
                Some(number) => format!("{kind} #{number}"),
                None => kind.to_string(),
            }];
            if let Some(state) = record.get("state").and_then(Value::as_str) {
                let state = state.trim().to_ascii_lowercase();
                if matches!(state.as_str(), "open" | "closed") {
                    lines.push(format!("State: {state}"));
                }
            }
            if let Some(reason) = record.get("state_reason").and_then(Value::as_str) {
                let reason = reason.trim().to_ascii_lowercase();
                if matches!(reason.as_str(), "completed" | "not_planned" | "reopened") {
                    lines.push(format!("State reason: {reason}"));
                }
            }
            if let Some(updated) = record
                .get("updated_at")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| text.len() <= 64 && !text.chars().any(char::is_control))
            {
                if let Some(ts) = rfc3339_to_ms(updated) {
                    item.ts_ms = ts;
                    lines.push(format!("Updated: {updated}"));
                }
            }
            if let Some(url) = ["html_url", "url"]
                .iter()
                .filter_map(|key| record.get(*key).and_then(Value::as_str).map(str::trim))
                .find(|raw| {
                    raw.len() <= 2048
                        && !raw.chars().any(char::is_whitespace)
                        && url::Url::parse(raw).is_ok_and(|url| {
                            url.scheme() == "https"
                                && url.host_str().is_some()
                                && url.username().is_empty()
                                && url.password().is_none()
                        })
                })
            {
                lines.push(format!("URL: {url}"));
            }
            item.body = format!("{}\n\n{}", lines.join("\n"), item.body);
            Some(item)
        })
        .collect())
}

/// Preserve a requested Granola note without guessing undocumented response field names.
/// `get_meetings` is a notes-only tool; transcripts and audio have separate provider tools.
pub fn granola_note(result: &Value, meeting_id: &str) -> Result<FetchedItem, String> {
    if let Some(reason) = tool_error_reason(result) {
        return Err(reason.into());
    }
    let body = if let Some(value) = result.get("structuredContent") {
        serde_json::to_string(value).map_err(|_| "Granola note was not serializable")?
    } else {
        result.get("content")
            .and_then(Value::as_array)
            .ok_or("Granola note has no content")?
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")
    };
    if body.trim().is_empty() || body == "{}" || body == "null" {
        return Err("Granola note is empty".into());
    }
    // Only the requested meeting's own record names it: a nested list (attendees) is not it.
    let metadata = parse_items(result)?
        .into_iter()
        .find(|item| item.external_id == meeting_id);
    Ok(FetchedItem {
        external_id: meeting_id.into(),
        title: metadata
            .as_ref()
            .map(|item| item.title.clone())
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| "Granola meeting".into()),
        body,
        // ponytail: the meeting date is unknown without a record, so the read time stands in
        // (0 would file the note in 1970, outside every recent window).
        ts_ms: metadata
            .map(|item| item.ts_ms)
            .filter(|ts| *ts > 0)
            .unwrap_or_else(crate::token::system_now_ms),
        mail: None,
        calendar: None,
    })
}

/// Pull the record array out of a result: prefer `structuredContent`, else the JSON embedded in the
/// text content blocks.
fn collect_records(result: &Value) -> Result<Vec<Value>, String> {
    if let Some(sc) = result.get("structuredContent") {
        if let Some(items) = as_record_array(sc) {
            return Ok(items);
        }
    }
    let content = result.get("content").and_then(Value::as_array);
    let Some(blocks) = content else {
        // No structured content and no content array — nothing to ingest (not an error).
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(text) = block.get("text").and_then(Value::as_str) else {
            continue;
        };
        // Each text block is expected to hold JSON (an array, or an object wrapping an array).
        if let Ok(parsed) = serde_json::from_str::<Value>(text) {
            if let Some(items) = as_record_array(&parsed) {
                out.extend(items);
            }
        }
    }
    Ok(out)
}

/// Wrapper keys the Google/Slack list responses put their record array under, tried in order
/// before the single-array heuristic.
const WRAPPER_KEYS: &[&str] =
    &["items", "threads", "messages", "events", "files", "results", "records", "issues"];

/// Structured result or JSON text from a provider. No content is included in failure messages.
pub fn payload(result: &Value) -> Result<Value, String> {
    if let Some(reason) = tool_error_reason(result) {
        return Err(reason.into());
    }
    if let Some(value) = result.get("structuredContent") {
        return Ok(value.clone());
    }
    result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|blocks| {
            blocks.iter().find_map(|block| {
                (block.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| {
                        block
                            .get("text")
                            .and_then(Value::as_str)
                            .and_then(|text| serde_json::from_str(text).ok())
                    })
                    .flatten()
            })
        })
        .ok_or_else(|| "provider returned no JSON payload".into())
}

/// Interpret a value as an array of record objects: either a bare array, an object whose record
/// array sits under a known wrapper key, or an object with a single array field
/// (e.g. `{ "threads": [...] }`).
fn as_record_array(v: &Value) -> Option<Vec<Value>> {
    if let Some(arr) = v.as_array() {
        return Some(arr.clone());
    }
    if let Some(obj) = v.as_object() {
        // Known wrapper keys first: Calendar's `{ "items": [...], "defaultReminders": [...] }`
        // carries TWO arrays, so the "exactly one array field" heuristic alone reads it as no
        // records at all — the service then shows "Synced" while ingesting nothing forever.
        for key in WRAPPER_KEYS {
            if let Some(arr) = obj.get(*key).and_then(Value::as_array) {
                return Some(arr.clone());
            }
        }
        // Fallback: a wrapper object with one array of results.
        let mut arrays = obj.values().filter(|val| val.is_array());
        if let (Some(first), None) = (arrays.next(), arrays.next()) {
            if let Some(arr) = first.as_array() {
                return Some(arr.clone());
            }
        }
    }
    None
}

fn record_to_item(rec: &Value) -> Option<FetchedItem> {
    let calendar = calendar_meta(rec);
    // Prefer a real body; fall back to the title so body-less records — Drive file metadata,
    // Calendar events with no description — still ingest as searchable memory instead of being
    // silently dropped while the service reports "Synced". A record with neither is dropped.
    let body = first_str(rec, BODY_KEYS)
        .filter(|body| !body.trim().is_empty())
        .or_else(|| first_str(rec, TITLE_KEYS))
        .or_else(|| {
            calendar
                .as_ref()
                .filter(|event| event.cancelled)
                .map(|_| "Cancelled calendar event".to_string())
        })?
        .trim()
        .to_string();
    if body.is_empty() {
        return None;
    }
    // Mail metadata (issue #319): the Composio Gmail transport attaches a `mail` object per
    // thread record; other services carry none.
    let mail = rec
        .get("mail")
        .and_then(|m| serde_json::from_value::<shogun_mcp::sync::MailMeta>(m.clone()).ok());
    let ts_ms = match first_ts(rec, TS_KEYS) {
        0 => TS_RFC3339_KEYS
            .iter()
            .find_map(|k| rec.get(*k).and_then(Value::as_str).and_then(rfc3339_to_ms))
            .unwrap_or(0),
        ts => ts,
    };
    Some(FetchedItem {
        external_id: ID_KEYS.iter().find_map(|key| match rec.get(*key) {
            Some(Value::String(value)) if !value.is_empty() => Some(value.clone()),
            Some(Value::Number(value)) => value.as_u64().map(|id| id.to_string()),
            _ => None,
        }).unwrap_or_default(),
        title: first_str(rec, TITLE_KEYS).unwrap_or_default(),
        body,
        ts_ms,
        mail,
        calendar,
    })
}

/// The structured facts of a Google Calendar event (FR-MT-06), when `rec` is one: it has a
/// `start` object carrying `dateTime` (a timed event) or `date` (an all-day one). Anything else
/// yields `None` and the record ingests unchanged.
fn calendar_meta(rec: &Value) -> Option<shogun_mcp::sync::CalendarMeta> {
    let cancelled = rec.get("status").and_then(Value::as_str) == Some("cancelled");
    // Google deletion tombstones omit title and start. Keep their id/status so stale rows clear.
    let tombstone = cancelled
        && (rec.get("kind").and_then(Value::as_str) == Some("calendar#event")
            || rec.get("originalStartTime").is_some()
            || rec.get("recurringEventId").is_some());
    let (start_ms, all_day) = rec
        .get("start")
        .and_then(event_time)
        .or_else(|| tombstone.then_some((0, false)))?;
    // A missing or unreadable end is a zero-length event, not a dropped one.
    let end_ms = rec.get("end").and_then(event_time).map_or(start_ms, |(ms, _)| ms).max(start_ms);
    // Prefer the join link when an invitation also names a physical room.
    let location = rec
        .get("hangoutLink")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            rec.pointer("/conferenceData/entryPoints")
                .and_then(Value::as_array)
                .and_then(|entries| {
                    entries.iter().find_map(|entry| {
                        (entry.get("entryPointType").and_then(Value::as_str) == Some("video"))
                            .then(|| {
                                entry
                                    .get("uri")
                                    .and_then(Value::as_str)
                                    .map(str::trim)
                                    .filter(|value| !value.is_empty())
                            })
                            .flatten()
                    })
                })
        })
        .or_else(|| {
            rec.get("location")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .map(str::to_string);
    let attendees = rec
        .get("attendees")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .map(|a| shogun_mcp::sync::CalendarAttendee {
                    email: a.get("email").and_then(Value::as_str).map(str::to_string),
                    name: a
                        .get("displayName")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|n| !n.is_empty())
                        .map(str::to_string),
                    is_self: a.get("self").and_then(Value::as_bool).unwrap_or(false),
                    response: a.get("responseStatus").and_then(Value::as_str).map(str::to_string),
                })
                .collect()
        })
        .unwrap_or_default();
    Some(shogun_mcp::sync::CalendarMeta {
        start_ms,
        end_ms,
        all_day,
        location,
        attendees,
        cancelled,
    })
}

/// A Calendar `start` / `end` object: `{ "dateTime": RFC3339 }` or `{ "date": "YYYY-MM-DD" }`.
/// Returns epoch ms and whether it was a date (all-day).
fn event_time(v: &Value) -> Option<(i64, bool)> {
    if let Some(dt) = v.get("dateTime").and_then(Value::as_str) {
        return rfc3339_to_ms(dt).map(|ms| (ms, false));
    }
    let date = v.get("date").and_then(Value::as_str)?;
    let (y, m, d) = parse_ymd(date)?;
    Some((days_from_civil(y, m, d) * 86_400_000, true))
}

/// `YYYY-MM-DD`.
fn parse_ymd(s: &str) -> Option<(i64, i64, i64)> {
    let mut parts = s.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some((y, m, d))
}

/// Days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse an RFC3339 timestamp (`2026-09-24T10:00:00+09:00`, `…Z`, optional fractional seconds)
/// to epoch ms. Hand-rolled to keep this crate free of a datetime dependency; anything it does not
/// recognise is `None`, never a guessed instant.
pub fn rfc3339_to_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, rest) = s.split_once(['T', 't', ' '])?;
    let (y, m, d) = parse_ymd(date)?;
    // Split the clock from the zone: `Z`, or the last `+` / `-` (the date part is gone already).
    let (clock, offset_secs) = if let Some(clock) = rest.strip_suffix(['Z', 'z']) {
        (clock, 0)
    } else {
        let at = rest.rfind(['+', '-'])?;
        let (clock, zone) = rest.split_at(at);
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let (hh, mm) = zone[1..].split_once(':').unwrap_or((&zone[1..3.min(zone.len())], "00"));
        let hh: i64 = hh.parse().ok()?;
        let mm: i64 = mm.parse().ok()?;
        if hh > 23 || mm > 59 {
            return None;
        }
        (clock, sign * (hh * 3600 + mm * 60))
    };
    let (hms, frac) = clock.split_once('.').unwrap_or((clock, ""));
    let mut t = hms.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let mi: i64 = t.next()?.parse().ok()?;
    let sec: i64 = t.next().unwrap_or("0").parse().ok()?;
    if t.next().is_some() || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let millis: i64 = if frac.is_empty() {
        0
    } else {
        let digits: String = frac.chars().take(3).collect();
        if !digits.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        format!("{digits:0<3}").parse().ok()?
    };
    let secs = days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60 + sec - offset_secs;
    Some(secs * 1000 + millis)
}

/// The first present, string-valued key from `keys`.
fn first_str(rec: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| rec.get(*k).and_then(Value::as_str).map(str::to_string))
}

/// The first present epoch-ms timestamp from `keys`. Accepts a JSON number or a numeric string;
/// non-numeric (e.g. an RFC3339 string) yields 0 — the daemon may refine ordering on ingest. This
/// keeps the normalizer dependency-free (no datetime crate).
fn first_ts(rec: &Value, keys: &[&str]) -> i64 {
    for k in keys {
        match rec.get(*k) {
            Some(Value::Number(n)) => {
                if let Some(i) = n.as_i64() {
                    return i;
                }
            }
            Some(Value::String(s)) => {
                if let Ok(i) = s.parse::<i64>() {
                    return i;
                }
            }
            _ => {}
        }
    }
    0
}

/// Content-free failure classification shared by read normalization and raw write results.
/// Provider text may echo private content; retain only a recognized authentication class.
pub fn tool_error_reason(result: &Value) -> Option<&'static str> {
    if result.get("isError").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    // Case-insensitive and anywhere in the text, like the runtime's `is_auth_failure`: providers
    // say "Unauthorized: token expired", Slack says `invalid_auth` / `token_revoked`.
    let is_auth = crate::runtime::is_auth_failure;
    let structured_auth = result.pointer("/structuredContent/error/code")
        .and_then(Value::as_str).is_some_and(is_auth);
    let text_auth = result.get("content").and_then(Value::as_array).is_some_and(|blocks| {
        blocks.iter().filter_map(|b| b.get("text").and_then(Value::as_str)).any(is_auth)
    });
    Some(if structured_auth || text_auth { "mcp tool unauthorized" } else { "mcp tool returned an error" })
}

#[cfg(test)]
mod tests {
    #[test]
    fn github_search_snapshots_preserve_status_version_and_url_in_both_envelopes() {
        let payload = json!({"items": [{
            "id": 123, "number": 7, "title": "Export timeout", "body": "Unchanged issue prose",
            "state": "CLOSED", "state_reason": "completed", "updated_at": "2026-10-27T10:00:00Z",
            "html_url": "https://github.com/example/harbor/issues/7"
        }], "total_count": 1});
        for result in [
            json!({"structuredContent": payload}),
            json!({"content": [{"type": "text", "text": payload.to_string()}]}),
        ] {
            let generic = parse_items(&result).unwrap();
            assert_eq!(
                generic[0].body, "Unchanged issue prose",
                "other services retain their existing mapping"
            );
            let items = parse_github_issues(&result).unwrap();
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].external_id, "123");
            assert_eq!(items[0].title, "Export timeout");
            assert_eq!(
                items[0].ts_ms,
                rfc3339_to_ms("2026-10-27T10:00:00Z").unwrap()
            );
            assert_eq!(items[0].body, "GitHub issue #7\nState: closed\nState reason: completed\nUpdated: 2026-10-27T10:00:00Z\nURL: https://github.com/example/harbor/issues/7\n\nUnchanged issue prose");
            assert_eq!(
                items,
                parse_github_issues(&result).unwrap(),
                "replaying a provider snapshot must be stable"
            );
        }
    }

    #[test]
    fn github_pull_request_marker_does_not_turn_closed_into_merged() {
        let result = json!({"structuredContent": {"items": [{
            "id": "pr-8", "number": 8, "title": "Fix export", "body": null,
            "state": "closed", "updated_at": "2026-10-27T11:00:00Z",
            "html_url": "https://github.com/example/harbor/pull/8",
            "pull_request": {"url": "https://api.github.com/repos/example/harbor/pulls/8"}
        }], "total_count": 1}});
        let items = parse_github_issues(&result).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].external_id, "pr-8");
        assert!(items[0]
            .body
            .starts_with("GitHub pull request #8\nState: closed\n"));
        assert!(
            items[0].body.ends_with("\n\nFix export"),
            "a null body retains the title fallback"
        );
        assert!(!items[0].body.contains("merged"));
        assert!(
            !items[0].body.contains("api.github.com"),
            "use the issue's canonical web URL"
        );
    }

    #[test]
    fn github_missing_or_invalid_metadata_does_not_invent_state_or_time() {
        let result = json!({"structuredContent": {"items": [{
            "id": 9, "title": "Imported issue", "body": "Retained source text",
            "state": "deleted", "state_reason": "made-up", "updated_at": "not a date",
            "html_url": "https://github.com/example/harbor/issues/9\nState: closed"
        }], "total_count": 1}});
        let item = &parse_github_issues(&result).unwrap()[0];
        assert_eq!(item.body, "GitHub issue\n\nRetained source text");
        assert_eq!(item.ts_ms, 0);
        assert!(parse_github_issues(
            &json!({"structuredContent": {"items": [], "total_count": 0}})
        )
        .unwrap()
        .is_empty());
        assert!(parse_github_issues(
            &json!({"isError": true, "content": [{"type": "text", "text": "unauthorized"}]})
        )
        .is_err());
    }

    #[test]
    fn permission_only_mcp_errors_do_not_require_new_sign_in() {
        for text in ["Forbidden", "HTTP 403 insufficient_scope", "missing_scope", "permission denied"] {
            let result = json!({"isError": true, "content": [{"type": "text", "text": text}]});
            assert_eq!(tool_error_reason(&result), Some("mcp tool returned an error"));
        }
        let result = json!({"isError": true, "structuredContent": {"error": {"code": "forbidden"}}});
        assert_eq!(tool_error_reason(&result), Some("mcp tool returned an error"));
    }

    #[test]
    fn granola_note_keeps_requested_id_and_text_without_response_field_guesses() {
        use serde_json::json;
        let note = super::granola_note(&json!({"content":[{"type":"text","text":"Meeting notes and action items"}]}), "meeting-1").unwrap();
        assert_eq!(note.external_id, "meeting-1");
        assert_eq!(note.body, "Meeting notes and action items");
        assert!(note.ts_ms > 0, "an undated note is filed at read time, not 1970");
        let dated = super::granola_note(&json!({"structuredContent":{"items":[
            {"id":"meeting-1","title":"Weekly sync","updated_at":"2026-09-27T12:00:00Z","attendees":[{"name":"Ann"}]}
        ]}}), "meeting-1").unwrap();
        assert_eq!(dated.title, "Weekly sync");
        assert_eq!(dated.ts_ms, 1_790_510_400_000);
        assert!(super::granola_note(&json!({"content":[]}), "meeting-1").is_err());
        assert!(super::granola_note(&json!({"isError":true,"content":[{"type":"text","text":"denied"}]}), "meeting-1").is_err());
    }

    #[test]
    fn issue_provider_wrappers_and_timestamps_normalize() {
        for result in [serde_json::json!({"structuredContent":{"items":[{"id":1,"title":"GitHub issue","body":"body","updated_at":"2026-09-27T12:00:00Z"}]}}),serde_json::json!({"structuredContent":{"issues":[{"id":"linear-id","title":"Linear issue","description":"body","updatedAt":"2026-09-27T12:00:00Z"}]}})] {
            let items = super::parse_items(&result).unwrap();
            assert_eq!(items.len(),1);
            assert_eq!(items[0].body,"body");
            assert!(items[0].ts_ms > 0);
            assert!(matches!(items[0].external_id.as_str(), "1" | "linear-id"));
        }
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn parses_structured_content_array() {
        let result = json!({
            "structuredContent": [
                { "id": "t1", "subject": "Roadmap", "snippet": "Ship Friday", "internalDate": 1000 },
                { "id": "t2", "subject": "Invoice", "snippet": "Due next week", "internalDate": "2000" },
            ],
            "isError": false
        });
        let items = parse_items(&result).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].external_id, "t1");
        assert_eq!(items[0].title, "Roadmap");
        assert_eq!(items[0].body, "Ship Friday");
        assert_eq!(items[0].ts_ms, 1000);
        // numeric string timestamp is parsed
        assert_eq!(items[1].ts_ms, 2000);
    }

    #[test]
    fn rfc3339_timestamps_read_as_the_instant_they_name() {
        // 2026-09-24T01:00:00Z, written four ways.
        let want = Some(1_790_211_600_000);
        assert_eq!(rfc3339_to_ms("2026-09-24T01:00:00Z"), want);
        assert_eq!(rfc3339_to_ms("2026-09-24T10:00:00+09:00"), want);
        assert_eq!(rfc3339_to_ms("2026-09-23T18:00:00-07:00"), want);
        assert_eq!(rfc3339_to_ms("2026-09-24T01:00:00.000Z"), want);
        assert_eq!(rfc3339_to_ms("2026-09-24T01:00:00.25Z"), Some(1_790_211_600_250));
        for bad in ["", "2026-09-24", "2026-13-01T00:00:00Z", "yesterday", "2026-09-24T25:00:00Z"] {
            assert_eq!(rfc3339_to_ms(bad), None, "{bad}");
        }
    }

    #[test]
    fn cancelled_calendar_tombstone_is_not_dropped_for_missing_title_or_start() {
        let items = parse_items(&json!({"structuredContent":{"items":[{"kind":"calendar#event", "id":"removed-event", "status":"cancelled"}]}})).unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].calendar.as_ref().unwrap().cancelled);
        assert_eq!(items[0].external_id, "removed-event");
    }

    #[test]
    fn calendar_join_link_survives_a_physical_room_location() {
        let mut event = json!({"start":{"dateTime":"2026-09-24T10:00:00Z"}, "location":"Room 4", "hangoutLink":"https://meet.google.com/abc-defg-hij"});
        assert_eq!(
            calendar_meta(&event).unwrap().location.as_deref(),
            Some("https://meet.google.com/abc-defg-hij")
        );
        event.as_object_mut().unwrap().remove("hangoutLink");
        event["conferenceData"] = json!({"entryPoints":[{"entryPointType":"phone", "uri":"tel:123"},{"entryPointType":"video", "uri":"https://zoom.us/j/123456789"}]});
        assert_eq!(
            calendar_meta(&event).unwrap().location.as_deref(),
            Some("https://zoom.us/j/123456789")
        );
    }

    #[test]
    fn calendar_events_carry_their_structure_and_a_real_timestamp() {
        let result = json!({ "structuredContent": { "items": [
            {
                "id": "ev1",
                "summary": "Design review",
                "updated": "2026-09-20T08:00:00Z",
                "start": { "dateTime": "2026-09-24T10:00:00+09:00" },
                "end": { "dateTime": "2026-09-24T11:00:00+09:00" },
                "hangoutLink": "https://meet.google.com/abc-defg-hij",
                "attendees": [
                    { "email": "me@example.com", "self": true, "responseStatus": "accepted" },
                    { "email": "tanaka@example.com", "displayName": "Tanaka Hanako" },
                    { "email": "room@resource.example.com" }
                ]
            },
            { "id": "ev2", "summary": "Offsite", "start": { "date": "2026-09-30" },
              "end": { "date": "2026-10-01" }, "status": "cancelled" },
            { "id": "not-an-event", "subject": "Mail", "snippet": "hello", "internalDate": 5 }
        ], "defaultReminders": [] } });
        let items = parse_items(&result).unwrap();
        assert_eq!(items.len(), 3);

        let ev = &items[0];
        // ts is the event's own `updated`, not 0.
        assert_eq!(ev.ts_ms, rfc3339_to_ms("2026-09-20T08:00:00Z").unwrap());
        let cal = ev.calendar.as_ref().expect("a calendar event");
        assert_eq!(cal.start_ms, 1_790_211_600_000);
        assert_eq!(cal.end_ms, 1_790_215_200_000);
        assert!(!cal.all_day && !cal.cancelled);
        assert_eq!(cal.location.as_deref(), Some("https://meet.google.com/abc-defg-hij"));
        assert_eq!(cal.attendees.len(), 3);
        assert!(cal.attendees[0].is_self);
        assert_eq!(cal.attendees[1].name.as_deref(), Some("Tanaka Hanako"));

        let offsite = items[1].calendar.as_ref().expect("an all-day event");
        assert!(offsite.all_day && offsite.cancelled);
        assert_eq!(offsite.end_ms - offsite.start_ms, 86_400_000);

        assert_eq!(items[2].calendar, None, "a non-calendar record gets no calendar facts");
    }

    #[test]
    fn parses_json_from_text_content_block_with_wrapper_array() {
        let payload = json!({ "threads": [ { "threadId": "x", "summary": "Hi", "body": "hello" } ] });
        let result = json!({
            "content": [ { "type": "text", "text": payload.to_string() } ]
        });
        let items = parse_items(&result).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].external_id, "x");
        assert_eq!(items[0].body, "hello");
    }

    #[test]
    fn is_error_result_never_leaks_provider_content() {
        let result = json!({
            "isError": true,
            "content": [ { "type": "text", "text": "private-content@example.com SECRET" } ]
        });
        let err = parse_items(&result).expect_err("provider error");
        assert_eq!(err, "mcp tool returned an error");
        let auth = json!({ "isError": true, "content": [{ "type": "text", "text": "invalid_grant: PRIVATE" }] });
        assert_eq!(parse_items(&auth).expect_err("provider error"), "mcp tool unauthorized");
        let structured = json!({ "isError": true, "structuredContent": { "error": { "code": "invalid_token", "message": "PRIVATE" } } });
        assert_eq!(tool_error_reason(&structured), Some("mcp tool unauthorized"));
        for text in ["Unauthorized: token expired", "error: invalid_auth", "token_revoked"] {
            let result = json!({"isError": true, "content": [{"type": "text", "text": text}]});
            assert_eq!(tool_error_reason(&result), Some("mcp tool unauthorized"), "{text}");
        }
    }

    #[test]
    fn records_with_no_body_fall_back_to_title_and_drop_only_when_both_are_missing() {
        let result = json!({
            "structuredContent": [
                { "id": "a", "subject": "only a title" },
                { "id": "b", "body": "   " },
                { "id": "c", "body": "real" },
            ]
        });
        let items = parse_items(&result).unwrap();
        assert_eq!(items.len(), 2);
        // Title stands in for a missing body (a Drive file, a description-less event).
        assert_eq!(items[0].external_id, "a");
        assert_eq!(items[0].body, "only a title");
        // Blank body with no title: nothing to ingest.
        assert_eq!(items[1].external_id, "c");
        assert_eq!(items[1].body, "real");
    }

    #[test]
    fn calendar_wrapper_with_two_arrays_still_yields_the_items() {
        // Calendar `events.list` shape: `items` (the records) plus `defaultReminders` (also an
        // array). The single-array heuristic alone would return no records here — green "Synced",
        // zero ingested, forever.
        let payload = json!({
            "kind": "calendar#events",
            "defaultReminders": [ { "method": "popup", "minutes": 10 } ],
            "items": [ { "id": "e1", "summary": "Standup", "description": "Daily 9am" } ],
        });
        let result = json!({
            "content": [ { "type": "text", "text": payload.to_string() } ]
        });
        let items = parse_items(&result).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].external_id, "e1");
        assert_eq!(items[0].body, "Daily 9am");
    }

    #[test]
    fn drive_file_metadata_without_a_body_key_ingests_by_name() {
        let result = json!({
            "structuredContent": {
                "files": [ { "id": "f1", "name": "Q3 deck.key", "mimeType": "application/x-iwork-keynote" } ]
            }
        });
        let items = parse_items(&result).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].external_id, "f1");
        assert_eq!(items[0].body, "Q3 deck.key");
    }

    #[test]
    fn empty_or_contentless_result_is_empty_not_error() {
        assert_eq!(parse_items(&json!({})).unwrap().len(), 0);
        assert_eq!(parse_items(&json!({ "content": [] })).unwrap().len(), 0);
    }
}
