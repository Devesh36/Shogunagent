//! The REST server's governance glue (E15 remote approvals, E18 audit / ACL / Team): the routes
//! no agent bearer reaches (deciding, granting, teams), the approval summaries, the audit record
//! appended after every write, and the approval link joined to every pending envelope.
//!
//! | Route                                  | Who                                   |
//! |----------------------------------------|---------------------------------------|
//! | `GET  /v1/approvals/{id}` + token header | the gateway (signed decision token) |
//! | `POST /v1/approvals/{id}/decide`       | the gateway (token) or the operator (admin bearer) |
//! | `POST /v1/acl/grant`, `/v1/acl/revoke` | the operator (admin bearer)           |
//! | `GET  /v1/teams`, `POST /v1/teams`     | the operator (admin bearer), multi-principal only |
//! | `DELETE /v1/principals/{id}`           | the operator (admin bearer), multi-principal only (E39) |
//!
//! An agent's bearer on any of them is `403 human_only`: an agent never approves its own L3 and
//! never grants itself access.

use std::sync::Arc;

use serde_json::{json, Value};
use subtle::ConstantTimeEq;

use super::AppState;
use crate::backend::MemoryBackend;
use crate::memory_api::Tool;
use crate::remote_approvals::{self, Applied, DecisionRecord, LedgerOp, LedgerReply, RemoteDecision};
use crate::rest::{Method, Routed};
use shogun_memory::approval_ledger::{DecisionRow, Execution};

/// The 503 of a decision the principal's store cannot record (V103): nothing was decided.
fn ledger_unavailable() -> (u16, String) {
    error(503, "approval_ledger_unavailable", "The principal's store cannot record the decision right now; nothing was decided. Retry.")
}

/// The 403 body of a cross-principal call that is not a world-state read.
pub(super) const CROSS_READ_ONLY: &str = r#"{"error":"cross_principal_read_only","message":"A teammate's store answers world-state reads only: no writes, scratch files, approvals, audit log or grants.","next_action":null}"#;

/// One request as the governance routes see it.
pub(super) struct Incoming<'a> {
    pub method: Method,
    pub path: &'a str,
    pub bearer: Option<&'a str>,
    pub approval_token: Option<&'a str>,
    pub body: Option<&'a str>,
    pub request_id: &'a str,
}

/// What a call was, for its audit record.
#[derive(Debug, Clone, Copy)]
pub(super) struct WriteInfo {
    route: &'static str,
    level: &'static str,
}

/// The facts `after_call` needs about the call it follows.
pub(super) struct CallFacts<'a> {
    pub actor: &'a str,
    pub principal: Option<&'a str>,
    pub write: Option<WriteInfo>,
    pub cross: Option<&'a str>,
    pub request_id: &'a str,
}

fn error(status: u16, code: &str, message: &str) -> (u16, String) {
    (status, json!({ "error": code, "message": message, "next_action": Value::Null }).to_string())
}

fn level_label(level: shogun_agents::permission::Level) -> &'static str {
    match level {
        shogun_agents::permission::Level::L1 => "L1",
        shogun_agents::permission::Level::L2 => "L2",
        shogun_agents::permission::Level::L3 => "L3",
    }
}

/// The audit label of a write route, or `None` for a read.
pub(super) fn write_info(routed: &Routed) -> Option<WriteInfo> {
    match routed {
        Routed::Write { tool, level } => Some(WriteInfo { route: tool.wire_name(), level: level_label(*level) }),
        Routed::Action => Some(WriteInfo { route: Tool::ActionsExecute.wire_name(), level: "per_action" }),
        _ => None,
    }
}

/// Whether a cross-principal (Team) request may take this route: world-state reads only.
pub(super) fn cross_allowed(routed: &Routed) -> bool {
    match routed {
        Routed::Read { tool, .. } => !matches!(
            tool,
            Tool::ApprovalsList | Tool::ApprovalsGet | Tool::AuditList | Tool::AclList | Tool::MemoryFs | Tool::ActionsStatus
        ),
        _ => false,
    }
}

impl AppState {
    fn now_u64(&self) -> u64 {
        u64::try_from((self.clock)()).unwrap_or(0)
    }

