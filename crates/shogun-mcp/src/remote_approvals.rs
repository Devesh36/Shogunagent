//! Remote L2/L3 approvals (ShogunAgent E15): how a hosted surface — the gateway's web page, its
//! Slack app, a webhook consumer acting through them — resolves an approval the sidecar queued.
//!
//! The [`ApprovalQueue`] (or the durable [`crate::approval_store::ApprovalStore`]) stays the one
//! source of truth: a remote decision is the same dedicated-button confirm or reject the desktop
//! performs, and nothing here grants anything on its own.
//!
//! - **Decision token.** When an approval is queued, the sidecar mints a short-lived token
//!   ([`ApprovalSigner::mint`]): `sat1.<base64url(claims)>.<base64url(HMAC-SHA256)>` over
//!   `shogun-approval-v1.<base64url(claims)>`, keyed with `SHOGUN_APPROVAL_SIGNING_KEY`, a secret
//!   the sidecar shares with the gateway only. The claims are the approval id, the principal (in
//!   multi-principal mode), the expiry (never later than the approval's own 10-minute window) and
//!   a random nonce. The approval URL is `<SHOGUN_APPROVAL_BASE_URL>/approve/<token>`.
//! - **Single use.** A decision presents the token and repeats its nonce; the [`ApprovalLedger`]
//!   consumes the nonce, so a replayed token is refused, and an expired one is refused before
//!   anything is looked up.
//! - **Who decided.** Every decision is recorded with `decided_by` and the time, in the ledger and
//!   in the principal's audit log.
//! - **Redacted summary.** [`summary`] is what a hosted surface may show a human: the kind, the
//!   tier, the recipient *class* (never the recipient), a short preview with secrets masked, and
//!   the times. Never the full body.
//!
//! Agents never decide: no MCP tool decides, and the REST decide route takes only a signed token
//! or the operator's admin credential (an agent's bearer is refused), so an agent cannot approve
//! its own L3.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use shogun_agents::approval::{
    ApprovalId, ApprovalQueue, ApprovalStatus, ConfirmIntent, Decision, RejectCause, APPROVAL_TIMEOUT_MS,
};
use shogun_agents::permission::SendAction;
use subtle::ConstantTimeEq;

/// The environment variable holding the shared signing key (at least 32 bytes).
pub const SIGNING_KEY_ENV: &str = "SHOGUN_APPROVAL_SIGNING_KEY";
/// The environment variable naming where a human approves (the gateway's public base URL).
pub const BASE_URL_ENV: &str = "SHOGUN_APPROVAL_BASE_URL";
/// The header a hosted surface carries the token in.
pub const TOKEN_HEADER: &str = "x-shogun-approval-token";
/// The token prefix (format version).
pub const TOKEN_PREFIX: &str = "sat1";
/// The MAC domain separator.
const MAC_DOMAIN: &str = "shogun-approval-v1.";
/// Shortest signing key accepted.
pub const MIN_KEY_BYTES: usize = 32;
/// Longest token accepted before any decoding.
pub const MAX_TOKEN_CHARS: usize = 1024;
/// Longest preview a summary shows (chars).
pub const PREVIEW_CHARS: usize = 280;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// HMAC-SHA256 (RFC 2104) over `sha2`, so this crate needs no other MAC dependency.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= block[i];
        opad[i] ^= block[i];
    }
    let inner = Sha256::new().chain_update(ipad).chain_update(message).finalize();
    let outer = Sha256::new().chain_update(opad).chain_update(inner).finalize();
    block.fill(0);
    outer.into()
}

/// The shared signing key. Never rendered (`Debug` is redacted), zeroed on drop.
pub struct ApprovalSigner {
    key: Vec<u8>,
}

impl std::fmt::Debug for ApprovalSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApprovalSigner(***redacted***)")
    }
}

impl Drop for ApprovalSigner {
    fn drop(&mut self) {
        self.key.fill(0);
    }
}

