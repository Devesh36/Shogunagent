//! Connector read-sync (ShogunAgent E44): fetch the content a provider's "changed" notice points
//! at, with the principal's own connector credentials, and store it as text events with
//! provenance through the `memory.ingest` path.
//!
//! Notion, Google Calendar and Gmail webhooks carry ids, not content. `memory.ingest_webhook`
//! acknowledges such a notice and queues it here ([`enqueue`]) in the principal's own sealed
//! store ([`CredentialSlot::SyncQueue`]); it does not fetch inline, because a fetch is one to
//! eleven vendor round trips (and a refresh) and the hook's request must answer within the
//! provider's webhook timeout. The background tick (`shogun-api --dream-tick`, hourly, before the
//! Dream Cycle review so the fetched events are reviewed in the same run) calls [`drain`] for each
//! principal.
//!
//! What a drain does, per queued notice (at most [`MAX_JOBS_PER_TICK`] per principal per tick):
//!
//! | provider | fetches | stores |
//! |---|---|---|
//! | `gmail` | the [`MAX_OBJECTS_PER_NOTICE`] newest messages of the last two days (`messages.list`, then `messages.get?format=full`) | one `gmail.message` event per message: sender, recipients, subject and the `text/plain` parts; drafts, spam and trash skipped |
//! | `google_calendar` | the events of the primary calendar updated since shortly before the notice (`events.list?updatedMin=…`) | one `calendar.event` per event: title, time, place, attendee addresses, description; cancelled ones skipped |
//! | `notion` | the page (`pages/{id}`) and its first 100 blocks (`blocks/{id}/children`) | one `notion.page` event: the title and the text of its text blocks |
//!
//! Text only: attachments, images, files and embeds are never fetched (a part with a file name or
//! an attachment id, and every non-text block, is skipped). Every event names its provider, the
//! object's id and URL (in its closing line and the receipt metadata, invariant 5), and is ingested
//! under `sync:<provider>:<hash(object id, version)>`, so the same object at the same version is
//! stored once however often it is fetched. A newer version (Gmail's `historyId`, Calendar's and
//! Notion's last-edited time) with changed text is a new event even for a one-character edit;
//! unchanged text still exact-deduplicates in its thread. The credential comes from the vault
//! ([`HostedConnectors::access_token`]: refreshed through the single egress when needed), every GET
//! goes through the core's HTTP client behind [`ProviderHttp`] (the single egress; a fake in
//! tests), and each one writes an audit row (action `sync`, the vendor host, the outcome).
//! Snapshot receipts are separate from old generic-ingest receipts, so refetching a previously
//! collapsed version can recover its text. This does not schedule a historical backfill.
//!
//! Failures: no grant (or a revoked one) drops the notice and counts it `not_connected`; a vendor
//! or store that did not answer re-queues it with a back-off, at most [`MAX_ATTEMPTS`] times; an
//! object that is gone is dropped. Nothing here logs content.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use shogun_mcp::backend::MemoryBackend;
use shogun_mcp::connector_vault::{ConnectorVault, SyncQueued, VaultError};
use shogun_mcp::ingest_api::{IngestItem, IngestRequest, Namespace, MAX_ITEM_CHARS, MAX_KIND_CHARS, MAX_THREAD_KEY_CHARS, MAX_TITLE_CHARS};
use shogun_mcp::scope::Service;

use crate::hosted_connect::{AuditEvent, HostedConnectors};
use crate::oauth::TokenExchange;
use crate::token_store::{CredentialSlot, Secret};

/// Notices one principal keeps waiting. Past it the oldest is dropped (a later notice of the same
/// mailbox or calendar covers it; a Notion page edited again sends another).
pub const MAX_QUEUE: usize = 64;
/// Notices one principal's drain handles per tick.
pub const MAX_JOBS_PER_TICK: usize = 16;
/// Objects one notice fetches at most.
pub const MAX_OBJECTS_PER_NOTICE: usize = 10;
/// Tries before a notice that keeps failing is dropped.
pub const MAX_ATTEMPTS: u32 = 3;
/// The back-off of a re-queued notice, per attempt.
pub const RETRY_AFTER_MS: i64 = 5 * 60_000;
/// How far before the notice a Calendar fetch looks (a notice can trail its change).
pub const CALENDAR_LOOKBACK_MS: i64 = 15 * 60_000;
/// The Notion API version the page and block reads speak.
pub const NOTION_VERSION: &str = "2022-06-28";

/// The providers whose notices are fetched here, as `memory.ingest_webhook` names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncProvider {
    Gmail,
    GoogleCalendar,
    Notion,
}

impl SyncProvider {
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "gmail" => Self::Gmail,
            "google_calendar" => Self::GoogleCalendar,
            "notion" => Self::Notion,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gmail => "gmail",
            Self::GoogleCalendar => "google_calendar",
            Self::Notion => "notion",
        }
    }

    /// The connector service whose grant opens it.
    pub fn service(self) -> Service {
        match self {
            Self::Gmail => Service::Gmail,
            Self::GoogleCalendar => Service::GoogleCalendar,
            Self::Notion => Service::Notion,
        }
    }

    /// The vault's provider id (the audit rows name it).
    fn provider_id(self) -> &'static str {
        match self {
            Self::Gmail => "gmail",
            Self::GoogleCalendar => "calendar",
            Self::Notion => "notion",
        }
    }

    fn host(self) -> &'static str {
        match self {
            Self::Gmail => "gmail.googleapis.com",
            Self::GoogleCalendar => "www.googleapis.com",
            Self::Notion => "api.notion.com",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Gmail => "Gmail",
            Self::GoogleCalendar => "Google Calendar",
            Self::Notion => "Notion",
        }
    }
}

