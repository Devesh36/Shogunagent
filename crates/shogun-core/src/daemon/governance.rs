//! The audit log and the access grants on the store (ShogunAgent E18, V101): the `Db` side of
//! `audit.list`, `acl.*` and the audit record every face appends after a write or a decision.

use serde_json::Value;
use shogun_mcp::governance_api::{self, AclOperation, AuditQuery};
use shogun_mcp::remote_approvals::{LedgerOp, LedgerReply};
use shogun_memory::acl;
use shogun_memory::actor::{Actor, Namespace};
use shogun_memory::approval_ledger;
use shogun_memory::audit::{self, AuditRecord};

use super::Db;

impl Db {
    /// Append one audit record.
    pub fn audit_append(&self, record: &AuditRecord) -> Result<(), String> {
        self.with_conn("audit.append", |conn| audit::append(conn, record).map(|_| ()).map_err(|e| e.to_string()))
            .map_err(|_| "audit_unavailable".to_string())
    }

    /// The durable approval ledger (V103): spent decision nonces and decisions. Any store fault is
    /// `approval_ledger_unavailable`, and the face refuses the decision rather than make it
    /// without a durable record.
    pub fn approval_ledger(&self, op: LedgerOp<'_>) -> Result<LedgerReply, String> {
        let now = self.now_ms();
        self.with_conn("approval.ledger", |conn| -> Result<LedgerReply, String> {
            let ms = |value: u64| i64::try_from(value).unwrap_or(i64::MAX);
            let fault = |e: approval_ledger::LedgerError| e.to_string();
            Ok(match op {
                LedgerOp::SpendNonce { nonce, approval_id, expires_ms, now_ms } => LedgerReply::Done(
                    approval_ledger::spend_nonce(conn, nonce, approval_id, ms(expires_ms), ms(now_ms)).map_err(fault)?,
                ),
                LedgerOp::Record(row) => LedgerReply::Done(approval_ledger::record_decision(conn, row).map_err(fault)?),
                LedgerOp::Get(id) => LedgerReply::Row(approval_ledger::get(conn, id).map_err(fault)?),
                LedgerOp::Claim(id) => LedgerReply::Done(approval_ledger::claim_execution(conn, id).map_err(fault)?),
                LedgerOp::Settle { approval_id, sent, reason_class, now_ms } => {
                    let outcome = if sent { approval_ledger::Execution::Sent } else { approval_ledger::Execution::Failed };
                    let at = if now_ms == 0 { now } else { ms(now_ms) };
                    LedgerReply::Done(
                        approval_ledger::settle_execution(conn, approval_id, outcome, reason_class, at).map_err(fault)?,
                    )
                }
            })
        })
        .map_err(|_| "approval_ledger_unavailable".to_string())
    }

    /// `audit.list`.
    pub fn audit_list(&self, query: &AuditQuery) -> Result<Value, String> {
        self.with_conn("audit.list", |conn| audit::list(conn, query.since_ms, query.limit).map_err(|e| e.to_string()))
            .map(|entries| governance_api::audit_json(&entries))
            .map_err(|_| "audit_unavailable".to_string())
    }

