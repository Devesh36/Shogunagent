//! `payments.*` (ShogunAgent E46): agent payment mandates and payments, one parser, one flow and
//! one envelope for MCP, REST (`/v1/payments/...`) and the CLI (`shogun payments ...`).
//!
//! The level design (`docs/api/payments.md`):
//!
//! | Tool                        | Level | What happens                                              |
//! |-----------------------------|-------|-----------------------------------------------------------|
//! | `payments.mandates.list`    | read  | the mandates, their bounds, spent and remaining; free      |
//! | `payments.mandates.request` | L3    | new / wider / longer terms: queued for a human (URL); terms that only narrow apply directly |
//! | `payments.mandates.revoke`  | L1    | revoked at once, from any face                             |
//! | `payments.spend`            | L3    | inside a mandate: the human's grant is the L3 confirmation and it executes through the payment rail; otherwise queued as a per-payment L3 approval |
//!
//! Every spend is recorded in the principal's store (`shogun_memory::payment_mandates`) under the
//! caller's actor and idempotency key *before* the rail is called, with the human decision that
//! authorized it as provenance; a retry with the same key replays the recorded outcome. The bounds
//! are `shogun_agents::payment` and nothing else. The store primitives are the backend's
//! ([`PaymentsOp`]); the payment itself runs in the core's send path (`send_exec`), which writes
//! the traceability row first.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use shogun_agents::approval::Preview;
use shogun_agents::payment::{
    self, MandateGrant, MandateStanding, MandateTerms, PayeeScope, PaymentOrder,
};
use shogun_agents::permission::SendAction;
use shogun_memory::payment_mandates::{MandateRow, NewSpend, Reserved, SpendRow, TermsRow};

use crate::backend::MemoryBackend;
use crate::memory_api::Tool;

pub use shogun_agents::approval::APPROVAL_TIMEOUT_MS;

/// The rail label recorded on every spend row when the rail does not name itself.
pub const DEFAULT_RAIL: &str = "stub";
/// The longest idempotency key accepted.
pub const MAX_KEY_CHARS: usize = 128;
/// The longest memo accepted (it travels in the approval preview and to the rail, never stored).
pub const MAX_MEMO_CHARS: usize = 280;
const DAY_MS: i64 = 86_400_000;

pub const LIST_DESCRIPTION: &str = "The payment mandates a human granted this principal's agents: payee or payee class, currency, per-payment and total caps (integer minor units), spent and remaining, expiry, purpose, single-use, status (active, expired, exhausted, revoked) and the approval that granted each. Pass mandate_id for one mandate with its recent spends. Never pays and never grants. Free read.";
pub const REQUEST_DESCRIPTION: &str = "Ask the human for a payment mandate so you can pay within bounds without asking each time: payee (exact) or payee_class, currency (ISO 4217), max_per_payment_minor, total_cap_minor, expires_in_days (max 366) or expires_at_ms, purpose, optional single_use, idempotency_key. A new, wider or longer mandate is L3: it returns approval_required with a URL for the human, and nothing is granted until a human approves. With mandate_id and terms that only narrow it, the change applies at once. Not a payment (payments.spend). Free at the call.";
pub const REVOKE_DESCRIPTION: &str = "Revoke a payment mandate by mandate_id, at once (L1, any face): no further payment runs under it; payments already made stay recorded. Revoking twice is harmless. Use when the task is done or the human asks. Free at the call.";
pub const SPEND_DESCRIPTION: &str = "Pay a payee: payee, currency, amount_minor (integer minor units), optional memo and mandate_id, idempotency_key (a retry with the same key returns the recorded outcome, never pays twice). A payment is L3: inside an active mandate granted to you it executes through the payment rail and is recorded against the mandate; otherwise it returns approval_required with a URL and runs only after a human approves. Never report a payment as made before status says settled. Priced as a run of the rail's fee (free while the rail is the stub).";

/// Whether `tool` is one of the four `payments.*` tools.
pub fn is_payments_tool(tool: Tool) -> bool {
    matches!(
        tool,
        Tool::PaymentsMandatesList | Tool::PaymentsMandatesRequest | Tool::PaymentsMandatesRevoke | Tool::PaymentsSpend
    )
}

pub fn list_schema() -> Value {
    json!({
        "mandate_id": { "type": "string" },
        "limit": { "type": "integer", "minimum": 1, "maximum": 100 },
    })
}

pub fn request_schema() -> Value {
    json!({
        "mandate_id": { "type": "string", "description": "Amend this mandate (narrow directly, widen or extend through approval)." },
        "payee": { "type": "string", "description": "One exact payee id. Give payee or payee_class." },
        "payee_class": { "type": "string", "description": "Every payee the rail classifies in this class (lower-case label)." },
        "currency": { "type": "string", "description": "ISO 4217 code, e.g. USD." },
        "max_per_payment_minor": { "type": "integer", "minimum": 1 },
        "total_cap_minor": { "type": "integer", "minimum": 1 },
        "expires_in_days": { "type": "integer", "minimum": 1, "maximum": 366 },
        "expires_at_ms": { "type": "integer" },
        "purpose": { "type": "string", "maxLength": 280 },
        "single_use": { "type": "boolean" },
        "idempotency_key": { "type": "string", "maxLength": 128 },
    })
}

pub fn revoke_schema() -> Value {
    json!({
        "mandate_id": { "type": "string" },
        "idempotency_key": { "type": "string", "maxLength": 128 },
    })
}

pub fn spend_schema() -> Value {
    json!({
        "payee": { "type": "string" },
        "currency": { "type": "string" },
        "amount_minor": { "type": "integer", "minimum": 1 },
        "memo": { "type": "string", "maxLength": 280 },
        "mandate_id": { "type": "string" },
        "idempotency_key": { "type": "string", "maxLength": 128 },
    })
}

/// The `required` list of each tool's input schema.
pub fn required(tool: Tool) -> &'static [&'static str] {
    match tool {
        Tool::PaymentsMandatesRequest => &["currency", "max_per_payment_minor", "total_cap_minor", "purpose", "idempotency_key"],
        Tool::PaymentsMandatesRevoke => &["mandate_id"],
        Tool::PaymentsSpend => &["payee", "currency", "amount_minor", "idempotency_key"],
        _ => &[],
    }
}

