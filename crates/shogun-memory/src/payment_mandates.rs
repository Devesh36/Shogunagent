//! Payment mandates and their spends in the principal's store (V105, ShogunAgent E46).
//!
//! This module only stores and reads rows; the bounds (what is a widening, whether a payment is
//! inside a mandate) are decided by `shogun_agents::payment`, and the faces orchestrate.
//!
//! - **A mandate needs a human decision.** [`grant`] writes or amends a mandate only when
//!   `approval_decisions` (V103) holds an `approve` decision for its grant approval id, and takes
//!   `granted_by` from that row: no code path can store a mandate a human did not approve.
//! - **Every spend is recorded.** [`reserve_spend`] inserts the spend row under
//!   `(actor, idempotency_key)` *before* any payment is attempted, re-reading the mandate's spent
//!   total in the same transaction, so two concurrent spends cannot both pass the cap. A replayed
//!   key answers the existing row instead.
//! - Labels, ids, amounts and times only: no memo text and no rail response.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

/// Statuses that count against a mandate's cap.
const COUNTED: &str = "('authorized', 'executing', 'settled')";
/// The longest label (actor, key, rail ref, class) stored.
const MAX_LABEL_CHARS: usize = 128;

/// Why a mandate operation failed. Never carries a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MandateError {
    #[error("payment mandate field `{0}` is empty, too long or not a plain label")]
    InvalidField(&'static str),
    #[error("no human approve decision is recorded for this grant")]
    NotApproved,
    #[error("the mandate does not exist")]
    NotFound,
    #[error("the mandate is revoked")]
    Revoked,
    #[error("the mandate belongs to another agent")]
    WrongAgent,
    #[error("the payment ledger could not be read or written")]
    Db,
}

impl From<rusqlite::Error> for MandateError {
    fn from(_: rusqlite::Error) -> Self {
        MandateError::Db
    }
}

fn label(field: &'static str, raw: &str) -> Result<(), MandateError> {
    if raw.is_empty() || raw.chars().count() > MAX_LABEL_CHARS || raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(MandateError::InvalidField(field));
    }
    Ok(())
}

fn to_i64(field: &'static str, v: u64) -> Result<i64, MandateError> {
    i64::try_from(v).map_err(|_| MandateError::InvalidField(field))
}

fn to_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// The terms columns of a mandate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermsRow {
    /// `payee` or `class`.
    pub payee_kind: String,
    pub payee: String,
    pub currency: String,
    pub max_per_payment_minor: u64,
    pub total_cap_minor: u64,
    pub expires_at: i64,
    pub purpose: String,
    pub single_use: bool,
}

/// One mandate with its spent totals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MandateRow {
    pub mandate_id: String,
    pub agent: String,
    pub terms: TermsRow,
    /// `active` or `revoked`.
    pub status: String,
    pub grant_approval_id: u64,
    pub granted_by: String,
    pub granted_at: i64,
    pub revision: i64,
    pub revoked_by: Option<String>,
    pub revoked_at: Option<i64>,
    pub updated_at: i64,
    pub spent_minor: u64,
    pub payments: u64,
}

const MANDATE_COLUMNS: &str = "m.mandate_id, m.agent, m.payee_kind, m.payee, m.currency, m.max_per_payment_minor,
    m.total_cap_minor, m.expires_at, m.purpose, m.single_use, m.status, m.grant_approval_id, m.granted_by,
    m.granted_at, m.revision, m.revoked_by, m.revoked_at, m.updated_at,
    (SELECT COALESCE(SUM(s.amount_minor), 0) FROM payment_spends s
       WHERE s.mandate_id = m.mandate_id AND s.authorization = 'mandate' AND s.status IN ('authorized', 'executing', 'settled')),
    (SELECT COUNT(*) FROM payment_spends s
       WHERE s.mandate_id = m.mandate_id AND s.authorization = 'mandate' AND s.status IN ('authorized', 'executing', 'settled'))";

