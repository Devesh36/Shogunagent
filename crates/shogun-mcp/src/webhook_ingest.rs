//! `memory.ingest_webhook` (plan E44): one provider webhook delivery turned into text events
//! with provenance, through the `memory.ingest` path (one parser here, then
//! [`crate::backend::MemoryBackend::ingest`] and the same receipt as `memory.ingest`).
//!
//! Who calls it: the hosted gateway's ingest hooks (`apps/api/src/ingest-hooks/`) after they
//! verified the provider's signature, as the hook's own actor (`agent:<hook id>`, from the derived
//! sidecar bearer), and any caller with the `write` scope replaying a saved delivery. The gateway
//! never parses a payload beyond what verification needs: it hands the verified bytes over as
//! `payload_b64` (base64, so the gateway builds the JSON-RPC envelope without reading the body),
//! and this module is where the payload is read (CLAUDE.md invariant 1).
//!
//! What it stores (text only; the media rule: no image, audio or attachment is ever read):
//!
//! | provider | stored | from |
//! |---|---|---|
//! | `github` | one event per delivery: push (commit messages), issues, issue and review comments, pull requests, reviews, releases | the payload |
//! | `linear` | one event per issue, comment or project update | the payload |
//! | `slack` | one event per message / app mention (Events API `event_callback`) | the payload |
//! | `notion` | nothing: a typed `changed` notice (Notion events carry ids, not content) | the payload's entity |
//! | `google_calendar` | nothing: a typed `changed` notice (a push carries only the channel) | `changed` (headers the gateway lifted) |
//! | `gmail` | nothing: a typed `changed` notice (a Pub/Sub push carries a history id) | `changed` |
//!
//! A `changed` notice settles at 0 events (free): fetching the content is the core's connector
//! read-sync with the principal's own credentials (`crate::sync`, E50), which ingests what it
//! fetches and is billed then. Anything else a provider sends (other event types, bot messages,
//! edits) is acknowledged as `ignored` and stores nothing.
//!
//! Provenance on every row (invariant 5): the row's `actor` is the caller (the hook), its kind is
//! `<provider>.<event>`, its title names the provider and the object, and its text ends with one
//! line naming the provider, the delivery id and the object's URL. The receipt's `metadata` carries
//! the same three. Text passes the `memory.ingest` gates: instruction-shaped text is refused,
//! secrets are masked and duplicates collapse, so a replayed delivery under a new delivery id adds
//! nothing.

use serde_json::{json, Value};

use crate::ingest_api::{IngestItem, IngestRequest, Namespace, Refusal, MAX_ITEM_CHARS, MAX_KEY_CHARS, MAX_TITLE_CHARS};
use crate::memory_api::Tool;

/// The providers a delivery may name, as the wire spells them.
pub const PROVIDERS: &[&str] = &["github", "linear", "slack", "notion", "google_calendar", "gmail"];
/// Largest decoded payload, in bytes (the gateway refuses a larger body with 413 first).
pub const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
/// Most events one delivery becomes (a push of many commits is one event).
pub const MAX_EVENTS_PER_DELIVERY: usize = 20;
/// Longest `delivery_id` / `event` / `hook_id`, in chars.
pub const MAX_ID_CHARS: usize = 128;
/// Longest value inside `changed`, in chars.
pub const MAX_CHANGED_VALUE_CHARS: usize = 512;
/// Commits of one push that are written out (the rest are counted).
const MAX_PUSH_COMMITS: usize = 20;

/// A provider the hooks serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    GitHub,
    Linear,
    Slack,
    Notion,
    GoogleCalendar,
    Gmail,
}

impl Provider {
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "github" => Self::GitHub,
            "linear" => Self::Linear,
            "slack" => Self::Slack,
            "notion" => Self::Notion,
            "google_calendar" => Self::GoogleCalendar,
            "gmail" => Self::Gmail,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::Linear => "linear",
            Self::Slack => "slack",
            Self::Notion => "notion",
            Self::GoogleCalendar => "google_calendar",
            Self::Gmail => "gmail",
        }
    }

    /// The name a person reads in a title.
    pub fn label(self) -> &'static str {
        match self {
            Self::GitHub => "GitHub",
            Self::Linear => "Linear",
            Self::Slack => "Slack",
            Self::Notion => "Notion",
            Self::GoogleCalendar => "Google Calendar",
            Self::Gmail => "Gmail",
        }
    }
}

/// A parsed `memory.ingest_webhook` request.
#[derive(Debug, Clone, PartialEq)]
pub struct WebhookRequest {
    pub provider: Provider,
    /// The provider's delivery id (a header, or the gateway's body digest when the provider sends
    /// none). Provenance, not identity: the idempotency key is what replays.
    pub delivery_id: String,
    pub idempotency_key: String,
    /// The provider's event type header (`X-GitHub-Event`, `Linear-Event`), when it sends one.
    pub event: Option<String>,
    /// The hook that received it (`hk_…`), when the gateway relayed it.
    pub hook_id: Option<String>,
    /// The decoded, parsed payload (`payload_b64`).
    pub payload: Option<Value>,
    /// The typed change a push named (Calendar, Gmail), flat string values only.
    pub changed: Option<serde_json::Map<String, Value>>,
    /// When the gateway received it (unix ms); the default time of the events.
    pub received_ms: Option<i64>,
}

/// A typed "something changed" notice: the provider said an object changed and sent no content.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangedNotice {
    /// `calendar`, `mailbox`, `page`, `database`, `comment`, … — what the core would fetch.
    pub resource: String,
    /// The ids the fetch starts from (channel and resource ids, history id, entity id).
    pub detail: Value,
}