/// The claims a decision token carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenClaims {
    pub approval_id: u64,
    /// The principal whose queue entry this is (multi-principal mode), else `None`.
    pub principal: Option<String>,
    pub expires_ms: u64,
    pub nonce: String,
}

/// Why a token was refused. Never carries the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenError {
    Malformed,
    BadSignature,
    Expired,
}

impl TokenError {
    /// `(status, error code, message)` for a refusal.
    pub fn refusal(self) -> (u16, &'static str, &'static str) {
        match self {
            TokenError::Malformed => (400, "invalid_token", "The approval token is malformed. Open the approval link again."),
            TokenError::BadSignature => (403, "invalid_token", "The approval token was not issued for this server."),
            TokenError::Expired => (410, "token_expired", "The approval link has expired; the request may have timed out. Ask the agent to submit it again."),
        }
    }
}

impl ApprovalSigner {
    /// A signer over `key`; `None` when it is shorter than [`MIN_KEY_BYTES`].
    pub fn new(key: &[u8]) -> Option<Self> {
        (key.len() >= MIN_KEY_BYTES).then(|| Self { key: key.to_vec() })
    }

    /// From `SHOGUN_APPROVAL_SIGNING_KEY`; `None` when unset or too short.
    pub fn from_env() -> Option<Self> {
        let raw = std::env::var(SIGNING_KEY_ENV).ok()?;
        let signer = Self::new(raw.trim().as_bytes());
        drop(raw);
        signer
    }

    fn mac(&self, payload_b64: &str) -> [u8; 32] {
        hmac_sha256(&self.key, format!("{MAC_DOMAIN}{payload_b64}").as_bytes())
    }

    /// Mint a token for `approval_id`, expiring at `expires_ms`, with a fresh nonce.
    pub fn mint(&self, approval_id: u64, principal: Option<&str>, expires_ms: u64) -> Option<(String, TokenClaims)> {
        let mut raw = [0u8; 16];
        getrandom::fill(&mut raw).ok()?;
        let nonce: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let claims = TokenClaims { approval_id, principal: principal.map(str::to_string), expires_ms, nonce };
        Some((self.encode(&claims), claims))
    }

    fn encode(&self, claims: &TokenClaims) -> String {
        let payload = json!({ "a": claims.approval_id, "p": claims.principal, "e": claims.expires_ms, "n": claims.nonce });
        let payload_b64 = B64.encode(payload.to_string());
        let mac = B64.encode(self.mac(&payload_b64));
        format!("{TOKEN_PREFIX}.{payload_b64}.{mac}")
    }

    /// Verify `token` at `now_ms`: format, signature (constant time), expiry.
    pub fn verify(&self, token: &str, now_ms: u64) -> Result<TokenClaims, TokenError> {
        let token = token.trim();
        if token.is_empty() || token.len() > MAX_TOKEN_CHARS {
            return Err(TokenError::Malformed);
        }
        let mut parts = token.split('.');
        let (Some(TOKEN_PREFIX), Some(payload_b64), Some(mac_b64), None) = (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(TokenError::Malformed);
        };
        let given = B64.decode(mac_b64).map_err(|_| TokenError::Malformed)?;
        let expected = self.mac(payload_b64);
        if given.len() != expected.len() || expected.ct_eq(&given).unwrap_u8() != 1 {
            return Err(TokenError::BadSignature);
        }
        let payload: Value = B64
            .decode(payload_b64)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or(TokenError::Malformed)?;
        let approval_id = payload["a"].as_u64().filter(|id| *id > 0).ok_or(TokenError::Malformed)?;
        let expires_ms = payload["e"].as_u64().ok_or(TokenError::Malformed)?;
        let nonce = payload["n"].as_str().filter(|n| !n.is_empty() && n.len() <= 64).ok_or(TokenError::Malformed)?;
        let principal = match &payload["p"] {
            Value::Null => None,
            Value::String(p) => Some(p.clone()),
            _ => return Err(TokenError::Malformed),
        };
        if now_ms >= expires_ms {
            return Err(TokenError::Expired);
        }
        Ok(TokenClaims { approval_id, principal, expires_ms, nonce: nonce.to_string() })
    }
}