    /// `acl.list` / `acl.grant` / `acl.revoke`. `by` is who changed the grants (an actor id).
    pub fn acl(&self, operation: &AclOperation, by: &str) -> Result<Value, String> {
        let by = Actor::parse(by).map_err(|_| "invalid_actor".to_string())?;
        // Refuse a malformed change before the store is touched, so a caller's mistake is never
        // recorded as a store fault.
        let refusal = self.acl_refusal(operation);
        if refusal != "acl_unavailable" {
            return Err(refusal);
        }
        let now = self.now_ms();
        let changed = self.with_conn("acl", |conn| -> Result<Option<bool>, String> {
            let agent = |id: &str| Actor::agent(id).map_err(|_| "invalid_grantee".to_string());
            let scratch = |id: &str| Namespace::scratch(id).map_err(|_| "invalid_agent".to_string());
            let fault = |e: acl::AclError| match e {
                acl::AclError::Db => "acl_unavailable".to_string(),
                acl::AclError::GranteeNotAgent => "invalid_grantee".to_string(),
                acl::AclError::OwnNamespace => "own_namespace".to_string(),
                acl::AclError::InvalidPrincipal => "invalid_grantee_principal".to_string(),
            };
            Ok(match operation {
                AclOperation::List => None,
                AclOperation::GrantScratch { grantee, owner_agent, access } => {
                    acl::grant_scratch(conn, &agent(grantee)?, &scratch(owner_agent)?, *access, &by, now).map_err(fault)?;
                    Some(true)
                }
                AclOperation::RevokeScratch { grantee, owner_agent } => {
                    Some(acl::revoke_scratch(conn, &agent(grantee)?, &scratch(owner_agent)?).map_err(fault)?)
                }
                AclOperation::GrantPrincipal { grantee_principal } => {
                    acl::grant_principal(conn, grantee_principal, &by, now).map_err(fault)?;
                    Some(true)
                }
                AclOperation::RevokePrincipal { grantee_principal } => {
                    Some(acl::revoke_principal(conn, grantee_principal).map_err(fault)?)
                }
            })
        });
        let changed = changed.map_err(|_| "acl_unavailable".to_string())?;
        let lists = self
            .with_conn("acl.list", |conn| -> Result<_, String> {
                Ok((
                    acl::list_scratch(conn).map_err(|e| e.to_string())?,
                    acl::list_principal(conn).map_err(|e| e.to_string())?,
                ))
            })
            .map_err(|_| "acl_unavailable".to_string())?;
        let mut out = governance_api::acl_list_json(&lists.0, &lists.1);
        if let Some(changed) = changed {
            out["changed"] = Value::Bool(changed);
            out["tool"] = Value::String(
                match operation {
                    AclOperation::GrantScratch { .. } | AclOperation::GrantPrincipal { .. } => "acl.grant",
                    _ => "acl.revoke",
                }
                .to_string(),
            );
        }
        Ok(out)
    }

    fn acl_refusal(&self, operation: &AclOperation) -> String {
        match operation {
            AclOperation::GrantScratch { grantee, owner_agent, .. } | AclOperation::RevokeScratch { grantee, owner_agent } => {
                if Actor::agent(grantee).is_err() {
                    "invalid_grantee".to_string()
                } else if Namespace::scratch(owner_agent).is_err() {
                    "invalid_agent".to_string()
                } else if grantee == owner_agent {
                    "own_namespace".to_string()
                } else {
                    "acl_unavailable".to_string()
                }
            }
            AclOperation::GrantPrincipal { grantee_principal } | AclOperation::RevokePrincipal { grantee_principal } => {
                if crate::tenancy::PrincipalId::parse(grantee_principal).is_err() {
                    "invalid_grantee_principal".to_string()
                } else {
                    "acl_unavailable".to_string()
                }
            }
            AclOperation::List => "acl_unavailable".to_string(),
        }
    }

    /// Team: whether `grantee_principal` may read this store's world state.
    pub fn principal_may_read(&self, grantee_principal: &str) -> bool {
        self.with_conn("acl.principal", |conn| acl::principal_may_read(conn, grantee_principal).map_err(|e| e.to_string()))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use shogun_memory::acl::ScratchAccess;

    #[test]
    fn grants_round_trip_and_refusals_are_named() {
        let db = Db::open_in_memory(std::sync::Arc::new(|| 1_000)).expect("db");
        let out = db
            .acl(&AclOperation::GrantScratch { grantee: "b".into(), owner_agent: "a".into(), access: ScratchAccess::Read }, "human:owner")
            .unwrap();
        assert_eq!(out["tool"], "acl.grant");
        assert_eq!(out["scratch"][0]["namespace"], "agent_scratch:a");
        assert_eq!(out["scratch"][0]["grantee"], "agent:b");
        let listed = db.acl(&AclOperation::List, "human:owner").unwrap();
        assert_eq!(listed["scratch"].as_array().unwrap().len(), 1);
        let revoked = db.acl(&AclOperation::RevokeScratch { grantee: "b".into(), owner_agent: "a".into() }, "human:owner").unwrap();
        assert_eq!(revoked["changed"], true);
        assert_eq!(
            db.acl(&AclOperation::GrantPrincipal { grantee_principal: "Bob".into() }, "human:owner").unwrap_err(),
            "invalid_grantee_principal"
        );
        db.acl(&AclOperation::GrantPrincipal { grantee_principal: "bob".into() }, "human:owner").unwrap();
        assert!(db.principal_may_read("bob"));
        assert!(!db.principal_may_read("carol"));
        db.audit_append(&AuditRecord::new(5, "agent:a", None, "memory.ingest", "L1", "ok", "r1").unwrap()).unwrap();
        let audit = db.audit_list(&AuditQuery::default()).unwrap();
        assert_eq!(audit["records"][0]["route"], "memory.ingest");
    }
}