/// What one delivery becomes.
#[derive(Debug, Clone, PartialEq)]
pub enum Normalized {
    /// Text events to append through the `memory.ingest` path.
    Ingest(IngestRequest),
    /// A change notice: nothing stored here; the connector read-sync fetches the content.
    Changed(ChangedNotice),
    /// Not something memory keeps (another event type, a bot message, an edit, a handshake).
    Ignored(&'static str),
}

/// Whether `tool` is served by this module.
pub fn is_webhook_tool(tool: Tool) -> bool {
    tool == Tool::MemoryIngestWebhook
}

fn bounded_id(args: &Value, key: &str, max: usize, required: bool) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) if required => Err(format!("missing_{key}: pass {key}, a non-empty string.")),
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let text = text.trim();
            if text.is_empty() {
                return if required { Err(format!("missing_{key}: pass {key}, a non-empty string.")) } else { Ok(None) };
            }
            if text.chars().count() > max || !text.chars().all(|c| c.is_ascii_graphic()) {
                return Err(format!("invalid_{key}: {key} is at most {max} visible ASCII characters."));
            }
            Ok(Some(text.to_string()))
        }
        Some(_) => Err(format!("invalid_{key}: {key} must be a string.")),
    }
}

/// Parse `memory.ingest_webhook` arguments. `Err` is `<code>: <sentence>` (MCP -32602, REST 400).
pub fn webhook_request(args: &Value) -> Result<WebhookRequest, String> {
    use base64::Engine as _;
    if !args.is_object() {
        return Err("invalid_request: the body must be a JSON object.".into());
    }
    let raw_provider = bounded_id(args, "provider", 32, true)?.unwrap_or_default();
    let provider = Provider::parse(&raw_provider)
        .ok_or_else(|| format!("invalid_provider: provider must be one of {}.", PROVIDERS.join(", ")))?;
    let idempotency_key = bounded_id(args, "idempotency_key", MAX_KEY_CHARS, true)?.unwrap_or_default();
    let delivery_id = bounded_id(args, "delivery_id", MAX_ID_CHARS, true)?.unwrap_or_default();
    let event = bounded_id(args, "event", MAX_ID_CHARS, false)?;
    let hook_id = bounded_id(args, "hook_id", MAX_ID_CHARS, false)?;
    let received_ms = match args.get("received_ms") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_i64().filter(|ms| *ms > 0).ok_or("invalid_received_ms: received_ms must be a positive integer (unix ms).")?),
    };
    let payload = match args.get("payload_b64") {
        None | Some(Value::Null) => None,
        Some(Value::String(encoded)) => {
            if encoded.len() > MAX_PAYLOAD_BYTES.div_ceil(3) * 4 + 4 {
                return Err(format!("invalid_payload_b64: the decoded payload is at most {MAX_PAYLOAD_BYTES} bytes."));
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .map_err(|_| "invalid_payload_b64: payload_b64 must be standard base64.".to_string())?;
            if bytes.len() > MAX_PAYLOAD_BYTES {
                return Err(format!("invalid_payload_b64: the decoded payload is at most {MAX_PAYLOAD_BYTES} bytes."));
            }
            Some(serde_json::from_slice::<Value>(&bytes).map_err(|_| "invalid_payload_b64: the payload must be JSON.".to_string())?)
        }
        Some(_) => return Err("invalid_payload_b64: payload_b64 must be a string.".into()),
    };
    let changed = match args.get("changed") {
        None | Some(Value::Null) => None,
        Some(Value::Object(map)) => {
            if map.len() > 8 {
                return Err("invalid_changed: changed holds at most 8 fields.".into());
            }
            for (key, value) in map {
                let ok_key = key.len() <= 32 && key.chars().all(|c| c.is_ascii_lowercase() || c == '_');
                let ok_value = value
                    .as_str()
                    .is_some_and(|s| s.chars().count() <= MAX_CHANGED_VALUE_CHARS && s.chars().all(|c| c.is_ascii_graphic()));
                if !ok_key || !ok_value {
                    return Err("invalid_changed: changed maps snake_case names to short visible-ASCII strings.".into());
                }
            }
            Some(map.clone())
        }
        Some(_) => return Err("invalid_changed: changed must be an object.".into()),
    };
    match provider {
        Provider::GoogleCalendar | Provider::Gmail if changed.is_none() => {
            return Err(format!("missing_changed: a {} delivery carries changed, not a payload.", provider.as_str()));
        }
        Provider::GitHub | Provider::Linear | Provider::Slack | Provider::Notion if payload.is_none() => {
            return Err(format!("missing_payload_b64: a {} delivery carries its payload as payload_b64.", provider.as_str()));
        }
        _ => {}
    }
    Ok(WebhookRequest { provider, delivery_id, idempotency_key, event, hook_id, payload, changed, received_ms })
}