/// What the sidecar knows about an approval beyond the queue: whose it is, who asked, and the
/// decision once one is made. Body-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerEntry {
    pub principal: Option<String>,
    pub requested_by: String,
    pub level: String,
    pub created_ms: u64,
    pub decision: Option<DecisionRecord>,
}

/// A recorded decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRecord {
    pub decision: RemoteDecision,
    pub decided_by: String,
    pub decided_ms: u64,
    /// `token` (a hosted surface) or `admin` (the operator's local credential).
    pub via: &'static str,
}

/// Approve or reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteDecision {
    Approve,
    Reject,
}

impl RemoteDecision {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "approve" => Some(Self::Approve),
            "reject" => Some(Self::Reject),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Reject => "reject",
        }
    }
}

/// The per-process record of queued approvals and spent token nonces.
#[derive(Debug, Default)]
pub struct ApprovalLedger {
    entries: Mutex<HashMap<u64, LedgerEntry>>,
    /// nonce → expiry (ms). Pruned on every consume.
    spent: Mutex<HashMap<String, u64>>,
}

/// The most entries the ledger keeps; the oldest are dropped first.
const MAX_LEDGER: usize = 4_096;

impl ApprovalLedger {
    /// Record a newly queued approval.
    pub fn record(&self, approval_id: u64, entry: LedgerEntry) {
        if let Ok(mut entries) = self.entries.lock() {
            if entries.len() >= MAX_LEDGER {
                if let Some(oldest) = entries.iter().min_by_key(|(_, e)| e.created_ms).map(|(id, _)| *id) {
                    entries.remove(&oldest);
                }
            }
            entries.insert(approval_id, entry);
        }
    }

    pub fn get(&self, approval_id: u64) -> Option<LedgerEntry> {
        self.entries.lock().ok()?.get(&approval_id).cloned()
    }

    /// Ids recorded for `principal` (`None` = every id).
    pub fn ids_for(&self, principal: Option<&str>) -> Vec<u64> {
        let Ok(entries) = self.entries.lock() else {
            return Vec::new();
        };
        let mut ids: Vec<u64> = entries
            .iter()
            .filter(|(_, e)| principal.is_none() || e.principal.as_deref() == principal)
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Record the decision on `approval_id`.
    pub fn decide(&self, approval_id: u64, record: DecisionRecord) {
        if let Ok(mut entries) = self.entries.lock() {
            if let Some(entry) = entries.get_mut(&approval_id) {
                entry.decision = Some(record);
            } else {
                entries.insert(
                    approval_id,
                    LedgerEntry {
                        principal: None,
                        requested_by: "unknown:unknown".into(),
                        level: "L3".into(),
                        created_ms: record.decided_ms,
                        decision: Some(record),
                    },
                );
            }
        }
    }

    /// Spend `nonce` (valid until `expires_ms`). `false` when it was already spent: a replay.
    pub fn consume(&self, nonce: &str, expires_ms: u64, now_ms: u64) -> bool {
        let Ok(mut spent) = self.spent.lock() else {
            return false;
        };
        spent.retain(|_, exp| *exp > now_ms);
        if spent.contains_key(nonce) {
            return false;
        }
        spent.insert(nonce.to_string(), expires_ms);
        true
    }
}

/// One operation on the principal's durable approval ledger (V103,
/// [`shogun_memory::approval_ledger`]): the spent nonces and the decisions, kept in the
/// principal's own store so a restart forgets neither.
#[derive(Debug, Clone, Copy)]
pub enum LedgerOp<'a> {
    /// Spend a decision token's nonce. Answers [`LedgerReply::Done`]`(false)` for a replay.
    SpendNonce { nonce: &'a str, approval_id: u64, expires_ms: u64, now_ms: u64 },
    /// Record a decision. `Done(false)` when the approval already had one.
    Record(&'a shogun_memory::approval_ledger::DecisionRow),
    /// The recorded decision on an approval.
    Get(u64),
    /// Claim an approved action for execution, once (`pending` → `executing`).
    Claim(u64),
    /// Settle a claimed execution (`executing` → `sent` / `failed`).
    Settle { approval_id: u64, sent: bool, reason_class: Option<&'a str>, now_ms: u64 },
}

/// What a [`LedgerOp`] answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedgerReply {
    Done(bool),
    Row(Option<shogun_memory::approval_ledger::DecisionRow>),
}

/// The status a persisted decision reports when the queue no longer knows the approval (after a
/// restart): the same names `actions.status` uses.
pub fn persisted_status(row: &shogun_memory::approval_ledger::DecisionRow) -> &'static str {
    use shogun_memory::approval_ledger::Execution;
    if !row.approve {
        return "rejected";
    }
    match row.execution {
        Execution::Sent => "sent",
        Execution::Failed => "send_failed",
        // Claimed by an executor that never settled it (a crash mid-send): delivery unknown.
        Execution::Executing => "outcome_unknown",
        Execution::None | Execution::Pending | Execution::Unavailable => "approved",
    }
}

/// The `next_action` of an approval's status: `execute_unavailable` for an approved action no
/// executor in this build will carry out, else none.
pub fn status_next_action(status: &str, executor: bool) -> Option<&'static str> {
    match status {
        "approved" if !executor => Some("execute_unavailable"),
        _ => None,
    }
}