fn mandate_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<MandateRow> {
    Ok(MandateRow {
        mandate_id: r.get(0)?,
        agent: r.get(1)?,
        terms: TermsRow {
            payee_kind: r.get(2)?,
            payee: r.get(3)?,
            currency: r.get(4)?,
            max_per_payment_minor: to_u64(r.get(5)?),
            total_cap_minor: to_u64(r.get(6)?),
            expires_at: r.get(7)?,
            purpose: r.get(8)?,
            single_use: r.get::<_, i64>(9)? == 1,
        },
        status: r.get(10)?,
        grant_approval_id: to_u64(r.get(11)?),
        granted_by: r.get(12)?,
        granted_at: r.get(13)?,
        revision: r.get(14)?,
        revoked_by: r.get(15)?,
        revoked_at: r.get(16)?,
        updated_at: r.get(17)?,
        spent_minor: to_u64(r.get(18)?),
        payments: to_u64(r.get(19)?),
    })
}

/// One mandate.
pub fn get(conn: &Connection, mandate_id: &str) -> Result<Option<MandateRow>, MandateError> {
    Ok(conn
        .query_row(&format!("SELECT {MANDATE_COLUMNS} FROM payment_mandates m WHERE m.mandate_id = ?1"), [mandate_id], mandate_from)
        .optional()?)
}

/// Every mandate, newest first, at most `limit`.
pub fn list(conn: &Connection, limit: usize) -> Result<Vec<MandateRow>, MandateError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {MANDATE_COLUMNS} FROM payment_mandates m ORDER BY m.granted_at DESC, m.mandate_id LIMIT ?1"
    ))?;
    let rows = stmt.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], mandate_from)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// What [`grant`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granted {
    Created,
    Amended,
    /// This decision was already applied (a replayed execution).
    AlreadyApplied,
}

fn validate_terms(terms: &TermsRow) -> Result<(), MandateError> {
    if !matches!(terms.payee_kind.as_str(), "payee" | "class") {
        return Err(MandateError::InvalidField("payee_kind"));
    }
    label("payee", &terms.payee)?;
    if terms.currency.len() != 3 {
        return Err(MandateError::InvalidField("currency"));
    }
    if terms.max_per_payment_minor == 0 || terms.total_cap_minor < terms.max_per_payment_minor {
        return Err(MandateError::InvalidField("caps"));
    }
    Ok(())
}

