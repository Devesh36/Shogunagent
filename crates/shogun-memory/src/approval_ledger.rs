//! The principal's approval ledger (V103, ShogunAgent E15 follow-up): remote decisions and the
//! decision-token nonces they spent, in the principal's own store so a restart forgets neither.
//!
//! - **Spent nonces.** [`spend_nonce`] inserts a token's nonce once; a second insert of the same
//!   nonce (a replay, in this process or after a restart) answers `false`. Rows are pruned once
//!   the token they belong to has expired: an expired token is refused before its nonce is read.
//! - **Decisions.** [`record_decision`] writes one row per approval id; a second decision on the
//!   same id is ignored (the approval queue already refused it). An approval the sidecar executes
//!   moves `pending` → `executing` exactly once ([`claim_execution`]: a compare-and-set, so two
//!   racing executors cannot both run it) and then `sent` / `failed` ([`settle_execution`]).
//!
//! Ids, labels and times only: no recipient, no body, no provider message. [`DecisionRow::new`]
//! refuses a field that is not a short label, like the audit log does.

use rusqlite::{params, Connection, OptionalExtension};

/// The longest `decided_by` / `level` / `reason_class` stored.
const MAX_LABEL_CHARS: usize = 128;
/// The longest nonce stored (the signer mints 32 hex characters).
const MAX_NONCE_CHARS: usize = 64;

/// What became of carrying out an approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Execution {
    /// A rejection: nothing to carry out.
    None,
    /// Approved; the executor has not claimed it yet.
    Pending,
    /// Claimed by an executor; the outcome is not recorded yet. A row left here by a crash is
    /// never re-run: its delivery is unknown.
    Executing,
    Sent,
    Failed,
    /// Approved, but this build carries no executor (`execute_unavailable`).
    Unavailable,
}

impl Execution {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Pending => "pending",
            Self::Executing => "executing",
            Self::Sent => "sent",
            Self::Failed => "failed",
            Self::Unavailable => "unavailable",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "none" => Self::None,
            "pending" => Self::Pending,
            "executing" => Self::Executing,
            "sent" => Self::Sent,
            "failed" => Self::Failed,
            "unavailable" => Self::Unavailable,
            _ => return None,
        })
    }
}

/// One decision row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRow {
    pub approval_id: u64,
    /// `true` approve, `false` reject.
    pub approve: bool,
    pub decided_by: String,
    pub decided_at: i64,
    /// `token` or `admin`.
    pub via: String,
    pub level: String,
    pub execution: Execution,
    pub reason_class: Option<String>,
    pub executed_at: Option<i64>,
}

/// Why a ledger operation failed. Never carries a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LedgerError {
    #[error("approval ledger field `{0}` is empty, too long or not a plain label")]
    InvalidField(&'static str),
    #[error("the approval ledger could not be read or written")]
    Db,
}

impl From<rusqlite::Error> for LedgerError {
    fn from(_: rusqlite::Error) -> Self {
        LedgerError::Db
    }
}

fn label(field: &'static str, raw: &str, max: usize) -> Result<String, LedgerError> {
    let value = raw.trim();
    if value.is_empty() || value.chars().count() > max || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(LedgerError::InvalidField(field));
    }
    Ok(value.to_string())
}

fn id_i64(id: u64) -> Result<i64, LedgerError> {
    i64::try_from(id).ok().filter(|id| *id > 0).ok_or(LedgerError::InvalidField("approval_id"))
}

impl DecisionRow {
    /// A validated decision row. `decided_by` follows the V92 actor rule; `via` is `token` or
    /// `admin`.
    pub fn new(
        approval_id: u64,
        approve: bool,
        decided_by: &str,
        decided_at: i64,
        via: &str,
        level: &str,
        execution: Execution,
    ) -> Result<Self, LedgerError> {
        id_i64(approval_id)?;
        let decided_by = crate::actor::Actor::parse(decided_by).map_err(|_| LedgerError::InvalidField("decided_by"))?;
        if !matches!(via, "token" | "admin") {
            return Err(LedgerError::InvalidField("via"));
        }
        Ok(Self {
            approval_id,
            approve,
            decided_by: decided_by.as_str().to_string(),
            decided_at,
            via: via.to_string(),
            level: label("level", level, MAX_LABEL_CHARS)?,
            execution,
            reason_class: None,
            executed_at: None,
        })
    }
}

/// Spend `nonce` (its token valid until `expires_at`) for `approval_id`. `Ok(false)` when it was
/// already spent: a replay. Prunes the nonces of expired tokens first.
pub fn spend_nonce(conn: &Connection, nonce: &str, approval_id: u64, expires_at: i64, now_ms: i64) -> Result<bool, LedgerError> {
    let nonce = label("nonce", nonce, MAX_NONCE_CHARS)?;
    let id = id_i64(approval_id)?;
    conn.execute("DELETE FROM approval_spent_nonces WHERE expires_at <= ?1", [now_ms])?;
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO approval_spent_nonces (nonce, approval_id, expires_at, spent_at) VALUES (?1, ?2, ?3, ?4)",
        params![nonce, id, expires_at, now_ms],
    )?;
    Ok(inserted == 1)
}

/// Record `row`. `Ok(false)` when the approval already has a decision (the first one stands).
pub fn record_decision(conn: &Connection, row: &DecisionRow) -> Result<bool, LedgerError> {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO approval_decisions
           (approval_id, decision, decided_by, decided_at, via, level, execution, reason_class, executed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            id_i64(row.approval_id)?,
            if row.approve { "approve" } else { "reject" },
            row.decided_by,
            row.decided_at,
            row.via,
            row.level,
            row.execution.as_str(),
            row.reason_class,
            row.executed_at,
        ],
    )?;
    Ok(inserted == 1)
}

