//! Durable, cross-process L3 approval queue. Store data is local, 0600, and contains full
//! previews only until a human resolves the request; terminal rows are deliberately body-free.
//!
//! **Sealed at rest (audit 2026-10-10 M2).** A pending row's recipient and body are the most
//! sensitive thing SHOGUN generates (a draft quoting the user's mail), and this file sits outside
//! the encrypted memory DB. So every writer that holds an [`ApprovalSealKey`] — derived from the
//! memory DB key, never stored itself — seals those two fields with ChaCha20-Poly1305 before the
//! row touches disk (`sealed:v1:<base64url(nonce ‖ ciphertext)>`), bound to the row id, kind and
//! field name so a ciphertext cannot be moved between rows. Readers unseal transparently; rows
//! written before sealing (plaintext) still load and are sealed on the next save. A process
//! without the key refuses to store a new pending row (no plaintext fallback), loads sealed rows
//! with a fixed placeholder in place of what it cannot read, carries their ciphertext through any
//! save untouched, and cannot confirm them.
//!
//! The four actions whose "destination" is the requested change itself (a capture-exclusion
//! change, a Visual recall widening, a Gmail archive request, a capture unpause) keep that field
//! in the clear: it is the same data as a settings file beside this one (`exclusions.json`,
//! `visual_recall.json`), and a process without the key must still rebuild the exact action. Their
//! bodies are sealed like every other row's.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use shogun_agents::approval::{
    ApprovalId, ApprovalOrigin, ApprovalQueue, ApprovalStatus, PendingRecord, Preview, Route,
    TerminalRecord,
};
use shogun_agents::permission::{CaptureExclusionChange, PausedCapture, SendAction, VisualRecallWidening};

pub const STORE_FILE: &str = "l3_approvals.json";
pub const STORE_ENV: &str = "SHOGUN_L3_APPROVALS";
/// Maximum pending approvals across all processes. In-flight sends are separate.
pub const MAX_PENDING: usize = 64;
const MAX_TERMINAL: usize = 256;
const MAX_BODY_BYTES: usize = 256 * 1024;
const MAX_DESTINATION_BYTES: usize = 8192;
/// A queued capture-exclusion change (issue #657) stores its complete requested lists as its
/// destination, so it gets a larger (still bounded) allowance than a recipient or a title.
const MAX_CAPTURE_CHANGE_BYTES: usize = 64 * 1024;
/// Sealing inflates a body by base64 (4/3) plus a nonce and tag, so the file bound leaves room
/// for `MAX_PENDING` sealed rows at `MAX_BODY_BYTES` each.
const MAX_STORE_BYTES: u64 = 24 * 1024 * 1024;
/// A sealed field longer than this cannot hold a body within `MAX_BODY_BYTES`; refused before it
/// is decoded.
const MAX_SEALED_FIELD_BYTES: usize = MAX_BODY_BYTES * 2;

/// The marker a sealed field starts with. A plaintext row written before sealing never does.
const SEALED_PREFIX: &str = "sealed:v1:";
/// What a process without the key sees in place of a sealed recipient or body.
pub const SEALED_PLACEHOLDER: &str = "[sealed]";
/// HKDF `info` for the sealing key: ties the derived key to this use, so the same memory key
/// never yields the same bytes for anything else.
const SEAL_KEY_INFO: &[u8] = b"shogun-l3-approvals-v1";
const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;

/// The key pending rows are sealed with: 32 bytes HKDF-SHA256-derived from the memory DB key
/// with [`SEAL_KEY_INFO`]. Never persisted, never logged (`Debug` is redacted), zeroed on drop.
///
/// `None` inside is a derivation that did not complete. HKDF-Expand to 32 bytes under SHA-256
/// cannot fail (the bound is 255 × the hash length), but the `hkdf` API returns a `Result`, and
/// the fallback for that unreachable branch must not be the DB key itself (audit 2026-10-10,
/// second pass): a key that holds `None` refuses to seal and to unseal instead. Callers keep the
/// infallible `from_db_key`, so the desktop and the headless faces construct it the same way.
#[derive(Clone)]
pub struct ApprovalSealKey(Option<[u8; 32]>);

impl ApprovalSealKey {
    /// Derive the sealing key from the memory DB key's raw bytes (no hex copy of the key is made:
    /// `DbKey::with_bytes` lends them for the derivation only). The derivation is the same
    /// whichever face (desktop, `shogun-mcp`, `shogun-api`) runs it.
    pub fn from_db_key(key: &shogun_memory::DbKey) -> Self {
        key.with_bytes(Self::derive)
    }

    fn derive(ikm: &[u8; 32]) -> Self {
        let mut okm = [0u8; 32];
        // HKDF-Expand to 32 bytes never fails (the bound is 255 × hash length). Were it to, the
        // key is unusable rather than equal to the DB key: `cipher` refuses, nothing is sealed
        // under the DB key and nothing already sealed is misread under it.
        if hkdf::Hkdf::<sha2::Sha256>::new(None, ikm)
            .expand(SEAL_KEY_INFO, &mut okm)
            .is_err()
        {
            zero(&mut okm);
            return Self(None);
        }
        Self(Some(okm))
    }

    fn cipher(&self) -> Result<ChaCha20Poly1305, String> {
        self.0
            .as_ref()
            .map(|key| ChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(key)))
            .ok_or_else(|| "approval sealing key could not be derived".to_string())
    }
}

impl std::fmt::Debug for ApprovalSealKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApprovalSealKey(***redacted***)")
    }
}

impl Drop for ApprovalSealKey {
    fn drop(&mut self) {
        if let Some(key) = self.0.as_mut() {
            zero(key);
        }
    }
}