/// The string at a JSON pointer, trimmed, when it is a non-empty string.
fn text_at<'a>(v: &'a Value, pointer: &str) -> Option<&'a str> {
    v.pointer(pointer).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn cut(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// One line: newlines and control characters folded into spaces.
fn one_line(text: &str) -> String {
    text.split(|c: char| c.is_control()).filter(|s| !s.trim().is_empty()).map(str::trim).collect::<Vec<_>>().join(" ")
}

/// An `http(s)` URL without whitespace, or nothing (a URL is provenance, never followed here).
fn web_url(raw: Option<&str>) -> Option<String> {
    raw.filter(|u| (u.starts_with("https://") || u.starts_with("http://")) && u.len() <= 2048 && !u.chars().any(char::is_whitespace))
        .map(str::to_string)
}

/// One stored event before it becomes an [`IngestItem`].
struct Draft {
    kind: String,
    title: String,
    body: String,
    url: Option<String>,
    ts_ms: Option<i64>,
    thread: Option<String>,
}

fn finish(req: &WebhookRequest, drafts: Vec<Draft>) -> Normalized {
    if drafts.is_empty() {
        return Normalized::Ignored("nothing_to_store");
    }
    let label = req.provider.label();
    let first_url = drafts.iter().find_map(|d| d.url.clone());
    let thread_key = drafts.iter().find_map(|d| d.thread.clone()).map(|t| cut(&t, crate::ingest_api::MAX_THREAD_KEY_CHARS));
    let items = drafts
        .into_iter()
        .take(MAX_EVENTS_PER_DELIVERY)
        .map(|d| {
            let footer = match &d.url {
                Some(url) => format!("\n\n(via {label} webhook, delivery {}, {url})", req.delivery_id),
                None => format!("\n\n(via {label} webhook, delivery {})", req.delivery_id),
            };
            let room = MAX_ITEM_CHARS.saturating_sub(footer.chars().count());
            IngestItem {
                ts_ms: d.ts_ms.or(req.received_ms),
                kind: cut(&d.kind, crate::ingest_api::MAX_KIND_CHARS),
                title: Some(cut(&one_line(&d.title), MAX_TITLE_CHARS)),
                content: format!("{}{footer}", cut(d.body.trim(), room)),
            }
        })
        .collect();
    let mut metadata = json!({ "provider": req.provider.as_str(), "delivery_id": req.delivery_id });
    if let Some(event) = &req.event {
        metadata["event"] = Value::String(event.clone());
    }
    if let Some(url) = first_url {
        metadata["url"] = Value::String(url);
    }
    if let Some(hook) = &req.hook_id {
        metadata["hook_id"] = Value::String(hook.clone());
    }
    Normalized::Ingest(IngestRequest {
        items,
        turns: false,
        source: format!("{}-webhook", req.provider.as_str()),
        actor: format!("hook:{}", req.provider.as_str()),
        thread_key,
        metadata: Some(metadata),
        idempotency_key: req.idempotency_key.clone(),
        namespace: Namespace::World,
    })
}

/// Turn a parsed delivery into what the store keeps. Pure.
pub fn normalize(req: &WebhookRequest) -> Normalized {
    let empty = Value::Null;
    let payload = req.payload.as_ref().unwrap_or(&empty);
    match req.provider {
        Provider::GitHub => finish(req, github(req.event.as_deref(), payload)),
        Provider::Linear => finish(req, linear(payload)),
        Provider::Slack => match slack(payload) {
            Ok(drafts) => finish(req, drafts),
            Err(reason) => Normalized::Ignored(reason),
        },
        Provider::Notion => notion(payload),
        Provider::GoogleCalendar => changed_notice("calendar", req.changed.as_ref(), &["channel_id", "resource_id", "resource_state", "message_number", "resource_uri"]),
        Provider::Gmail => changed_notice("mailbox", req.changed.as_ref(), &["history_id", "message_id"]),
    }
}

fn changed_notice(resource: &str, changed: Option<&serde_json::Map<String, Value>>, keys: &[&str]) -> Normalized {
    let Some(changed) = changed else {
        return Normalized::Ignored("no_change_named");
    };
    // A Calendar `sync` message only opens the channel; the gateway answers it, but a replayed one
    // stores nothing here either.
    if changed.get("resource_state").and_then(Value::as_str) == Some("sync") {
        return Normalized::Ignored("handshake");
    }
    let detail: serde_json::Map<String, Value> = keys
        .iter()
        .filter_map(|k| changed.get(*k).map(|v| ((*k).to_string(), v.clone())))
        .collect();
    Normalized::Changed(ChangedNotice { resource: resource.to_string(), detail: Value::Object(detail) })
}

// --- GitHub -------------------------------------------------------------------------------------

fn github(event: Option<&str>, p: &Value) -> Vec<Draft> {
    let repo = text_at(p, "/repository/full_name").unwrap_or("a repository");
    let who = text_at(p, "/sender/login").unwrap_or("someone");
    let action = text_at(p, "/action").unwrap_or("");
    let draft = |kind: &str, title: String, body: String, url: Option<&str>, thread: Option<String>| Draft {
        kind: format!("github.{kind}"),
        title,
        body,
        url: web_url(url),
        ts_ms: None,
        thread,
    };
    match event.unwrap_or("") {
        "push" => {
            let branch = text_at(p, "/ref").map(|r| r.trim_start_matches("refs/heads/").to_string()).unwrap_or_default();
            let commits = p.get("commits").and_then(Value::as_array).cloned().unwrap_or_default();
            if commits.is_empty() {
                return Vec::new();
            }
            let mut lines: Vec<String> = commits
                .iter()
                .take(MAX_PUSH_COMMITS)
                .filter_map(|c| {
                    let message = text_at(c, "/message")?;
                    let sha: String = text_at(c, "/id").unwrap_or("").chars().take(7).collect();
                    Some(format!("- {sha} {}", one_line(message)))
                })
                .collect();
            if commits.len() > MAX_PUSH_COMMITS {
                lines.push(format!("- … and {} more commits", commits.len() - MAX_PUSH_COMMITS));
            }
            let body = format!("{who} pushed {} commit(s) to {repo} ({branch}):\n{}", commits.len(), lines.join("\n"));
            vec![draft("push", format!("GitHub push: {repo} {branch}"), body, text_at(p, "/compare"), Some(format!("github:{repo}@{branch}")))]
        }
        "issues" if matches!(action, "opened" | "edited" | "closed" | "reopened") => {
            let number = p.pointer("/issue/number").and_then(Value::as_i64).unwrap_or(0);
            let title = text_at(p, "/issue/title").unwrap_or("");
            let body = text_at(p, "/issue/body").unwrap_or("");
            vec![draft(
                "issue",
                format!("GitHub issue {repo}#{number}: {title}"),
                format!("{who} {action} issue {repo}#{number} \"{title}\".\n\n{body}"),
                text_at(p, "/issue/html_url"),
                Some(format!("github:{repo}#{number}")),
            )]
        }
        "issue_comment" if action == "created" => {
            let number = p.pointer("/issue/number").and_then(Value::as_i64).unwrap_or(0);
            let title = text_at(p, "/issue/title").unwrap_or("");
            let Some(body) = text_at(p, "/comment/body") else { return Vec::new() };
            vec![draft(
                "comment",
                format!("GitHub comment on {repo}#{number}: {title}"),
                format!("{who} commented on {repo}#{number} \"{title}\":\n\n{body}"),
                text_at(p, "/comment/html_url"),
                Some(format!("github:{repo}#{number}")),
            )]
        }
        "pull_request" if matches!(action, "opened" | "edited" | "closed" | "reopened" | "ready_for_review") => {
            let number = p.pointer("/pull_request/number").or_else(|| p.get("number")).and_then(Value::as_i64).unwrap_or(0);
            let title = text_at(p, "/pull_request/title").unwrap_or("");
            let body = text_at(p, "/pull_request/body").unwrap_or("");
            let merged = p.pointer("/pull_request/merged").and_then(Value::as_bool) == Some(true);
            let verb = if action == "closed" && merged { "merged" } else { action };
            vec![draft(
                "pull_request",
                format!("GitHub PR {repo}#{number}: {title}"),
                format!("{who} {verb} pull request {repo}#{number} \"{title}\".\n\n{body}"),
                text_at(p, "/pull_request/html_url"),
                Some(format!("github:{repo}#{number}")),
            )]
        }
        "pull_request_review" if action == "submitted" => {
            let number = p.pointer("/pull_request/number").and_then(Value::as_i64).unwrap_or(0);
            let state = text_at(p, "/review/state").unwrap_or("commented").to_lowercase().replace('_', " ");
            let body = text_at(p, "/review/body").unwrap_or("");
            vec![draft(
                "review",
                format!("GitHub review on {repo}#{number}"),
                format!("{who} reviewed {repo}#{number} ({state}).\n\n{body}"),
                text_at(p, "/review/html_url"),
                Some(format!("github:{repo}#{number}")),
            )]
        }
        "pull_request_review_comment" if action == "created" => {
            let number = p.pointer("/pull_request/number").and_then(Value::as_i64).unwrap_or(0);
            let Some(body) = text_at(p, "/comment/body") else { return Vec::new() };
            let path = text_at(p, "/comment/path").unwrap_or("");
            vec![draft(
                "review_comment",
                format!("GitHub review comment on {repo}#{number}"),
                format!("{who} commented on {path} in {repo}#{number}:\n\n{body}"),
                text_at(p, "/comment/html_url"),
                Some(format!("github:{repo}#{number}")),
            )]
        }
        "release" if action == "published" => {
            let tag = text_at(p, "/release/tag_name").unwrap_or("");
            let name = text_at(p, "/release/name").unwrap_or(tag);
            let body = text_at(p, "/release/body").unwrap_or("");
            vec![draft(
                "release",
                format!("GitHub release {repo} {tag}"),
                format!("{who} published release {name} ({tag}) of {repo}.\n\n{body}"),
                text_at(p, "/release/html_url"),
                Some(format!("github:{repo}@{tag}")),
            )]
        }
        _ => Vec::new(),
    }
}

// --- Linear -------------------------------------------------------------------------------------

fn linear(p: &Value) -> Vec<Draft> {
    let kind = text_at(p, "/type").unwrap_or("");
    let action = text_at(p, "/action").unwrap_or("");
    if action == "remove" {
        return Vec::new();
    }
    let verb = match action {
        "create" => "created",
        "update" => "updated",
        _ => "changed",
    };
    let who = text_at(p, "/actor/name").unwrap_or("someone");
    let url = web_url(text_at(p, "/url").or_else(|| text_at(p, "/data/url")));
    let ts_ms = p.get("webhookTimestamp").and_then(Value::as_i64).filter(|ms| *ms > 0);
    match kind {
        "Issue" => {
            let id = text_at(p, "/data/identifier").unwrap_or("an issue");
            let title = text_at(p, "/data/title").unwrap_or("");
            let state = text_at(p, "/data/state/name").map(|s| format!(" (state: {s})")).unwrap_or_default();
            let description = text_at(p, "/data/description").unwrap_or("");
            vec![Draft {
                kind: "linear.issue".into(),
                title: format!("Linear issue {id}: {title}"),
                body: format!("{who} {verb} Linear issue {id} \"{title}\"{state}.\n\n{description}"),
                url,
                ts_ms,
                thread: Some(format!("linear:{id}")),
            }]
        }
        "Comment" => {
            let Some(body) = text_at(p, "/data/body") else { return Vec::new() };
            let id = text_at(p, "/data/issue/identifier").unwrap_or("an issue");
            let title = text_at(p, "/data/issue/title").unwrap_or("");
            vec![Draft {
                kind: "linear.comment".into(),
                title: format!("Linear comment on {id}: {title}"),
                body: format!("{who} commented on Linear issue {id} \"{title}\":\n\n{body}"),
                url,
                ts_ms,
                thread: Some(format!("linear:{id}")),
            }]
        }
        "Project" => {
            let name = text_at(p, "/data/name").unwrap_or("a project");
            let description = text_at(p, "/data/description").unwrap_or("");
            vec![Draft {
                kind: "linear.project".into(),
                title: format!("Linear project: {name}"),
                body: format!("{who} {verb} Linear project \"{name}\".\n\n{description}"),
                url,
                ts_ms,
                thread: Some(format!("linear:project:{name}")),
            }]
        }
        "ProjectUpdate" => {
            let Some(body) = text_at(p, "/data/body") else { return Vec::new() };
            let name = text_at(p, "/data/project/name").unwrap_or("a project");
            let health = text_at(p, "/data/health").map(|h| format!(" (health: {h})")).unwrap_or_default();
            vec![Draft {
                kind: "linear.project_update".into(),
                title: format!("Linear project update: {name}"),
                body: format!("{who} posted an update on Linear project \"{name}\"{health}:\n\n{body}"),
                url,
                ts_ms,
                thread: Some(format!("linear:project:{name}")),
            }]
        }
        _ => Vec::new(),
    }
}

// --- Slack --------------------------------------------------------------------------------------

/// Slack's `ts` (`"1700000000.000200"`) as unix ms.
fn slack_ts_ms(ts: &str) -> Option<i64> {
    let (secs, frac) = ts.split_once('.').unwrap_or((ts, "0"));
    let secs: i64 = secs.parse().ok()?;
    let millis: i64 = frac.chars().chain(std::iter::repeat('0')).take(3).collect::<String>().parse().ok()?;
    secs.checked_mul(1000)?.checked_add(millis).filter(|ms| *ms > 0)
}

fn slack(p: &Value) -> Result<Vec<Draft>, &'static str> {
    match text_at(p, "/type") {
        Some("url_verification") => return Err("handshake"),
        Some("event_callback") => {}
        _ => return Err("unsupported_event"),
    }
    let event = p.get("event").unwrap_or(&Value::Null);
    let kind = text_at(event, "/type").unwrap_or("");
    if !matches!(kind, "message" | "app_mention") {
        return Err("unsupported_event");
    }
    // Edits, deletions, joins and bot posts are not new things the person said.
    if let Some(subtype) = text_at(event, "/subtype") {
        if !matches!(subtype, "thread_broadcast" | "file_share") {
            return Err("unsupported_event");
        }
    }
    if event.get("bot_id").is_some() {
        return Err("bot_message");
    }
    let Some(text) = text_at(event, "/text") else { return Err("nothing_to_store") };
    let team = text_at(p, "/team_id").unwrap_or("");
    let channel = text_at(event, "/channel").unwrap_or("");
    let user = text_at(event, "/user").unwrap_or("someone");
    let ts = text_at(event, "/ts").unwrap_or("");
    let thread = text_at(event, "/thread_ts").unwrap_or(ts);
    let valid_id = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric());
    let url = (valid_id(team) && valid_id(channel)).then(|| format!("https://app.slack.com/client/{team}/{channel}"));
    let label = if kind == "app_mention" { "mentioned the app" } else { "wrote" };
    Ok(vec![Draft {
        kind: format!("slack.{kind}"),
        title: format!("Slack message in {channel}"),
        body: format!("<@{user}> {label} in <#{channel}>:\n\n{text}"),
        url,
        ts_ms: slack_ts_ms(ts),
        thread: Some(format!("slack:{team}/{channel}/{thread}")),
    }])
}