/// One queued notice. Ids only (the `changed` detail `memory.ingest_webhook` parsed), never
/// content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncJob {
    pub provider: String,
    /// What coalesces: one mailbox, one calendar, one Notion page.
    pub key: String,
    pub detail: Value,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub not_before_ms: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct Queue {
    #[serde(default)]
    jobs: Vec<SyncJob>,
}

fn id_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The coalescing key of a notice, or `None` when it names nothing the read-sync fetches (a
/// Notion database or comment, a malformed id).
pub fn job_key(provider: SyncProvider, detail: &Value) -> Option<String> {
    let text = |k: &str| detail.get(k).and_then(Value::as_str).filter(|s| id_ok(s));
    match provider {
        SyncProvider::Gmail => Some("gmail:mailbox".into()),
        SyncProvider::GoogleCalendar => Some(format!("calendar:{}", text("resource_id").or_else(|| text("channel_id")).unwrap_or("primary"))),
        SyncProvider::Notion => {
            if detail.get("entity_type").and_then(Value::as_str).unwrap_or("page") != "page" {
                return None;
            }
            text("entity_id").map(|id| format!("notion:{id}"))
        }
    }
}

/// Queue one notice for `principal`. A notice already waiting with the same key is refreshed, not
/// added twice. `UnknownSlot` when the provider or the object is not one the read-sync fetches.
pub fn enqueue<X: TokenExchange + Send + Sync>(vault: &HostedConnectors<X>, principal: &str, provider: &str, detail: &Value, now_ms: i64) -> Result<SyncQueued, VaultError> {
    let provider = SyncProvider::parse(provider).ok_or(VaultError::UnknownSlot)?;
    let key = job_key(provider, detail).ok_or(VaultError::UnknownSlot)?;
    vault.modify_json(principal, CredentialSlot::SyncQueue, |queue: &mut Queue| {
        if let Some(job) = queue.jobs.iter_mut().find(|j| j.key == key) {
            job.last_seen_ms = now_ms;
            job.detail = detail.clone();
            return SyncQueued { added: false, depth: queue.jobs.len() };
        }
        queue.jobs.push(SyncJob {
            provider: provider.as_str().to_string(),
            key: key.clone(),
            detail: detail.clone(),
            first_seen_ms: now_ms,
            last_seen_ms: now_ms,
            attempts: 0,
            not_before_ms: 0,
        });
        while queue.jobs.len() > MAX_QUEUE {
            queue.jobs.remove(0);
        }
        SyncQueued { added: true, depth: queue.jobs.len() }
    })
}

/// The notices waiting for `principal` (for tests and the operator's view; ids only).
pub fn pending<X: TokenExchange + Send + Sync>(vault: &HostedConnectors<X>, principal: &str) -> Result<Vec<SyncJob>, VaultError> {
    vault.modify_json(principal, CredentialSlot::SyncQueue, |queue: &mut Queue| queue.jobs.clone())
}

/// Why a GET did not answer with a body. Fixed words: never a body or a URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchError {
    /// 401 / 403: the grant no longer opens the object.
    Unauthorized,
    /// 404 / 410: the object is gone.
    NotFound,
    /// No answer, 408, 429 or 5xx: try again later.
    Transient,
    /// Anything else (a refused request, a body too large or not text).
    Rejected,
}

/// The core's HTTP GET for the read-sync (the single egress). `bearer` goes into the
/// `Authorization` header only.
pub trait ProviderHttp: Send + Sync {
    fn get(&self, url: &str, bearer: &Secret, headers: &[(&'static str, &'static str)]) -> Result<String, FetchError>;
}

/// One fetched object before it becomes an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedObject {
    pub kind: &'static str,
    pub object_id: String,
    /// What makes a change a new event (Gmail `historyId`, Calendar `updated`, Notion `last_edited_time`).
    pub version: String,
    pub title: String,
    pub body: String,
    pub url: Option<String>,
    pub ts_ms: Option<i64>,
    pub thread: String,
}

fn cut(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn one_line(text: &str) -> String {
    text.split(|c: char| c.is_control()).filter(|s| !s.trim().is_empty()).map(str::trim).collect::<Vec<_>>().join(" ")
}

fn web_url(raw: Option<&str>) -> Option<String> {
    raw.filter(|u| u.starts_with("https://") && u.len() <= 2048 && !u.chars().any(char::is_whitespace)).map(str::to_string)
}

fn s<'a>(v: &'a Value, pointer: &str) -> Option<&'a str> {
    v.pointer(pointer).and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty())
}

/// The event `obj` becomes: provenance in the closing line and the metadata, an idempotency key
/// per object and version.
pub fn ingest_request(provider: SyncProvider, obj: &FetchedObject) -> IngestRequest {
    let footer = match &obj.url {
        Some(url) => format!("\n\n(via {} read-sync, {}, {url})", provider.label(), obj.object_id),
        None => format!("\n\n(via {} read-sync, {})", provider.label(), obj.object_id),
    };
    let room = MAX_ITEM_CHARS.saturating_sub(footer.chars().count());
    let digest = Sha256::digest(format!("{}\n{}", obj.object_id, obj.version).as_bytes());
    let digest: String = digest.iter().take(20).map(|b| format!("{b:02x}")).collect();
    let mut metadata = json!({ "provider": provider.as_str(), "object_id": obj.object_id, "version": obj.version, "fetch": "connector_sync" });
    if let Some(url) = &obj.url {
        metadata["url"] = Value::String(url.clone());
    }
    IngestRequest {
        items: vec![IngestItem {
            ts_ms: obj.ts_ms,
            kind: cut(obj.kind, MAX_KIND_CHARS),
            title: Some(cut(&one_line(&obj.title), MAX_TITLE_CHARS)),
            content: format!("{}{footer}", cut(obj.body.trim(), room)),
        }],
        turns: false,
        source: format!("{}-sync", provider.as_str()),
        actor: format!("sync:{}", provider.as_str()),
        thread_key: Some(cut(&obj.thread, MAX_THREAD_KEY_CHARS)),
        metadata: Some(metadata),
        idempotency_key: format!("sync:{}:{digest}", provider.as_str()),
        namespace: Namespace::World,
    }
}