/// The decision on `approval_id`, if one was recorded.
pub fn get(conn: &Connection, approval_id: u64) -> Result<Option<DecisionRow>, LedgerError> {
    let id = id_i64(approval_id)?;
    let row = conn
        .query_row(
            "SELECT decision, decided_by, decided_at, via, level, execution, reason_class, executed_at
               FROM approval_decisions WHERE approval_id = ?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                ))
            },
        )
        .optional()?;
    let Some((decision, decided_by, decided_at, via, level, execution, reason_class, executed_at)) = row else {
        return Ok(None);
    };
    Ok(Some(DecisionRow {
        approval_id,
        approve: decision == "approve",
        decided_by,
        decided_at,
        via,
        level,
        execution: Execution::parse(&execution).ok_or(LedgerError::Db)?,
        reason_class,
        executed_at,
    }))
}

/// Claim `approval_id` for execution: `pending` → `executing`, once. `Ok(false)` when it is not
/// pending (never approved here, already claimed, or settled): the caller runs nothing.
pub fn claim_execution(conn: &Connection, approval_id: u64) -> Result<bool, LedgerError> {
    let changed = conn.execute(
        "UPDATE approval_decisions SET execution = 'executing'
          WHERE approval_id = ?1 AND execution = 'pending'",
        [id_i64(approval_id)?],
    )?;
    Ok(changed == 1)
}

/// Settle a claimed execution: `executing` → `sent` / `failed` (with its class). `Ok(false)` when
/// the row was not executing.
pub fn settle_execution(
    conn: &Connection,
    approval_id: u64,
    outcome: Execution,
    reason_class: Option<&str>,
    now_ms: i64,
) -> Result<bool, LedgerError> {
    if !matches!(outcome, Execution::Sent | Execution::Failed) {
        return Err(LedgerError::InvalidField("execution"));
    }
    let reason = match reason_class {
        Some(reason) => Some(label("reason_class", reason, MAX_LABEL_CHARS)?),
        None => None,
    };
    let changed = conn.execute(
        "UPDATE approval_decisions SET execution = ?2, reason_class = ?3, executed_at = ?4
          WHERE approval_id = ?1 AND execution = 'executing'",
        params![id_i64(approval_id)?, outcome.as_str(), reason, now_ms],
    )?;
    Ok(changed == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        crate::open_in_memory().expect("store")
    }

    #[test]
    fn a_nonce_is_spent_once_until_its_token_expires() {
        let conn = conn();
        assert!(spend_nonce(&conn, "n1", 7, 100, 10).unwrap());
        assert!(!spend_nonce(&conn, "n1", 7, 100, 20).unwrap(), "a replay is refused");
        assert!(spend_nonce(&conn, "n2", 7, 100, 20).unwrap());
        // After the token expired its nonce is pruned (the token itself is refused before then).
        assert!(spend_nonce(&conn, "n1", 7, 300, 150).unwrap());
        assert!(spend_nonce(&conn, "bad nonce", 7, 300, 150).is_err());
    }

    #[test]
    fn a_decision_is_recorded_once_and_executes_once() {
        let conn = conn();
        let row = DecisionRow::new(9, true, "web:approval-page", 5, "token", "L3", Execution::Pending).unwrap();
        assert!(record_decision(&conn, &row).unwrap());
        let mut again = row.clone();
        again.approve = false;
        assert!(!record_decision(&conn, &again).unwrap(), "the first decision stands");
        assert_eq!(get(&conn, 9).unwrap().unwrap(), row);
        assert!(claim_execution(&conn, 9).unwrap());
        assert!(!claim_execution(&conn, 9).unwrap(), "claimed once");
        assert!(settle_execution(&conn, 9, Execution::Failed, Some("not_connected"), 6).unwrap());
        assert!(!settle_execution(&conn, 9, Execution::Sent, None, 7).unwrap(), "settled once");
        let settled = get(&conn, 9).unwrap().unwrap();
        assert_eq!((settled.execution, settled.reason_class.as_deref(), settled.executed_at), (Execution::Failed, Some("not_connected"), Some(6)));
        assert!(get(&conn, 10).unwrap().is_none());
        // A rejection is never claimable.
        let rejected = DecisionRow::new(11, false, "human:operator", 5, "admin", "L3", Execution::None).unwrap();
        record_decision(&conn, &rejected).unwrap();
        assert!(!claim_execution(&conn, 11).unwrap());
    }

    #[test]
    fn rows_refuse_values_that_could_carry_content() {
        assert!(DecisionRow::new(0, true, "web:x", 1, "token", "L3", Execution::Pending).is_err());
        assert!(DecisionRow::new(1, true, "nobody", 1, "token", "L3", Execution::Pending).is_err());
        assert!(DecisionRow::new(1, true, "web:x", 1, "email", "L3", Execution::Pending).is_err());
        assert!(DecisionRow::new(1, true, "web:x", 1, "token", "send it to aiko", Execution::Pending).is_err());
        let conn = conn();
        record_decision(&conn, &DecisionRow::new(1, true, "web:x", 1, "token", "L3", Execution::Pending).unwrap()).unwrap();
        claim_execution(&conn, 1).unwrap();
        assert!(settle_execution(&conn, 1, Execution::Failed, Some("the provider said no"), 2).is_err());
        assert!(settle_execution(&conn, 1, Execution::Pending, None, 2).is_err());
    }
}