// --- Notion -------------------------------------------------------------------------------------

fn notion(p: &Value) -> Normalized {
    if p.get("verification_token").is_some() {
        return Normalized::Ignored("handshake");
    }
    let Some(kind) = text_at(p, "/type") else { return Normalized::Ignored("unsupported_event") };
    let Some(entity_id) = text_at(p, "/entity/id") else { return Normalized::Ignored("unsupported_event") };
    let entity_type = text_at(p, "/entity/type").unwrap_or("page");
    let valid = |s: &str| s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_');
    if !valid(entity_id) || !valid(entity_type) || !valid(kind) {
        return Normalized::Ignored("unsupported_event");
    }
    let mut detail = json!({ "event_type": kind, "entity_type": entity_type, "entity_id": entity_id });
    if matches!(entity_type, "page" | "database") {
        detail["url"] = Value::String(format!("https://www.notion.so/{}", entity_id.replace('-', "")));
    }
    if let Some(id) = text_at(p, "/id").filter(|s| valid(s)) {
        detail["notion_event_id"] = Value::String(id.to_string());
    }
    Normalized::Changed(ChangedNotice { resource: entity_type.to_string(), detail })
}

// --- serve --------------------------------------------------------------------------------------

/// The envelope of a delivery that stored nothing (a change notice, or ignored).
fn render_unstored(req: &WebhookRequest, outcome: &str, changed: Option<&ChangedNotice>, reason: Option<&str>, latency_ms: u64) -> String {
    json!({
        "tool": Tool::MemoryIngestWebhook.wire_name(),
        "result": {
            "provider": req.provider.as_str(),
            "delivery_id": req.delivery_id,
            "outcome": outcome,
            "accepted": 0,
            "inserted": 0,
            "changed": changed.map(|c| json!({ "resource": c.resource, "detail": c.detail })),
            "ignored_reason": reason,
            "fetch": changed.map(|_| "connector_sync"),
            "idempotency_key": req.idempotency_key,
        },
        "usage": { "unit": "events", "quantity": 0 },
        "latency_ms": latency_ms,
        "next_action": Value::Null,
    })
    .to_string()
}