fn zero(bytes: &mut [u8; 32]) {
    for byte in bytes.iter_mut() {
        // Volatile so the compiler cannot elide the wipe of a value about to be dropped.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
}

/// Which pending rows seal their destination: every kind except the four whose destination is
/// the requested change itself (see the module doc). The body is sealed for every kind.
fn seals_destination(kind: &str) -> bool {
    !matches!(
        kind,
        "resume_capture" | "widen_visual_recall" | "archive_mail_thread" | "unpause_capture"
    )
}

/// Associated data: the row id, its kind and the field name, so a ciphertext decrypts only in
/// the row and field it was written for.
fn seal_aad(id: u64, kind: &str, field: &str) -> Vec<u8> {
    format!("{id}:{kind}:{field}").into_bytes()
}

fn is_sealed(value: &str) -> bool {
    value.starts_with(SEALED_PREFIX)
}

fn seal_field(key: &ApprovalSealKey, aad: &[u8], plain: &str) -> Result<String, String> {
    let mut nonce = [0u8; NONCE_BYTES];
    getrandom::fill(&mut nonce)
        .map_err(|_| "the system random source is unavailable".to_string())?;
    let ciphertext = key
        .cipher()?
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: plain.as_bytes(), aad })
        .map_err(|_| "cannot seal approval row".to_string())?;
    let mut bytes = Vec::with_capacity(NONCE_BYTES + ciphertext.len());
    bytes.extend_from_slice(&nonce);
    bytes.extend_from_slice(&ciphertext);
    Ok(format!(
        "{SEALED_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    ))
}

fn unseal_field(key: &ApprovalSealKey, aad: &[u8], sealed: &str) -> Result<String, String> {
    let encoded = sealed
        .strip_prefix(SEALED_PREFIX)
        .ok_or_else(|| "approval row is not sealed".to_string())?;
    if encoded.len() > MAX_SEALED_FIELD_BYTES {
        return Err("sealed approval row too large".into());
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| "approval row cannot be unsealed".to_string())?;
    if bytes.len() < NONCE_BYTES + TAG_BYTES {
        return Err("approval row cannot be unsealed".into());
    }
    let (nonce, ciphertext) = bytes.split_at(NONCE_BYTES);
    let plain = key
        .cipher()?
        .decrypt(Nonce::from_slice(nonce), Payload { msg: ciphertext, aad })
        .map_err(|_| "approval row cannot be unsealed".to_string())?;
    String::from_utf8(plain).map_err(|_| "approval row cannot be unsealed".to_string())
}

/// Seal a plain row for disk. The destination of a kind that keeps it in the clear is left alone.
fn seal_row(key: &ApprovalSealKey, row: &WirePending) -> Result<WirePending, String> {
    let mut sealed = row.clone();
    if seals_destination(&row.kind) {
        sealed.destination =
            seal_field(key, &seal_aad(row.id, &row.kind, "destination"), &row.destination)?;
    }
    if let Some(thread) = &row.reply_thread {
        sealed.reply_thread = Some(seal_field(key, &seal_aad(row.id, &row.kind, "reply_thread"), thread)?);
    }
    sealed.full_body = seal_field(key, &seal_aad(row.id, &row.kind, "full_body"), &row.full_body)?;
    Ok(sealed)
}

/// The sealed wire strings of a row a key-less process loaded with placeholders: written back
/// verbatim on save, so a status poll or an expiry sweep in that process never loses a body.
#[derive(Debug, Clone, PartialEq)]
struct CarriedSeal {
    destination: String,
    full_body: String,
    reply_thread: Option<String>,
}

/// Open a row as read from disk: with the key, sealed fields are unsealed (a plaintext field is
/// a row from before sealing and is taken as is); without it, each sealed field becomes
/// [`SEALED_PLACEHOLDER`] and the row's ciphertext is carried for the next save.
fn open_row(
    key: Option<&ApprovalSealKey>,
    mut row: WirePending,
) -> Result<(WirePending, Option<CarriedSeal>), String> {
    let destination_sealed = is_sealed(&row.destination);
    let body_sealed = is_sealed(&row.full_body);
    if !destination_sealed && !body_sealed {
        return Ok((row, None));
    }
    match key {
        Some(key) => {
            if let Some(thread) = row.reply_thread.as_ref().filter(|s| is_sealed(s)) {
                row.reply_thread = Some(unseal_field(key, &seal_aad(row.id, &row.kind, "reply_thread"), thread)?);
            }
            if destination_sealed {
                row.destination = unseal_field(
                    key,
                    &seal_aad(row.id, &row.kind, "destination"),
                    &row.destination,
                )?;
            }
            if body_sealed {
                row.full_body =
                    unseal_field(key, &seal_aad(row.id, &row.kind, "full_body"), &row.full_body)?;
            }
            Ok((row, None))
        }
        None => {
            if row.destination.len() > MAX_SEALED_FIELD_BYTES
                || row.full_body.len() > MAX_SEALED_FIELD_BYTES
            {
                return Err("sealed approval row too large".into());
            }
            let carried = CarriedSeal {
                destination: row.destination.clone(),
                full_body: row.full_body.clone(),
                reply_thread: row.reply_thread.take(),
            };
            if destination_sealed {
                row.destination = SEALED_PLACEHOLDER.into();
            }
            if body_sealed {
                row.full_body = SEALED_PLACEHOLDER.into();
            }
            Ok((row, Some(carried)))
        }
    }
}

/// The action a key-less process holds for a row whose destination it cannot read: the row's
/// kind with the placeholder where the recipient would be. It exists so the row keeps its id and
/// status (a poll or an expiry sees it); it is never executed — [`ApprovalStore::with_queue`]
/// refuses to confirm a carried row without the key.
fn placeholder_action(kind: &str) -> Result<SendAction, String> {
    let destination = SEALED_PLACEHOLDER.to_string();
    Ok(match kind {
        "send_email" => SendAction::SendEmail { to: destination },
        "post_message" => SendAction::PostMessage { channel: destination },
        "add_reaction" => SendAction::AddReaction { target: destination },
        "create_calendar_event" => SendAction::CreateCalendarEvent { title: destination },
        "update_calendar_event" => SendAction::UpdateCalendarEvent { title: destination },
        "post_comment" => SendAction::PostComment { target: destination },
        "create_document" => SendAction::CreateDocument { title: destination },
        "update_document" => SendAction::UpdateDocument { title: destination },
        "change_issue_status" => SendAction::ChangeIssueStatus { target: destination },
        "share_writing_style" => SendAction::ShareWritingStyle,
        "enable_dictionary_autoadd" => SendAction::EnableDictionaryAutoAdd,
        "use_subscription_agent" => SendAction::UseSubscriptionAgent { delegate: destination },
        // These kinds never seal their destination; a sealed one was not written by this code.
        _ => return Err("invalid approval action kind".into()),
    })
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WireStore {
    schema: u8,
    next_id: u64,
    pending: Vec<WirePending>,
    terminal: Vec<WireTerminal>,
    in_flight: Vec<u64>,
}
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePending {
    id: u64,
    kind: String,
    destination: String,
    full_body: String,
    route: String,
    origin: String,
    created_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reply_thread: Option<String>,
}
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTerminal {
    id: u64,
    status: String,
    resolved_ms: u64,
    #[serde(default, skip_serializing)]
    reason: Option<String>,
}

pub fn resolve_store_path(db_path: &str) -> PathBuf {
    if let Ok(path) = std::env::var(STORE_ENV) {
        if !path.trim().is_empty() {
            return PathBuf::from(path);
        }
    }
    Path::new(db_path)
        .parent()
        .map(|dir| dir.join(STORE_FILE))
        .unwrap_or_else(|| PathBuf::from(STORE_FILE))
}

fn route_wire(route: Route) -> &'static str {
    match route {
        Route::DirectMcp => "direct",
        Route::ViaComposio => "composio",
    }
}
fn route_parse(value: &str) -> Result<Route, String> {
    match value {
        "direct" => Ok(Route::DirectMcp),
        "composio" => Ok(Route::ViaComposio),
        _ => Err("invalid approval route".into()),
    }
}
fn origin_wire(origin: ApprovalOrigin) -> &'static str {
    origin.as_str()
}
fn origin_parse(value: &str) -> Result<ApprovalOrigin, String> {
    match value {
        "ui" => Ok(ApprovalOrigin::Ui),
        "assistant" => Ok(ApprovalOrigin::Assistant),
        "api" => Ok(ApprovalOrigin::Api),
        "mcp" => Ok(ApprovalOrigin::Mcp),
        _ => Err("invalid approval origin".into()),
    }
}
fn status_wire(status: ApprovalStatus) -> &'static str {
    match status {
        ApprovalStatus::Pending => "pending",
        ApprovalStatus::Rejected => "rejected",
        ApprovalStatus::TimedOut => "timed_out",
        ApprovalStatus::Sent => "sent",
        ApprovalStatus::SendFailed => "send_failed",
        ApprovalStatus::DraftSaved => "draft_saved",
        ApprovalStatus::OutcomeUnknown => "outcome_unknown",
    }
}
fn status_parse(value: &str) -> Result<ApprovalStatus, String> {
    match value {
        "rejected" => Ok(ApprovalStatus::Rejected),
        "timed_out" => Ok(ApprovalStatus::TimedOut),
        "sent" => Ok(ApprovalStatus::Sent),
        "send_failed" => Ok(ApprovalStatus::SendFailed),
        "draft_saved" => Ok(ApprovalStatus::DraftSaved),
        "outcome_unknown" => Ok(ApprovalStatus::OutcomeUnknown),
        _ => Err("invalid terminal approval status".into()),
    }
}

fn action_kind(action: &SendAction) -> &'static str {
    match action {
        SendAction::SendEmail { .. } => "send_email",
        SendAction::PostMessage { .. } => "post_message",
        SendAction::AddReaction { .. } => "add_reaction",
        SendAction::CreateCalendarEvent { .. } => "create_calendar_event",
        SendAction::UpdateCalendarEvent { .. } => "update_calendar_event",
        SendAction::PostComment { .. } => "post_comment",
        SendAction::CreateDocument { .. } => "create_document",
        SendAction::UpdateDocument { .. } => "update_document",
        SendAction::ChangeIssueStatus { .. } => "change_issue_status",
        SendAction::ShareWritingStyle => "share_writing_style",
        SendAction::EnableDictionaryAutoAdd => "enable_dictionary_autoadd",
        SendAction::ResumeCapture { .. } => "resume_capture",
        SendAction::UseSubscriptionAgent { .. } => "use_subscription_agent",
        SendAction::WidenVisualRecall { .. } => "widen_visual_recall",
        SendAction::ArchiveMailThread { .. } => "archive_mail_thread",
        SendAction::UnpauseCapture { .. } => "unpause_capture",
    }
}

/// A delegate id as the store may carry it (issue #110): short, lower-case, dash-separated. The
/// desktop resolves it against its own delegate list on confirm and refuses anything unknown.
fn plausible_delegate_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.chars().all(|c| c.is_ascii_lowercase() || c == '-')
}

/// The wire form of a queued capture-exclusion change (issue #657). The preview's destination is
/// only the fixed label "this Mac", so the store keeps the change itself in the `destination`
/// field: loading must rebuild exactly what the user will approve and the desktop will apply.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WireCaptureChange {
    apps: Vec<String>,
    websites: Vec<String>,
    resumed_apps: Vec<String>,
    resumed_websites: Vec<String>,
}

fn capture_change_to_wire(change: &CaptureExclusionChange) -> String {
    serde_json::to_string(&WireCaptureChange {
        apps: change.apps.clone(),
        websites: change.websites.clone(),
        resumed_apps: change.resumed_apps.clone(),
        resumed_websites: change.resumed_websites.clone(),
    })
    .unwrap_or_default()
}

fn capture_change_from_wire(destination: &str) -> Result<CaptureExclusionChange, String> {
    if destination.len() > MAX_CAPTURE_CHANGE_BYTES {
        return Err("invalid approval destination".into());
    }
    let wire: WireCaptureChange = serde_json::from_str(destination)
        .map_err(|_| "invalid capture exclusion change".to_string())?;
    let change = CaptureExclusionChange {
        apps: wire.apps,
        websites: wire.websites,
        resumed_apps: wire.resumed_apps,
        resumed_websites: wire.resumed_websites,
    };
    // A change that resumes nothing is never queued; one on disk was tampered with.
    if !change.resumes_capture() {
        return Err("invalid capture exclusion change".into());
    }
    Ok(change)
}

/// The wire form of a queued Visual recall widening (issue #782). Like a capture change, the
/// preview's destination is only "this Mac", so the row keeps the requested change itself.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WireVisualRecallWidening {
    enable: bool,
    retention_days: Option<u32>,
    previous_retention_days: u32,
}

fn visual_recall_to_wire(change: &VisualRecallWidening) -> String {
    serde_json::to_string(&WireVisualRecallWidening {
        enable: change.enable,
        retention_days: change.retention_days,
        previous_retention_days: change.previous_retention_days,
    })
    .unwrap_or_default()
}

fn visual_recall_from_wire(destination: &str) -> Result<VisualRecallWidening, String> {
    if destination.len() > MAX_DESTINATION_BYTES {
        return Err("invalid approval destination".into());
    }
    let wire: WireVisualRecallWidening = serde_json::from_str(destination)
        .map_err(|_| "invalid visual recall change".to_string())?;
    let change = VisualRecallWidening {
        enable: wire.enable,
        retention_days: wire.retention_days,
        previous_retention_days: wire.previous_retention_days,
    };
    // A change that widens nothing, or a day count out of bounds, is never queued; a row on
    // disk that says otherwise was tampered with.
    if !change.is_valid() {
        return Err("invalid visual recall change".into());
    }
    Ok(change)
}

/// The wire form of a queued Gmail archive request (issue #779). The preview's destination is
/// the thread key alone, which cannot say which way the request goes, so the row keeps both.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMailArchive {
    thread_key: String,
    unarchive: bool,
}

fn mail_archive_to_wire(thread_key: &str, unarchive: bool) -> String {
    serde_json::to_string(&WireMailArchive { thread_key: thread_key.to_string(), unarchive })
        .unwrap_or_default()
}