/// One store primitive the flow needs from the principal's backend.
pub enum PaymentsOp<'a> {
    /// One mandate.
    Get(&'a str),
    /// Mandates, newest first.
    List(usize),
    /// Recent spends against a mandate.
    Spends(&'a str),
    /// Replace an active mandate's terms with checked-narrower ones.
    Narrow { mandate_id: &'a str, terms: &'a TermsRow },
    /// Revoke (`Done(false)` when already revoked).
    Revoke { mandate_id: &'a str, by: &'a str },
    /// The spend `(actor, key)` recorded, if any.
    SpendByKey { actor: &'a str, key: &'a str },
    /// The payment rail's class for a payee. `Err("payment_rail_unavailable")` when no rail is
    /// configured: nothing is then recorded or paid.
    PayeeClass(&'a str),
    /// Record a spend; for a mandate spend `check` re-tests the bounds inside the transaction.
    Reserve { spend: &'a NewSpend<'a>, check: &'a dyn Fn(Option<&MandateRow>) -> Result<(), &'static str> },
    /// Execute a reserved mandate spend through the rail (traced first) and settle its row.
    PayReserved { confirmed: &'a shogun_agents::approval::ConfirmedSend },
    /// A per-payment approval was queued (`Some(id)`) or refused (`None`, the row is closed).
    Queued { spend_ref: &'a str, approval_id: Option<u64> },
    /// The per-payment approval `approval_id` will never run: a human rejected it (`outcome`
    /// `rejected`, `by` the human) or its window elapsed (`expired`). Closes the spend still
    /// waiting on it (`Done(false)` when none is). Called by the decide route.
    Close { spend_ref: &'a str, approval_id: u64, outcome: &'a str, by: &'a str },
    /// Close every waiting per-payment spend whose approval the ledger records as rejected, or
    /// whose approval window ([`APPROVAL_TIMEOUT_MS`]) elapsed at `now_ms` without an approval.
    /// Answers `Done(any closed)`. Run before a payments read so `payments.*` and
    /// `actions.status` agree.
    Reconcile { now_ms: i64 },
}

/// What a [`PaymentsOp`] answered.
pub enum PaymentsReply {
    Mandate(Option<MandateRow>),
    Mandates(Vec<MandateRow>),
    Spend(Option<SpendRow>),
    Spends(Vec<SpendRow>),
    Done(bool),
    Class(Option<String>),
    Reserved(Reserved),
}

/// A face's answer: HTTP-style status and the JSON body.
pub type Answer = (u16, String);

fn refuse(status: u16, code: &str, message: &str, next: Option<&str>) -> Answer {
    (status, json!({ "error": code, "message": message, "next_action": next }).to_string())
}

fn bad(code: &str, message: &str) -> Answer {
    refuse(400, code, message, Some("fix_arguments"))
}

fn store_fault(code: &str) -> Answer {
    match code {
        "payment_rail_unavailable" => refuse(
            503,
            code,
            "No payment rail is configured on this server, so nothing was recorded or paid. The operator sets SHOGUN_PAYMENT_RAIL.",
            None,
        ),
        _ => refuse(503, "payments_unavailable", "The principal's payment ledger cannot be read or written right now; nothing was paid. Retry.", Some("retry_after")),
    }
}

/// Deterministic id for `(actor, key)`: a retry maps to the same mandate or spend.
pub fn derived_id(prefix: &str, actor: &str, key: &str) -> String {
    let digest = Sha256::digest(format!("shogun-payments-v1|{prefix}|{actor}|{key}").as_bytes());
    let hex: String = digest.iter().take(12).map(|b| format!("{b:02x}")).collect();
    format!("{prefix}_{hex}")
}

fn key_arg(args: &Value, required: bool) -> Result<Option<String>, Answer> {
    match args.get("idempotency_key") {
        None | Some(Value::Null) if !required => Ok(None),
        Some(Value::String(k))
            if !k.trim().is_empty() && k.chars().count() <= MAX_KEY_CHARS && k.chars().all(|c| c.is_ascii_graphic()) =>
        {
            Ok(Some(k.clone()))
        }
        _ => Err(bad("invalid_idempotency_key", "idempotency_key must be 1-128 visible ASCII characters, unique per payment or request.")),
    }
}

fn str_arg<'a>(args: &'a Value, name: &str) -> Option<&'a str> {
    args.get(name).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn amount_arg(args: &Value, name: &str) -> Option<u64> {
    args.get(name).and_then(Value::as_u64).filter(|n| *n > 0 && *n <= payment::MAX_AMOUNT_MINOR)
}

/// Parse `payments.mandates.request` terms.
pub fn parse_terms(args: &Value, now_ms: i64) -> Result<MandateTerms, Answer> {
    let payee = match (str_arg(args, "payee"), str_arg(args, "payee_class")) {
        (Some(p), None) => PayeeScope::Payee(p.to_string()),
        (None, Some(c)) => PayeeScope::Class(c.to_string()),
        _ => return Err(bad("invalid_payee", "Give exactly one of payee (an exact payee id) or payee_class.")),
    };
    let expires_at_ms = match (args.get("expires_at_ms").and_then(Value::as_i64), args.get("expires_in_days").and_then(Value::as_i64)) {
        (Some(at), None) => at,
        (None, Some(days)) if (1..=payment::MAX_MANDATE_DAYS).contains(&days) => now_ms.saturating_add(days * DAY_MS),
        _ => return Err(bad("invalid_expiry", "Give expires_in_days (1-366) or expires_at_ms, not both.")),
    };
    let terms = MandateTerms {
        payee,
        currency: str_arg(args, "currency").unwrap_or_default().to_string(),
        max_per_payment_minor: amount_arg(args, "max_per_payment_minor").unwrap_or(0),
        total_cap_minor: amount_arg(args, "total_cap_minor").unwrap_or(0),
        expires_at_ms,
        purpose: args.get("purpose").and_then(Value::as_str).unwrap_or_default().trim().to_string(),
        single_use: match args.get("single_use") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(bad("invalid_single_use", "single_use must be true or false.")),
        },
    };
    terms.validate(now_ms).map_err(|code| bad(code, terms_message(code)))?;
    Ok(terms)
}

fn terms_message(code: &str) -> &'static str {
    match code {
        "invalid_payee" => "payee must be 1-128 visible ASCII characters.",
        "invalid_payee_class" => "payee_class must be 1-64 lower-case letters, digits, '_' or '-'.",
        "invalid_currency" => "currency must be an ISO 4217 code in capitals, e.g. USD.",
        "invalid_max_per_payment" => "max_per_payment_minor must be a positive integer in minor units.",
        "invalid_total_cap" => "total_cap_minor must be at least max_per_payment_minor.",
        "invalid_expiry" => "The expiry must be in the future and at most 366 days away.",
        _ => "purpose must be 1-280 characters and say what the payments are for.",
    }
}

/// The store row form of terms.
pub fn terms_row(terms: &MandateTerms) -> TermsRow {
    TermsRow {
        payee_kind: terms.payee.kind().to_string(),
        payee: terms.payee.value().to_string(),
        currency: terms.currency.clone(),
        max_per_payment_minor: terms.max_per_payment_minor,
        total_cap_minor: terms.total_cap_minor,
        expires_at: terms.expires_at_ms,
        purpose: terms.purpose.clone(),
        single_use: terms.single_use,
    }
}

/// The terms of a stored row, or `None` for a row this code would not have written.
pub fn row_terms(row: &MandateRow) -> Option<MandateTerms> {
    Some(MandateTerms {
        payee: PayeeScope::parse(&row.terms.payee_kind, &row.terms.payee)?,
        currency: row.terms.currency.clone(),
        max_per_payment_minor: row.terms.max_per_payment_minor,
        total_cap_minor: row.terms.total_cap_minor,
        expires_at_ms: row.terms.expires_at,
        purpose: row.terms.purpose.clone(),
        single_use: row.terms.single_use,
    })
}

/// The standing a spend is checked against.
pub fn standing(row: &MandateRow) -> Option<MandateStanding> {
    Some(MandateStanding {
        mandate_id: row.mandate_id.clone(),
        agent: row.agent.clone(),
        terms: row_terms(row)?,
        revoked: row.status != "active",
        spent_minor: row.spent_minor,
        payments: row.payments,
        grant_approval_id: row.grant_approval_id,
        granted_by: row.granted_by.clone(),
    })
}

/// A mandate's status as a caller sees it at `now_ms`.
pub fn status_of(row: &MandateRow, now_ms: i64) -> &'static str {
    if row.status != "active" {
        "revoked"
    } else if now_ms >= row.terms.expires_at {
        "expired"
    } else if row.spent_minor >= row.terms.total_cap_minor || (row.terms.single_use && row.payments > 0) {
        "exhausted"
    } else {
        "active"
    }
}

/// A mandate as JSON.
pub fn mandate_json(row: &MandateRow, now_ms: i64) -> Value {
    json!({
        "mandate_id": row.mandate_id,
        "agent": row.agent,
        "payee": { "kind": row.terms.payee_kind, "value": row.terms.payee },
        "currency": row.terms.currency,
        "max_per_payment_minor": row.terms.max_per_payment_minor,
        "total_cap_minor": row.terms.total_cap_minor,
        "spent_minor": row.spent_minor,
        "remaining_minor": row.terms.total_cap_minor.saturating_sub(row.spent_minor),
        "payments": row.payments,
        "expires_at_ms": row.terms.expires_at,
        "purpose": row.terms.purpose,
        "single_use": row.terms.single_use,
        "status": status_of(row, now_ms),
        "revision": row.revision,
        "granted": { "approval_id": row.grant_approval_id, "by": row.granted_by, "at_ms": row.granted_at },
        "revoked": row.revoked_by.as_ref().map(|by| json!({ "by": by, "at_ms": row.revoked_at })),
    })
}

/// A spend as JSON (no memo: it is never stored).
pub fn spend_json(row: &SpendRow) -> Value {
    json!({
        "spend_ref": row.spend_ref,
        "actor": row.actor,
        "idempotency_key": row.idempotency_key,
        "mandate_id": row.mandate_id,
        "authorization": row.authorization,
        "approval_id": row.approval_id,
        "decided_by": row.decided_by,
        "payee": row.payee,
        "currency": row.currency,
        "amount_minor": row.amount_minor,
        "status": row.reported_status(),
        "reason_class": row.reason_class,
        "rail": row.rail,
        "rail_ref": row.rail_ref,
        "created_at_ms": row.created_at,
        "settled_at_ms": row.settled_at,
    })
}

/// Serve one `payments.*` call for `actor`. `enqueue` puts an L3 request into the face's one
/// approval queue and answers its pending envelope (`rest::enqueue` on every face).
pub fn serve<B: MemoryBackend + ?Sized>(
    tool: Tool,
    args: &Value,
    backend: &B,
    actor: &str,
    now_ms: i64,
    enqueue: impl FnOnce(SendAction, Preview) -> Answer,
) -> Answer {
    let args = if args.is_null() { &Value::Object(Default::default()) } else { args };
    if !args.is_object() {
        return bad("invalid_arguments", "The arguments must be one JSON object.");
    }
    if matches!(tool, Tool::PaymentsMandatesList | Tool::PaymentsSpend) {
        // Best effort: a store that cannot reconcile answers the read or the spend as it can.
        let _ = backend.payments(PaymentsOp::Reconcile { now_ms });
    }
    match tool {
        Tool::PaymentsMandatesList => list(args, backend, now_ms),
        Tool::PaymentsMandatesRequest => request(args, backend, actor, now_ms, enqueue),
        Tool::PaymentsMandatesRevoke => revoke(args, backend, actor, now_ms),
        Tool::PaymentsSpend => spend(args, backend, actor, now_ms, enqueue),
        _ => bad("unknown_tool", "Not a payments tool."),
    }
}

fn get_mandate<B: MemoryBackend + ?Sized>(backend: &B, id: &str) -> Result<Option<MandateRow>, Answer> {
    match backend.payments(PaymentsOp::Get(id)) {
        Ok(PaymentsReply::Mandate(row)) => Ok(row),
        Ok(_) => Err(store_fault("payments_unavailable")),
        Err(code) => Err(store_fault(&code)),
    }
}

fn not_found() -> Answer {
    refuse(404, "mandate_not_found", "No mandate with that id in this principal's store. List them with payments.mandates.list.", None)
}

fn list<B: MemoryBackend + ?Sized>(args: &Value, backend: &B, now_ms: i64) -> Answer {
    if let Some(id) = args.get("mandate_id") {
        let Some(id) = id.as_str() else {
            return bad("invalid_mandate_id", "mandate_id must be a string.");
        };
        let row = match get_mandate(backend, id) {
            Ok(Some(row)) => row,
            Ok(None) => return not_found(),
            Err(answer) => return answer,
        };
        let spends = match backend.payments(PaymentsOp::Spends(id)) {
            Ok(PaymentsReply::Spends(rows)) => rows,
            Ok(_) => return store_fault("payments_unavailable"),
            Err(code) => return store_fault(&code),
        };
        return (
            200,
            json!({
                "tool": Tool::PaymentsMandatesList.wire_name(),
                "mandate": mandate_json(&row, now_ms),
                "spends": spends.iter().map(spend_json).collect::<Vec<_>>(),
            })
            .to_string(),
        );
    }
    let limit = match args.get("limit") {
        None | Some(Value::Null) => 50,
        Some(v) => match v.as_u64().filter(|n| (1..=100).contains(n)) {
            Some(n) => usize::try_from(n).unwrap_or(50),
            None => return bad("invalid_limit", "limit must be 1-100."),
        },
    };
    match backend.payments(PaymentsOp::List(limit)) {
        Ok(PaymentsReply::Mandates(rows)) => (
            200,
            json!({
                "tool": Tool::PaymentsMandatesList.wire_name(),
                "mandates": rows.iter().map(|row| mandate_json(row, now_ms)).collect::<Vec<_>>(),
            })
            .to_string(),
        ),
        Ok(_) => store_fault("payments_unavailable"),
        Err(code) => store_fault(&code),
    }
}

fn request<B: MemoryBackend + ?Sized>(
    args: &Value,
    backend: &B,
    actor: &str,
    now_ms: i64,
    enqueue: impl FnOnce(SendAction, Preview) -> Answer,
) -> Answer {
    let key = match key_arg(args, true) {
        Ok(key) => key.unwrap_or_default(),
        Err(answer) => return answer,
    };
    let terms = match parse_terms(args, now_ms) {
        Ok(terms) => terms,
        Err(answer) => return answer,
    };
    let tool = Tool::PaymentsMandatesRequest.wire_name();
    let (mandate_id, amends) = match args.get("mandate_id") {
        None | Some(Value::Null) => (derived_id("pmd", actor, &key), false),
        Some(Value::String(id)) => {
            let row = match get_mandate(backend, id) {
                Ok(Some(row)) => row,
                Ok(None) => return not_found(),
                Err(answer) => return answer,
            };
            if row.agent != actor {
                return refuse(403, "mandate_not_yours", "This mandate was granted to another agent; request your own.", None);
            }
            if row.status != "active" {
                return refuse(409, "mandate_revoked", "A revoked mandate cannot be changed; request a new one.", Some("request_new_mandate"));
            }
            let Some(current) = row_terms(&row) else {
                return store_fault("payments_unavailable");
            };
            if !terms.widens(&current) {
                // Narrowing: direct, like a privacy-narrowing setting.
                return match backend.payments(PaymentsOp::Narrow { mandate_id: id, terms: &terms_row(&terms) }) {
                    Ok(_) => match get_mandate(backend, id) {
                        Ok(Some(row)) => (200, json!({ "tool": tool, "narrowed": true, "mandate": mandate_json(&row, now_ms) }).to_string()),
                        Ok(None) => not_found(),
                        Err(answer) => answer,
                    },
                    Err(code) if code == "mandate_revoked" => {
                        refuse(409, "mandate_revoked", "A revoked mandate cannot be changed; request a new one.", Some("request_new_mandate"))
                    }
                    Err(code) => store_fault(&code),
                };
            }
            (id.clone(), true)
        }
        Some(_) => return bad("invalid_mandate_id", "mandate_id must be a string."),
    };
    if !amends {
        match get_mandate(backend, &mandate_id) {
            // A retried request whose mandate a human already granted: answer it.
            Ok(Some(row)) => {
                return (200, json!({ "tool": tool, "replayed": true, "mandate": mandate_json(&row, now_ms) }).to_string())
            }
            Ok(None) => {}
            Err(answer) => return answer,
        }
    }
    let (send, preview) = payment::mandate_request(MandateGrant {
        mandate_id: mandate_id.clone(),
        agent: actor.to_string(),
        terms,
        amends,
    });
    let (status, body) = enqueue(send, preview);
    (status, with_fields(&body, json!({ "tool": tool, "mandate_id": mandate_id })))
}

fn revoke<B: MemoryBackend + ?Sized>(args: &Value, backend: &B, actor: &str, now_ms: i64) -> Answer {
    if let Err(answer) = key_arg(args, false) {
        return answer;
    }
    let Some(id) = str_arg(args, "mandate_id") else {
        return bad("invalid_mandate_id", "mandate_id is required.");
    };
    let revoked = match backend.payments(PaymentsOp::Revoke { mandate_id: id, by: actor }) {
        Ok(PaymentsReply::Done(done)) => done,
        Ok(_) => return store_fault("payments_unavailable"),
        Err(code) if code == "mandate_not_found" => return not_found(),
        Err(code) => return store_fault(&code),
    };
    match get_mandate(backend, id) {
        Ok(Some(row)) => (
            200,
            json!({ "tool": Tool::PaymentsMandatesRevoke.wire_name(), "revoked": revoked, "already_revoked": !revoked, "mandate": mandate_json(&row, now_ms) })
                .to_string(),
        ),
        Ok(None) => not_found(),
        Err(answer) => answer,
    }
}

fn with_fields(body: &str, extra: Value) -> String {
    let mut value: Value = serde_json::from_str(body).unwrap_or_else(|_| json!({}));
    if let (Some(object), Value::Object(extra)) = (value.as_object_mut(), extra) {
        for (k, v) in extra {
            object.entry(k).or_insert(v);
        }
    }
    value.to_string()
}

fn replay(row: &SpendRow) -> Answer {
    let mut body = json!({ "tool": Tool::PaymentsSpend.wire_name(), "replayed": true, "status": row.reported_status(), "spend": spend_json(row) });
    if matches!(row.reported_status(), "rejected" | "expired") {
        body["message"] = Value::String(format!(
            "This payment was not made: its approval was {}. Nothing was paid. Ask the human before requesting it again under a new idempotency_key.",
            row.reported_status()
        ));
        body["next_action"] = Value::String("ask_human".into());
    }
    if row.status == "awaiting_approval" {
        body["next_action"] = Value::String("poll_status".into());
        if let Some(id) = row.approval_id {
            body["poll"] = json!({ "tool": Tool::ActionsStatus.wire_name(), "arguments": { "approval_id": id } });
        }
    }
    (200, body.to_string())
}

fn spend<B: MemoryBackend + ?Sized>(
    args: &Value,
    backend: &B,
    actor: &str,
    now_ms: i64,
    enqueue: impl FnOnce(SendAction, Preview) -> Answer,
) -> Answer {
    let tool = Tool::PaymentsSpend.wire_name();
    let key = match key_arg(args, true) {
        Ok(key) => key.unwrap_or_default(),
        Err(answer) => return answer,
    };
    let memo = args.get("memo").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    if memo.chars().count() > MAX_MEMO_CHARS || memo.chars().any(char::is_control) {
        return bad("invalid_memo", "memo must be at most 280 characters on one line.");
    }
    let order = PaymentOrder {
        spend_ref: derived_id("psp", actor, &key),
        payee: str_arg(args, "payee").unwrap_or_default().to_string(),
        currency: str_arg(args, "currency").unwrap_or_default().to_string(),
        amount_minor: amount_arg(args, "amount_minor").unwrap_or(0),
        mandate_id: None,
    };
    if let Err(code) = order.validate() {
        let message = match code {
            "invalid_payee" => "payee must be 1-128 visible ASCII characters.",
            "invalid_currency" => "currency must be an ISO 4217 code in capitals, e.g. USD.",
            _ => "amount_minor must be a positive integer in minor units.",
        };
        return bad(code, message);
    }
    // A retry replays what was recorded: a payment is never attempted twice for one key.
    match backend.payments(PaymentsOp::SpendByKey { actor, key: &key }) {
        Ok(PaymentsReply::Spend(Some(row))) => return replay(&row),
        Ok(PaymentsReply::Spend(None)) => {}
        Ok(_) => return store_fault("payments_unavailable"),
        Err(code) => return store_fault(&code),
    }
    // The rail is asked first: without one nothing is recorded or paid, and a class mandate is
    // checked against the class the rail reports, never one the caller asserts.
    let payee_class = match backend.payments(PaymentsOp::PayeeClass(&order.payee)) {
        Ok(PaymentsReply::Class(class)) => class,
        Ok(_) => return store_fault("payments_unavailable"),
        Err(code) => return store_fault(&code),
    };
    // The mandate to check: the one named, else the first active mandate of this agent that
    // covers the payment.
    let candidates: Vec<MandateRow> = match args.get("mandate_id") {
        None | Some(Value::Null) => match backend.payments(PaymentsOp::List(100)) {
            Ok(PaymentsReply::Mandates(rows)) => rows.into_iter().filter(|row| row.agent == actor && row.status == "active").collect(),
            Ok(_) => return store_fault("payments_unavailable"),
            Err(code) => return store_fault(&code),
        },
        Some(Value::String(id)) => match get_mandate(backend, id) {
            Ok(Some(row)) => vec![row],
            Ok(None) => return not_found(),
            Err(answer) => return answer,
        },
        Some(_) => return bad("invalid_mandate_id", "mandate_id must be a string."),
    };
    let mut why = "no_mandate";
    let mut chosen = None;
    for row in &candidates {
        let Some(standing) = standing(row) else { continue };
        match payment::authorize(&standing, actor, &order, payee_class.as_deref(), now_ms) {
            Ok(authorization) => {
                chosen = Some(authorization);
                break;
            }
            Err(reason) => why = reason.as_str(),
        }
    }
    if let Some(authorization) = chosen {
        let confirmed = payment::confirmed_under_mandate(&authorization, order.clone(), &memo);
        let check_order = order.clone();
        let check_class = payee_class.clone();
        let check_actor = actor.to_string();
        // The same bounds, re-read inside the reservation's transaction.
        let check = move |row: Option<&MandateRow>| -> Result<(), &'static str> {
            let row = row.ok_or("mandate_not_found")?;
            let standing = standing(row).ok_or("mandate_unreadable")?;
            payment::authorize(&standing, &check_actor, &check_order, check_class.as_deref(), now_ms)
                .map(|_| ())
                .map_err(|reason| reason.as_str())
        };
        let new = NewSpend {
            spend_ref: &order.spend_ref,
            actor,
            idempotency_key: &key,
            mandate_id: Some(authorization.mandate_id()),
            authorization: "mandate",
            approval_id: Some(authorization.grant_approval_id()),
            decided_by: Some(authorization.granted_by()),
            payee: &order.payee,
            currency: &order.currency,
            amount_minor: order.amount_minor,
            rail: DEFAULT_RAIL,
            now_ms,
        };
        match backend.payments(PaymentsOp::Reserve { spend: &new, check: &check }) {
            Ok(PaymentsReply::Reserved(Reserved::New)) => {}
            Ok(PaymentsReply::Reserved(Reserved::Replay(row))) => return replay(&row),
            // Lost a race for the last of the cap: fall back to a per-payment approval below.
            Ok(PaymentsReply::Reserved(Reserved::Refused(reason))) => {
                return queue_payment(backend, actor, &key, order, &memo, reason, now_ms, enqueue)
            }
            Ok(_) => return store_fault("payments_unavailable"),
            Err(code) => return store_fault(&code),
        }
        let settled = backend.payments(PaymentsOp::PayReserved { confirmed: &confirmed });
        let row = match backend.payments(PaymentsOp::SpendByKey { actor, key: &key }) {
            Ok(PaymentsReply::Spend(Some(row))) => row,
            _ => return store_fault("payments_unavailable"),
        };
        let body = json!({
            "tool": tool,
            "status": row.status,
            "authorization": "mandate",
            "mandate_id": authorization.mandate_id(),
            "spend": spend_json(&row),
        });
        return match (settled, row.status.as_str()) {
            (Ok(_), "settled") => (200, body.to_string()),
            _ => {
                let mut body = body;
                body["error"] = Value::String("payment_failed".into());
                body["message"] = Value::String(
                    "The payment did not settle. Check spend.status and reason_class; if it says executing or authorized the outcome is unknown, so check with the payee before paying again under a new key.".into(),
                );
                body["next_action"] = Value::String("check_status".into());
                (502, body.to_string())
            }
        };
    }
    queue_payment(backend, actor, &key, order, &memo, why, now_ms, enqueue)
}