/// Parse, normalize, store and render one delivery for any face.
pub fn serve<B: crate::backend::MemoryBackend + ?Sized>(args: &Value, backend: &B) -> Result<String, Refusal> {
    let started = std::time::Instant::now();
    let elapsed = || u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let req = webhook_request(args).map_err(Refusal::BadArguments)?;
    match normalize(&req) {
        Normalized::Ingest(request) => {
            let outcome = backend.ingest(&request).map_err(|_| Refusal::Unavailable("ingest_unavailable"))?;
            let rendered = crate::ingest_api::render_ingest(&request, &outcome, elapsed());
            let mut value: Value = serde_json::from_str(&rendered).map_err(|_| Refusal::Unavailable("ingest_unavailable"))?;
            value["tool"] = Value::String(Tool::MemoryIngestWebhook.wire_name().to_string());
            value["result"]["provider"] = Value::String(req.provider.as_str().to_string());
            value["result"]["delivery_id"] = Value::String(req.delivery_id.clone());
            value["result"]["outcome"] = Value::String("ingested".into());
            Ok(value.to_string())
        }
        Normalized::Changed(notice) => Ok(render_unstored(&req, "changed", Some(&notice), None, elapsed())),
        Normalized::Ignored(reason) => Ok(render_unstored(&req, "ignored", None, Some(reason), elapsed())),
    }
}

/// The MCP `inputSchema` of `memory.ingest_webhook`.
pub fn schema() -> Value {
    json!({
        "provider": { "type": "string", "enum": PROVIDERS },
        "delivery_id": { "type": "string", "maxLength": MAX_ID_CHARS, "description": "The provider's delivery id (provenance)" },
        "idempotency_key": { "type": "string", "maxLength": MAX_KEY_CHARS, "description": "Required; the same key replays the first result" },
        "event": { "type": "string", "maxLength": MAX_ID_CHARS, "description": "The provider's event type header (X-GitHub-Event, Linear-Event)" },
        "hook_id": { "type": "string", "maxLength": MAX_ID_CHARS, "description": "The ingest hook that received it" },
        "payload_b64": { "type": "string", "description": "The delivery body, base64 (GitHub, Linear, Slack, Notion); at most 1 MiB decoded" },
        "changed": { "type": "object", "description": "The ids a Calendar or Gmail push named (string values)" },
        "received_ms": { "type": "integer", "minimum": 1, "description": "When it was received (unix ms); default time of the events" }
    })
}