/// The redacted summary of a decision the ledger persisted, for an approval the queue and the
/// in-process ledger no longer know (after a restart). Body-free, like every terminal row.
pub fn persisted_summary(row: &shogun_memory::approval_ledger::DecisionRow, executor: bool) -> Value {
    let status = persisted_status(row);
    let decided_ms = u64::try_from(row.decided_at).unwrap_or(0);
    let mut out = json!({
        "approval_id": row.approval_id,
        "status": status,
        "level": row.level,
        "decision": {
            "decision": if row.approve { "approve" } else { "reject" },
            "decided_by": row.decided_by,
            "decided_at": iso_ms(decided_ms),
            "via": row.via,
        },
    });
    if let Some(reason) = &row.reason_class {
        out["reason"] = Value::String(reason.clone());
    }
    if let Some(at) = row.executed_at {
        out["executed_at"] = iso(u64::try_from(at).unwrap_or(0));
    }
    if let Some(next) = status_next_action(status, executor) {
        out["next_action"] = Value::String(next.into());
    }
    out
}

/// The remote-approval configuration one process shares across its faces: where humans approve,
/// the signing key, and the ledger. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct RemoteApprovals {
    base_url: Option<String>,
    signer: Option<Arc<ApprovalSigner>>,
    ledger: Arc<ApprovalLedger>,
}

impl RemoteApprovals {
    /// From `SHOGUN_APPROVAL_BASE_URL` and `SHOGUN_APPROVAL_SIGNING_KEY`.
    pub fn from_env() -> Self {
        Self::default()
            .with_base_url(std::env::var(BASE_URL_ENV).ok())
            .with_signer(ApprovalSigner::from_env())
    }

    #[must_use]
    pub fn with_base_url(mut self, base_url: Option<String>) -> Self {
        self.base_url = base_url
            .map(|url| url.trim().trim_end_matches('/').to_string())
            .filter(|url| !url.is_empty());
        self
    }