// --- Gmail -------------------------------------------------------------------------------------

const GMAIL_MESSAGES: &str = "https://gmail.googleapis.com/gmail/v1/users/me/messages";
/// Characters of one message's text that are kept.
const MAX_MESSAGE_TEXT: usize = 12_000;

fn base64url(data: &str) -> Option<String> {
    use base64::Engine as _;
    let trimmed = data.trim_end_matches('=');
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(trimmed).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The `text/plain` parts of a Gmail payload, depth-first. A part with a file name or an
/// attachment id is an attachment and is never read.
fn plain_parts(part: &Value, out: &mut Vec<String>, depth: usize) {
    if depth > 8 || out.len() >= 8 {
        return;
    }
    let is_attachment = s(part, "/filename").is_some() || s(part, "/body/attachmentId").is_some();
    if !is_attachment && s(part, "/mimeType") == Some("text/plain") {
        if let Some(text) = s(part, "/body/data").and_then(base64url) {
            out.push(text);
        }
    }
    if let Some(parts) = part.get("parts").and_then(Value::as_array) {
        for child in parts {
            plain_parts(child, out, depth + 1);
        }
    }
}

/// One `messages.get?format=full` answer as an object, or `None` for a draft, spam, trash or a
/// malformed answer.
pub fn gmail_message(m: &Value) -> Option<FetchedObject> {
    let id = s(m, "/id").filter(|id| id_ok(id))?;
    let labels: Vec<&str> = m.get("labelIds").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    if labels.iter().any(|l| matches!(*l, "DRAFT" | "SPAM" | "TRASH")) {
        return None;
    }
    let header = |name: &str| {
        m.pointer("/payload/headers")
            .and_then(Value::as_array)?
            .iter()
            .find(|h| s(h, "/name").is_some_and(|n| n.eq_ignore_ascii_case(name)))
            .and_then(|h| s(h, "/value"))
            .map(one_line)
    };
    let subject = header("Subject").unwrap_or_else(|| "(no subject)".into());
    let mut parts = Vec::new();
    if let Some(payload) = m.get("payload") {
        plain_parts(payload, &mut parts, 0);
    }
    let text = if parts.is_empty() { s(m, "/snippet").unwrap_or("").to_string() } else { parts.join("\n\n") };
    let mut lines = Vec::new();
    for (label, name) in [("From", "From"), ("To", "To"), ("Cc", "Cc"), ("Date", "Date")] {
        if let Some(value) = header(name) {
            lines.push(format!("{label}: {}", cut(&value, 500)));
        }
    }
    lines.push(format!("Subject: {subject}"));
    let thread = s(m, "/threadId").filter(|t| id_ok(t)).unwrap_or(id);
    Some(FetchedObject {
        kind: "gmail.message",
        object_id: id.to_string(),
        version: s(m, "/historyId").unwrap_or("0").to_string(),
        title: format!("Email: {subject}"),
        body: format!("{}\n\n{}", lines.join("\n"), cut(text.trim(), MAX_MESSAGE_TEXT)),
        url: Some(format!("https://mail.google.com/mail/u/0/#all/{thread}")),
        ts_ms: s(m, "/internalDate").and_then(|d| d.parse::<i64>().ok()).filter(|ms| *ms > 0),
        thread: format!("gmail:{thread}"),
    })
}

fn gmail(http: &dyn ProviderHttp, token: &Secret) -> Result<Vec<FetchedObject>, FetchError> {
    let list = http.get(&format!("{GMAIL_MESSAGES}?maxResults={MAX_OBJECTS_PER_NOTICE}&q=newer_than%3A2d"), token, &[])?;
    let list: Value = serde_json::from_str(&list).map_err(|_| FetchError::Rejected)?;
    let ids: Vec<String> = list
        .get("messages")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|m| s(m, "/id")).filter(|id| id_ok(id)).take(MAX_OBJECTS_PER_NOTICE).map(str::to_string).collect())
        .unwrap_or_default();
    let mut out = Vec::new();
    for id in ids {
        match http.get(&format!("{GMAIL_MESSAGES}/{id}?format=full"), token, &[]) {
            Ok(body) => {
                if let Some(obj) = serde_json::from_str::<Value>(&body).ok().as_ref().and_then(gmail_message) {
                    out.push(obj);
                }
            }
            // Deleted between the list and the read.
            Err(FetchError::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

// --- Google Calendar ----------------------------------------------------------------------------

const CALENDAR_EVENTS: &str = "https://www.googleapis.com/calendar/v3/calendars/primary/events";

/// Unix ms as RFC 3339 UTC (`2026-10-25T09:30:00Z`).
pub fn rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// One Calendar event resource as an object, or `None` for a cancelled or malformed one.
pub fn calendar_event(e: &Value) -> Option<FetchedObject> {
    let id = s(e, "/id").filter(|id| id_ok(id))?;
    if s(e, "/status") == Some("cancelled") {
        return None;
    }
    let when = |k: &str| s(e, &format!("/{k}/dateTime")).or_else(|| s(e, &format!("/{k}/date"))).map(str::to_string);
    let title = s(e, "/summary").map(one_line).unwrap_or_else(|| "(no title)".into());
    let mut lines = vec![title.clone()];
    match (when("start"), when("end")) {
        (Some(start), Some(end)) => lines.push(format!("When: {start} to {end}")),
        (Some(start), None) => lines.push(format!("When: {start}")),
        _ => {}
    }
    if let Some(place) = s(e, "/location") {
        lines.push(format!("Where: {}", one_line(place)));
    }
    let attendees: Vec<&str> = e.get("attendees").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| s(x, "/email")).take(30).collect()).unwrap_or_default();
    if !attendees.is_empty() {
        lines.push(format!("Attendees: {}", attendees.join(", ")));
    }
    if let Some(organizer) = s(e, "/organizer/email") {
        lines.push(format!("Organizer: {organizer}"));
    }
    if let Some(description) = s(e, "/description") {
        lines.push(String::new());
        lines.push(cut(description, 8_000));
    }
    let updated = s(e, "/updated").unwrap_or("0").to_string();
    Some(FetchedObject {
        kind: "calendar.event",
        object_id: id.to_string(),
        ts_ms: crate::result::rfc3339_to_ms(&updated),
        version: updated,
        title: format!("Calendar: {title}"),
        body: lines.join("\n"),
        url: web_url(s(e, "/htmlLink")),
        thread: format!("calendar:{id}"),
    })
}

fn calendar(http: &dyn ProviderHttp, token: &Secret, job: &SyncJob) -> Result<Vec<FetchedObject>, FetchError> {
    let since = rfc3339(job.first_seen_ms.saturating_sub(CALENDAR_LOOKBACK_MS));
    let url = format!(
        "{CALENDAR_EVENTS}?maxResults={MAX_OBJECTS_PER_NOTICE}&orderBy=updated&showDeleted=false&singleEvents=true&updatedMin={}",
        since.replace(':', "%3A")
    );
    let body: Value = serde_json::from_str(&http.get(&url, token, &[])?).map_err(|_| FetchError::Rejected)?;
    Ok(body
        .get("items")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(calendar_event).take(MAX_OBJECTS_PER_NOTICE).collect())
        .unwrap_or_default())
}