/// Outside every mandate: record the spend as awaiting a human, then queue the per-payment L3.
#[allow(clippy::too_many_arguments)]
fn queue_payment<B: MemoryBackend + ?Sized>(
    backend: &B,
    actor: &str,
    key: &str,
    order: PaymentOrder,
    memo: &str,
    why: &str,
    now_ms: i64,
    enqueue: impl FnOnce(SendAction, Preview) -> Answer,
) -> Answer {
    let new = NewSpend {
        spend_ref: &order.spend_ref,
        actor,
        idempotency_key: key,
        mandate_id: None,
        authorization: "approval",
        approval_id: None,
        decided_by: None,
        payee: &order.payee,
        currency: &order.currency,
        amount_minor: order.amount_minor,
        rail: DEFAULT_RAIL,
        now_ms,
    };
    let none = |_: Option<&MandateRow>| -> Result<(), &'static str> { Ok(()) };
    match backend.payments(PaymentsOp::Reserve { spend: &new, check: &none }) {
        Ok(PaymentsReply::Reserved(Reserved::New)) => {}
        Ok(PaymentsReply::Reserved(Reserved::Replay(row))) => return replay(&row),
        Ok(_) => return store_fault("payments_unavailable"),
        Err(code) => return store_fault(&code),
    }
    let spend_ref = order.spend_ref.clone();
    let (send, preview) = payment::payment_request(order, memo);
    let (status, body) = enqueue(send, preview);
    let approval_id = (status == 202)
        .then(|| serde_json::from_str::<Value>(&body).ok().and_then(|v| v["approval_id"].as_u64()))
        .flatten();
    // Best effort: a row whose approval could not be recorded is still claimed by its spend_ref.
    let _ = backend.payments(PaymentsOp::Queued { spend_ref: &spend_ref, approval_id });
    (
        status,
        with_fields(
            &body,
            json!({
                "tool": Tool::PaymentsSpend.wire_name(),
                "spend_ref": spend_ref,
                "authorization": "approval",
                "reason": why,
            }),
        ),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::backend::{ReadItem, ReadParams};
    use shogun_agents::approval::ConfirmedSend;
    use std::sync::Mutex;

    const NOW: i64 = 1_760_000_000_000;

    /// A store in memory with the real V105 schema behind it.
    struct Store {
        conn: Mutex<rusqlite::Connection>,
        rail: bool,
        paid: Mutex<Vec<ConfirmedSend>>,
    }

    impl Store {
        fn new(rail: bool) -> Self {
            Self { conn: Mutex::new(shogun_memory::open_in_memory().unwrap()), rail, paid: Mutex::new(Vec::new()) }
        }
        fn approve(&self, approval_id: i64) {
            self.conn
                .lock()
                .unwrap()
                .execute(
                    "INSERT INTO approval_decisions (approval_id, decision, decided_by, decided_at, via, level) VALUES (?1, 'approve', 'human:owner', 1, 'token', 'L3')",
                    [approval_id],
                )
                .unwrap();
        }
        fn grant(&self, grant: &MandateGrant, approval_id: u64) {
            let mut conn = self.conn.lock().unwrap();
            shogun_memory::payment_mandates::grant(&mut conn, &grant.mandate_id, &grant.agent, &terms_row(&grant.terms), grant.amends, approval_id, NOW).unwrap();
        }
    }

    impl MemoryBackend for Store {
        fn read(&self, _: Tool, _: &ReadParams) -> Vec<ReadItem> {
            vec![]
        }
        fn payments(&self, op: PaymentsOp<'_>) -> Result<PaymentsReply, String> {
            use shogun_memory::payment_mandates as pm;
            let mut conn = self.conn.lock().unwrap();
            let e = |e: pm::MandateError| match e {
                pm::MandateError::NotFound => "mandate_not_found".to_string(),
                pm::MandateError::Revoked => "mandate_revoked".to_string(),
                _ => "payments_unavailable".to_string(),
            };
            Ok(match op {
                PaymentsOp::Get(id) => PaymentsReply::Mandate(pm::get(&conn, id).map_err(e)?),
                PaymentsOp::List(n) => PaymentsReply::Mandates(pm::list(&conn, n).map_err(e)?),
                PaymentsOp::Spends(id) => PaymentsReply::Spends(pm::spends_for(&conn, id, 20).map_err(e)?),
                PaymentsOp::Narrow { mandate_id, terms } => {
                    pm::narrow(&conn, mandate_id, terms, NOW).map_err(e)?;
                    PaymentsReply::Done(true)
                }
                PaymentsOp::Revoke { mandate_id, by } => PaymentsReply::Done(pm::revoke(&conn, mandate_id, by, NOW).map_err(e)?),
                PaymentsOp::SpendByKey { actor, key } => PaymentsReply::Spend(pm::spend_by_key(&conn, actor, key).map_err(e)?),
                PaymentsOp::PayeeClass(payee) if self.rail => PaymentsReply::Class(payee.split_once(':').map(|(c, _)| c.to_string())),
                PaymentsOp::PayeeClass(_) => return Err("payment_rail_unavailable".into()),
                PaymentsOp::Reserve { spend, check } => PaymentsReply::Reserved(pm::reserve_spend(&mut conn, spend, check).map_err(e)?),
                PaymentsOp::PayReserved { confirmed } => {
                    let SendAction::Pay { order } = &confirmed.action else { return Err("not_a_payment".into()) };
                    self.paid.lock().unwrap().push(confirmed.clone());
                    PaymentsReply::Done(pm::settle_spend(&conn, &order.spend_ref, "settled", None, Some("stub_1"), NOW).map_err(e)?)
                }
                PaymentsOp::Queued { spend_ref, approval_id: Some(id) } => PaymentsReply::Done(pm::set_spend_approval(&conn, spend_ref, id).map_err(e)?),
                PaymentsOp::Queued { spend_ref, approval_id: None } => {
                    PaymentsReply::Done(pm::settle_spend(&conn, spend_ref, "not_queued", Some("not_queued"), None, NOW).map_err(e)?)
                }
                PaymentsOp::Close { spend_ref, approval_id, outcome, by } => {
                    PaymentsReply::Done(pm::close_unapproved_spend(&conn, spend_ref, approval_id, outcome, by, NOW).map_err(e)?)
                }
                PaymentsOp::Reconcile { now_ms } => PaymentsReply::Done(
                    pm::reconcile_unapproved_spends(&conn, now_ms, i64::try_from(APPROVAL_TIMEOUT_MS).unwrap()).map_err(e)? > 0,
                ),
            })
        }
    }

    type Queued = Mutex<Vec<(SendAction, Preview)>>;

    fn call(store: &Store, queued: &Queued, tool: Tool, args: Value, actor: &str) -> (u16, Value) {
        let (status, body) = serve(tool, &args, store, actor, NOW, |send, preview| {
            let mut q = queued.lock().unwrap();
            q.push((send, preview));
            (202, json!({ "pending": true, "approval_id": q.len(), "level": "L3", "origin": "mcp" }).to_string())
        });
        (status, serde_json::from_str(&body).unwrap())
    }

    fn request_args() -> Value {
        json!({ "payee_class": "cloud", "currency": "USD", "max_per_payment_minor": 5000, "total_cap_minor": 8000,
                "expires_in_days": 30, "purpose": "Build minutes", "idempotency_key": "req-1" })
    }

    /// Request → human approves → the mandate exists; returns its id.
    fn granted(store: &Store, queued: &Queued) -> String {
        let (status, body) = call(store, queued, Tool::PaymentsMandatesRequest, request_args(), "agent:builder");
        assert_eq!(status, 202, "{body}");
        assert_eq!(body["pending"], true);
        let (send, _) = queued.lock().unwrap().last().cloned().unwrap();
        let SendAction::GrantPaymentMandate { grant } = send else { panic!("a grant") };
        assert_eq!(body["mandate_id"], grant.mandate_id.as_str());
        store.approve(1);
        store.grant(&grant, 1);
        grant.mandate_id
    }

    #[test]
    fn a_mandate_is_granted_only_through_the_queue_and_pre_approves_spends_within_bounds() {
        let store = Store::new(true);
        let queued = Queued::default();
        let id = granted(&store, &queued);

        let spend = |amount: u64, key: &str| json!({ "payee": "cloud:hetzner", "currency": "USD", "amount_minor": amount, "idempotency_key": key, "memo": "June" });
        let (status, body) = call(&store, &queued, Tool::PaymentsSpend, spend(5000, "s1"), "agent:builder");
        assert_eq!(status, 200, "{body}");
        assert_eq!((body["status"].as_str(), body["authorization"].as_str()), (Some("settled"), Some("mandate")));
        assert_eq!(body["spend"]["approval_id"], 1, "provenance: the grant decision");
        assert_eq!(body["spend"]["decided_by"], "human:owner");
        assert_eq!(store.paid.lock().unwrap().len(), 1);

        // Replay: same key, nothing paid twice.
        let (status, body) = call(&store, &queued, Tool::PaymentsSpend, spend(5000, "s1"), "agent:builder");
        assert_eq!((status, body["replayed"].as_bool()), (200, Some(true)));
        assert_eq!(store.paid.lock().unwrap().len(), 1);

        // Over the remaining cap: a per-payment L3 approval, never a payment.
        let (status, body) = call(&store, &queued, Tool::PaymentsSpend, spend(4000, "s2"), "agent:builder");
        assert_eq!(status, 202, "{body}");
        assert_eq!((body["authorization"].as_str(), body["reason"].as_str()), (Some("approval"), Some("over_total_cap")));
        assert!(matches!(queued.lock().unwrap().last(), Some((SendAction::Pay { .. }, _))));
        assert_eq!(store.paid.lock().unwrap().len(), 1);

        // Another agent cannot spend under it.
        let (status, body) = call(&store, &queued, Tool::PaymentsSpend, spend(10, "s3"), "agent:other");
        assert_eq!((status, body["reason"].as_str()), (202, Some("no_mandate")));

        // Listing shows spent and remaining.
        let (_, body) = call(&store, &queued, Tool::PaymentsMandatesList, json!({}), "agent:builder");
        assert_eq!(body["mandates"][0]["spent_minor"], 5000);
        assert_eq!(body["mandates"][0]["remaining_minor"], 3000);

        // Revoked: every later spend asks a human.
        let (status, body) = call(&store, &queued, Tool::PaymentsMandatesRevoke, json!({ "mandate_id": id }), "human:owner");
        assert_eq!((status, body["revoked"].as_bool()), (200, Some(true)));
        let (status, body) = call(&store, &queued, Tool::PaymentsSpend, spend(10, "s4"), "agent:builder");
        assert_eq!(status, 202);
        assert_eq!(body["reason"], "no_mandate");
        let (status, body) = call(&store, &queued, Tool::PaymentsMandatesRevoke, json!({ "mandate_id": id }), "human:owner");
        assert_eq!((status, body["already_revoked"].as_bool()), (200, Some(true)));
        assert_eq!(store.paid.lock().unwrap().len(), 1);
    }

    #[test]
    fn narrowing_applies_directly_and_widening_queues() {
        let store = Store::new(true);
        let queued = Queued::default();
        let id = granted(&store, &queued);
        let mut narrower = request_args();
        narrower["mandate_id"] = json!(id);
        narrower["total_cap_minor"] = json!(6000);
        narrower["expires_in_days"] = json!(10);
        let (status, body) = call(&store, &queued, Tool::PaymentsMandatesRequest, narrower, "agent:builder");
        assert_eq!((status, body["narrowed"].as_bool()), (200, Some(true)), "{body}");
        assert_eq!(body["mandate"]["total_cap_minor"], 6000);
        assert_eq!(queued.lock().unwrap().len(), 1, "nothing queued");

        let mut wider = request_args();
        wider["mandate_id"] = json!(id);
        wider["total_cap_minor"] = json!(9000);
        let (status, _) = call(&store, &queued, Tool::PaymentsMandatesRequest, wider.clone(), "agent:builder");
        assert_eq!(status, 202);
        assert!(matches!(queued.lock().unwrap().last(), Some((SendAction::GrantPaymentMandate { grant }, _)) if grant.amends));
        // Only the agent it was granted to may amend it.
        let (status, body) = call(&store, &queued, Tool::PaymentsMandatesRequest, wider, "agent:other");
        assert_eq!((status, body["error"].as_str()), (403, Some("mandate_not_yours")));
    }

    #[test]
    fn no_rail_no_record_and_bad_arguments_name_the_fix() {
        let store = Store::new(false);
        let queued = Queued::default();
        let (status, body) = call(&store, &queued, Tool::PaymentsSpend,
            json!({ "payee": "cloud:x", "currency": "USD", "amount_minor": 1, "idempotency_key": "k" }), "agent:builder");
        assert_eq!((status, body["error"].as_str()), (503, Some("payment_rail_unavailable")));
        assert!(queued.lock().unwrap().is_empty());
        for (args, code) in [
            (json!({ "payee": "x", "currency": "usd", "amount_minor": 1, "idempotency_key": "k" }), "invalid_currency"),
            (json!({ "payee": "x", "currency": "USD", "amount_minor": 0, "idempotency_key": "k" }), "invalid_amount"),
            (json!({ "payee": "x", "currency": "USD", "amount_minor": 1 }), "invalid_idempotency_key"),
        ] {
            let (status, body) = call(&store, &queued, Tool::PaymentsSpend, args, "agent:builder");
            assert_eq!((status, body["error"].as_str()), (400, Some(code)));
        }
        let mut both = request_args();
        both["payee"] = json!("cloud:x");
        let (status, body) = call(&store, &queued, Tool::PaymentsMandatesRequest, both, "agent:builder");
        assert_eq!((status, body["error"].as_str()), (400, Some("invalid_payee")));
    }

    fn call_at(store: &Store, queued: &Queued, tool: Tool, args: Value, actor: &str, now: i64) -> (u16, Value) {
        let (status, body) = serve(tool, &args, store, actor, now, |send, preview| {
            let mut q = queued.lock().unwrap();
            q.push((send, preview));
            (202, json!({ "pending": true, "approval_id": q.len(), "level": "L3", "origin": "mcp" }).to_string())
        });
        (status, serde_json::from_str(&body).unwrap())
    }

    #[test]
    fn a_rejected_or_expired_per_payment_approval_closes_the_spend_and_never_pays() {
        let store = Store::new(true);
        let queued = Queued::default();
        let spend = |key: &str| json!({ "payee": "cloud:hetzner", "currency": "USD", "amount_minor": 700, "idempotency_key": key });
        let (status, body) = call(&store, &queued, Tool::PaymentsSpend, spend("r1"), "agent:builder");
        assert_eq!((status, body["authorization"].as_str()), (202, Some("approval")), "{body}");
        let spend_ref = body["spend_ref"].as_str().unwrap().to_string();
        let approval_id = body["approval_id"].as_u64().unwrap();

        // The decide route closes it when the human rejects; twice is harmless.
        for expected in [true, false] {
            let closed = store.payments(PaymentsOp::Close { spend_ref: &spend_ref, approval_id, outcome: "rejected", by: "human:owner" });
            assert!(matches!(closed, Ok(PaymentsReply::Done(done)) if done == expected));
        }
        let (status, body) = call(&store, &queued, Tool::PaymentsSpend, spend("r1"), "agent:builder");
        assert_eq!((status, body["replayed"].as_bool(), body["status"].as_str()), (200, Some(true), Some("rejected")), "{body}");
        assert_eq!(body["spend"]["decided_by"], "human:owner");
        assert_eq!(body["spend"]["approval_id"], approval_id);
        assert_eq!(body["next_action"], "ask_human");
        assert!(body.get("poll").is_none());

        // Never approved and the window elapsed: the next payments read closes it as expired.
        let (status, body) = call(&store, &queued, Tool::PaymentsSpend, spend("e1"), "agent:builder");
        assert_eq!(status, 202, "{body}");
        let window = i64::try_from(APPROVAL_TIMEOUT_MS).unwrap();
        let (_, body) = call_at(&store, &queued, Tool::PaymentsSpend, spend("e1"), "agent:builder", NOW + window);
        assert_eq!(body["status"], "awaiting_approval", "inside the window: {body}");
        let (_, body) = call_at(&store, &queued, Tool::PaymentsSpend, spend("e1"), "agent:builder", NOW + window + 1);
        assert_eq!((body["status"].as_str(), body["spend"]["decided_by"].as_str()), (Some("expired"), Some("system:approval-timeout")), "{body}");
        assert!(store.paid.lock().unwrap().is_empty(), "nothing was paid");
        // A closed spend never counts against anything and is never claimed.
        let conn = store.conn.lock().unwrap();
        assert_eq!(shogun_memory::payment_mandates::claim_approved_spend(&conn, &spend_ref, approval_id).ok().flatten(), None);
    }

    #[test]
    fn ids_are_stable_per_actor_and_key() {
        assert_eq!(derived_id("psp", "agent:a", "k"), derived_id("psp", "agent:a", "k"));
        assert_ne!(derived_id("psp", "agent:a", "k"), derived_id("psp", "agent:b", "k"));
        assert!(payment::plausible_ref(&derived_id("pmd", "agent:a", "k")));
    }
}