/// Write (or, with `amends`, replace the terms of) mandate `mandate_id` for `agent` under the
/// human decision on `approval_id`. Refused with [`MandateError::NotApproved`] unless
/// `approval_decisions` holds an `approve` row for that id.
pub fn grant(
    conn: &mut Connection,
    mandate_id: &str,
    agent: &str,
    terms: &TermsRow,
    amends: bool,
    approval_id: u64,
    now_ms: i64,
) -> Result<Granted, MandateError> {
    label("mandate_id", mandate_id)?;
    label("agent", agent)?;
    validate_terms(terms)?;
    let approval = to_i64("approval_id", approval_id)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let decided_by: Option<String> = tx
        .query_row(
            "SELECT decided_by FROM approval_decisions WHERE approval_id = ?1 AND decision = 'approve'",
            [approval],
            |r| r.get(0),
        )
        .optional()?;
    let Some(decided_by) = decided_by else {
        return Err(MandateError::NotApproved);
    };
    let existing: Option<(String, String, i64)> = tx
        .query_row(
            "SELECT agent, status, grant_approval_id FROM payment_mandates WHERE mandate_id = ?1",
            [mandate_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let outcome = match existing {
        Some((_, _, applied)) if applied == approval => Granted::AlreadyApplied,
        Some(_) if !amends => Granted::AlreadyApplied,
        Some((owner, _, _)) if owner != agent => return Err(MandateError::WrongAgent),
        Some((_, status, _)) if status != "active" => return Err(MandateError::Revoked),
        Some(_) => {
            tx.execute(
                "UPDATE payment_mandates SET payee_kind = ?2, payee = ?3, currency = ?4, max_per_payment_minor = ?5,
                    total_cap_minor = ?6, expires_at = ?7, purpose = ?8, single_use = ?9, grant_approval_id = ?10,
                    granted_by = ?11, granted_at = ?12, revision = revision + 1, updated_at = ?12
                 WHERE mandate_id = ?1",
                params![
                    mandate_id,
                    terms.payee_kind,
                    terms.payee,
                    terms.currency,
                    to_i64("max_per_payment_minor", terms.max_per_payment_minor)?,
                    to_i64("total_cap_minor", terms.total_cap_minor)?,
                    terms.expires_at,
                    terms.purpose,
                    i64::from(terms.single_use),
                    approval,
                    decided_by,
                    now_ms
                ],
            )?;
            Granted::Amended
        }
        None if amends => return Err(MandateError::NotFound),
        None => {
            tx.execute(
                "INSERT INTO payment_mandates (mandate_id, agent, payee_kind, payee, currency, max_per_payment_minor,
                    total_cap_minor, expires_at, purpose, single_use, grant_approval_id, granted_by, granted_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)",
                params![
                    mandate_id,
                    agent,
                    terms.payee_kind,
                    terms.payee,
                    terms.currency,
                    to_i64("max_per_payment_minor", terms.max_per_payment_minor)?,
                    to_i64("total_cap_minor", terms.total_cap_minor)?,
                    terms.expires_at,
                    terms.purpose,
                    i64::from(terms.single_use),
                    approval,
                    decided_by,
                    now_ms
                ],
            )?;
            Granted::Created
        }
    };
    tx.commit()?;
    Ok(outcome)
}

/// Replace the terms of an active mandate with terms the caller has checked widen nothing
/// (`shogun_agents::payment::MandateTerms::widens`). The grant decision stays the provenance.
pub fn narrow(conn: &Connection, mandate_id: &str, terms: &TermsRow, now_ms: i64) -> Result<(), MandateError> {
    validate_terms(terms)?;
    let changed = conn.execute(
        "UPDATE payment_mandates SET max_per_payment_minor = ?2, total_cap_minor = ?3, expires_at = ?4,
            single_use = ?5, revision = revision + 1, updated_at = ?6
         WHERE mandate_id = ?1 AND status = 'active'",
        params![
            mandate_id,
            to_i64("max_per_payment_minor", terms.max_per_payment_minor)?,
            to_i64("total_cap_minor", terms.total_cap_minor)?,
            terms.expires_at,
            i64::from(terms.single_use),
            now_ms
        ],
    )?;
    if changed == 1 {
        Ok(())
    } else if get(conn, mandate_id)?.is_some() {
        Err(MandateError::Revoked)
    } else {
        Err(MandateError::NotFound)
    }
}

/// Revoke `mandate_id`. `Ok(false)` when it was already revoked (idempotent).
pub fn revoke(conn: &Connection, mandate_id: &str, by: &str, now_ms: i64) -> Result<bool, MandateError> {
    label("revoked_by", by)?;
    let changed = conn.execute(
        "UPDATE payment_mandates SET status = 'revoked', revoked_by = ?2, revoked_at = ?3, updated_at = ?3
         WHERE mandate_id = ?1 AND status = 'active'",
        params![mandate_id, by, now_ms],
    )?;
    if changed == 1 {
        return Ok(true);
    }
    match get(conn, mandate_id)? {
        Some(_) => Ok(false),
        None => Err(MandateError::NotFound),
    }
}

/// One spend row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpendRow {
    pub spend_ref: String,
    pub actor: String,
    pub idempotency_key: String,
    pub mandate_id: Option<String>,
    /// `mandate` or `approval`.
    pub authorization: String,
    pub approval_id: Option<u64>,
    pub decided_by: Option<String>,
    pub payee: String,
    pub currency: String,
    pub amount_minor: u64,
    pub status: String,
    pub reason_class: Option<String>,
    pub rail: String,
    pub rail_ref: Option<String>,
    pub created_at: i64,
    pub settled_at: Option<i64>,
}

const SPEND_COLUMNS: &str = "spend_ref, actor, idempotency_key, mandate_id, authorization, approval_id, decided_by, payee,
    currency, amount_minor, status, reason_class, rail, rail_ref, created_at, settled_at";

fn spend_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<SpendRow> {
    Ok(SpendRow {
        spend_ref: r.get(0)?,
        actor: r.get(1)?,
        idempotency_key: r.get(2)?,
        mandate_id: r.get(3)?,
        authorization: r.get(4)?,
        approval_id: r.get::<_, Option<i64>>(5)?.map(to_u64),
        decided_by: r.get(6)?,
        payee: r.get(7)?,
        currency: r.get(8)?,
        amount_minor: to_u64(r.get(9)?),
        status: r.get(10)?,
        reason_class: r.get(11)?,
        rail: r.get(12)?,
        rail_ref: r.get(13)?,
        created_at: r.get(14)?,
        settled_at: r.get(15)?,
    })
}

/// The spend `actor` recorded under `key`, if any.
pub fn spend_by_key(conn: &Connection, actor: &str, key: &str) -> Result<Option<SpendRow>, MandateError> {
    Ok(conn
        .query_row(&format!("SELECT {SPEND_COLUMNS} FROM payment_spends WHERE actor = ?1 AND idempotency_key = ?2"), [actor, key], spend_from)
        .optional()?)
}