// --- Notion -------------------------------------------------------------------------------------

const NOTION_API: &str = "https://api.notion.com/v1";
const NOTION_HEADERS: &[(&str, &str)] = &[("Notion-Version", NOTION_VERSION)];

fn rich_text(v: Option<&Value>) -> String {
    v.and_then(Value::as_array)
        .map(|parts| parts.iter().filter_map(|p| p.get("plain_text").and_then(Value::as_str)).collect::<String>())
        .unwrap_or_default()
}

/// The text of one block, or `None` for a block that is not text (an image, a file, an embed…).
fn block_text(block: &Value) -> Option<String> {
    let kind = s(block, "/type")?;
    let prefix = match kind {
        "paragraph" | "quote" | "callout" | "toggle" | "code" => "",
        "heading_1" => "# ",
        "heading_2" => "## ",
        "heading_3" => "### ",
        "bulleted_list_item" => "- ",
        "numbered_list_item" => "1. ",
        "to_do" => {
            if block.pointer("/to_do/checked").and_then(Value::as_bool) == Some(true) {
                "[x] "
            } else {
                "[ ] "
            }
        }
        _ => return None,
    };
    let text = rich_text(block.pointer(&format!("/{kind}/rich_text")));
    (!text.trim().is_empty()).then(|| format!("{prefix}{}", text.trim()))
}

/// A Notion page and its first blocks as an object, or `None` for an archived page.
pub fn notion_page(page: &Value, blocks: &Value) -> Option<FetchedObject> {
    let id = s(page, "/id").filter(|id| id_ok(id))?;
    if page.get("archived").and_then(Value::as_bool) == Some(true) || page.get("in_trash").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let title = page
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|props| props.values().find(|p| s(p, "/type") == Some("title")))
        .map(|p| rich_text(p.get("title")))
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| "(untitled)".into());
    let lines: Vec<String> = blocks.get("results").and_then(Value::as_array).map(|b| b.iter().filter_map(block_text).collect()).unwrap_or_default();
    let edited = s(page, "/last_edited_time").unwrap_or("0").to_string();
    Some(FetchedObject {
        kind: "notion.page",
        object_id: id.to_string(),
        ts_ms: crate::result::rfc3339_to_ms(&edited),
        version: edited,
        title: format!("Notion: {}", one_line(&title)),
        body: format!("{}\n\n{}", one_line(&title), lines.join("\n")),
        url: web_url(s(page, "/url")),
        thread: format!("notion:{id}"),
    })
}

fn notion(http: &dyn ProviderHttp, token: &Secret, job: &SyncJob) -> Result<Vec<FetchedObject>, FetchError> {
    let id = job.detail.get("entity_id").and_then(Value::as_str).filter(|id| id_ok(id)).ok_or(FetchError::Rejected)?;
    let page: Value = serde_json::from_str(&http.get(&format!("{NOTION_API}/pages/{id}"), token, NOTION_HEADERS)?).map_err(|_| FetchError::Rejected)?;
    let blocks: Value = match http.get(&format!("{NOTION_API}/blocks/{id}/children?page_size=100"), token, NOTION_HEADERS) {
        Ok(body) => serde_json::from_str(&body).map_err(|_| FetchError::Rejected)?,
        // A page whose blocks the integration cannot read still has a title.
        Err(FetchError::NotFound) => Value::Null,
        Err(e) => return Err(e),
    };
    Ok(notion_page(&page, &blocks).into_iter().collect())
}

/// Fetch what `job` points at.
pub fn fetch(provider: SyncProvider, http: &dyn ProviderHttp, token: &Secret, job: &SyncJob) -> Result<Vec<FetchedObject>, FetchError> {
    match provider {
        SyncProvider::Gmail => gmail(http, token),
        SyncProvider::GoogleCalendar => calendar(http, token, job),
        SyncProvider::Notion => notion(http, token, job),
    }
}

