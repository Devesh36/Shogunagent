//! Governance on every face (ShogunAgent E15 / E18): the audit log, the access grants and the
//! approval summaries. One parser and one JSON shape per operation, shared by MCP, REST and the
//! CLI (invariant 4).
//!
//! | Operation      | MCP            | REST                                   | CLI                         |
//! |----------------|----------------|----------------------------------------|-----------------------------|
//! | approvals.list | `approvals.list` | `GET /v1/approvals`                  | `shogun approvals list`     |
//! | approvals.get  | `approvals.get`  | `GET /v1/approvals/{id}`             | `shogun approvals get <id>` |
//! | decide         | — (never)      | `POST /v1/approvals/{id}/decide` (signed token or admin) | `shogun approvals decide <id> approve\|reject` (admin) |
//! | audit.list     | `audit.list`   | `GET /v1/audit?since_ms=&limit=`       | `shogun audit [--since-ms N] [--limit N]` |
//! | acl.list       | `acl.list`     | `GET /v1/acl`                          | `shogun acl list`           |
//! | acl.grant      | — (never)      | `POST /v1/acl/grant` (admin)           | `shogun acl grant <json>` (admin) |
//! | acl.revoke     | — (never)      | `POST /v1/acl/revoke` (admin)          | `shogun acl revoke <json>` (admin) |
//!
//! Deciding and granting are a human's or an operator's: they are not tools an agent can call,
//! and the REST routes refuse an agent's bearer.

use serde_json::{json, Value};
use shogun_memory::acl::{PrincipalGrant, ScratchAccess, ScratchGrant};
use shogun_memory::audit::{AuditEntry, AuditRecord, MAX_LIST};

/// `audit.list` arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditQuery {
    pub since_ms: Option<i64>,
    pub limit: usize,
}

impl Default for AuditQuery {
    fn default() -> Self {
        Self { since_ms: None, limit: 100 }
    }
}

/// Parse `{"since_ms"?: int, "limit"?: 1..=500}`.
pub fn audit_query(args: &Value) -> Result<AuditQuery, String> {
    let since_ms = match args.get("since_ms") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_i64().ok_or_else(|| "invalid_since_ms: since_ms must be milliseconds since the epoch.".to_string())?),
    };
    let limit = match args.get("limit") {
        None | Some(Value::Null) => 100,
        Some(v) => match v.as_u64() {
            Some(n) if (1..=MAX_LIST as u64).contains(&n) => usize::try_from(n).unwrap_or(100),
            _ => return Err(format!("invalid_limit: limit must be between 1 and {MAX_LIST}.")),
        },
    };
    Ok(AuditQuery { since_ms, limit })
}

/// The `audit.list` result.
pub fn audit_json(entries: &[AuditEntry]) -> Value {
    let records: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "id": e.id,
                "ts": e.record.ts,
                "at": crate::remote_approvals::iso_ms(u64::try_from(e.record.ts).unwrap_or(0)),
                "actor": e.record.actor,
                "principal": e.record.principal,
                "route": e.record.route,
                "level": e.record.level,
                "outcome": e.record.outcome,
                "request_id": e.record.request_id,
            })
        })
        .collect();
    let next = entries.last().map(|e| e.record.ts);
    json!({ "tool": "audit.list", "records": records, "next_since_ms": next })
}

/// A change to the grants (`acl.grant` / `acl.revoke`), or the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AclOperation {
    List,
    /// Let `grantee` (an agent actor) read or write `owner_agent`'s scratch.
    GrantScratch { grantee: String, owner_agent: String, access: ScratchAccess },
    RevokeScratch { grantee: String, owner_agent: String },
    /// Let `grantee_principal` (same team) read this principal's world state.
    GrantPrincipal { grantee_principal: String },
    RevokePrincipal { grantee_principal: String },
}

fn agent_id(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let id = raw.strip_prefix("agent:").unwrap_or(raw);
    shogun_memory::actor::Actor::agent(id).ok().map(|a| a.id().to_string())
}

/// Parse an `acl.grant` (`grant = true`) or `acl.revoke` body:
/// `{"kind":"scratch","grantee":"agent:b","agent":"a","access":"read"|"write"}` (the scratch of
/// agent `a`; `"namespace":"agent_scratch:a"` is accepted for `agent`), or
/// `{"kind":"principal","grantee_principal":"bob"}`. A `"principal"` field (multi-principal mode)
/// is read by the server, not here.
pub fn acl_change(body: &Value, grant: bool) -> Result<AclOperation, String> {
    let kind = body["kind"].as_str().unwrap_or("scratch");
    match kind {
        "scratch" => {
            let grantee = body["grantee"]
                .as_str()
                .and_then(agent_id)
                .ok_or_else(|| "invalid_grantee: grantee must be an agent id (agent:<id>).".to_string())?;
            let owner = body["agent"]
                .as_str()
                .or_else(|| body["namespace"].as_str().and_then(|ns| ns.strip_prefix(shogun_memory::actor::SCRATCH_PREFIX)))
                .and_then(agent_id)
                .ok_or_else(|| "invalid_agent: name whose scratch with agent (or namespace agent_scratch:<id>).".to_string())?;
            if owner == grantee {
                return Err("own_namespace: an agent's own scratch needs no grant.".to_string());
            }
            if !grant {
                return Ok(AclOperation::RevokeScratch { grantee, owner_agent: owner });
            }
            let access = match body.get("access") {
                None | Some(Value::Null) => ScratchAccess::Read,
                Some(v) => v
                    .as_str()
                    .and_then(ScratchAccess::parse)
                    .ok_or_else(|| "invalid_access: access must be \"read\" or \"write\".".to_string())?,
            };
            Ok(AclOperation::GrantScratch { grantee, owner_agent: owner, access })
        }
        "principal" => {
            let grantee_principal = body["grantee_principal"]
                .as_str()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .ok_or_else(|| "invalid_grantee_principal: name the team member's principal id.".to_string())?
                .to_string();
            Ok(if grant {
                AclOperation::GrantPrincipal { grantee_principal }
            } else {
                AclOperation::RevokePrincipal { grantee_principal }
            })
        }
        _ => Err("invalid_kind: kind must be \"scratch\" or \"principal\".".to_string()),
    }
}