/// The spend `spend_ref`, if any.
pub fn spend_by_ref(conn: &Connection, spend_ref: &str) -> Result<Option<SpendRow>, MandateError> {
    Ok(conn
        .query_row(&format!("SELECT {SPEND_COLUMNS} FROM payment_spends WHERE spend_ref = ?1"), [spend_ref], spend_from)
        .optional()?)
}

/// The spends recorded against `mandate_id`, newest first, at most `limit`.
pub fn spends_for(conn: &Connection, mandate_id: &str, limit: usize) -> Result<Vec<SpendRow>, MandateError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SPEND_COLUMNS} FROM payment_spends WHERE mandate_id = ?1 ORDER BY created_at DESC, id DESC LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![mandate_id, i64::try_from(limit).unwrap_or(i64::MAX)], spend_from)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// A new spend row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSpend<'a> {
    pub spend_ref: &'a str,
    pub actor: &'a str,
    pub idempotency_key: &'a str,
    pub mandate_id: Option<&'a str>,
    /// `mandate` (reserved against the mandate, status `authorized`) or `approval` (status
    /// `awaiting_approval`).
    pub authorization: &'a str,
    pub approval_id: Option<u64>,
    pub decided_by: Option<&'a str>,
    pub payee: &'a str,
    pub currency: &'a str,
    pub amount_minor: u64,
    pub rail: &'a str,
    pub now_ms: i64,
}

/// What [`reserve_spend`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reserved {
    /// The row was written.
    New,
    /// `(actor, idempotency_key)` already has a row: this one.
    Replay(Box<SpendRow>),
    /// The mandate no longer covers the amount (checked again inside the transaction by
    /// `check`, which sees the mandate as it is now).
    Refused(&'static str),
}

/// Record `spend` in one transaction: replay an existing `(actor, key)`, else (for a mandate
/// spend) re-read the mandate and its spent total and let `check` refuse, else insert.
pub fn reserve_spend(
    conn: &mut Connection,
    spend: &NewSpend<'_>,
    check: impl FnOnce(Option<&MandateRow>) -> Result<(), &'static str>,
) -> Result<Reserved, MandateError> {
    label("spend_ref", spend.spend_ref)?;
    label("actor", spend.actor)?;
    label("idempotency_key", spend.idempotency_key)?;
    label("payee", spend.payee)?;
    label("rail", spend.rail)?;
    let status = match spend.authorization {
        "mandate" => "authorized",
        "approval" => "awaiting_approval",
        _ => return Err(MandateError::InvalidField("authorization")),
    };
    let amount = to_i64("amount_minor", spend.amount_minor)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(existing) = spend_by_key(&tx, spend.actor, spend.idempotency_key)? {
        return Ok(Reserved::Replay(Box::new(existing)));
    }
    if let Some(mandate_id) = spend.mandate_id {
        let mandate = get(&tx, mandate_id)?;
        if let Err(why) = check(mandate.as_ref()) {
            return Ok(Reserved::Refused(why));
        }
    }
    tx.execute(
        "INSERT INTO payment_spends (spend_ref, actor, idempotency_key, mandate_id, authorization, approval_id, decided_by,
            payee, currency, amount_minor, status, rail, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            spend.spend_ref,
            spend.actor,
            spend.idempotency_key,
            spend.mandate_id,
            spend.authorization,
            spend.approval_id.map(|id| to_i64("approval_id", id)).transpose()?,
            spend.decided_by,
            spend.payee,
            spend.currency,
            amount,
            status,
            spend.rail,
            spend.now_ms
        ],
    )?;
    tx.commit()?;
    Ok(Reserved::New)
}

/// Attach the per-payment approval a spend waits on.
pub fn set_spend_approval(conn: &Connection, spend_ref: &str, approval_id: u64) -> Result<bool, MandateError> {
    Ok(conn.execute(
        "UPDATE payment_spends SET approval_id = ?2 WHERE spend_ref = ?1 AND status = 'awaiting_approval'",
        params![spend_ref, to_i64("approval_id", approval_id)?],
    )? == 1)
}