/// Every GET of a drain, audited (action `sync`, the vendor host, the outcome; never the URL).
struct Audited<'a, X> {
    inner: &'a dyn ProviderHttp,
    vault: &'a HostedConnectors<X>,
    principal: &'a str,
    provider: SyncProvider,
}

impl<X: TokenExchange + Send + Sync> ProviderHttp for Audited<'_, X> {
    fn get(&self, url: &str, bearer: &Secret, headers: &[(&'static str, &'static str)]) -> Result<String, FetchError> {
        let answer = self.inner.get(url, bearer, headers);
        let outcome = match &answer {
            Ok(_) => "ok",
            Err(FetchError::Unauthorized) => "rejected",
            Err(FetchError::Transient) => "unavailable",
            Err(_) => "failed",
        };
        self.vault.audit(self.principal, AuditEvent { action: "sync", provider: self.provider.provider_id(), outcome, destination: self.provider.host() });
        answer
    }
}

/// What one principal's drain did. Counts only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrainReport {
    /// Notices taken from the queue.
    pub jobs: usize,
    /// Objects fetched.
    pub objects: usize,
    /// Events that became new rows (a replayed object version adds none).
    pub stored: usize,
    /// Notices dropped because no usable grant is connected.
    pub not_connected: usize,
    /// Notices put back for a later tick.
    pub retried: usize,
    /// Notices dropped after their last attempt, or for an object that is gone.
    pub dropped: usize,
}

enum JobOutcome {
    Done { objects: usize, stored: usize },
    NotConnected,
    Retry,
    Drop,
}

fn run_job<X: TokenExchange + Send + Sync>(vault: &HostedConnectors<X>, http: &dyn ProviderHttp, backend: &dyn MemoryBackend, principal: &str, job: &SyncJob, now_ms: i64) -> JobOutcome {
    let Some(provider) = SyncProvider::parse(&job.provider) else {
        return JobOutcome::Drop;
    };
    let slot = CredentialSlot::Connector(provider.service()).account();
    let token = match vault.access_token(principal, &slot, now_ms) {
        Ok(issued) => issued.access_token,
        Err(VaultError::Unavailable) => return JobOutcome::Retry,
        Err(_) => return JobOutcome::NotConnected,
    };
    let audited = Audited { inner: http, vault, principal, provider };
    let objects = match fetch(provider, &audited, &token, job) {
        Ok(objects) => objects,
        Err(FetchError::Unauthorized) => return JobOutcome::NotConnected,
        Err(FetchError::Transient) => return JobOutcome::Retry,
        Err(FetchError::NotFound | FetchError::Rejected) => return JobOutcome::Drop,
    };
    let mut stored = 0;
    for obj in &objects {
        // Provider versions already identify retries. Similarity must not erase a small
        // factual edit in the fetched snapshot; exact content dedup remains in the store.
        match backend.ingest_versioned_snapshot(&ingest_request(provider, obj)) {
            Ok(outcome) => stored += outcome.inserted,
            // The store did not take it: the same notice runs again (ingest is idempotent).
            Err(_) => return JobOutcome::Retry,
        }
    }
    JobOutcome::Done { objects: objects.len(), stored }
}

/// Fetch and store the notices waiting for `principal` (at most [`MAX_JOBS_PER_TICK`]) into
/// `backend` (the principal's own store). Blocking: network calls.
pub fn drain<X: TokenExchange + Send + Sync>(vault: &HostedConnectors<X>, http: &dyn ProviderHttp, backend: &dyn MemoryBackend, principal: &str, now_ms: i64) -> Result<DrainReport, VaultError> {
    let taken = vault.modify_json(principal, CredentialSlot::SyncQueue, |queue: &mut Queue| {
        let mut taken = Vec::new();
        queue.jobs.retain(|job| {
            if taken.len() < MAX_JOBS_PER_TICK && job.not_before_ms <= now_ms {
                taken.push(job.clone());
                false
            } else {
                true
            }
        });
        taken
    })?;
    let mut report = DrainReport { jobs: taken.len(), ..DrainReport::default() };
    let mut retry = Vec::new();
    for job in taken {
        match run_job(vault, http, backend, principal, &job, now_ms) {
            JobOutcome::Done { objects, stored } => {
                report.objects += objects;
                report.stored += stored;
            }
            JobOutcome::NotConnected => report.not_connected += 1,
            JobOutcome::Drop => report.dropped += 1,
            JobOutcome::Retry if job.attempts + 1 >= MAX_ATTEMPTS => report.dropped += 1,
            JobOutcome::Retry => {
                report.retried += 1;
                let attempts = job.attempts + 1;
                retry.push(SyncJob { attempts, not_before_ms: now_ms.saturating_add(RETRY_AFTER_MS.saturating_mul(i64::from(attempts))), ..job });
            }
        }
    }
    if !retry.is_empty() {
        vault.modify_json(principal, CredentialSlot::SyncQueue, |queue: &mut Queue| {
            for job in retry {
                // A notice that arrived meanwhile for the same object supersedes the retry.
                if !queue.jobs.iter().any(|j| j.key == job.key) {
                    queue.jobs.push(job);
                }
            }
            while queue.jobs.len() > MAX_QUEUE {
                queue.jobs.remove(0);
            }
        })?;
    }
    Ok(report)
}