/// The `acl.list` result.
pub fn acl_list_json(scratch: &[ScratchGrant], principals: &[PrincipalGrant]) -> Value {
    json!({
        "tool": "acl.list",
        "scratch": scratch.iter().map(|g| json!({
            "grantee": g.grantee,
            "namespace": g.namespace,
            "access": g.access.as_str(),
            "granted_by": g.granted_by,
            "granted_at": g.granted_at,
        })).collect::<Vec<_>>(),
        "principals": principals.iter().map(|g| json!({
            "grantee_principal": g.grantee_principal,
            "scope": g.scope,
            "granted_by": g.granted_by,
            "granted_at": g.granted_at,
        })).collect::<Vec<_>>(),
    })
}

/// A fresh request id: `req_<16 hex>`.
pub fn new_request_id() -> String {
    let mut raw = [0u8; 8];
    if getrandom::fill(&mut raw).is_err() {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        raw = (now as u64).to_be_bytes();
    }
    format!("req_{}", raw.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// The caller's `X-Request-Id` when it is a plain short id, else a fresh one.
pub fn request_id(header: Option<&str>) -> String {
    header
        .map(str::trim)
        .filter(|id| !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b)))
        .map(str::to_string)
        .unwrap_or_else(new_request_id)
}

/// The outcome label of a face's `(status, body)`: `pending` for a queued approval, `ok`,
/// `refused` (4xx) or `error` (5xx).
pub fn outcome_of(status: u16, body: &str) -> &'static str {
    match status {
        200..=299 if body.contains("\"pending\":true") => "pending",
        200..=299 => "ok",
        400..=499 => "refused",
        _ => "error",
    }
}

/// Build an audit record, or `None` when a field is unusable (it is then not recorded rather
/// than recorded with content).
pub fn record(
    now_ms: i64,
    actor: &str,
    principal: Option<&str>,
    route: &str,
    level: &str,
    outcome: &str,
    request_id: &str,
) -> Option<AuditRecord> {
    AuditRecord::new(now_ms, actor, principal, route, level, outcome, request_id).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_query_bounds() {
        assert_eq!(audit_query(&json!({})).unwrap(), AuditQuery::default());
        assert_eq!(audit_query(&json!({"since_ms": 5, "limit": 2})).unwrap(), AuditQuery { since_ms: Some(5), limit: 2 });
        assert!(audit_query(&json!({"limit": 0})).is_err());
        assert!(audit_query(&json!({"limit": 501})).is_err());
        assert!(audit_query(&json!({"since_ms": "x"})).is_err());
    }

    #[test]
    fn acl_bodies() {
        assert_eq!(
            acl_change(&json!({"grantee":"agent:b","agent":"a","access":"write"}), true).unwrap(),
            AclOperation::GrantScratch { grantee: "b".into(), owner_agent: "a".into(), access: ScratchAccess::Write }
        );
        assert_eq!(
            acl_change(&json!({"grantee":"b","namespace":"agent_scratch:a"}), false).unwrap(),
            AclOperation::RevokeScratch { grantee: "b".into(), owner_agent: "a".into() }
        );
        assert_eq!(
            acl_change(&json!({"kind":"principal","grantee_principal":"bob"}), true).unwrap(),
            AclOperation::GrantPrincipal { grantee_principal: "bob".into() }
        );
        assert!(acl_change(&json!({"grantee":"a","agent":"a"}), true).is_err());
        assert!(acl_change(&json!({"grantee":"b","agent":"a","access":"admin"}), true).is_err());
        assert!(acl_change(&json!({"kind":"world"}), true).is_err());
    }

    #[test]
    fn request_ids_and_outcomes() {
        assert_eq!(request_id(Some("abc-1")), "abc-1");
        assert!(request_id(Some("has space")).starts_with("req_"));
        assert!(request_id(None).starts_with("req_"));
        assert_eq!(outcome_of(202, r#"{"pending":true}"#), "pending");
        assert_eq!(outcome_of(200, "{}"), "ok");
        assert_eq!(outcome_of(403, "{}"), "refused");
        assert_eq!(outcome_of(503, "{}"), "error");
    }
}