    fn multi(&self) -> bool {
        self.principals.is_some()
    }

    /// Whether this server carries out an approved action itself.
    pub(super) fn executes(&self) -> bool {
        self.executor.is_some()
    }

    /// Whether `bearer` is the operator's admin credential (constant-time).
    fn admin_ok(&self, bearer: Option<&str>) -> bool {
        use sha2::Digest as _;
        match (self.admin_digest, bearer) {
            (Some(expected), Some(given)) => {
                let digest: [u8; 32] = sha2::Sha256::digest(given.trim().as_bytes()).into();
                expected.ct_eq(&digest).unwrap_u8() == 1
            }
            _ => false,
        }
    }

    /// The refusal for a non-admin caller of an operator route: 403 for a known agent bearer, 401
    /// otherwise.
    fn not_admin(&self, bearer: Option<&str>, what: &str) -> (u16, String) {
        let known = bearer.is_some_and(|token| {
            self.tokens.current().token_id(token).is_some()
                || self.principals.as_ref().is_some_and(|resolver| resolver.resolve(token).is_ok())
        });
        if known {
            error(403, "human_only", &format!("{what} is a human's decision: an agent key cannot do it. Use the approval link, the Slack app, or the operator's CLI with SHOGUN_ADMIN_TOKEN."))
        } else {
            error(401, "unauthorized", "Present the operator's admin credential or a signed approval token.")
        }
    }