fn mail_archive_from_wire(destination: &str) -> Result<SendAction, String> {
    if destination.len() > MAX_DESTINATION_BYTES {
        return Err("invalid approval destination".into());
    }
    let wire: WireMailArchive = serde_json::from_str(destination)
        .map_err(|_| "invalid mail archive request".to_string())?;
    // A key that cannot name a Gmail triage thread is never queued; a row on disk that carries
    // one was tampered with.
    if !shogun_agents::permission::plausible_mail_thread_key(&wire.thread_key) {
        return Err("invalid mail archive request".into());
    }
    Ok(SendAction::ArchiveMailThread { thread_key: wire.thread_key, unarchive: wire.unarchive })
}

/// The wire form of a queued request to end the capture pause (issue #897): the pause it was
/// made against, which the preview's "this Mac" cannot carry. Confirming checks it against the
/// pause in force, so it must load back exactly.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePausedCapture {
    resume_at_ms: Option<i64>,
}

fn paused_capture_to_wire(pause: PausedCapture) -> String {
    serde_json::to_string(&WirePausedCapture { resume_at_ms: pause.resume_at_ms }).unwrap_or_default()
}

fn paused_capture_from_wire(destination: &str) -> Result<PausedCapture, String> {
    if destination.len() > MAX_DESTINATION_BYTES {
        return Err("invalid approval destination".into());
    }
    let wire: WirePausedCapture = serde_json::from_str(destination)
        .map_err(|_| "invalid capture pause request".to_string())?;
    // A resume time is a positive Unix time in ms; a row that says otherwise was tampered with.
    if wire.resume_at_ms.is_some_and(|ms| ms <= 0) {
        return Err("invalid capture pause request".into());
    }
    Ok(PausedCapture { resume_at_ms: wire.resume_at_ms })
}

/// The `destination` a pending row stores: the preview's, except for the actions whose preview
/// label cannot rebuild them.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SlackDestination { id: String, label: String }

fn slack_destination(destination: &str) -> Result<SlackDestination, String> {
    if shogun_agents::permission::is_slack_conversation_id(destination) {
        return Ok(SlackDestination { id: destination.into(), label: destination.into() });
    }
    let target: SlackDestination = serde_json::from_str(destination)
        .map_err(|_| "invalid Slack approval destination")?;
    if !shogun_agents::permission::is_slack_conversation_id(&target.id)
        || target.label.trim().is_empty() || target.label.len() > MAX_DESTINATION_BYTES
        || target.label.chars().any(char::is_control) {
        return Err("invalid Slack approval destination".into());
    }
    Ok(target)
}

fn wire_destination(action: &SendAction, preview: &Preview) -> String {
    match action {
        SendAction::PostMessage { channel } if preview.destination != *channel => {
            serde_json::json!({"id":channel,"label":preview.destination}).to_string()
        }
        SendAction::ResumeCapture { change } => capture_change_to_wire(change),
        SendAction::WidenVisualRecall { change } => visual_recall_to_wire(change),
        SendAction::ArchiveMailThread { thread_key, unarchive } => mail_archive_to_wire(thread_key, *unarchive),
        SendAction::UnpauseCapture { pause } => paused_capture_to_wire(*pause),
        _ => preview.destination.clone(),
    }
}

fn action_from_wire(kind: &str, destination: String) -> Result<SendAction, String> {
    if kind == "resume_capture" {
        return capture_change_from_wire(&destination).map(|change| SendAction::ResumeCapture { change });
    }
    if kind == "widen_visual_recall" {
        return visual_recall_from_wire(&destination).map(|change| SendAction::WidenVisualRecall { change });
    }
    if kind == "archive_mail_thread" {
        return mail_archive_from_wire(&destination);
    }
    if kind == "unpause_capture" {
        return paused_capture_from_wire(&destination).map(|pause| SendAction::UnpauseCapture { pause });
    }
    if destination.trim().is_empty() || destination.len() > MAX_DESTINATION_BYTES {
        return Err("invalid approval destination".into());
    }
    Ok(match kind {
        "send_email" => SendAction::SendEmail { to: destination },
        "post_message" => SendAction::PostMessage {
            channel: slack_destination(&destination)?.id,
        },
        "add_reaction" => SendAction::AddReaction {
            target: destination,
        },
        "create_calendar_event" => SendAction::CreateCalendarEvent { title: destination },
        "update_calendar_event" => SendAction::UpdateCalendarEvent { title: destination },
        "post_comment" => SendAction::PostComment {
            target: destination,
        },
        "create_document" => SendAction::CreateDocument { title: destination },
        "update_document" => SendAction::UpdateDocument { title: destination },
        "change_issue_status" => SendAction::ChangeIssueStatus {
            target: destination,
        },
        // A unit action: the stored destination is only the preview's fixed label.
        "share_writing_style" => SendAction::ShareWritingStyle,
        "enable_dictionary_autoadd" => SendAction::EnableDictionaryAutoAdd,
        // The destination is the delegate id itself (the preview shows it as the destination).
        "use_subscription_agent" if plausible_delegate_id(&destination) => {
            SendAction::UseSubscriptionAgent { delegate: destination }
        }
        _ => return Err("invalid approval action kind".into()),
    })
}
fn expected_route(_action: &SendAction) -> Route {
    Route::DirectMcp
}
fn pending_to_wire(record: &PendingRecord) -> WirePending {
    WirePending {
        id: record.id.0,
        kind: action_kind(&record.action).into(),
        destination: wire_destination(&record.action, &record.preview),
        full_body: record.preview.full_body.clone(),
        route: route_wire(record.preview.route).into(),
        origin: origin_wire(record.origin).into(),
        created_ms: record.created_ms,
        reply_thread: record.preview.reply_thread.as_ref().map(|thread|
            crate::reply_thread::to_value(&record.action, thread).to_string()),
    }
}
/// Build the record for an opened row. `placeholder` says the destination is the key-less
/// placeholder, not a real one, so the action is rebuilt by kind alone.
fn pending_from_wire(row: WirePending, placeholder: bool) -> Result<PendingRecord, String> {
    if row.id == 0 || row.full_body.len() > MAX_BODY_BYTES {
        return Err("invalid approval preview".into());
    }
    let action = if placeholder {
        placeholder_action(&row.kind)?
    } else {
        action_from_wire(&row.kind, row.destination.clone())?
    };
    let route = route_parse(&row.route)?;
    let legacy_mail = route == Route::ViaComposio
        && matches!(
            action,
            SendAction::SendEmail { .. } | SendAction::ArchiveMailThread { .. }
        );
    if route != expected_route(&action) && !legacy_mail {
        return Err("approval route does not match action".into());
    }
    let mut preview = Preview::for_send(&action, row.full_body, route);
    if row.kind == "post_message" && !placeholder {
        preview.destination = slack_destination(&row.destination)?.label;
    }
    if let Some(thread) = row.reply_thread {
        let input = serde_json::from_str(&thread).map_err(|_| "invalid reply thread")?;
        preview.reply_thread = crate::reply_thread::parse(&action, &input).map_err(str::to_string)?;
    }
    Ok(PendingRecord {
        id: ApprovalId(row.id),
        action: action.clone(),
        preview,
        origin: origin_parse(&row.origin)?,
        created_ms: row.created_ms,
    })
}
fn queue_to_wire(queue: &ApprovalQueue) -> WireStore {
    let (next_id, pending) = queue.export();
    WireStore {
        schema: 1,
        next_id,
        pending: pending.iter().map(pending_to_wire).collect(),
        terminal: queue
            .terminal_records()
            .iter()
            .map(|row| WireTerminal {
                id: row.id.0,
                status: status_wire(row.status).into(),
                resolved_ms: row.resolved_ms,
                reason: row.reason.clone(),
            })
            .collect(),
        in_flight: queue.in_flight_ids().iter().map(|id| id.0).collect(),
    }
}
/// A store as one process holds it: the queue, plus the ciphertext of every row this process
/// could not unseal (empty whenever the key was present).
struct Loaded {
    queue: ApprovalQueue,
    carried: HashMap<u64, CarriedSeal>,
}

/// Validate an in-memory wire store (every field plain) and build its queue.
fn queue_from_wire(store: WireStore) -> Result<ApprovalQueue, String> {
    rows_from_wire(store, None).map(|loaded| loaded.queue)
}

fn rows_from_wire(store: WireStore, key: Option<&ApprovalSealKey>) -> Result<Loaded, String> {
    if store.schema != 1
        || store.next_id == 0
        || store.pending.len() > MAX_PENDING
        || store.terminal.len() > MAX_TERMINAL
    {
        return Err("invalid approval store limits".into());
    }
    let mut ids = HashSet::new();
    let mut pending = Vec::with_capacity(store.pending.len());
    let mut carried = HashMap::new();
    for row in store.pending {
        if !ids.insert(row.id) {
            return Err("duplicate approval id".into());
        }
        let (row, seal) = open_row(key, row)?;
        let placeholder = seal.as_ref().is_some_and(|seal| is_sealed(&seal.destination));
        if let Some(seal) = seal {
            carried.insert(row.id, seal);
        }
        pending.push(pending_from_wire(row, placeholder)?);
    }
    let mut terminal = Vec::with_capacity(store.terminal.len());
    for row in store.terminal {
        if row.id == 0 || !ids.insert(row.id) {
            return Err("duplicate approval id".into());
        }
        terminal.push(TerminalRecord {
            id: ApprovalId(row.id),
            status: status_parse(&row.status)?,
            resolved_ms: row.resolved_ms,
            reason: match row.reason.as_deref() {
                Some("provider_policy_stop") => Some("provider_policy_stop".to_string()),
                _ => None,
            },
        });
    }
    let mut in_flight = Vec::with_capacity(store.in_flight.len());
    for id in store.in_flight {
        if id == 0 || !ids.insert(id) {
            return Err("duplicate approval id".into());
        }
        in_flight.push(ApprovalId(id));
    }
    Ok(Loaded {
        queue: ApprovalQueue::import_with_terminal(store.next_id, pending, terminal, in_flight),
        carried,
    })
}