    #[must_use]
    pub fn with_signer(mut self, signer: Option<ApprovalSigner>) -> Self {
        self.signer = signer.map(Arc::new);
        self
    }

    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }

    pub fn signer(&self) -> Option<&ApprovalSigner> {
        self.signer.as_deref()
    }

    pub fn ledger(&self) -> &ApprovalLedger {
        &self.ledger
    }

    /// Whether a hosted surface can decide approvals here (a signing key is configured).
    pub fn enabled(&self) -> bool {
        self.signer.is_some()
    }

    /// Record a newly queued approval and mint its URL: `(url, token)` when a base URL and a key
    /// are configured. The token expires with the approval's own window.
    pub fn on_queued(
        &self,
        approval_id: u64,
        principal: Option<&str>,
        requested_by: &str,
        level: &str,
        now_ms: u64,
    ) -> Option<(String, String)> {
        self.ledger.record(
            approval_id,
            LedgerEntry {
                principal: principal.map(str::to_string),
                requested_by: requested_by.to_string(),
                level: level.to_string(),
                created_ms: now_ms,
                decision: None,
            },
        );
        self.link(approval_id, principal, now_ms)
    }

    /// The `(url, token)` for `approval_id` without recording anything.
    pub fn link(&self, approval_id: u64, principal: Option<&str>, now_ms: u64) -> Option<(String, String)> {
        let base = self.base_url.as_deref()?;
        let (token, _) = self.signer.as_ref()?.mint(approval_id, principal, now_ms.saturating_add(APPROVAL_TIMEOUT_MS))?;
        Some((approval_url(base, &token), token))
    }
}

/// The URL a human opens: `<base>/approve/<token>`.
pub fn approval_url(base: &str, token: &str) -> String {
    format!("{}/approve/{token}", base.trim().trim_end_matches('/'))
}

/// The class of who or what an action reaches — never the recipient itself.
pub fn recipient_class(action: &SendAction) -> &'static str {
    match action {
        SendAction::SendEmail { .. } => "email_recipient",
        SendAction::PostMessage { .. } | SendAction::AddReaction { .. } => "chat_channel",
        SendAction::CreateCalendarEvent { .. } | SendAction::UpdateCalendarEvent { .. } => "calendar_attendees",
        SendAction::PostComment { .. } | SendAction::ChangeIssueStatus { .. } => "issue_tracker",
        SendAction::CreateDocument { .. } | SendAction::UpdateDocument { .. } => "document_service",
        _ => "local_setting",
    }
}

/// A preview safe to show on a hosted surface: secrets and personal data masked, one line, at
/// most [`PREVIEW_CHARS`].
pub fn safe_preview(body: &str) -> String {
    let masked = shogun_redact::redact_log(body);
    let one_line: String = masked.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = one_line.chars().take(PREVIEW_CHARS).collect();
    if one_line.chars().count() > PREVIEW_CHARS {
        out.push('…');
    }
    out
}