    /// Run `f` on the one approval queue this server reads (the durable store, else the
    /// in-process queue).
    fn on_queue<R>(&self, f: impl FnOnce(&mut shogun_agents::approval::ApprovalQueue) -> R) -> Result<R, (u16, String)> {
        if let Some(store) = &self.approval_store {
            return store.with_queue(f).map_err(|why| (500, super::approval_store_error(&why)));
        }
        match self.approvals.lock() {
            Ok(mut queue) => Ok(f(&mut queue)),
            Err(_) => Err((500, r#"{"error":"internal"}"#.to_string())),
        }
    }

    /// The store an operator route or a decision writes to: the principal's (multi), else the
    /// server's own.
    fn store_of(&self, principal: Option<&str>) -> Result<Arc<dyn MemoryBackend>, (u16, String)> {
        match (&self.principals, principal) {
            (Some(resolver), Some(principal)) => resolver.backend_for_principal(principal),
            (Some(_), None) => Err(error(400, "principal_required", "Name the principal (\"principal\": \"<id>\") in multi-principal mode.")),
            (None, _) => Ok(self.backend.clone()),
        }
    }

    /// `who` is (actor, principal); `what` is (route, level, outcome).
    fn audit(&self, backend: &dyn MemoryBackend, who: (&str, Option<&str>), what: (&str, &str, &str), request_id: &str) {
        let (actor, principal) = who;
        let (route, level, outcome) = what;
        let now = (self.clock)();
        if let Some(record) = crate::governance_api::record(now, actor, principal, route, level, outcome, request_id) {
            // The write it describes already happened: a failed audit append is reported once on
            // stderr (labels only) and the answer stands.
            if backend.audit_append(&record).is_err() {
                eprintln!("shogun-api: audit append failed for {route} ({outcome})");
            }
        }
    }
}

/// E50: who a connector route (`connector_vault`) acts for. The caller must present the operator's
/// admin credential (the gateway's `SHOGUN_SIDECAR_ADMIN_TOKEN`) in `Authorization` and the
/// principal's own sidecar bearer (a derived `sgw1` bearer or a listed token) in
/// `X-Shogun-Principal-Authorization`; the principal is the one that bearer resolves to, never one
/// the body names. An agent bearer alone is refused (403), and a purged principal stays purged.
pub(super) fn connector_principal(state: &AppState, admin: Option<&str>, principal_bearer: Option<&str>) -> Result<String, (u16, String)> {
    if !state.admin_ok(admin) {
        let known = admin.is_some_and(|token| {
            state.tokens.current().token_id(token).is_some()
                || state.principals.as_ref().is_some_and(|resolver| resolver.resolve(token).is_ok())
        });
        return Err(if known {
            error(403, "admin_required", "Connector credentials are served to the gateway only: present the operator's admin credential, not an agent's bearer.")
        } else {
            error(401, "unauthorized", "Present the operator's admin credential.")
        });
    }
    let Some(resolver) = &state.principals else {
        return Err(error(404, "principals_unavailable", "Connector credentials exist only in multi-principal mode."));
    };
    let Some(bearer) = principal_bearer else {
        return Err(error(401, "principal_required", "Name the principal with its own sidecar bearer in X-Shogun-Principal-Authorization."));
    };
    let resolved = resolver.resolve(bearer)?;
    resolved
        .principal
        .ok_or_else(|| error(500, "internal", "The resolver named no principal."))
}

/// Answer an operator / hosted-surface route, or `None` to let the agent routing take it.
pub(super) fn admin_route(state: &AppState, req: &Incoming<'_>) -> Option<(u16, String)> {
    let segments: Vec<&str> = req.path.trim_matches('/').split('/').collect();
    match (req.method, segments.as_slice()) {
        (Method::Post, ["v1", "approvals", id, "decide"]) => Some(match id.parse::<u64>() {
            Ok(id) if id > 0 => decide(state, req, id),
            _ => error(404, "not_found", "No such approval."),
        }),
        (Method::Get, ["v1", "approvals", id]) if req.approval_token.is_some() => Some(match id.parse::<u64>() {
            Ok(id) if id > 0 => summary_by_token(state, req, id),
            _ => error(404, "not_found", "No such approval."),
        }),
        (Method::Post, ["v1", "acl", op @ ("grant" | "revoke")]) => Some(acl_change(state, req, *op == "grant")),
        (_, ["v1", "teams"]) => Some(teams(state, req)),
        _ => None,
    }
}

fn verify_token(state: &AppState, token: &str, id: u64) -> Result<remote_approvals::TokenClaims, (u16, String)> {
    let Some(signer) = state.remote.signer() else {
        return Err(error(503, "approvals_not_configured", "Remote approvals are not configured on this server (SHOGUN_APPROVAL_SIGNING_KEY)."));
    };
    let claims = signer.verify(token, state.now_u64()).map_err(|e| {
        let (status, code, message) = e.refusal();
        error(status, code, message)
    })?;
    if claims.approval_id != id {
        return Err(error(403, "invalid_token", "The approval token was issued for another approval."));
    }
    // The ledger names whose approval this is; a token minted for one principal never reads or
    // decides another's.
    if let Some(entry) = state.remote.ledger().get(id) {
        if entry.principal != claims.principal {
            return Err(error(404, "not_found", "No such approval."));
        }
    }
    Ok(claims)
}

fn summary_by_token(state: &AppState, req: &Incoming<'_>, id: u64) -> (u16, String) {
    let Some(token) = req.approval_token else {
        return error(401, "unauthorized", "Present the approval token.");
    };
    let claims = match verify_token(state, token, id) {
        Ok(claims) => claims,
        Err(refusal) => return refusal,
    };
    let now = state.now_u64();
    match state.on_queue(|queue| remote_approvals::summary(queue, state.remote.ledger(), id, now)) {
        Ok(Some(summary)) => (200, json!({ "approval": summary }).to_string()),
        // After a restart the queue and the in-process ledger forget; the principal's store
        // still holds the decision (V103).
        Ok(None) => match state.store_of(claims.principal.as_deref()).ok().and_then(|store| persisted(store.as_ref(), id)) {
            Some(row) => (200, json!({ "approval": remote_approvals::persisted_summary(&row, state.executes()) }).to_string()),
            None => error(404, "not_found", "No such approval."),
        },
        Err(refusal) => refusal,
    }
}

/// The decision the principal's store persisted for `id`, if any.
fn persisted(backend: &dyn MemoryBackend, id: u64) -> Option<DecisionRow> {
    match backend.approval_ledger(LedgerOp::Get(id)) {
        Ok(LedgerReply::Row(row)) => row,
        _ => None,
    }
}

/// `actions.status` on REST: the queue's answer for an approval the caller may see, else the
/// decision the caller's store persisted, else `unknown`. Multi-principal: an approval another
/// principal queued reads as `unknown`, exactly like one that never existed.
pub(super) fn approval_status(state: &AppState, backend: &dyn MemoryBackend, id: u64, principal: Option<&str>) -> (u16, String) {
    let now = (state.clock)();
    let owner_ok = !state.multi() || state.remote.ledger().get(id).is_some_and(|e| e.principal.as_deref() == principal);
    let polled = if owner_ok {
        match state.on_queue(|queue| crate::rest::poll_approval(id, queue, now)) {
            Ok(body) => body,
            Err(refusal) => return refusal,
        }
    } else {
        json!({ "approval_id": id, "status": "unknown" }).to_string()
    };
    (200, remote_approvals::augment_status(id, &polled, || persisted(backend, id), state.executes()))
}

fn decide(state: &AppState, req: &Incoming<'_>, id: u64) -> (u16, String) {
    let body = match remote_approvals::decide_request(req.body) {
        Ok(body) => body,
        Err(message) => {
            let code = message.split(':').next().unwrap_or("invalid_request").to_string();
            return error(400, &code, &message);
        }
    };
    let now = state.now_u64();
    let token = req.approval_token.map(str::to_string).or_else(|| body.token.clone());
    let (principal, via) = match token {
        Some(token) => {
            let claims = match verify_token(state, &token, id) {
                Ok(claims) => claims,
                Err(refusal) => return refusal,
            };
            if body.nonce.as_deref() != Some(claims.nonce.as_str()) {
                return error(403, "nonce_mismatch", "The nonce does not match the approval token.");
            }
            if !state.remote.ledger().consume(&claims.nonce, claims.expires_ms, now) {
                return error(409, "token_replayed", "This approval token was already used. Each link decides once.");
            }
            // The in-process ledger forgets on restart; the principal's store does not (V103). A
            // nonce the store cannot record is refused: no decision without a durable record.
            let spent = state.store_of(claims.principal.as_deref()).and_then(|store| {
                store
                    .approval_ledger(LedgerOp::SpendNonce {
                        nonce: &claims.nonce,
                        approval_id: id,
                        expires_ms: claims.expires_ms,
                        now_ms: now,
                    })
                    .map_err(|_| ledger_unavailable())
            });
            match spent {
                Ok(LedgerReply::Done(true)) => {}
                Ok(_) => return error(409, "token_replayed", "This approval token was already used. Each link decides once."),
                Err(refusal) => return refusal,
            }
            (claims.principal, "token")
        }
        None if state.admin_ok(req.bearer) => {
            let principal = req
                .body
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                .and_then(|v| v["principal"].as_str().map(str::to_string))
                .or_else(|| state.remote.ledger().get(id).and_then(|e| e.principal));
            (principal, "admin")
        }
        None => return state.not_admin(req.bearer, "Approving or rejecting"),
    };
    // In multi-principal mode an approval belongs to the principal its ledger entry names.
    if state.multi() {
        let owner = state.remote.ledger().get(id).and_then(|e| e.principal);
        if owner.is_none() || owner != principal {
            return error(404, "not_found", "No such approval.");
        }
    }
    // The queue releases the confirmed action to this one decision (a racing decision on the same
    // id finds nothing pending); a rejection, an expiry or an unknown id releases nothing.
    let (applied, confirmed) = match state.on_queue(|queue| remote_approvals::apply_with_send(queue, id, body.decision, now)) {
        Ok(answer) => answer,
        Err(refusal) => return refusal,
    };
    let level = state.remote.ledger().get(id).map_or_else(|| "L3".to_string(), |e| e.level);
    let outcome = match applied {
        Applied::Approved => "approved",
        Applied::Rejected => "rejected",
        Applied::Expired => "expired",
        Applied::NotPending => "not_pending",
    };
    let store = state.store_of(principal.as_deref()).ok();
    let mut recorded = false;
    if matches!(applied, Applied::Approved | Applied::Rejected) {
        state.remote.ledger().decide(
            id,
            DecisionRecord { decision: body.decision, decided_by: body.decided_by.clone(), decided_ms: now, via },
        );
        // The durable record (V103): a restart still knows who decided and what came of it.
        let approve = body.decision == RemoteDecision::Approve;
        let execution = match (approve, state.executes()) {
            (false, _) => Execution::None,
            (true, true) => Execution::Pending,
            (true, false) => Execution::Unavailable,
        };
        recorded = DecisionRow::new(id, approve, &body.decided_by, i64::try_from(now).unwrap_or(i64::MAX), via, &level, execution)
            .map_err(|_| "approval_ledger_unavailable".to_string())
            .and_then(|row| match &store {
                Some(store) => store.approval_ledger(LedgerOp::Record(&row)).map(|reply| reply == LedgerReply::Done(true)),
                None => Err("approval_ledger_unavailable".to_string()),
            })
            .unwrap_or(false);
        if !recorded {
            eprintln!("shogun-api: approval ledger record failed for approvals.decide ({outcome})");
        }
    }
    if let Some(store) = &store {
        state.audit(store.as_ref(), (&body.decided_by, principal.as_deref()), ("approvals.decide", &level, outcome), req.request_id);
    }
    // Carry the approved action out, once (E15 follow-up).
    let executed = match (&state.executor, confirmed, &store) {
        (Some(executor), Some(confirmed), Some(store)) => {
            Some(execute_approved(state, executor.as_ref(), store.as_ref(), (id, principal.as_deref(), &level), &confirmed, recorded, req.request_id))
        }
        (Some(_), Some(_), None) => {
            // No store to record into: nothing runs, and the approval says why.
            let failed = crate::approval_exec::ExecOutcome::Failed("ledger_unavailable");
            let _ = state.on_queue(|queue| remote_approvals::settle(queue, id, &failed, state.now_u64()));
            Some(failed)
        }
        _ => None,
    };
    match applied {
        Applied::Approved | Applied::Rejected => {
            let status = match (&executed, body.decision) {
                (_, RemoteDecision::Reject) => "rejected",
                (Some(crate::approval_exec::ExecOutcome::Sent), _) => "sent",
                (Some(crate::approval_exec::ExecOutcome::Failed(_)), _) => "send_failed",
                (None, _) => "approved",
            };
            let mut answer = json!({
                "approval_id": id,
                "decision": body.decision.as_str(),
                "status": status,
                "decided_by": body.decided_by,
                "decided_at": remote_approvals::iso_ms(now),
                "via": via,
                "executed": executed.is_some(),
                "next_action": remote_approvals::status_next_action(status, state.executes()),
            });
            if let Some(reason) = executed.as_ref().and_then(crate::approval_exec::ExecOutcome::reason) {
                answer["reason"] = Value::String(reason.into());
            }
            (200, answer.to_string())
        }
        Applied::Expired => error(410, "approval_expired", "The approval window elapsed before the decision; nothing was approved. Ask the agent to submit it again."),
        Applied::NotPending => error(409, "not_pending", "This approval is no longer pending: it was already decided, timed out, or never existed."),
    }
}

/// Run one approved action (E15 follow-up): claim it in the principal's ledger (`pending` →
/// `executing`, a compare-and-set, so it runs at most once whatever races), call the executor
/// outside every lock, then record the outcome on the queue, in the ledger and in the audit log
/// (`approvals.execute`, `sent` or `failed:<class>`). A decision the ledger did not record, or a
/// claim it refused, runs nothing and reads `send_failed` / `ledger_unavailable`.
fn execute_approved(
    state: &AppState,
    executor: &dyn crate::approval_exec::ApprovalExecutor,
    store: &dyn MemoryBackend,
    (id, principal, level): (u64, Option<&str>, &str),
    confirmed: &shogun_agents::approval::ConfirmedSend,
    recorded: bool,
    request_id: &str,
) -> crate::approval_exec::ExecOutcome {
    use crate::approval_exec::{allowlisted, ExecOutcome, ExecRequest};
    let claimed = recorded && matches!(store.approval_ledger(LedgerOp::Claim(id)), Ok(LedgerReply::Done(true)));
    let outcome = if claimed {
        allowlisted(executor.execute(&ExecRequest { approval_id: id, principal, confirmed }))
    } else {
        ExecOutcome::Failed("ledger_unavailable")
    };
    let now = state.now_u64();
    if claimed {
        let settled = store.approval_ledger(LedgerOp::Settle {
            approval_id: id,
            sent: outcome == ExecOutcome::Sent,
            reason_class: outcome.reason(),
            now_ms: now,
        });
        if !matches!(settled, Ok(LedgerReply::Done(true))) {
            eprintln!("shogun-api: approval ledger could not record an execution outcome");
        }
    }
    if state.on_queue(|queue| remote_approvals::settle(queue, id, &outcome, now)).is_err() {
        eprintln!("shogun-api: approval queue could not record an execution outcome");
    }
    let label = match outcome.reason() {
        None => "sent".to_string(),
        Some(class) => format!("failed:{class}"),
    };
    let actor = state.remote.ledger().get(id).and_then(|e| e.decision).map_or_else(|| "human:admin".to_string(), |d| d.decided_by);
    state.audit(store, (&actor, principal), ("approvals.execute", level, &label), request_id);
    outcome
}

fn acl_change(state: &AppState, req: &Incoming<'_>, grant: bool) -> (u16, String) {
    if !state.admin_ok(req.bearer) {
        return state.not_admin(req.bearer, "Granting or revoking access");
    }
    let body: Value = match req.body.map(serde_json::from_str::<Value>) {
        Some(Ok(body)) if body.is_object() => body,
        _ => return error(400, "invalid_json", "The body must be a JSON object, for example {\"grantee\":\"agent:b\",\"agent\":\"a\",\"access\":\"read\"}."),
    };
    let operation = match crate::governance_api::acl_change(&body, grant) {
        Ok(operation) => operation,
        Err(message) => {
            let code = message.split(':').next().unwrap_or("invalid_request").to_string();
            return error(400, &code, &message);
        }
    };
    let principal = body["principal"].as_str().map(str::to_string);
    // A Team grant names a teammate: refuse one across teams up front (the read path checks it
    // again, so a team change later still closes access).
    if let (Some(resolver), Some(owner), crate::governance_api::AclOperation::GrantPrincipal { grantee_principal }) =
        (&state.principals, principal.as_deref(), &operation)
    {
        if let Some(teams) = resolver.teams() {
            let shared = teams["teams"].as_array().is_some_and(|teams| {
                teams.iter().any(|t| {
                    let members: Vec<&str> = t["members"].as_array().map(|m| m.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                    members.contains(&owner) && members.contains(&grantee_principal.as_str())
                })
            });
            if !shared {
                return error(409, "not_same_team", "A principal grants world read access only to a member of one of its teams. Define the team with POST /v1/teams first.");
            }
        }
    }
    let backend = match state.store_of(principal.as_deref()) {
        Ok(backend) => backend,
        Err(refusal) => return refusal,
    };
    let by = body["granted_by"]
        .as_str()
        .and_then(|raw| shogun_memory::actor::Actor::parse(raw).ok())
        .map_or_else(|| "human:admin".to_string(), |a| a.as_str().to_string());
    let route = if grant { "acl.grant" } else { "acl.revoke" };
    let answer = match backend.acl(&operation, &by) {
        Ok(value) => (200, value.to_string()),
        Err(code) if code.starts_with("invalid_") || code == "own_namespace" => error(400, &code, "The grant names an invalid agent, namespace or principal."),
        Err(code) => error(503, &code, "The principal's store is unavailable; retry."),
    };
    state.audit(backend.as_ref(), (&by, principal.as_deref()), (route, "admin", crate::governance_api::outcome_of(answer.0, &answer.1)), req.request_id);
    answer
}

fn teams(state: &AppState, req: &Incoming<'_>) -> (u16, String) {
    if !state.admin_ok(req.bearer) {
        return state.not_admin(req.bearer, "Defining teams");
    }
    let Some(resolver) = &state.principals else {
        return error(404, "teams_unavailable", "Teams exist only in multi-principal mode.");
    };
    match req.method {
        Method::Get => (200, resolver.teams().unwrap_or_else(|| json!({ "teams": [] })).to_string()),
        Method::Post => {
            let body: Value = match req.body.map(serde_json::from_str::<Value>) {
                Some(Ok(body)) if body.is_object() => body,
                _ => return error(400, "invalid_json", "The body must be {\"tenant\":\"acme\",\"team\":\"ops\",\"members\":[\"alice\",\"bob\"]}."),
            };
            let members: Vec<String> =
                body["members"].as_array().map(|m| m.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
            match resolver.set_team(body["tenant"].as_str().unwrap_or(""), body["team"].as_str().unwrap_or(""), &members) {
                Ok(team) => (200, team.to_string()),
                Err(refusal) => refusal,
            }
        }
    }
}

/// `DELETE /v1/principals/{id}` (E39): the operator purges a principal's store after a SCIM
/// delete's grace period. Admin bearer only (an agent key is `403 human_only`); multi-principal
/// only; idempotent (`200` with `purged: false` when nothing was left). Any other DELETE is 405.
pub(super) fn purge_principal(state: &AppState, bearer: Option<&str>, path: &str, request_id: &str) -> (u16, String) {
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let ["v1", "principals", id] = segments.as_slice() else {
        return (405, r#"{"error":"method_not_allowed"}"#.to_string());
    };
    if !state.admin_ok(bearer) {
        return state.not_admin(bearer, "Purging a principal");
    }
    let Some(resolver) = &state.principals else {
        return error(404, "principals_unavailable", "Principals exist only in multi-principal mode.");
    };
    match resolver.purge_principal(id, request_id, (state.clock)()) {
        Ok(report) => (200, report.to_string()),
        Err(refusal) => refusal,
    }
}

/// `approvals.list` / `approvals.get` on REST (a bearer read). Multi-principal: only the caller's
/// principal's approvals.
pub(super) fn approvals_read(
    state: &AppState,
    backend: &dyn MemoryBackend,
    tool: Tool,
    id: Option<i64>,
    principal: Option<&str>,
) -> (u16, String) {
    let now = state.now_u64();
    let multi = state.multi();
    let ledger = state.remote.ledger();
    let executes = state.executes();
    let answered = state.on_queue(|queue| match tool {
        Tool::ApprovalsGet => {
            let Some(id) = id.and_then(|id| u64::try_from(id).ok()).filter(|id| *id > 0) else {
                return error(404, "not_found", "No such approval.");
            };
            let entry = ledger.get(id);
            if multi && entry.as_ref().is_some_and(|e| e.principal.as_deref() != principal) {
                return error(404, "not_found", "No such approval.");
            }
            // Multi-principal: the shared queue answers only for an approval the ledger says is
            // the caller's; anything else is read from the caller's own store (a file boundary).
            let from_queue = if multi && entry.is_none() { None } else { remote_approvals::summary(queue, ledger, id, now) };
            match from_queue {
                Some(summary) => (200, json!({ "tool": "approvals.get", "approval": summary }).to_string()),
                None => match persisted(backend, id) {
                    Some(row) => (
                        200,
                        json!({ "tool": "approvals.get", "approval": remote_approvals::persisted_summary(&row, executes) }).to_string(),
                    ),
                    None => error(404, "not_found", "No such approval."),
                },
            }
        }
        _ => {
            let mut listed = remote_approvals::list(queue, ledger, principal, multi, now);
            listed["tool"] = Value::String("approvals.list".into());
            (200, listed.to_string())
        }
    });
    answered.unwrap_or_else(|refusal| refusal)
}

/// After a call: label a cross-principal answer with its source, join the approval link to a
/// pending envelope (recording the approval in the ledger), and append the audit record of a
/// write. Returns the status, the body and the decision token for the response header.
pub(super) fn after_call(
    state: &AppState,
    backend: &dyn MemoryBackend,
    facts: &CallFacts<'_>,
    (status, body): (u16, String),
) -> (u16, String, Option<String>) {
    if let Some(source) = facts.cross {
        return (status, label_source(body, source), None);
    }
    let Some(write) = facts.write else {
        return (status, body, None);
    };
    let mut token = None;
    let mut body = body;
    if status == 202 {
        if let Ok(mut value) = serde_json::from_str::<Value>(&body) {
            if value["pending"] == Value::Bool(true) {
                if let Some(id) = value["approval_id"].as_u64() {
                    let level = value["level"].as_str().unwrap_or("L3").to_string();
                    // E4 follow-up: the REST and CLI envelopes carry the same approval URL the MCP
                    // face's `approval_required` result does.
                    if let Some((url, minted)) = state.remote.on_queued(id, facts.principal, facts.actor, &level, state.now_u64()) {
                        value["url"] = Value::String(url);
                        value["next_action"] = Value::String("approve_at_url".into());
                        token = Some(minted);
                    } else {
                        value["next_action"] = Value::String("poll_status".into());
                    }
                    value["poll"] = json!({ "tool": Tool::ActionsStatus.wire_name(), "path": format!("/v1/actions/status/{id}") });
                    body = value.to_string();
                }
            }
        }
    }
    let outcome = crate::governance_api::outcome_of(status, &body);
    state.audit(backend, (facts.actor, facts.principal), (write.route, write.level, outcome), facts.request_id);
    (status, body, token)
}

/// `{"source_principal": "<id>", …}`: a cross-principal answer names whose store it came from.
fn label_source(body: String, source: &str) -> String {
    match serde_json::from_str::<Value>(&body) {
        Ok(Value::Object(mut map)) => {
            map.insert("source_principal".into(), Value::String(source.to_string()));
            Value::Object(map).to_string()
        }
        _ => body,
    }
}

/// `GET /v1/audit/export?since_ms=&until_ms=&format=jsonl|csv[&principal=]` (E26): the operator's
/// hash-chained export of a principal's audit log ([`crate::audit_export`]). Admin credential
/// only: an agent's bearer is `403 human_only`, and no MCP tool exposes it (a human-only
/// operation). The export itself is recorded first (`audit.export`), so it appears in the log it
/// exports. The body is streamed page by page; a stream cut short has no trailer and fails
/// `shogun audit verify`.
pub(super) fn audit_export(state: &AppState, bearer: Option<&str>, raw_query: &str, request_id: &str) -> axum::response::Response {
    if !state.admin_ok(bearer) {
        let (status, body) = state.not_admin(bearer, "Exporting the audit log");
        return super::render_with(status, body, request_id, None);
    }
    let query = match crate::audit_export::export_query(raw_query) {
        Ok(query) => query,
        Err(message) => {
            let code = message.split(':').next().unwrap_or("invalid_request").to_string();
            let (status, body) = error(400, &code, &message);
            return super::render_with(status, body, request_id, None);
        }
    };
    let store = match state.store_of(query.principal.as_deref()) {
        Ok(store) => store,
        Err((status, body)) => return super::render_with(status, body, request_id, None),
    };
    state.audit(store.as_ref(), ("human:admin", query.principal.as_deref()), ("audit.export", "admin", "ok"), request_id);
    let format = query.format;
    let stream = ExportStream {
        backend: store,
        writer: crate::audit_export::ChainWriter::new(format, query.since_ms, query.until_ms),
        after_id: 0,
        since_ms: query.since_ms,
        until_ms: query.until_ms,
        started: false,
        done: false,
    };
    axum::response::Response::builder()
        .status(200)
        .header(axum::http::header::CONTENT_TYPE, format.content_type())
        .header(axum::http::header::CACHE_CONTROL, "no-store")
        .header("x-request-id", request_id)
        .body(axum::body::Body::from_stream(stream))
        .unwrap_or_default()
}

/// The export body: one store page per poll, chained as it goes, then the trailer.
struct ExportStream {
    backend: Arc<dyn MemoryBackend>,
    writer: crate::audit_export::ChainWriter,
    after_id: i64,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    started: bool,
    done: bool,
}

impl futures_core::Stream for ExportStream {
    type Item = Result<axum::body::Bytes, std::io::Error>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        let mut chunk = if this.started { String::new() } else { this.writer.header() };
        this.started = true;
        match this.backend.audit_page(this.after_id, this.since_ms, this.until_ms, crate::audit_export::PAGE) {
            Ok(rows) if rows.is_empty() => {
                chunk.push_str(&this.writer.trailer());
                this.done = true;
            }
            Ok(rows) => {
                for row in &rows {
                    chunk.push_str(&this.writer.line(row));
                }
                this.after_id = rows.last().map_or(this.after_id, |row| row.id);
            }
            Err(_) => {
                // No trailer: the export reads as truncated, never as complete.
                this.done = true;
                return Poll::Ready(Some(Err(std::io::Error::other("audit_unavailable"))));
            }
        }
        Poll::Ready(Some(Ok(axum::body::Bytes::from(chunk))))
    }
}