/// Whether any notice waits for `principal` (a tick skips the rest when none does).
pub fn has_pending<X: TokenExchange + Send + Sync>(vault: &HostedConnectors<X>, principal: &str) -> bool {
    vault.tokens().get(principal, CredentialSlot::SyncQueue).ok().flatten().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosted_connect::HostedClients;
    use crate::oauth::TokenSet;
    use crate::token::GrantStore;
    use crate::token_store::{MemorySecretStore, PrincipalGrants, TokenStore};
    use shogun_mcp::backend::{ReadItem, ReadParams};
    use shogun_mcp::ingest_api::IngestOutcome;
    use shogun_mcp::memory_api::Tool;
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};

    const NOW: i64 = 1_800_000_000_000;

    struct NoExchange;
    impl TokenExchange for NoExchange {
        fn post_form(&self, _: &str, _: &[(String, String)]) -> Result<String, String> {
            Err("token exchange failed: none".into())
        }
    }

    /// One GET the fake received: the URL, the bearer, the extra headers.
    type Call = (String, Secret, Vec<(&'static str, &'static str)>);

    /// Fake vendors behind the transport seam: URL prefix → answer.
    #[derive(Default)]
    struct FakeHttp {
        answers: Mutex<HashMap<String, Result<String, FetchError>>>,
        calls: Mutex<Vec<Call>>,
    }

    impl FakeHttp {
        fn on(&self, url: &str, answer: Result<Value, FetchError>) {
            self.answers.lock().unwrap().insert(url.to_string(), answer.map(|v| v.to_string()));
        }
    }

    impl ProviderHttp for FakeHttp {
        fn get(&self, url: &str, bearer: &Secret, headers: &[(&'static str, &'static str)]) -> Result<String, FetchError> {
            self.calls.lock().unwrap().push((url.to_string(), bearer.clone(), headers.to_vec()));
            let answers = self.answers.lock().unwrap();
            let base = url.split('?').next().unwrap_or(url);
            answers.get(url).or_else(|| answers.get(base)).cloned().unwrap_or(Err(FetchError::NotFound))
        }
    }

    /// A store that keeps what is ingested and replays a key it has seen.
    #[derive(Default)]
    struct Store {
        requests: Mutex<Vec<IngestRequest>>,
        keys: Mutex<HashSet<String>>,
    }

    impl MemoryBackend for Store {
        fn read(&self, _: Tool, _: &ReadParams) -> Vec<ReadItem> {
            Vec::new()
        }
        fn ingest_versioned_snapshot(&self, request: &IngestRequest) -> Result<IngestOutcome, String> {
            let fresh = self.keys.lock().unwrap().insert(request.idempotency_key.clone());
            self.requests.lock().unwrap().push(request.clone());
            Ok(IngestOutcome { inserted: usize::from(fresh), replayed: !fresh, ..Default::default() })
        }
    }

    fn setup() -> (Arc<MemorySecretStore>, HostedConnectors<NoExchange>) {
        let store = Arc::new(MemorySecretStore::new());
        let vault = HostedConnectors::new(store.clone(), HostedClients::default(), || Ok(NoExchange));
        (store, vault)
    }

    fn connect(store: &Arc<MemorySecretStore>, principal: &str, service: Service, token: &str) {
        let tokens = TokenSet { access_token: token.into(), refresh_token: None, expires_at_ms: NOW + 3_600_000, account_label: None };
        PrincipalGrants::new(store.as_ref(), principal).save(service, &tokens).unwrap();
    }

    fn b64(text: &str) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text)
    }

    fn gmail_fixture(http: &FakeHttp) {
        http.on(GMAIL_MESSAGES, Ok(json!({ "messages": [{ "id": "m1" }, { "id": "m2" }, { "id": "d1" }, { "id": "../x" }] })));
        http.on(&format!("{GMAIL_MESSAGES}/m1"), Ok(json!({
            "id": "m1", "threadId": "t1", "historyId": "901", "internalDate": "1799999990000", "labelIds": ["INBOX"],
            "payload": { "mimeType": "multipart/mixed", "headers": [{ "name": "Subject", "value": "Deck by Friday?" }, { "name": "From", "value": "Mika <mika@example.com>" }, { "name": "To", "value": "me@example.com" }],
                "parts": [
                    { "mimeType": "text/plain", "body": { "data": b64("Can you send the deck by Friday?") } },
                    { "mimeType": "image/png", "filename": "chart.png", "body": { "attachmentId": "att1", "data": b64("PNGBYTES") } },
                    { "mimeType": "text/plain", "filename": "notes.txt", "body": { "attachmentId": "att2" } }
                ] }
        })));
        http.on(&format!("{GMAIL_MESSAGES}/m2"), Ok(json!({ "id": "m2", "threadId": "t2", "historyId": "902", "labelIds": ["INBOX"], "snippet": "Lunch moved to 1pm", "payload": { "headers": [{ "name": "Subject", "value": "Lunch" }] } })));
        http.on(&format!("{GMAIL_MESSAGES}/d1"), Ok(json!({ "id": "d1", "labelIds": ["DRAFT"], "snippet": "unsent" })));
    }

    #[test]
    fn a_gmail_notice_is_queued_coalesced_and_drained_into_text_events() {
        let (store, vault) = setup();
        connect(&store, "prn_a", Service::Gmail, "ya29.alice-token");
        let detail = json!({ "history_id": "900" });
        assert_eq!(enqueue(&vault, "prn_a", "gmail", &detail, NOW).unwrap(), SyncQueued { added: true, depth: 1 });
        // A burst of notices for one mailbox is one fetch.
        assert_eq!(enqueue(&vault, "prn_a", "gmail", &json!({ "history_id": "905" }), NOW + 1).unwrap(), SyncQueued { added: false, depth: 1 });
        // The queue holds ids only, sealed in the principal's store.
        let queued = pending(&vault, "prn_a").unwrap();
        assert_eq!((queued.len(), queued[0].detail["history_id"].as_str()), (1, Some("905")));
        assert!(store.get("prn_a", CredentialSlot::SyncQueue).unwrap().is_some());

        let http = FakeHttp::default();
        gmail_fixture(&http);
        let backend = Store::default();
        let events: Arc<Mutex<Vec<AuditEvent>>> = Arc::default();
        let sink = events.clone();
        let vault = vault.with_audit(Arc::new(move |_: &str, e: &AuditEvent| sink.lock().unwrap().push(*e)));
        let report = drain(&vault, &http, &backend, "prn_a", NOW + 60_000).unwrap();
        assert_eq!(report, DrainReport { jobs: 1, objects: 2, stored: 2, ..DrainReport::default() });
        let requests = backend.requests.lock().unwrap().clone();
        let first = &requests[0];
        assert_eq!(first.source, "gmail-sync");
        assert_eq!(first.actor, "sync:gmail");
        assert_eq!(first.items[0].kind, "gmail.message");
        assert_eq!(first.items[0].title.as_deref(), Some("Email: Deck by Friday?"));
        assert!(first.items[0].content.contains("From: Mika <mika@example.com>"));
        assert!(first.items[0].content.contains("Can you send the deck by Friday?"));
        assert!(!first.items[0].content.contains("PNGBYTES"), "attachments are never read");
        assert!(first.items[0].content.ends_with("(via Gmail read-sync, m1, https://mail.google.com/mail/u/0/#all/t1)"));
        assert_eq!(first.items[0].ts_ms, Some(1_799_999_990_000));
        assert_eq!(first.metadata.as_ref().unwrap()["object_id"], "m1");
        assert_eq!(first.metadata.as_ref().unwrap()["version"], "901");
        assert!(requests[1].items[0].content.contains("Lunch moved to 1pm"));
        // The bearer went to the vendor; the malformed id and the attachment were never asked for.
        let calls = http.calls.lock().unwrap().clone();
        assert!(calls.iter().all(|(url, bearer, _)| *bearer == Secret::new("ya29.alice-token") && !url.contains("../") && !url.contains("attachments")));
        assert_eq!(calls.len(), 4);
        assert_eq!(events.lock().unwrap().len(), 4);
        assert!(events.lock().unwrap().iter().all(|e| e.action == "sync" && e.destination == "gmail.googleapis.com"));
        assert!(store.get("prn_a", CredentialSlot::SyncQueue).unwrap().is_none(), "drained");

        // The same messages at the same version are stored once.
        enqueue(&vault, "prn_a", "gmail", &detail, NOW + 120_000).unwrap();
        let again = drain(&vault, &http, &backend, "prn_a", NOW + 180_000).unwrap();
        assert_eq!((again.objects, again.stored), (2, 0));
    }

    #[test]
    fn calendar_and_notion_notices_fetch_their_objects() {
        let (store, vault) = setup();
        connect(&store, "prn_a", Service::GoogleCalendar, "ya29.cal-token");
        connect(&store, "prn_a", Service::Notion, "ntn_page_token");
        enqueue(&vault, "prn_a", "google_calendar", &json!({ "channel_id": "hk_1", "resource_id": "r-9", "resource_state": "exists" }), NOW).unwrap();
        enqueue(&vault, "prn_a", "notion", &json!({ "entity_type": "page", "entity_id": "1a2b3c4d-0000-1111-2222-333344445555" }), NOW).unwrap();
        // A database or comment change is not fetched.
        assert_eq!(enqueue(&vault, "prn_a", "notion", &json!({ "entity_type": "database", "entity_id": "db1" }), NOW).err(), Some(VaultError::UnknownSlot));
        assert_eq!(enqueue(&vault, "prn_a", "github", &json!({}), NOW).err(), Some(VaultError::UnknownSlot));

        let http = FakeHttp::default();
        http.on(CALENDAR_EVENTS, Ok(json!({ "items": [
            { "id": "ev1", "status": "confirmed", "summary": "Deck review", "updated": "2026-10-25T09:00:00.000Z", "start": { "dateTime": "2026-10-27T10:00:00+09:00" }, "end": { "dateTime": "2026-10-27T10:30:00+09:00" }, "location": "Room 4", "attendees": [{ "email": "mika@example.com" }], "description": "Bring the numbers", "htmlLink": "https://www.google.com/calendar/event?eid=ev1" },
            { "id": "ev2", "status": "cancelled", "summary": "Gone" }
        ] })));
        http.on(&format!("{NOTION_API}/pages/1a2b3c4d-0000-1111-2222-333344445555"), Ok(json!({
            "id": "1a2b3c4d-0000-1111-2222-333344445555", "url": "https://www.notion.so/Plan-1a2b3c4d000011112222333344445555", "last_edited_time": "2026-10-25T08:00:00.000Z",
            "properties": { "Name": { "type": "title", "title": [{ "plain_text": "Q4 plan" }] } }
        })));
        http.on(&format!("{NOTION_API}/blocks/1a2b3c4d-0000-1111-2222-333344445555/children"), Ok(json!({ "results": [
            { "type": "heading_2", "heading_2": { "rich_text": [{ "plain_text": "Goals" }] } },
            { "type": "to_do", "to_do": { "checked": false, "rich_text": [{ "plain_text": "Ship the router" }] } },
            { "type": "image", "image": { "file": { "url": "https://files.example/img.png" } } }
        ] })));
        let backend = Store::default();
        let report = drain(&vault, &http, &backend, "prn_a", NOW + 1_000).unwrap();
        assert_eq!((report.jobs, report.objects, report.stored), (2, 2, 2));
        let requests = backend.requests.lock().unwrap().clone();
        let cal = requests.iter().find(|r| r.source == "google_calendar-sync").unwrap();
        assert_eq!(cal.items[0].title.as_deref(), Some("Calendar: Deck review"));
        assert!(cal.items[0].content.contains("When: 2026-10-27T10:00:00+09:00 to 2026-10-27T10:30:00+09:00"));
        assert!(cal.items[0].content.contains("Attendees: mika@example.com"));
        let page = requests.iter().find(|r| r.source == "notion-sync").unwrap();
        assert_eq!(page.items[0].kind, "notion.page");
        assert!(page.items[0].content.contains("## Goals\n[ ] Ship the router"));
        assert!(!page.items[0].content.contains("img.png"), "non-text blocks are skipped");
        let calls = http.calls.lock().unwrap().clone();
        let cal_call = calls.iter().find(|(u, _, _)| u.starts_with(CALENDAR_EVENTS)).unwrap();
        assert!(cal_call.0.contains(&format!("updatedMin={}", rfc3339(NOW - CALENDAR_LOOKBACK_MS).replace(':', "%3A"))));
        assert!(calls.iter().filter(|(u, _, _)| u.starts_with(NOTION_API)).all(|(_, b, h)| *b == Secret::new("ntn_page_token") && h == &vec![("Notion-Version", NOTION_VERSION)]));
    }

    #[test]
    fn failures_drop_retry_or_count_not_connected_and_stay_per_principal() {
        let (store, vault) = setup();
        // No grant: dropped and counted, no fetch.
        enqueue(&vault, "prn_a", "gmail", &json!({ "history_id": "1" }), NOW).unwrap();
        let http = FakeHttp::default();
        let backend = Store::default();
        assert_eq!(drain(&vault, &http, &backend, "prn_a", NOW).unwrap(), DrainReport { jobs: 1, not_connected: 1, ..DrainReport::default() });
        assert!(http.calls.lock().unwrap().is_empty());
        // A vendor that does not answer: re-queued with a back-off, then dropped after the last try.
        connect(&store, "prn_a", Service::Gmail, "ya29.token-a");
        http.on(GMAIL_MESSAGES, Err(FetchError::Transient));
        enqueue(&vault, "prn_a", "gmail", &json!({ "history_id": "2" }), NOW).unwrap();
        assert_eq!(drain(&vault, &http, &backend, "prn_a", NOW).unwrap().retried, 1);
        assert_eq!(drain(&vault, &http, &backend, "prn_a", NOW + 1).unwrap().jobs, 0, "not before the back-off");
        let queued = pending(&vault, "prn_a").unwrap();
        assert_eq!((queued.len(), queued[0].attempts), (1, 1));
        assert_eq!(drain(&vault, &http, &backend, "prn_a", NOW + RETRY_AFTER_MS).unwrap().retried, 1);
        assert_eq!(drain(&vault, &http, &backend, "prn_a", NOW + 3 * RETRY_AFTER_MS + 1).unwrap().dropped, 1);
        assert!(pending(&vault, "prn_a").unwrap().is_empty());
        // A revoked grant (401) counts as not connected.
        http.on(GMAIL_MESSAGES, Err(FetchError::Unauthorized));
        enqueue(&vault, "prn_a", "gmail", &json!({ "history_id": "3" }), NOW).unwrap();
        assert_eq!(drain(&vault, &http, &backend, "prn_a", NOW).unwrap().not_connected, 1);
        // Another principal's queue and grant are its own.
        enqueue(&vault, "prn_b", "gmail", &json!({ "history_id": "4" }), NOW).unwrap();
        assert!(pending(&vault, "prn_a").unwrap().is_empty());
        assert_eq!(pending(&vault, "prn_b").unwrap().len(), 1);
        assert!(has_pending(&vault, "prn_b") && !has_pending(&vault, "prn_a"));
        http.calls.lock().unwrap().clear();
        assert_eq!(drain(&vault, &http, &backend, "prn_b", NOW).unwrap().not_connected, 1, "prn_a's grant never serves prn_b");
        assert!(http.calls.lock().unwrap().is_empty());
        // The queue is bounded.
        for i in 0..MAX_QUEUE + 5 {
            enqueue(&vault, "prn_a", "notion", &json!({ "entity_type": "page", "entity_id": format!("page-{i}") }), NOW).unwrap();
        }
        let queued = pending(&vault, "prn_a").unwrap();
        assert_eq!(queued.len(), MAX_QUEUE);
        assert_eq!(queued[0].key, "notion:page-5", "the oldest went first");
    }

    #[test]
    fn rfc3339_and_keys() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_761_384_600_000), "2025-10-25T09:30:00Z");
        assert_eq!(crate::result::rfc3339_to_ms(&rfc3339(NOW)), Some(NOW));
        assert_eq!(job_key(SyncProvider::GoogleCalendar, &json!({ "channel_id": "hk_1" })).as_deref(), Some("calendar:hk_1"));
        assert_eq!(job_key(SyncProvider::Notion, &json!({ "entity_id": "a/b" })), None);
        let obj = FetchedObject { kind: "gmail.message", object_id: "m1".into(), version: "1".into(), title: "t".into(), body: "b".into(), url: None, ts_ms: None, thread: "gmail:t".into() };
        let a = ingest_request(SyncProvider::Gmail, &obj);
        let b = ingest_request(SyncProvider::Gmail, &FetchedObject { version: "2".into(), ..obj.clone() });
        assert_ne!(a.idempotency_key, b.idempotency_key, "a new version is a new event");
        assert_eq!(a.idempotency_key, ingest_request(SyncProvider::Gmail, &obj).idempotency_key);
        assert!(a.idempotency_key.len() <= shogun_mcp::ingest_api::MAX_KEY_CHARS);
    }
}