fn status_name(status: Option<ApprovalStatus>) -> &'static str {
    match status {
        Some(ApprovalStatus::Pending) => "pending",
        Some(ApprovalStatus::Rejected) => "rejected",
        Some(ApprovalStatus::TimedOut) => "timed_out",
        Some(ApprovalStatus::Sent) => "sent",
        Some(ApprovalStatus::SendFailed) => "send_failed",
        Some(ApprovalStatus::DraftSaved) => "draft_saved",
        Some(ApprovalStatus::OutcomeUnknown) => "outcome_unknown",
        Some(ApprovalStatus::Approved) => "approved",
        None => "unknown",
    }
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` for unix milliseconds (proleptic Gregorian, UTC).
pub fn iso_ms(ms: u64) -> String {
    let days = i64::try_from(ms / 86_400_000).unwrap_or(0);
    let rem = ms % 86_400_000;
    let (h, m, s, milli) = (rem / 3_600_000, rem / 60_000 % 60, rem / 1_000 % 60, rem % 1_000);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.{milli:03}Z")
}

fn iso(ms: u64) -> Value {
    Value::String(iso_ms(ms))
}

/// The redacted summary of `approval_id`, or `None` when neither the queue nor the ledger knows
/// it. Expires stale rows first. A resolved approval has no preview: terminal rows are body-free.
pub fn summary(queue: &mut ApprovalQueue, ledger: &ApprovalLedger, approval_id: u64, now_ms: u64) -> Option<Value> {
    queue.expire_due(now_ms);
    let id = ApprovalId(approval_id);
    let entry = ledger.get(approval_id);
    let status = queue.status(id);
    if status.is_none() && entry.is_none() {
        return None;
    }
    let mut out = json!({
        "approval_id": approval_id,
        "status": status_name(status),
        "level": entry.as_ref().map_or("L3", |e| e.level.as_str()),
    });
    if let (Some(preview), Some(action)) = (queue.preview(id), queue.action(id)) {
        out["kind"] = Value::String(preview.op_type.to_string());
        out["recipient_class"] = Value::String(recipient_class(action).to_string());
        out["preview"] = Value::String(safe_preview(&preview.full_body));
    }
    if let Some(origin) = queue.origin(id) {
        out["origin"] = Value::String(origin.as_str().to_string());
    }
    if let Some(expires) = queue.expires_at_ms(id) {
        out["expires_at"] = iso(expires);
        out["created_at"] = iso(expires.saturating_sub(APPROVAL_TIMEOUT_MS));
    } else if let Some(entry) = &entry {
        out["created_at"] = iso(entry.created_ms);
        out["expires_at"] = iso(entry.created_ms.saturating_add(APPROVAL_TIMEOUT_MS));
    }
    if let Some(entry) = &entry {
        out["requested_by"] = Value::String(entry.requested_by.clone());
        if let Some(principal) = &entry.principal {
            out["principal"] = Value::String(principal.clone());
        }
        if let Some(decision) = &entry.decision {
            out["decision"] = json!({
                "decision": decision.decision.as_str(),
                "decided_by": decision.decided_by,
                "decided_at": iso(decision.decided_ms),
                "via": decision.via,
            });
        }
    }
    Some(out)
}

/// Every approval `principal` may see (`None` = all of them, the single-principal store): the
/// queue's pending rows plus the ledger's recent ones, newest first, each as its [`summary`].
pub fn list(queue: &mut ApprovalQueue, ledger: &ApprovalLedger, principal: Option<&str>, multi: bool, now_ms: u64) -> Value {
    queue.expire_due(now_ms);
    let mut ids: Vec<u64> = ledger.ids_for(principal);
    if !multi {
        ids.extend(queue.pending_ids().into_iter().map(|id| id.0));
    }
    ids.sort_unstable_by(|a, b| b.cmp(a));
    ids.dedup();
    let approvals: Vec<Value> = ids
        .into_iter()
        .take(100)
        .filter_map(|id| summary(queue, ledger, id, now_ms))
        .collect();
    json!({ "approvals": approvals })
}

/// The outcome of applying a decision to the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// Approved: the row left the queue and reads `approved` until an executor reports.
    Approved,
    Rejected,
    /// The approval window had already elapsed: recorded as timed out, nothing approved.
    Expired,
    /// Not pending (unknown, or already decided).
    NotPending,
}

/// Apply `decision` to `approval_id` as the dedicated-button confirm (or a user rejection). An
/// approved row is recorded `approved` — confirmed by a human, for the executor to carry out; the
/// confirmed action is not run here.
pub fn apply(queue: &mut ApprovalQueue, approval_id: u64, decision: RemoteDecision, now_ms: u64) -> Applied {
    let id = ApprovalId(approval_id);
    match decision {
        RemoteDecision::Approve => match queue.confirm(id, ConfirmIntent::DedicatedButton, now_ms) {
            Decision::Confirmed(_) => {
                queue.mark_status(id, ApprovalStatus::Approved, now_ms);
                Applied::Approved
            }
            Decision::Rejected(RejectCause::TimedOut) => Applied::Expired,
            _ => Applied::NotPending,
        },
        RemoteDecision::Reject => match queue.reject(id, RejectCause::UserRejected, now_ms) {
            Decision::Rejected(_) => Applied::Rejected,
            _ => Applied::NotPending,
        },
    }
}

/// A validated decision request body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecideRequest {
    pub decision: RemoteDecision,
    pub decided_by: String,
    pub nonce: Option<String>,
    pub token: Option<String>,
}

/// Parse `{"decision": "approve"|"reject", "decided_by": "<kind>:<id>", "nonce": "…", "token": "…"}`.
/// `decided_by` must be a valid actor (`slack:U123`, `web:approval-page`, `human:owner`).
pub fn decide_request(body: Option<&str>) -> Result<DecideRequest, String> {
    let value: Value = serde_json::from_str(body.unwrap_or("")).map_err(|_| "invalid_json: the body must be a JSON object.".to_string())?;
    let decision = value["decision"]
        .as_str()
        .and_then(RemoteDecision::parse)
        .ok_or_else(|| "invalid_decision: decision must be \"approve\" or \"reject\".".to_string())?;
    let decided_by = value["decided_by"]
        .as_str()
        .and_then(|raw| shogun_memory::actor::Actor::parse(raw).ok())
        .ok_or_else(|| "invalid_decided_by: decided_by must name who decided as <kind>:<id> (for example slack:U0123).".to_string())?;
    let text = |key: &str| value[key].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    Ok(DecideRequest { decision, decided_by: decided_by.as_str().to_string(), nonce: text("nonce"), token: text("token") })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shogun_agents::approval::{ApprovalOrigin, Preview, Route};

    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef-test";

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        // A key longer than the block is hashed first (RFC 4231 case 6).
        let mac = hmac_sha256(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54");
    }

    #[test]
    fn tokens_verify_expire_and_refuse_tampering() {
        assert!(ApprovalSigner::new(b"short").is_none());
        let signer = ApprovalSigner::new(KEY).unwrap();
        let (token, claims) = signer.mint(7, Some("alice"), 1_000).unwrap();
        assert!(token.starts_with("sat1."));
        assert_eq!(signer.verify(&token, 999).unwrap(), claims);
        assert_eq!(claims.principal.as_deref(), Some("alice"));
        assert_eq!(signer.verify(&token, 1_000), Err(TokenError::Expired));
        let other = ApprovalSigner::new(b"another key that is long enough!!").unwrap();
        assert_eq!(other.verify(&token, 1), Err(TokenError::BadSignature));
        let mut parts: Vec<&str> = token.split('.').collect();
        let forged = B64.encode(r#"{"a":8,"p":"alice","e":1000,"n":"x"}"#);
        parts[1] = &forged;
        assert_eq!(signer.verify(&parts.join("."), 1), Err(TokenError::BadSignature));
        assert_eq!(signer.verify("sat1.x", 1), Err(TokenError::Malformed));
        assert_eq!(signer.verify("", 1), Err(TokenError::Malformed));
        assert!(format!("{signer:?}").contains("redacted"));
    }

    #[test]
    fn iso_formats_unix_milliseconds() {
        assert_eq!(iso_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_ms(1_760_000_000_123), "2025-10-09T08:53:20.123Z");
        assert_eq!(iso_ms(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    /// The same vector `apps/api/test/approvals.test.ts` mints with the gateway's code: a token
    /// one side signs, the other verifies.
    #[test]
    fn verifies_the_gateways_cross_language_vector() {
        const VECTOR: &str = "sat1.eyJhIjo3LCJwIjoiYWxpY2UiLCJlIjo0MTAyNDQ0ODAwMDAwLCJuIjoiMDAxMTIyMzM0NDU1NjY3Nzg4OTlhYWJiY2NkZGVlZmYifQ.MTBMDvcUdsb4kU0IE2BA9v_tuD4t5FxLo4XeYpOYxYk";
        let signer = ApprovalSigner::new(b"cross-language-vector-key-0123456789").unwrap();
        let claims = signer.verify(VECTOR, 1_760_000_000_000).unwrap();
        assert_eq!(claims.approval_id, 7);
        assert_eq!(claims.principal.as_deref(), Some("alice"));
        assert_eq!(claims.expires_ms, 4_102_444_800_000);
        assert_eq!(claims.nonce, "00112233445566778899aabbccddeeff");
    }

    #[test]
    fn nonces_are_single_use_until_they_expire() {
        let ledger = ApprovalLedger::default();
        assert!(ledger.consume("n1", 100, 10));
        assert!(!ledger.consume("n1", 100, 20), "a replay is refused");
        assert!(ledger.consume("n2", 100, 20));
    }

    fn queued() -> (ApprovalQueue, u64) {
        let mut queue = ApprovalQueue::new();
        let action = SendAction::SendEmail { to: "aiko@example.com".into() };
        let preview = Preview::for_send(&action, "Forecast attached for aiko@example.com. api_key=sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789", Route::DirectMcp);
        let id = queue.try_request(action, preview, ApprovalOrigin::Api, 1_000).unwrap();
        (queue, id.0)
    }

    #[test]
    fn summary_is_redacted_and_names_a_recipient_class_only() {
        let (mut queue, id) = queued();
        let ledger = ApprovalLedger::default();
        ledger.record(id, LedgerEntry { principal: None, requested_by: "agent:codex".into(), level: "L3".into(), created_ms: 1_000, decision: None });
        let s = summary(&mut queue, &ledger, id, 2_000).unwrap();
        assert_eq!(s["status"], "pending");
        assert_eq!(s["recipient_class"], "email_recipient");
        let text = s.to_string();
        assert!(!text.contains("aiko@example.com"), "{text}");
        assert!(!text.contains("abcdefghijklmnopqrstuvwxyz0123456789"), "{text}");
        assert!(s["preview"].as_str().unwrap().starts_with("Forecast attached for "));
        assert!(s["expires_at"].is_string());
        assert!(summary(&mut queue, &ledger, 999, 2_000).is_none());
    }

    #[test]
    fn approve_and_reject_move_the_queue_once() {
        let (mut queue, id) = queued();
        assert_eq!(apply(&mut queue, id, RemoteDecision::Approve, 2_000), Applied::Approved);
        assert_eq!(queue.status(ApprovalId(id)), Some(ApprovalStatus::Approved));
        assert_eq!(apply(&mut queue, id, RemoteDecision::Approve, 2_001), Applied::NotPending);
        let (mut queue, id) = queued();
        assert_eq!(apply(&mut queue, id, RemoteDecision::Reject, 2_000), Applied::Rejected);
        assert_eq!(apply(&mut queue, id, RemoteDecision::Reject, 2_000), Applied::NotPending);
        let (mut queue, id) = queued();
        assert_eq!(apply(&mut queue, id, RemoteDecision::Approve, 1_000 + APPROVAL_TIMEOUT_MS + 1), Applied::Expired);
    }

    #[test]
    fn decide_bodies_name_a_decision_and_a_decider() {
        let ok = decide_request(Some(r#"{"decision":"approve","decided_by":"slack:U1","nonce":"n"}"#)).unwrap();
        assert_eq!((ok.decision, ok.decided_by.as_str(), ok.nonce.as_deref()), (RemoteDecision::Approve, "slack:U1", Some("n")));
        assert!(decide_request(Some(r#"{"decision":"maybe","decided_by":"slack:U1"}"#)).is_err());
        assert!(decide_request(Some(r#"{"decision":"reject","decided_by":"nobody"}"#)).is_err());
        assert!(decide_request(None).is_err());
    }

    #[test]
    fn links_point_at_the_approve_page_with_a_token() {
        let remote = RemoteApprovals::default()
            .with_base_url(Some("https://gw.example/".into()))
            .with_signer(ApprovalSigner::new(KEY));
        let (url, token) = remote.on_queued(3, None, "agent:codex", "L3", 10).unwrap();
        assert_eq!(url, format!("https://gw.example/approve/{token}"));
        let claims = remote.signer().unwrap().verify(&token, 11).unwrap();
        assert_eq!((claims.approval_id, claims.expires_ms), (3, 10 + APPROVAL_TIMEOUT_MS));
        assert_eq!(remote.ledger().get(3).unwrap().requested_by, "agent:codex");
        assert!(RemoteApprovals::default().with_base_url(Some("https://gw.example".into())).link(3, None, 10).is_none());
    }
}