/// Claim an approved per-payment spend for execution once (`awaiting_approval` → `executing`),
/// recording who decided. `Ok(None)` when there is no such waiting spend (never queued, already
/// claimed, or tampered with): nothing may then be paid.
pub fn claim_approved_spend(
    conn: &Connection,
    spend_ref: &str,
    approval_id: u64,
) -> Result<Option<SpendRow>, MandateError> {
    let approval = to_i64("approval_id", approval_id)?;
    let decided_by: Option<String> = conn
        .query_row(
            "SELECT decided_by FROM approval_decisions WHERE approval_id = ?1 AND decision = 'approve'",
            [approval],
            |r| r.get(0),
        )
        .optional()?;
    let Some(decided_by) = decided_by else {
        return Err(MandateError::NotApproved);
    };
    let changed = conn.execute(
        "UPDATE payment_spends SET status = 'executing', approval_id = ?2, decided_by = ?3
         WHERE spend_ref = ?1 AND status = 'awaiting_approval' AND authorization = 'approval'",
        params![spend_ref, approval, decided_by],
    )?;
    if changed == 1 {
        spend_by_ref(conn, spend_ref)
    } else {
        Ok(None)
    }
}

/// Settle a spend that was `authorized` or `executing`: `settled` with the rail's reference, or
/// `failed` / `not_queued` with a fixed class. `Ok(false)` when it was not in a settleable state.
pub fn settle_spend(
    conn: &Connection,
    spend_ref: &str,
    status: &str,
    reason_class: Option<&str>,
    rail_ref: Option<&str>,
    now_ms: i64,
) -> Result<bool, MandateError> {
    if !matches!(status, "settled" | "failed" | "not_queued") {
        return Err(MandateError::InvalidField("status"));
    }
    if let Some(class) = reason_class {
        label("reason_class", class)?;
    }
    if let Some(r) = rail_ref {
        label("rail_ref", r)?;
    }
    let from = if status == "not_queued" { "('awaiting_approval')" } else { "('authorized', 'executing')" };
    Ok(conn.execute(
        &format!(
            "UPDATE payment_spends SET status = ?2, reason_class = ?3, rail_ref = ?4, settled_at = ?5
             WHERE spend_ref = ?1 AND status IN {from}"
        ),
        params![spend_ref, status, reason_class, rail_ref, now_ms],
    )? == 1)
}