/// The store file as written: `None` when there is none yet.
fn load_wire(path: &Path) -> Result<Option<WireStore>, String> {
    match fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_STORE_BYTES => return Err("approval store too large".into()),
        #[cfg(unix)]
        Ok(meta) if std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o077 != 0 => {
            return Err("approval store permissions are not private".into());
        }
        Ok(_) | Err(_) => {}
    }
    match fs::read_to_string(path) {
        Ok(text) => {
            let mut store: WireStore =
                serde_json::from_str(&text).map_err(|_| "invalid approval store".to_string())?;
            // Optional metadata must never make pending approvals unreadable. Matching both
            // id and resolution time prevents stale metadata from annotating a changed row.
            let reasons = policy_stop_path(path);
            let stops: HashSet<(u64, u64)> = fs::metadata(&reasons)
                .ok()
                .filter(|meta| meta.len() <= (MAX_TERMINAL * 64) as u64)
                .and_then(|_| fs::read(reasons).ok())
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default();
            for row in &mut store.terminal {
                if row.status == "send_failed" && stops.contains(&(row.id, row.resolved_ms)) {
                    row.reason = Some("provider_policy_stop".into());
                }
            }
            Ok(Some(store))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("cannot read approval store".into()),
    }
}

/// The durable queue at one path, with the key its writer seals pending rows under.
///
/// Every face constructs one: the desktop and the headless `shogun-mcp` / `shogun-api` from the
/// memory DB key they opened the store with ([`ApprovalSealKey::from_db_key`]). `None` is a
/// process that never obtained the key: it can list and poll, cannot store a new pending row,
/// and cannot confirm a sealed one.
#[derive(Debug, Clone)]
pub struct ApprovalStore {
    path: PathBuf,
    seal: Option<ApprovalSealKey>,
}

impl ApprovalStore {
    pub fn new(path: PathBuf, seal: Option<ApprovalSealKey>) -> Self {
        Self { path, seal }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether this process can seal and unseal pending rows.
    pub fn can_unseal(&self) -> bool {
        self.seal.is_some()
    }

    fn load(&self) -> Result<Loaded, String> {
        match load_wire(&self.path)? {
            Some(store) => rows_from_wire(store, self.seal.as_ref()),
            None => Ok(Loaded { queue: ApprovalQueue::new(), carried: HashMap::new() }),
        }
    }

    /// The queue as this process can read it. Without the key, a sealed row's recipient and body
    /// read as [`SEALED_PLACEHOLDER`]; [`Self::sealed_unreadable`] names those rows.
    pub fn load_queue(&self) -> Result<ApprovalQueue, String> {
        self.load().map(|loaded| loaded.queue)
    }

    /// The pending rows whose recipient or body this process cannot read (`sealed: true` to a
    /// face that lists them). Always empty when the key is present.
    pub fn sealed_unreadable(&self) -> Result<Vec<ApprovalId>, String> {
        let mut ids: Vec<u64> = self.load()?.carried.into_keys().collect();
        ids.sort_unstable();
        Ok(ids.into_iter().map(ApprovalId).collect())
    }

    /// Run `change` under the store lock and persist what it changed.
    ///
    /// Without the key, a change that adds a pending row is refused (there is no plaintext
    /// fallback) and so is one that confirms a row this process could not unseal; nothing is
    /// written in either case.
    pub fn with_queue<R>(
        &self,
        change: impl FnOnce(&mut ApprovalQueue) -> R,
    ) -> Result<R, String> {
        let _lock = acquire_lock(&self.path)?;
        let Loaded { mut queue, carried } = self.load()?;
        let before = queue_to_wire(&queue);
        let output = change(&mut queue);
        // Read-only passes (the desktop's list polls, badge counts, status reads) leave the wire
        // state untouched — skip the rewrite so the idle path costs no disk writes (idle-CPU SLO).
        // `expire_due` and every real mutation change the wire state and still persist.
        let after = queue_to_wire(&queue);
        if after != before {
            if self.seal.is_none() {
                let known: HashSet<u64> = before.pending.iter().map(|row| row.id).collect();
                if after.pending.iter().any(|row| !known.contains(&row.id)) {
                    return Err(
                        "approval sealing key unavailable: a new L3 request cannot be stored"
                            .into(),
                    );
                }
                if after.in_flight.iter().any(|id| carried.contains_key(id)) {
                    return Err(
                        "sealed approval cannot be confirmed without the sealing key".into()
                    );
                }
            }
            // Persist-side belt for the load-time braces: not every producer runs
            // [`validate_enqueue`] (the desktop propose path enqueues via `producer::propose`), so
            // a change that produced a store `load_queue` would refuse (empty/oversized
            // destination, oversized body, >MAX_PENDING) must be refused here — persisting it
            // would brick the queue for every face until the JSON is hand-deleted.
            queue_from_wire(after)
                .map_err(|reason| format!("refusing to persist invalid approval store: {reason}"))?;
            self.save_unlocked(&queue, &carried)?;
            changed();
        }
        Ok(output)
    }

    /// Resolve work left in flight by a previous desktop process.
    ///
    /// Deliberately separate from [`Self::with_queue`]. An in-flight row can belong to a send
    /// that is still running outside the file lock, so ordinary reads, polls, and post-send
    /// writes must not mistake live work for a crash. The desktop calls this once when it
    /// becomes the queue executor.
    pub fn recover_in_flight(&self, recovered_ms: u64) -> Result<Vec<ApprovalId>, String> {
        let _lock = acquire_lock(&self.path)?;
        let Loaded { mut queue, carried } = self.load()?;
        let recovered = queue.recover_in_flight(recovered_ms);
        self.save_unlocked(&queue, &carried)?;
        if !recovered.is_empty() {
            changed();
        }
        Ok(recovered)
    }

    fn save_unlocked(
        &self,
        queue: &ApprovalQueue,
        carried: &HashMap<u64, CarriedSeal>,
    ) -> Result<(), String> {
        let mut wire = queue_to_wire(queue);
        for row in &mut wire.pending {
            if let Some(seal) = carried.get(&row.id) {
                // Loaded without the key: the row's ciphertext goes back exactly as it came.
                row.destination = seal.destination.clone();
                row.full_body = seal.full_body.clone();
                row.reply_thread = seal.reply_thread.clone();
            } else if let Some(key) = &self.seal {
                *row = seal_row(key, row)?;
            }
        }
        save_wire(&self.path, &wire, queue)
    }
}

// The handle is only *read* on unix (flock/unlock); on other platforms it exists to hold the
// file open for the lock file's lifetime.
pub(crate) struct StoreLock(#[cfg_attr(not(unix), allow(dead_code))] File);
impl Drop for StoreLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.0), libc::LOCK_UN);
        }
    }
}
fn lock_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.lock",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(STORE_FILE)
    ))
}
pub(crate) fn acquire_lock(path: &Path) -> Result<StoreLock, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|_| "cannot create approval store directory".to_string())?;
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path(path))
        .map_err(|_| "cannot lock approval store".to_string())?;
    #[cfg(unix)]
    for _ in 0..500 {
        if unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&lock),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        } == 0
        {
            return Ok(StoreLock(lock));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    #[cfg(not(unix))]
    return Ok(StoreLock(lock));
    #[cfg(unix)]
    Err("approval store lock timed out".into())
}

fn executor_lock_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.executor.lock",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(STORE_FILE)
    ))
}

/// Process-lifetime claim that this desktop is the L3 send executor.
///
/// Distinct from the short [`acquire_lock`] used around a single store transaction: an in-flight
/// send releases that lock while the wire call runs. Holding this claim stops a second instance
/// from crash-recovering rows that still belong to a live send.
pub struct ExecutorGuard {
    inner: std::sync::Mutex<Option<StoreLock>>,
}

impl Drop for ExecutorGuard {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.inner.lock() {
            slot.take();
        }
    }
}

/// Non-blocking exclusive claim. `Ok(None)` means another live executor already holds the queue.
pub fn try_claim_executor(path: &Path) -> Result<Option<ExecutorGuard>, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|_| "cannot create approval store directory".to_string())?;
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(executor_lock_path(path))
        .map_err(|_| "cannot lock approval executor".to_string())?;
    #[cfg(unix)]
    {
        let rc = unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&lock),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        };
        if rc != 0 {
            return Ok(None);
        }
    }
    Ok(Some(ExecutorGuard {
        inner: std::sync::Mutex::new(Some(StoreLock(lock))),
    }))
}
fn policy_stop_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.policy-stops.json",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(STORE_FILE)
    ))
}

/// Write `wire` (the queue's rows, already sealed where this process can) and the reason sidecar.
fn save_wire(path: &Path, wire: &WireStore, queue: &ApprovalQueue) -> Result<(), String> {
    let data = serde_json::to_vec(wire).map_err(|_| "cannot encode approval store".to_string())?;
    // Keep schema 1 byte-shape compatible with older desktop/MCP readers, whose terminal
    // records deny unknown fields. Optional reasons live beside it under the SAME queue lock.
    // Save the authoritative queue first: interruption may lose a reason, never an approval.
    save_bytes(path, &data)?;
    let stops: Vec<_> = queue
        .terminal_records()
        .iter()
        .filter(|row| {
            row.status == ApprovalStatus::SendFailed
                && row.reason.as_deref() == Some("provider_policy_stop")
        })
        .map(|row| (row.id.0, row.resolved_ms))
        .collect();
    let sidecar = policy_stop_path(path);
    if !stops.is_empty() || sidecar.exists() {
        let data =
            serde_json::to_vec(&stops).map_err(|_| "cannot encode approval reasons".to_string())?;
        save_bytes(&sidecar, &data)?;
    }
    Ok(())
}