/// The agent-facing description.
pub const DESCRIPTION: &str = "Store one provider webhook delivery (GitHub, Linear, Slack, Notion, Google Calendar, Gmail) in the user's memory as text events with provenance (provider, delivery id, URL): commits, issues, comments, pull requests, reviews, Linear issues and project updates, Slack messages. Calendar, Gmail and Notion deliveries carry only ids: they are acknowledged as changed and store nothing until the connector fetches the content. The hosted ingest hooks call it after verifying the provider's signature; use it yourself only to replay a saved delivery. Not for your own notes or turns (memory.ingest). Local L1 write; the same idempotency_key replays. Metered per event stored (ingest).";

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn b64(v: &Value) -> String {
        base64::engine::general_purpose::STANDARD.encode(v.to_string())
    }

    fn args(provider: &str, event: Option<&str>, payload: &Value) -> Value {
        let mut a = json!({ "provider": provider, "delivery_id": "d-1", "idempotency_key": format!("hook:{provider}:d-1"), "payload_b64": b64(payload), "received_ms": 1_700_000_000_000_i64, "hook_id": "hk_test" });
        if let Some(event) = event {
            a["event"] = Value::String(event.into());
        }
        a
    }

    fn ingest_of(n: Normalized) -> IngestRequest {
        match n {
            Normalized::Ingest(r) => r,
            other => panic!("expected an ingest, got {other:?}"),
        }
    }

    fn github_issue() -> Value {
        json!({
            "action": "opened",
            "issue": { "number": 42, "title": "Login fails on Safari", "body": "Steps: open the page, sign in.", "html_url": "https://github.com/acme/web/issues/42" },
            "repository": { "full_name": "acme/web" },
            "sender": { "login": "octo" }
        })
    }

    #[test]
    fn github_issue_becomes_one_event_with_provenance() {
        let req = webhook_request(&args("github", Some("issues"), &github_issue())).unwrap();
        let r = ingest_of(normalize(&req));
        assert_eq!(r.items.len(), 1);
        let item = &r.items[0];
        assert_eq!(item.kind, "github.issue");
        assert_eq!(item.title.as_deref(), Some("GitHub issue acme/web#42: Login fails on Safari"));
        assert!(item.content.starts_with("octo opened issue acme/web#42 \"Login fails on Safari\"."));
        assert!(item.content.contains("Steps: open the page, sign in."));
        assert!(item.content.ends_with("(via GitHub webhook, delivery d-1, https://github.com/acme/web/issues/42)"));
        assert_eq!(item.ts_ms, Some(1_700_000_000_000));
        assert_eq!(r.thread_key.as_deref(), Some("github:acme/web#42"));
        assert_eq!(r.idempotency_key, "hook:github:d-1");
        assert_eq!(r.actor, "hook:github");
        assert_eq!(r.namespace, Namespace::World);
        let meta = r.metadata.unwrap();
        assert_eq!(meta["provider"], "github");
        assert_eq!(meta["delivery_id"], "d-1");
        assert_eq!(meta["url"], "https://github.com/acme/web/issues/42");
        assert_eq!(meta["hook_id"], "hk_test");
    }

    #[test]
    fn github_push_lists_commits_in_one_event() {
        let commits: Vec<Value> = (0..25).map(|i| json!({ "id": format!("abcdef{i:02}99"), "message": format!("fix: item {i}\n\nlong body") })).collect();
        let p = json!({ "ref": "refs/heads/main", "compare": "https://github.com/acme/web/compare/a...b", "commits": commits, "repository": { "full_name": "acme/web" }, "sender": { "login": "octo" } });
        let r = ingest_of(normalize(&webhook_request(&args("github", Some("push"), &p)).unwrap()));
        assert_eq!(r.items.len(), 1);
        let c = &r.items[0].content;
        assert!(c.starts_with("octo pushed 25 commit(s) to acme/web (main):"));
        assert!(c.contains("- abcdef0 fix: item 0 long body"));
        assert!(c.contains("… and 5 more commits"));
        assert!(!c.contains("item 24"));
        assert_eq!(r.thread_key.as_deref(), Some("github:acme/web@main"));
    }

    #[test]
    fn github_comment_pr_review_and_release_are_kept() {
        for (event, payload, kind) in [
            ("issue_comment", json!({ "action": "created", "issue": { "number": 7, "title": "T" }, "comment": { "body": "Looks good", "html_url": "https://github.com/a/b/issues/7#c1" }, "repository": { "full_name": "a/b" } }), "github.comment"),
            ("pull_request", json!({ "action": "closed", "pull_request": { "number": 8, "title": "Add X", "body": "", "merged": true, "html_url": "https://github.com/a/b/pull/8" }, "repository": { "full_name": "a/b" } }), "github.pull_request"),
            ("pull_request_review", json!({ "action": "submitted", "pull_request": { "number": 8 }, "review": { "state": "CHANGES_REQUESTED", "body": "Please add tests", "html_url": "https://github.com/a/b/pull/8#r1" }, "repository": { "full_name": "a/b" } }), "github.review"),
            ("pull_request_review_comment", json!({ "action": "created", "pull_request": { "number": 8 }, "comment": { "body": "nit", "path": "src/x.rs", "html_url": "https://github.com/a/b/pull/8#d1" }, "repository": { "full_name": "a/b" } }), "github.review_comment"),
            ("release", json!({ "action": "published", "release": { "tag_name": "v1.2.0", "name": "1.2", "body": "Notes", "html_url": "https://github.com/a/b/releases/v1.2.0" }, "repository": { "full_name": "a/b" } }), "github.release"),
        ] {
            let r = ingest_of(normalize(&webhook_request(&args("github", Some(event), &payload)).unwrap()));
            assert_eq!(r.items[0].kind, kind, "{event}");
        }
        let merged = ingest_of(normalize(&webhook_request(&args("github", Some("pull_request"), &json!({ "action": "closed", "pull_request": { "number": 8, "title": "Add X", "merged": true }, "repository": { "full_name": "a/b" }, "sender": { "login": "o" } }))).unwrap()));
        assert!(merged.items[0].content.starts_with("o merged pull request a/b#8"));
    }

    #[test]
    fn github_other_events_and_actions_are_ignored() {
        for (event, payload) in [
            ("ping", json!({ "zen": "Keep it logically awesome." })),
            ("star", json!({ "action": "created" })),
            ("issues", json!({ "action": "labeled", "issue": { "number": 1 } })),
            ("push", json!({ "ref": "refs/heads/main", "commits": [] })),
        ] {
            assert_eq!(normalize(&webhook_request(&args("github", Some(event), &payload)).unwrap()), Normalized::Ignored("nothing_to_store"), "{event}");
        }
    }

    #[test]
    fn linear_issue_comment_and_project_update() {
        let issue = json!({ "action": "create", "type": "Issue", "url": "https://linear.app/acme/issue/ENG-12/x", "webhookTimestamp": 1_700_000_100_000_i64, "actor": { "name": "Ana" }, "data": { "identifier": "ENG-12", "title": "Fix billing", "description": "Off by one", "state": { "name": "Todo" } } });
        let r = ingest_of(normalize(&webhook_request(&args("linear", Some("Issue"), &issue)).unwrap()));
        assert_eq!(r.items[0].kind, "linear.issue");
        assert_eq!(r.items[0].ts_ms, Some(1_700_000_100_000));
        assert!(r.items[0].content.starts_with("Ana created Linear issue ENG-12 \"Fix billing\" (state: Todo)."));
        assert!(r.items[0].content.ends_with("(via Linear webhook, delivery d-1, https://linear.app/acme/issue/ENG-12/x)"));
        assert_eq!(r.thread_key.as_deref(), Some("linear:ENG-12"));

        let comment = json!({ "action": "create", "type": "Comment", "url": "https://linear.app/acme/issue/ENG-12#c", "data": { "body": "Shipping Friday", "issue": { "identifier": "ENG-12", "title": "Fix billing" } } });
        let r = ingest_of(normalize(&webhook_request(&args("linear", Some("Comment"), &comment)).unwrap()));
        assert!(r.items[0].content.contains("Shipping Friday"));

        let update = json!({ "action": "create", "type": "ProjectUpdate", "data": { "body": "On track", "health": "onTrack", "project": { "name": "Q4 launch" } } });
        let r = ingest_of(normalize(&webhook_request(&args("linear", None, &update)).unwrap()));
        assert_eq!(r.items[0].kind, "linear.project_update");

        let removed = json!({ "action": "remove", "type": "Issue", "data": { "identifier": "ENG-1" } });
        assert!(matches!(normalize(&webhook_request(&args("linear", None, &removed)).unwrap()), Normalized::Ignored(_)));
        let other = json!({ "action": "create", "type": "Reaction", "data": {} });
        assert!(matches!(normalize(&webhook_request(&args("linear", None, &other)).unwrap()), Normalized::Ignored(_)));
    }

    #[test]
    fn slack_message_is_kept_bots_edits_and_handshakes_are_not() {
        let msg = json!({ "type": "event_callback", "team_id": "T1", "event_id": "Ev1", "event": { "type": "message", "user": "U1", "channel": "C1", "text": "Can you send the deck by Friday?", "ts": "1700000000.000200" } });
        let r = ingest_of(normalize(&webhook_request(&args("slack", None, &msg)).unwrap()));
        let item = &r.items[0];
        assert_eq!(item.kind, "slack.message");
        assert_eq!(item.ts_ms, Some(1_700_000_000_000));
        assert!(item.content.starts_with("<@U1> wrote in <#C1>:\n\nCan you send the deck by Friday?"));
        assert!(item.content.ends_with("(via Slack webhook, delivery d-1, https://app.slack.com/client/T1/C1)"));
        assert_eq!(r.thread_key.as_deref(), Some("slack:T1/C1/1700000000.000200"));

        let bot = json!({ "type": "event_callback", "event": { "type": "message", "bot_id": "B1", "text": "deploy ok", "channel": "C1", "ts": "1.0" } });
        assert_eq!(normalize(&webhook_request(&args("slack", None, &bot)).unwrap()), Normalized::Ignored("bot_message"));
        let edit = json!({ "type": "event_callback", "event": { "type": "message", "subtype": "message_changed", "channel": "C1" } });
        assert_eq!(normalize(&webhook_request(&args("slack", None, &edit)).unwrap()), Normalized::Ignored("unsupported_event"));
        let challenge = json!({ "type": "url_verification", "challenge": "abc", "token": "t" });
        assert_eq!(normalize(&webhook_request(&args("slack", None, &challenge)).unwrap()), Normalized::Ignored("handshake"));
    }

    #[test]
    fn notion_calendar_and_gmail_are_change_notices() {
        let page = json!({ "id": "evt-1", "type": "page.content_updated", "entity": { "id": "1a2b3c4d-0000-1111-2222-333344445555", "type": "page" }, "data": {} });
        match normalize(&webhook_request(&args("notion", None, &page)).unwrap()) {
            Normalized::Changed(n) => {
                assert_eq!(n.resource, "page");
                assert_eq!(n.detail["entity_id"], "1a2b3c4d-0000-1111-2222-333344445555");
                assert_eq!(n.detail["url"], "https://www.notion.so/1a2b3c4d000011112222333344445555");
            }
            other => panic!("{other:?}"),
        }
        let verify = json!({ "verification_token": "secret_x" });
        assert_eq!(normalize(&webhook_request(&args("notion", None, &verify)).unwrap()), Normalized::Ignored("handshake"));

        let cal = json!({ "provider": "google_calendar", "delivery_id": "hk_1:5", "idempotency_key": "hook:google_calendar:hk_1:5", "changed": { "channel_id": "hk_1", "resource_id": "r-9", "resource_state": "exists", "message_number": "5" } });
        match normalize(&webhook_request(&cal).unwrap()) {
            Normalized::Changed(n) => assert_eq!((n.resource.as_str(), n.detail["resource_id"].as_str()), ("calendar", Some("r-9"))),
            other => panic!("{other:?}"),
        }
        let mut sync = cal.clone();
        sync["changed"]["resource_state"] = json!("sync");
        assert_eq!(normalize(&webhook_request(&sync).unwrap()), Normalized::Ignored("handshake"));

        let gmail = json!({ "provider": "gmail", "delivery_id": "m-1", "idempotency_key": "hook:gmail:m-1", "changed": { "history_id": "12345", "message_id": "m-1" } });
        match normalize(&webhook_request(&gmail).unwrap()) {
            Normalized::Changed(n) => assert_eq!((n.resource.as_str(), n.detail["history_id"].as_str()), ("mailbox", Some("12345"))),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn arguments_are_validated() {
        let bad = |a: Value| webhook_request(&a).unwrap_err();
        assert!(bad(json!({ "provider": "jira", "delivery_id": "d", "idempotency_key": "k" })).starts_with("invalid_provider"));
        assert!(bad(json!({ "provider": "github", "delivery_id": "d", "payload_b64": "e30=" })).starts_with("missing_idempotency_key"));
        assert!(bad(json!({ "provider": "github", "idempotency_key": "k", "payload_b64": "e30=" })).starts_with("missing_delivery_id"));
        assert!(bad(json!({ "provider": "github", "delivery_id": "d", "idempotency_key": "k" })).starts_with("missing_payload_b64"));
        assert!(bad(json!({ "provider": "github", "delivery_id": "d", "idempotency_key": "k", "payload_b64": "!!" })).starts_with("invalid_payload_b64"));
        assert!(bad(json!({ "provider": "github", "delivery_id": "d", "idempotency_key": "k", "payload_b64": "bm90IGpzb24=" })).starts_with("invalid_payload_b64"));
        assert!(bad(json!({ "provider": "gmail", "delivery_id": "d", "idempotency_key": "k" })).starts_with("missing_changed"));
        assert!(bad(json!({ "provider": "gmail", "delivery_id": "d", "idempotency_key": "k", "changed": { "history_id": 5 } })).starts_with("invalid_changed"));
        assert!(bad(json!({ "provider": "github", "delivery_id": "d e", "idempotency_key": "k", "payload_b64": "e30=" })).starts_with("invalid_delivery_id"));
        let huge = "A".repeat(MAX_PAYLOAD_BYTES / 3 * 4 + 400);
        assert!(bad(json!({ "provider": "github", "delivery_id": "d", "idempotency_key": "k", "payload_b64": huge })).starts_with("invalid_payload_b64"));
    }

    #[test]
    fn long_bodies_are_cut_and_the_footer_survives() {
        let mut p = github_issue();
        p["issue"]["body"] = Value::String("x".repeat(MAX_ITEM_CHARS * 2));
        let r = ingest_of(normalize(&webhook_request(&args("github", Some("issues"), &p)).unwrap()));
        let c = &r.items[0].content;
        assert!(c.chars().count() <= MAX_ITEM_CHARS);
        assert!(c.ends_with("https://github.com/acme/web/issues/42)"));
    }

    #[test]
    fn serve_renders_the_ingest_receipt_under_its_own_name() {
        use crate::backend::{MemoryBackend, ReadItem, ReadParams};
        use std::sync::Mutex;
        struct Recording(Mutex<Vec<IngestRequest>>);
        impl MemoryBackend for Recording {
            fn read(&self, _: Tool, _: &ReadParams) -> Vec<ReadItem> {
                Vec::new()
            }
            fn ingest(&self, request: &IngestRequest) -> Result<crate::ingest_api::IngestOutcome, String> {
                self.0.lock().unwrap().push(request.clone());
                Ok(crate::ingest_api::IngestOutcome { event_ids: vec![9], inserted: 1, ..Default::default() })
            }
        }
        let backend = Recording(Mutex::new(Vec::new()));
        let value: Value = serde_json::from_str(&serve(&args("github", Some("issues"), &github_issue()), &backend).unwrap()).unwrap();
        assert_eq!(value["tool"], "memory.ingest_webhook");
        assert_eq!(value["result"]["outcome"], "ingested");
        assert_eq!(value["result"]["provider"], "github");
        assert_eq!(value["result"]["delivery_id"], "d-1");
        assert_eq!(value["result"]["metadata"]["url"], "https://github.com/acme/web/issues/42");
        assert_eq!(value["usage"]["quantity"], 1);
        let seen = backend.0.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].source, "github-webhook");
    }

    #[test]
    fn serve_stores_through_ingest_and_reports_usage() {
        let backend = crate::backend::StubBackend;
        let stored = serve(&args("github", Some("issues"), &github_issue()), &backend);
        // The stub backend has no store: the ingest path answers unavailable, not a bad request.
        assert_eq!(stored, Err(Refusal::Unavailable("ingest_unavailable")));
        let ignored: Value = serde_json::from_str(&serve(&args("github", Some("ping"), &json!({})), &backend).unwrap()).unwrap();
        assert_eq!(ignored["tool"], "memory.ingest_webhook");
        assert_eq!(ignored["result"]["outcome"], "ignored");
        assert_eq!(ignored["usage"]["quantity"], 0);
        let gmail = json!({ "provider": "gmail", "delivery_id": "m-1", "idempotency_key": "hook:gmail:m-1", "changed": { "history_id": "12345" } });
        let changed: Value = serde_json::from_str(&serve(&gmail, &backend).unwrap()).unwrap();
        assert_eq!(changed["result"]["outcome"], "changed");
        assert_eq!(changed["result"]["fetch"], "connector_sync");
        assert_eq!(changed["usage"]["quantity"], 0);
    }
}