/// Spent total and count against `mandate_id` (the statuses that count).
pub fn spent_against(conn: &Connection, mandate_id: &str) -> Result<(u64, u64), MandateError> {
    let (sum, count): (i64, i64) = conn.query_row(
        &format!(
            "SELECT COALESCE(SUM(amount_minor), 0), COUNT(*) FROM payment_spends
             WHERE mandate_id = ?1 AND authorization = 'mandate' AND status IN {COUNTED}"
        ),
        [mandate_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((to_u64(sum), to_u64(count)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Connection {
        crate::open_in_memory().expect("store")
    }

    fn terms() -> TermsRow {
        TermsRow {
            payee_kind: "class".into(),
            payee: "cloud".into(),
            currency: "USD".into(),
            max_per_payment_minor: 500,
            total_cap_minor: 1_000,
            expires_at: 10_000,
            purpose: "Build minutes".into(),
            single_use: false,
        }
    }

    fn approve(conn: &Connection, id: i64) {
        conn.execute(
            "INSERT INTO approval_decisions (approval_id, decision, decided_by, decided_at, via, level)
             VALUES (?1, 'approve', 'human:owner', 1, 'token', 'L3')",
            [id],
        )
        .expect("decision");
    }

    fn spend<'a>(key: &'a str, r: &'a str, amount: u64) -> NewSpend<'a> {
        NewSpend {
            spend_ref: r,
            actor: "agent:builder",
            idempotency_key: key,
            mandate_id: Some("pmd_000000000001"),
            authorization: "mandate",
            approval_id: Some(1),
            decided_by: Some("human:owner"),
            payee: "cloud:hetzner",
            currency: "USD",
            amount_minor: amount,
            rail: "stub",
            now_ms: 5,
        }
    }

    #[test]
    fn a_mandate_needs_a_recorded_human_approval() {
        let mut conn = store();
        assert_eq!(
            grant(&mut conn, "pmd_000000000001", "agent:builder", &terms(), false, 1, 2),
            Err(MandateError::NotApproved)
        );
        approve(&conn, 1);
        assert_eq!(grant(&mut conn, "pmd_000000000001", "agent:builder", &terms(), false, 1, 2), Ok(Granted::Created));
        assert_eq!(grant(&mut conn, "pmd_000000000001", "agent:builder", &terms(), false, 1, 2), Ok(Granted::AlreadyApplied));
        let row = get(&conn, "pmd_000000000001").expect("get").expect("row");
        assert_eq!((row.granted_by.as_str(), row.grant_approval_id, row.revision), ("human:owner", 1, 1));

        let mut wider = terms();
        wider.total_cap_minor = 5_000;
        approve(&conn, 2);
        assert_eq!(grant(&mut conn, "pmd_000000000001", "agent:other", &wider, true, 2, 3), Err(MandateError::WrongAgent));
        assert_eq!(grant(&mut conn, "pmd_000000000001", "agent:builder", &wider, true, 2, 3), Ok(Granted::Amended));
        let row = get(&conn, "pmd_000000000001").expect("get").expect("row");
        assert_eq!((row.terms.total_cap_minor, row.grant_approval_id, row.revision), (5_000, 2, 2));
    }

    #[test]
    fn spends_reserve_replay_and_settle() {
        let mut conn = store();
        approve(&conn, 1);
        grant(&mut conn, "pmd_000000000001", "agent:builder", &terms(), false, 1, 2).expect("grant");
        let cap = |m: Option<&MandateRow>| -> Result<(), &'static str> {
            let m = m.ok_or("mandate_not_found")?;
            if m.spent_minor + 400 > m.terms.total_cap_minor { Err("over_total_cap") } else { Ok(()) }
        };
        assert_eq!(reserve_spend(&mut conn, &spend("k1", "psp_000000000001", 400), cap).expect("r"), Reserved::New);
        assert!(matches!(reserve_spend(&mut conn, &spend("k1", "psp_000000000009", 400), cap).expect("r"), Reserved::Replay(row) if row.spend_ref == "psp_000000000001"));
        assert_eq!(reserve_spend(&mut conn, &spend("k2", "psp_000000000002", 400), cap).expect("r"), Reserved::New);
        assert_eq!(spent_against(&conn, "pmd_000000000001").expect("s"), (800, 2));
        assert_eq!(reserve_spend(&mut conn, &spend("k3", "psp_000000000003", 400), cap).expect("r"), Reserved::Refused("over_total_cap"));
        // A failed payment releases its reservation.
        assert!(settle_spend(&conn, "psp_000000000002", "failed", Some("provider_error"), None, 6).expect("settle"));
        assert!(!settle_spend(&conn, "psp_000000000002", "settled", None, Some("stub_1"), 7).expect("settle"));
        assert_eq!(spent_against(&conn, "pmd_000000000001").expect("s"), (400, 1));
        assert!(settle_spend(&conn, "psp_000000000001", "settled", None, Some("stub_1"), 7).expect("settle"));
        let listed = list(&conn, 10).expect("list");
        assert_eq!((listed[0].spent_minor, listed[0].payments), (400, 1));
        assert!(revoke(&conn, "pmd_000000000001", "agent:builder", 8).expect("revoke"));
        assert!(!revoke(&conn, "pmd_000000000001", "agent:builder", 8).expect("revoke"));
        assert_eq!(revoke(&conn, "pmd_nope00000000", "agent:builder", 8), Err(MandateError::NotFound));
        assert_eq!(narrow(&conn, "pmd_000000000001", &terms(), 9), Err(MandateError::Revoked));
    }

    #[test]
    fn a_per_payment_spend_is_claimed_once_after_approval() {
        let mut conn = store();
        let mut s = spend("k1", "psp_000000000001", 900);
        s.mandate_id = None;
        s.authorization = "approval";
        s.approval_id = None;
        s.decided_by = None;
        assert_eq!(reserve_spend(&mut conn, &s, |_| Ok(())).expect("r"), Reserved::New);
        assert!(set_spend_approval(&conn, "psp_000000000001", 4).expect("set"));
        assert_eq!(claim_approved_spend(&conn, "psp_000000000001", 4), Err(MandateError::NotApproved));
        approve(&conn, 4);
        let claimed = claim_approved_spend(&conn, "psp_000000000001", 4).expect("claim").expect("row");
        assert_eq!((claimed.status.as_str(), claimed.decided_by.as_deref()), ("executing", Some("human:owner")));
        assert_eq!(claim_approved_spend(&conn, "psp_000000000001", 4).expect("claim"), None);
    }
}