pub(crate) fn save_bytes(path: &Path, data: &[u8]) -> Result<(), String> {
    if data.len() > MAX_STORE_BYTES as usize {
        return Err("approval store too large".into());
    }
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(STORE_FILE);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "clock error".to_string())?
        .as_nanos();
    let temp = path.with_file_name(format!(".{filename}.{}.{}.tmp", std::process::id(), nonce));
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    #[cfg(unix)]
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|_| "cannot write approval store".to_string())?;
    #[cfg(not(unix))]
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|_| "cannot write approval store".to_string())?;
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|_| "cannot protect approval store".to_string())?;
    file.write_all(data)
        .and_then(|_| file.sync_all())
        .map_err(|_| "cannot persist approval store".to_string())?;
    fs::rename(&temp, path).map_err(|_| "cannot replace approval store".to_string())?;
    // Directory fsync is a unix durability step; Windows cannot File::open a directory at all,
    // so this must not run there (it would fail every save).
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|_| "cannot sync approval store".to_string())?;
    }
    Ok(())
}
/// Why [`validate_enqueue`] refused a send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueRefusal {
    /// The request itself is malformed (empty or oversized destination, oversized body).
    Invalid(&'static str),
    /// The store already holds `MAX_PENDING` unresolved sends; retry after some resolve.
    QueueFull,
}

/// Reject only the reviewed pending IDs. Call inside `with_queue` so concurrent confirms and
/// producers cannot race the decision. Returned snapshots feed the desktop's normal L5 hook.
pub fn reject_pending(
    queue: &mut ApprovalQueue,
    ids: &[ApprovalId],
    now_ms: u64,
) -> Vec<(ApprovalId, SendAction, String)> {
    use shogun_agents::approval::{Decision, RejectCause};
    let mut rejected = Vec::new();
    for &id in ids {
        let snapshot = queue.action(id).cloned().and_then(|action| {
            queue
                .preview(id)
                .map(|preview| (action, preview.full_body.clone()))
        });
        if let Some((action, body)) = snapshot {
            if matches!(
                queue.reject(id, RejectCause::UserRejected, now_ms),
                Decision::Rejected(_)
            ) {
                rejected.push((id, action, body));
            }
        }
    }
    rejected
}

/// Enqueue-side twin of the load-time row checks. `load_queue` rejects a store whose rows break
/// these limits, so an enqueue that skips them persists a file every later load refuses — one bad
/// request would brick the whole queue for every face. Callers must validate before `try_request`.
pub fn validate_enqueue(queue: &ApprovalQueue, preview: &Preview) -> Result<(), EnqueueRefusal> {
    if preview.destination.trim().is_empty() || preview.destination.len() > MAX_DESTINATION_BYTES {
        return Err(EnqueueRefusal::Invalid("invalid approval destination"));
    }
    if preview.full_body.len() > MAX_BODY_BYTES {
        return Err(EnqueueRefusal::Invalid("approval body too large"));
    }
    if queue.pending_len() >= MAX_PENDING {
        return Err(EnqueueRefusal::QueueFull);
    }
    Ok(())
}

/// Refuse an action the persisted store could not load back (issue #657): a capture-exclusion
/// change keeps its lists in the row (and a Visual recall widening its change, issue #782), so an oversized or empty one would brick the shared queue
/// the same way an oversized preview would. Every other action is covered by
/// [`validate_enqueue`].
pub fn validate_action(action: &SendAction) -> Result<(), EnqueueRefusal> {
    match action {
        SendAction::ResumeCapture { change } => {
            if !change.resumes_capture() {
                return Err(EnqueueRefusal::Invalid("capture exclusion change removes nothing"));
            }
            if capture_change_to_wire(change).len() > MAX_CAPTURE_CHANGE_BYTES {
                return Err(EnqueueRefusal::Invalid("capture exclusion lists too large to review"));
            }
            Ok(())
        }
        SendAction::UseSubscriptionAgent { delegate } if !plausible_delegate_id(delegate) => {
            Err(EnqueueRefusal::Invalid("unknown subscription delegate"))
        }
        SendAction::WidenVisualRecall { change } if !change.is_valid() => {
            Err(EnqueueRefusal::Invalid("visual recall change widens nothing"))
        }
        SendAction::ArchiveMailThread { thread_key, .. }
            if !shogun_agents::permission::plausible_mail_thread_key(thread_key) =>
        {
            Err(EnqueueRefusal::Invalid("invalid mail thread key"))
        }
        // The same refusal `try_request` makes, surfaced as a 400 with its reason.
        _ => shogun_agents::permission::send_target_refusal(action)
            .map_or(Ok(()), |reason| Err(EnqueueRefusal::Invalid(reason))),
    }
}

/// Called after any in-process write that changed the persisted queue (issue #357). The desktop
/// installs one that pushes "approvals changed" to the notch panel; every other process leaves
/// it unset. Writers in other processes (CLI / REST daemon) cannot reach it — the panel's slow
/// backstop poll covers those.
static ON_CHANGE: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// Install the change hook. First call wins; returns whether this call installed it.
pub fn set_on_change(hook: fn()) -> bool {
    ON_CHANGE.set(hook).is_ok()
}

fn changed() {
    if let Some(hook) = ON_CHANGE.get() {
        hook();
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_key() -> ApprovalSealKey {
        ApprovalSealKey::from_db_key(&shogun_memory::DbKey::new([0x5a; 32]))
    }
    fn sealed(path: &Path) -> ApprovalStore {
        ApprovalStore::new(path.to_path_buf(), Some(test_key()))
    }
    fn keyless(path: &Path) -> ApprovalStore {
        ApprovalStore::new(path.to_path_buf(), None)
    }
    fn path() -> PathBuf {
        // A clock reading is not unique across parallel test threads: two tests that land in the
        // same tick share a file, and `rejects_invalid_or_duplicate_wire` writes 0644 on purpose,
        // so its neighbour fails the privacy check. Count instead of timing.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "shogun-l3-{}-{}.json",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }
    #[test]
    fn resolved_slack_label_and_id_survive_sealed_storage() {
        let action = SendAction::PostMessage { channel: "C0123ABCD".into() };
        let mut preview = Preview::for_send(&action, "hello", Route::DirectMcp);
        preview.destination = "#design · Acme".into();
        let row = pending_to_wire(&PendingRecord { id: ApprovalId(1), action: action.clone(), preview,
            origin: ApprovalOrigin::Ui, created_ms: 1 });
        let key = test_key();
        let sealed = seal_row(&key, &row).expect("test");
        assert!(!sealed.destination.contains("design"));
        assert!(!sealed.destination.contains("C0123ABCD"));
        let (opened, carried) = open_row(Some(&key), sealed).expect("test");
        assert!(carried.is_none());
        let loaded = pending_from_wire(opened, false).expect("test");
        assert_eq!(loaded.action, action);
        assert_eq!(loaded.preview.destination, "#design · Acme");
        let mut queue = ApprovalQueue::import_with_terminal(2, vec![loaded], vec![], vec![]);
        let decision = queue.confirm(ApprovalId(1), shogun_agents::approval::ConfirmIntent::DedicatedButton, 2);
        match decision {
            shogun_agents::approval::Decision::Confirmed(confirmed) => {
                assert_eq!(confirmed.action, action);
                assert_eq!(confirmed.preview.destination, "#design · Acme");
            }
            _ => panic!("dedicated confirmation must retain the resolved target"),
        }
        assert!(slack_destination(r##"{"id":"#design","label":"#design · Acme"}"##).is_err());
    }

    #[test]
    fn reply_marker_is_sealed_and_survives_a_keyless_status_poll() {
        let path = path();
        let store = sealed(&path);
        let action = SendAction::SendEmail { to: "a@b.com".into() };
        let marker = shogun_agents::approval::ReplyThread {
            thread_id: "private-thread-42".into(), latest_message_id: "message-7".into(),
        };
        let id = store.with_queue(|q| {
            let mut preview = Preview::for_send(&action, "private draft", Route::DirectMcp);
            preview.reply_thread = Some(marker.clone());
            q.try_request(action.clone(), preview, ApprovalOrigin::Mcp, 1)
        }).expect("test").expect("test");
        assert!(!fs::read_to_string(&path).expect("test").contains("private-thread-42"));
        // Force a save while the process cannot decrypt pending rows.
        keyless(&path).with_queue(|q| q.record_policy_stop(2)).expect("test").expect("test");
        store.with_queue(|q| assert_eq!(q.preview(id).expect("test").reply_thread, Some(marker))).expect("test");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn bulk_reject_preserves_in_flight_and_frees_capacity() {
        use shogun_agents::approval::{ConfirmIntent, Decision};
        let path = path();
        let store = sealed(&path);
        let send = SendAction::PostMessage {
            channel: "C0123ABCD".into(),
        };
        let preview = Preview::for_send(&send, "private", Route::DirectMcp);
        let (in_flight, ids) = store
            .with_queue(|q| {
                let in_flight = q
                    .try_request(send.clone(), preview.clone(), ApprovalOrigin::Api, 1)
                    .expect("queue in-flight request");
                assert!(matches!(
                    q.confirm(in_flight, ConfirmIntent::DedicatedButton, 2),
                    Decision::Confirmed(_)
                ));
                let ids: Vec<_> = (0..MAX_PENDING)
                    .map(|_| {
                        q.try_request(send.clone(), preview.clone(), ApprovalOrigin::Mcp, 3)
                            .expect("queue pending request")
                    })
                    .collect();
                assert_eq!(
                    validate_enqueue(q, &preview),
                    Err(EnqueueRefusal::QueueFull)
                );
                (in_flight, ids)
            })
            .expect("persist full queue");
        let rejected = store
            .with_queue(|q| {
                let mut reviewed = ids.clone();
                reviewed.extend([in_flight, ids[0]]);
                reject_pending(q, &reviewed, 4)
            })
            .expect("persist bulk rejection");
        assert_eq!(rejected.len(), MAX_PENDING);
        assert!(rejected
            .iter()
            .all(|(_, action, body)| action == &send && body == "private"));
        let loaded = store.load_queue().expect("reload rejected queue");
        assert_eq!(loaded.in_flight_ids(), &[in_flight]);
        assert_eq!(loaded.pending_len(), 0);
        assert!(ids
            .iter()
            .all(|id| loaded.status(*id) == Some(ApprovalStatus::Rejected)));
        assert_eq!(validate_enqueue(&loaded, &preview), Ok(()));
        let new_id = store
            .with_queue(|q| {
                q.try_request(send, preview, ApprovalOrigin::Mcp, 5)
                    .expect("enqueue after freeing capacity")
            })
            .expect("persist new request");
        store
            .with_queue(|q| {
                assert!(reject_pending(q, &ids, 6).is_empty());
                assert_eq!(q.status(new_id), Some(ApprovalStatus::Pending));
            })
            .expect("new arrivals survive stale confirmation");
        fs::remove_file(path).expect("remove test queue");
    }

    #[test]
    fn assistant_origin_round_trips() {
        assert_eq!(origin_parse(origin_wire(ApprovalOrigin::Assistant)), Ok(ApprovalOrigin::Assistant));
    }

    #[test]
    fn change_hook_fires_on_writes_only() {
        thread_local! {
            static FIRED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        }
        fn hook() {
            FIRED.with(|f| f.set(f.get() + 1));
        }
        // Tests share the process-wide hook; the counter is per thread, so neighbours running
        // `with_queue` on other threads cannot move it.
        set_on_change(hook);
        let path = path();
        let store = sealed(&path);
        let send = SendAction::SendEmail {
            to: "a@b.com".into(),
        };
        store.with_queue(|q| q.pending_len()).unwrap();
        assert_eq!(FIRED.with(std::cell::Cell::get), 0, "a read-only pass is not a change");
        store.with_queue(|q| {
            q.try_request(
                send.clone(),
                Preview::for_send(&send, "private", Route::ViaComposio),
                ApprovalOrigin::Mcp,
                1,
            )
        })
        .unwrap()
        .unwrap();
        assert_eq!(FIRED.with(std::cell::Cell::get), 1);
        store.with_queue(|q| q.pending_len()).unwrap();
        assert_eq!(FIRED.with(std::cell::Cell::get), 1);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn persists_pending_and_body_free_terminal() {
        let path = path();
        let store = sealed(&path);
        let send = SendAction::SendEmail {
            to: "a@b.com".into(),
        };
        let id = store.with_queue(|q| {
            q.try_request(
                send.clone(),
                Preview::for_send(&send, "private", Route::ViaComposio),
                ApprovalOrigin::Mcp,
                1,
            )
        })
        .unwrap()
        .unwrap();
        store.with_queue(|q| {
            assert!(matches!(
                q.confirm(
                    id,
                    shogun_agents::approval::ConfirmIntent::DedicatedButton,
                    2
                ),
                shogun_agents::approval::Decision::Confirmed(_)
            ));
            assert!(q.mark_status(id, ApprovalStatus::Sent, 3));
        })
        .unwrap();
        assert_eq!(
            store.load_queue().unwrap().status(id),
            Some(ApprovalStatus::Sent)
        );
        assert!(!fs::read_to_string(&path).unwrap().contains("private"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn a_writing_style_approval_survives_the_store_with_its_preview() {
        // Issue #654: the durable queue carries the enable across processes (MCP/REST write it,
        // the desktop drains it), so it must load back as the same action and preview.
        let path = path();
        let store = sealed(&path);
        let (action, preview) = shogun_agents::approval::writing_style_request();
        let id = store.with_queue(|q| {
            validate_enqueue(q, &preview).unwrap();
            q.try_request(action.clone(), preview.clone(), ApprovalOrigin::Api, 1)
        })
        .unwrap()
        .unwrap();
        let loaded = store.load_queue().unwrap();
        assert_eq!(loaded.action(id), Some(&SendAction::ShareWritingStyle));
        assert_eq!(loaded.preview(id), Some(&preview));
        assert_eq!(loaded.origin(id), Some(ApprovalOrigin::Api));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn a_capture_unpause_approval_survives_the_store_with_its_preview() {
        // Issue #897: MCP / REST write the request and the desktop drains it, so it must load
        // back as the same action and preview.
        // The pause it was made against comes back exactly: confirming checks it (fail closed).
        let path = path();
        let store = sealed(&path);
        for pause in [
            PausedCapture { resume_at_ms: None },
            PausedCapture { resume_at_ms: Some(1_800_000_000_000) },
        ] {
            let (action, preview) = shogun_agents::approval::capture_unpause_request(pause);
            let id = store.with_queue(|q| {
                validate_enqueue(q, &preview).unwrap();
                q.try_request(action.clone(), preview.clone(), ApprovalOrigin::Api, 1)
            })
            .unwrap()
            .unwrap();
            let loaded = store.load_queue().unwrap();
            assert_eq!(loaded.action(id), Some(&SendAction::UnpauseCapture { pause }));
            assert_eq!(loaded.preview(id), Some(&preview));
        }
        // A tampered snapshot does not load.
        assert!(action_from_wire("unpause_capture", r#"{"resume_at_ms":-5}"#.into()).is_err());
        assert!(action_from_wire("unpause_capture", "this Mac".into()).is_err());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn a_subscription_agent_approval_survives_the_store_with_its_delegate() {
        // Issue #110: the desktop applies what it loads, so the delegate id must come back exactly.
        let path = path();
        let store = sealed(&path);
        let (action, preview) =
            shogun_agents::approval::subscription_agent_request("gemini-cli", "Gemini CLI", "Google account");
        let id = store.with_queue(|q| {
            validate_enqueue(q, &preview).unwrap();
            validate_action(&action).unwrap();
            q.try_request(action.clone(), preview.clone(), ApprovalOrigin::Mcp, 1)
        })
        .unwrap()
        .unwrap();
        let loaded = store.load_queue().unwrap();
        assert_eq!(
            loaded.action(id),
            Some(&SendAction::UseSubscriptionAgent { delegate: "gemini-cli".into() })
        );
        assert_eq!(loaded.preview(id), Some(&preview));
        let _ = fs::remove_file(path);
        // An implausible id is refused before the store, and never loads back from a tampered row.
        let odd = SendAction::UseSubscriptionAgent { delegate: "../../bin/sh".into() };
        assert!(matches!(validate_action(&odd), Err(EnqueueRefusal::Invalid(_))));
        assert!(action_from_wire("use_subscription_agent", "Claude Code".into()).is_err());
    }

    #[test]
    fn a_capture_resume_approval_survives_the_store_with_its_lists() {
        // Issue #657: the desktop applies what it loads, so the requested lists and the entries
        // the card names must come back exactly, not just the "this Mac" label.
        let path = path();
        let store = sealed(&path);
        let change = CaptureExclusionChange {
            apps: vec!["com.example.Keep".into(), "com.example.New".into()],
            websites: vec!["new.example".into()],
            resumed_apps: vec!["com.example.Secret".into()],
            resumed_websites: vec!["private.example".into()],
        };
        let (action, preview) = shogun_agents::approval::capture_resume_request(change.clone());
        let id = store.with_queue(|q| {
            validate_enqueue(q, &preview).unwrap();
            validate_action(&action).unwrap();
            q.try_request(action.clone(), preview.clone(), ApprovalOrigin::Mcp, 1)
        })
        .unwrap()
        .unwrap();
        let loaded = store.load_queue().unwrap();
        assert_eq!(loaded.action(id), Some(&SendAction::ResumeCapture { change }));
        assert_eq!(loaded.preview(id), Some(&preview));
        assert_eq!(loaded.origin(id), Some(ApprovalOrigin::Mcp));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn a_capture_change_that_resumes_nothing_or_is_huge_is_refused_before_the_store() {
        let empty = SendAction::ResumeCapture { change: CaptureExclusionChange::default() };
        assert!(matches!(validate_action(&empty), Err(EnqueueRefusal::Invalid(_))));
        let huge = SendAction::ResumeCapture {
            change: CaptureExclusionChange {
                apps: (0..10_000).map(|n| format!("com.example.app{n}")).collect(),
                resumed_apps: vec!["com.example.Secret".into()],
                ..Default::default()
            },
        };
        assert!(matches!(validate_action(&huge), Err(EnqueueRefusal::Invalid(_))));
        // And a tampered row that resumes nothing never loads as an approvable change.
        assert!(action_from_wire(
            "resume_capture",
            r#"{"apps":[],"websites":[],"resumed_apps":[],"resumed_websites":[]}"#.into()
        )
        .is_err());
    }

    #[test]
    fn a_mail_archive_request_survives_the_store_and_a_tampered_one_does_not_load() {
        // Issue #779: the desktop records what it loads, so the thread and the direction must
        // come back exactly, not just the thread key the card shows.
        let path = path();
        let store = sealed(&path);
        for unarchive in [false, true] {
            let (action, preview) = shogun_agents::approval::mail_archive_request("gmail:18f3", unarchive);
            let id = store.with_queue(|q| {
                validate_enqueue(q, &preview).unwrap();
                validate_action(&action).unwrap();
                q.try_request(action.clone(), preview.clone(), ApprovalOrigin::Mcp, 1)
            })
            .unwrap()
            .unwrap();
            let loaded = store.load_queue().unwrap();
            assert_eq!(
                loaded.action(id),
                Some(&SendAction::ArchiveMailThread { thread_key: "gmail:18f3".into(), unarchive })
            );
            assert_eq!(loaded.preview(id), Some(&preview));
            assert_eq!(loaded.origin(id), Some(ApprovalOrigin::Mcp));
        }
        let _ = fs::remove_file(path);

        // Never queued, never loaded: a key that cannot name a Gmail thread.
        for bad in ["", "gmail:", "slack:C1", "gmail:a b"] {
            let action = SendAction::ArchiveMailThread { thread_key: bad.into(), unarchive: false };
            assert!(matches!(validate_action(&action), Err(EnqueueRefusal::Invalid(_))), "{bad:?}");
            assert!(action_from_wire("archive_mail_thread", mail_archive_to_wire(bad, false)).is_err(), "{bad:?}");
        }
        // The bare thread key (the preview's label) is not the wire form, and nothing extra loads.
        assert!(action_from_wire("archive_mail_thread", "gmail:18f3".into()).is_err());
        assert!(action_from_wire(
            "archive_mail_thread",
            r#"{"thread_key":"gmail:18f3","unarchive":false,"extra":1}"#.into()
        )
        .is_err());
        assert!(action_from_wire("archive_mail_thread", r#"{"thread_key":"gmail:18f3"}"#.into()).is_err());
    }

    #[test]
    fn a_visual_recall_widening_survives_the_store_and_a_tampered_one_does_not_load() {
        // Issue #782: the desktop applies what it loads, so the requested change must come back
        // exactly, not just the "this Mac" label.
        let path = path();
        let store = sealed(&path);
        for change in [
            VisualRecallWidening { enable: true, retention_days: None, previous_retention_days: 3 },
            VisualRecallWidening { enable: false, retention_days: Some(3650), previous_retention_days: 7 },
        ] {
            let (action, preview) = shogun_agents::approval::visual_recall_request(change.clone());
            let id = store.with_queue(|q| {
                validate_enqueue(q, &preview).unwrap();
                validate_action(&action).unwrap();
                q.try_request(action.clone(), preview.clone(), ApprovalOrigin::Api, 1)
            })
            .unwrap()
            .unwrap();
            let loaded = store.load_queue().unwrap();
            assert_eq!(loaded.action(id), Some(&SendAction::WidenVisualRecall { change }));
            assert_eq!(loaded.preview(id), Some(&preview));
        }
        let _ = fs::remove_file(path);

        // Never queued, never loaded: a change that widens nothing or is out of bounds.
        for bad in [
            VisualRecallWidening { enable: false, retention_days: None, previous_retention_days: 3 },
            VisualRecallWidening { enable: false, retention_days: Some(2), previous_retention_days: 3 },
            VisualRecallWidening { enable: false, retention_days: Some(3651), previous_retention_days: 3 },
            VisualRecallWidening { enable: true, retention_days: None, previous_retention_days: 0 },
        ] {
            let action = SendAction::WidenVisualRecall { change: bad.clone() };
            assert!(matches!(validate_action(&action), Err(EnqueueRefusal::Invalid(_))), "{bad:?}");
            assert!(action_from_wire("widen_visual_recall", visual_recall_to_wire(&bad)).is_err(), "{bad:?}");
        }
        assert!(action_from_wire("widen_visual_recall", "this Mac".into()).is_err());
        assert!(action_from_wire(
            "widen_visual_recall",
            r#"{"enable":true,"retention_days":null,"previous_retention_days":3,"extra":1}"#.into()
        )
        .is_err());
    }

    #[test]
    fn persists_allowlisted_policy_stop_reason_without_bodies() {
        let path = path();
        let store = sealed(&path);
        let send = SendAction::SendEmail {
            to: "a@b.com".into(),
        };
        let id = store.with_queue(|q| {
            q.try_request(
                send.clone(),
                Preview::for_send(&send, "private", Route::ViaComposio),
                ApprovalOrigin::Mcp,
                1,
            )
        })
        .unwrap()
        .unwrap();
        store.with_queue(|q| {
            assert!(matches!(
                q.confirm(
                    id,
                    shogun_agents::approval::ConfirmIntent::DedicatedButton,
                    2
                ),
                shogun_agents::approval::Decision::Confirmed(_)
            ));
            assert!(q.mark_status_with_reason(
                id,
                ApprovalStatus::SendFailed,
                3,
                Some("provider_policy_stop"),
            ));
        })
        .unwrap();
        let loaded = store.load_queue().unwrap();
        assert_eq!(loaded.status(id), Some(ApprovalStatus::SendFailed));
        assert_eq!(loaded.failure_reason(id), Some("provider_policy_stop"));
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("reason"));
        assert!(!text.contains("private"));
        let _ = fs::remove_file(policy_stop_path(&path));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn policy_stop_metadata_keeps_legacy_readers_and_pending_approvals_working(
    ) -> Result<(), String> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LegacyTerminal {
            id: u64,
            status: String,
            resolved_ms: u64,
        }
        let path = path();
        let store = sealed(&path);
        let (stopped, pending) = store.with_queue(|q| {
            let stopped = q.record_policy_stop(3)?;
            let action = SendAction::PostMessage {
                channel: "C0123ABCD".into(),
            };
            let pending = q.try_request(
                action.clone(),
                Preview::for_send(&action, "pending draft", Route::DirectMcp),
                ApprovalOrigin::Ui,
                4,
            )?;
            Ok::<_, &str>((stopped, pending))
        })?
        .map_err(str::to_string)?;
        let text = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let wire: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let legacy: LegacyTerminal =
            serde_json::from_value(wire["terminal"][0].clone()).map_err(|e| e.to_string())?;
        assert_eq!(
            (legacy.id, legacy.status.as_str(), legacy.resolved_ms),
            (stopped.0, "send_failed", 3)
        );
        assert_eq!(
            store.load_queue()?.failure_reason(stopped),
            Some("provider_policy_stop")
        );
        // Missing/corrupt optional metadata cannot strand unrelated pending L3 work.
        fs::write(policy_stop_path(&path), "broken").map_err(|e| e.to_string())?;
        let loaded = store.load_queue()?;
        assert_eq!(loaded.status(pending), Some(ApprovalStatus::Pending));
        assert_eq!(loaded.status(stopped), Some(ApprovalStatus::SendFailed));
        assert_eq!(loaded.failure_reason(stopped), None);
        fs::remove_file(policy_stop_path(&path)).map_err(|e| e.to_string())?;
        fs::remove_file(path).map_err(|e| e.to_string())?;
        Ok(())
    }

    #[test]
    fn polling_live_in_flight_work_cannot_force_failure() {
        let path = path();
        let store = sealed(&path);
        let send = SendAction::PostMessage {
            channel: "C0123ABCD".into(),
        };
        let id = store.with_queue(|q| {
            q.try_request(
                send.clone(),
                Preview::for_send(&send, "private", Route::DirectMcp),
                ApprovalOrigin::Api,
                1,
            )
        })
        .unwrap()
        .unwrap();

        store.with_queue(|q| {
            assert!(matches!(
                q.confirm(
                    id,
                    shogun_agents::approval::ConfirmIntent::DedicatedButton,
                    2
                ),
                shogun_agents::approval::Decision::Confirmed(_)
            ));
        })
        .unwrap();

        store.with_queue(|q| {
            assert_eq!(q.status(id), Some(ApprovalStatus::Pending));
        })
        .unwrap();

        store.with_queue(|q| {
            assert!(q.mark_status(id, ApprovalStatus::Sent, 3));
        })
        .unwrap();
        assert_eq!(
            store.load_queue().unwrap().status(id),
            Some(ApprovalStatus::Sent)
        );
        let _ = fs::remove_file(path);
    }
    #[test]
    fn with_queue_refuses_to_persist_a_row_load_queue_would_reject() {
        let path = path();
        let store = sealed(&path);
        // Seed one valid row so a store file exists.
        let good = SendAction::PostMessage {
            channel: "C0123ABCD".into(),
        };
        store.with_queue(|q| {
            q.try_request(
                good.clone(),
                Preview::for_send(&good, "x", Route::DirectMcp),
                ApprovalOrigin::Ui,
                1,
            )
        })
        .unwrap()
        .unwrap();
        // A blank destination is exactly what `load_queue` refuses. The desktop propose path
        // enqueues without `validate_enqueue`, so the store itself must refuse the write instead
        // of persisting a file every later load rejects (bricking the queue for every face).
        // A blank document title passes `send_target_refusal`, so the store's own guard is what
        // refuses it here.
        let bad = SendAction::CreateDocument {
            title: "   ".into(),
        };
        let result = store.with_queue(|q| {
            q.try_request(
                bad.clone(),
                Preview::for_send(&bad, "y", Route::DirectMcp),
                ApprovalOrigin::Ui,
                2,
            )
        });
        assert!(result.is_err(), "an invalid row must not persist");
        // The on-disk store is still loadable and still holds only the good row.
        let queue = store.load_queue().unwrap();
        assert_eq!(queue.pending_len(), 1);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn rejects_invalid_or_duplicate_wire() {
        let path = path();
        let store = sealed(&path);
        fs::write(&path, r#"{"schema":1,"next_id":2,"pending":[{"id":1,"kind":"send_email","destination":"a","full_body":"x","route":"direct","origin":"mcp","created_ms":1}],"terminal":[],"in_flight":[]}"#).unwrap();
        assert!(store.load_queue().is_err());
        let _ = fs::remove_file(path);
    }
    #[test]
    fn recovers_in_flight_as_outcome_unknown() {
        let path = path();
        let store = sealed(&path);
        let send = SendAction::PostMessage {
            channel: "C0123ABCD".into(),
        };
        let id = store.with_queue(|q| {
            let id = q
                .try_request(
                    send.clone(),
                    Preview::for_send(&send, "x", Route::DirectMcp),
                    ApprovalOrigin::Api,
                    1,
                )
                .unwrap();
            let _ = q.confirm(
                id,
                shogun_agents::approval::ConfirmIntent::DedicatedButton,
                2,
            );
            id
        })
        .unwrap();
        assert_eq!(store.recover_in_flight(3).unwrap(), vec![id]);
        assert_eq!(
            store.load_queue().unwrap().status(id),
            Some(ApprovalStatus::OutcomeUnknown)
        );
        let _ = fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn a_live_executor_claim_blocks_a_second_claim() {
        let path = path();
        let first = try_claim_executor(&path).unwrap().expect("first claim");
        assert!(
            try_claim_executor(&path).unwrap().is_none(),
            "second instance must not steal the executor"
        );
        drop(first);
        assert!(
            try_claim_executor(&path).unwrap().is_some(),
            "claim is free after the live executor exits"
        );
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(executor_lock_path(&path));
    }
    // sealing at rest (audit 2026-10-10 M2)

    fn email(to: &str, body: &str) -> (SendAction, Preview) {
        let send = SendAction::SendEmail { to: to.into() };
        let preview = Preview::for_send(&send, body, Route::DirectMcp);
        (send, preview)
    }

    #[test]
    fn pending_rows_are_sealed_on_disk_and_round_trip_with_the_key() {
        let path = path();
        let store = sealed(&path);
        let (send, preview) = email("alice@example.com", "Hi Alice, the private draft body");
        let id = store
            .with_queue(|q| q.try_request(send.clone(), preview.clone(), ApprovalOrigin::Ui, 1))
            .unwrap()
            .unwrap();
        // The change itself stays readable for a capture-exclusion row; its body is sealed.
        let change = CaptureExclusionChange {
            apps: vec!["com.example.Keep".into()],
            resumed_apps: vec!["com.example.Secret".into()],
            ..Default::default()
        };
        let (resume, resume_preview) = shogun_agents::approval::capture_resume_request(change.clone());
        let resume_id = store
            .with_queue(|q| q.try_request(resume.clone(), resume_preview.clone(), ApprovalOrigin::Mcp, 2))
            .unwrap()
            .unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains(SEALED_PREFIX), "{text}");
        assert!(!text.contains("private draft"), "body in the clear: {text}");
        assert!(!text.contains("alice@example.com"), "recipient in the clear: {text}");
        assert!(text.contains("com.example.Secret"), "the change itself must stay loadable: {text}");
        assert!(!text.contains(&resume_preview.full_body), "capture-change body in the clear: {text}");

        let loaded = store.load_queue().unwrap();
        assert_eq!(loaded.action(id), Some(&send));
        assert_eq!(loaded.preview(id), Some(&preview));
        assert_eq!(loaded.action(resume_id), Some(&SendAction::ResumeCapture { change }));
        assert_eq!(loaded.preview(resume_id), Some(&resume_preview));
        assert!(store.sealed_unreadable().unwrap().is_empty());
        // The sealed form is a fresh nonce per write: two saves of the same row never match.
        store.with_queue(|q| q.record_policy_stop(3)).unwrap().unwrap();
        let again = fs::read_to_string(&path).unwrap();
        let first = text.find(SEALED_PREFIX).map(|at| &text[at..at + 40]).unwrap();
        assert!(!again.contains(first), "a re-save must re-seal with a new nonce");
        assert_eq!(store.load_queue().unwrap().preview(id), Some(&preview));
        let _ = fs::remove_file(policy_stop_path(&path));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn a_tampered_or_foreign_key_sealed_row_does_not_load() {
        let path = path();
        let store = sealed(&path);
        let (send, preview) = email("alice@example.com", "private");
        store
            .with_queue(|q| q.try_request(send, preview, ApprovalOrigin::Ui, 1))
            .unwrap()
            .unwrap();
        // Another key (a different memory DB) cannot read the rows.
        let other = ApprovalStore::new(
            path.clone(),
            Some(ApprovalSealKey::from_db_key(&shogun_memory::DbKey::new([0x11; 32]))),
        );
        assert!(other.load_queue().is_err());
        // One flipped character inside the ciphertext fails authentication.
        let text = fs::read_to_string(&path).unwrap();
        let at = text.find(SEALED_PREFIX).unwrap() + SEALED_PREFIX.len() + 20;
        let mut bytes = text.into_bytes();
        bytes[at] = if bytes[at] == b'A' { b'B' } else { b'A' };
        save_bytes(&path, &bytes).unwrap();
        assert!(store.load_queue().is_err());
        // A ciphertext moved to another row id does not open there either.
        let good = sealed(&path);
        let _ = fs::remove_file(&path);
        let (send, preview) = email("alice@example.com", "private");
        good.with_queue(|q| q.try_request(send, preview, ApprovalOrigin::Ui, 1)).unwrap().unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let moved = text.replacen(r#""id":1,"#, r#""id":7,"#, 1);
        assert_ne!(moved, text);
        save_bytes(&path, moved.as_bytes()).unwrap();
        assert!(good.load_queue().is_err());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn legacy_plaintext_rows_load_and_are_sealed_on_the_next_save() {
        let path = path();
        let store = sealed(&path);
        save_bytes(
            &path,
            br#"{"schema":1,"next_id":2,"pending":[{"id":1,"kind":"send_email","destination":"bob@example.com","full_body":"old plaintext draft","route":"direct","origin":"ui","created_ms":1}],"terminal":[],"in_flight":[]}"#,
        )
        .unwrap();
        let loaded = store.load_queue().unwrap();
        assert_eq!(loaded.action(ApprovalId(1)), Some(&SendAction::SendEmail { to: "bob@example.com".into() }));
        assert_eq!(loaded.preview(ApprovalId(1)).map(|p| p.full_body.as_str()), Some("old plaintext draft"));
        // The first write seals what was in the clear.
        store.with_queue(|q| q.record_policy_stop(3)).unwrap().unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("old plaintext draft"), "{text}");
        assert!(!text.contains("bob@example.com"), "{text}");
        assert_eq!(
            store.load_queue().unwrap().preview(ApprovalId(1)).map(|p| p.full_body.as_str()),
            Some("old plaintext draft")
        );
        let _ = fs::remove_file(policy_stop_path(&path));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn without_the_key_sealed_rows_list_as_placeholders_and_nothing_new_is_stored() {
        let path = path();
        let store = sealed(&path);
        let (send, preview) = email("alice@example.com", "private");
        let id = store
            .with_queue(|q| q.try_request(send.clone(), preview.clone(), ApprovalOrigin::Ui, 1))
            .unwrap()
            .unwrap();
        let headless = keyless(&path);
        assert!(!headless.can_unseal());
        // Listing works, with the placeholder where the recipient and body would be.
        let loaded = headless.load_queue().unwrap();
        assert_eq!(loaded.status(id), Some(ApprovalStatus::Pending));
        assert_eq!(loaded.action(id), Some(&SendAction::SendEmail { to: SEALED_PLACEHOLDER.into() }));
        let seen = loaded.preview(id).unwrap();
        assert_eq!(seen.full_body, SEALED_PLACEHOLDER);
        assert_eq!(seen.destination, SEALED_PLACEHOLDER);
        assert_eq!(headless.sealed_unreadable().unwrap(), vec![id]);
        // A new pending row is refused — never written in the clear.
        let (other, other_preview) = email("carol@example.com", "another private draft");
        let refused = headless.with_queue(|q| q.try_request(other, other_preview, ApprovalOrigin::Api, 2));
        assert!(matches!(refused, Err(ref why) if why.contains("sealing key")), "{refused:?}");
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("another private draft") && !text.contains("carol@example.com"));
        // Confirming a row it cannot read is refused too.
        let refused = headless.with_queue(|q| {
            q.confirm(id, shogun_agents::approval::ConfirmIntent::DedicatedButton, 3)
        });
        assert!(refused.is_err());
        assert_eq!(sealed(&path).load_queue().unwrap().status(id), Some(ApprovalStatus::Pending));
        // A write it may make (a terminal row) carries the ciphertext through untouched.
        headless.with_queue(|q| q.record_policy_stop(4)).unwrap().unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains(SEALED_PLACEHOLDER), "placeholder written over a body: {text}");
        assert_eq!(sealed(&path).load_queue().unwrap().preview(id), Some(&preview));
        let _ = fs::remove_file(policy_stop_path(&path));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn a_keyless_store_can_still_recover_in_flight_rows_and_expire() {
        let path = path();
        let store = sealed(&path);
        let (send, preview) = email("alice@example.com", "private");
        let id = store
            .with_queue(|q| {
                let id = q.try_request(send.clone(), preview.clone(), ApprovalOrigin::Ui, 1).unwrap();
                let _ = q.confirm(id, shogun_agents::approval::ConfirmIntent::DedicatedButton, 2);
                id
            })
            .unwrap();
        let headless = keyless(&path);
        assert_eq!(headless.recover_in_flight(3).unwrap(), vec![id]);
        assert_eq!(store.load_queue().unwrap().status(id), Some(ApprovalStatus::OutcomeUnknown));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn the_seal_key_is_derived_per_memory_key_and_never_printed() {
        let a = ApprovalSealKey::from_db_key(&shogun_memory::DbKey::new([1; 32]));
        let b = ApprovalSealKey::from_db_key(&shogun_memory::DbKey::new([2; 32]));
        let a2 = ApprovalSealKey::from_db_key(&shogun_memory::DbKey::new([1; 32]));
        assert!(a.0.is_some(), "HKDF-Expand to 32 bytes completes");
        assert_eq!(a.0, a2.0, "derivation is deterministic");
        assert_ne!(a.0, b.0);
        assert_ne!(a.0, Some([1; 32]), "the memory key itself is never the sealing key");
        let shown = format!("{a:?}");
        assert!(shown.contains("redacted"));
        assert!(!shown.contains("01"));
        let store = ApprovalStore::new(PathBuf::from("x"), Some(a));
        assert!(!format!("{store:?}").contains("01"));
    }

    #[test]
    fn a_key_whose_derivation_did_not_complete_seals_and_unseals_nothing() {
        // The unreachable HKDF failure branch: the fallback is refusal, not the DB key.
        let none = ApprovalSealKey(None);
        let aad = seal_aad(1, "send_email", "full_body");
        assert!(seal_field(&none, &aad, "private").is_err());
        let sealed = seal_field(&test_key(), &aad, "private").unwrap();
        assert!(unseal_field(&none, &aad, &sealed).is_err());
        assert!(!format!("{none:?}").contains("None"), "redacted whatever it holds");
        // A store holding it stores no new pending row (no plaintext fallback either).
        let path = path();
        let store = ApprovalStore::new(path.clone(), Some(none));
        let send = SendAction::SendEmail { to: "a@b.com".into() };
        let out = store.with_queue(|q| {
            q.try_request(send.clone(), Preview::for_send(&send, "private", Route::DirectMcp), ApprovalOrigin::Mcp, 1)
        });
        assert!(out.is_err());
        assert!(!path.exists() || !fs::read_to_string(&path).unwrap_or_default().contains("private"));
        let _ = fs::remove_file(path);
    }

}
