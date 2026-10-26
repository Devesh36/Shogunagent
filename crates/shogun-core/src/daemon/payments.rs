//! The payment ledger on the store (ShogunAgent E46, V105): the `Db` side of `payments.*` and of
//! the executor that applies a human's approval of a mandate grant or a payment.
//!
//! Store faults are `payments_unavailable`: the face then pays nothing.

use shogun_mcp::payments_api::{PaymentsOp, PaymentsReply};
use shogun_memory::payment_mandates::{self as pm, Granted, MandateError, SpendRow, TermsRow};

use super::Db;

/// Spends listed with one mandate.
const SPENDS_LISTED: usize = 20;

fn code(e: MandateError) -> String {
    match e {
        MandateError::NotFound => "mandate_not_found",
        MandateError::Revoked => "mandate_revoked",
        MandateError::WrongAgent => "mandate_not_yours",
        MandateError::NotApproved => "not_approved",
        MandateError::InvalidField(_) => "invalid_field",
        MandateError::Db => "payments_unavailable",
    }
    .to_string()
}

impl Db {
    /// The store primitives of `payments.*`. The rail operations (`PayeeClass`, `PayReserved`)
    /// are the backend's: this answers them as unavailable.
    pub fn payments_store(&self, op: PaymentsOp<'_>) -> Result<PaymentsReply, String> {
        let now = self.now_ms();
        let answered = self.with_conn_mut("payments", |conn| -> Result<Result<PaymentsReply, String>, String> {
            Ok(match op {
                PaymentsOp::Get(id) => pm::get(conn, id).map(PaymentsReply::Mandate).map_err(code),
                PaymentsOp::List(limit) => pm::list(conn, limit).map(PaymentsReply::Mandates).map_err(code),
                PaymentsOp::Spends(id) => pm::spends_for(conn, id, SPENDS_LISTED).map(PaymentsReply::Spends).map_err(code),
                PaymentsOp::Narrow { mandate_id, terms } => pm::narrow(conn, mandate_id, terms, now).map(|()| PaymentsReply::Done(true)).map_err(code),
                PaymentsOp::Revoke { mandate_id, by } => pm::revoke(conn, mandate_id, by, now).map(PaymentsReply::Done).map_err(code),
                PaymentsOp::SpendByKey { actor, key } => pm::spend_by_key(conn, actor, key).map(PaymentsReply::Spend).map_err(code),
                PaymentsOp::Reserve { spend, check } => pm::reserve_spend(conn, spend, check).map(PaymentsReply::Reserved).map_err(code),
                PaymentsOp::Queued { spend_ref, approval_id: Some(id) } => {
                    pm::set_spend_approval(conn, spend_ref, id).map(PaymentsReply::Done).map_err(code)
                }
                PaymentsOp::Queued { spend_ref, approval_id: None } => {
                    pm::settle_spend(conn, spend_ref, "not_queued", Some("not_queued"), None, now).map(PaymentsReply::Done).map_err(code)
                }
                PaymentsOp::Close { spend_ref, approval_id, outcome, by } => {
                    pm::close_unapproved_spend(conn, spend_ref, approval_id, outcome, by, now).map(PaymentsReply::Done).map_err(code)
                }
                PaymentsOp::Reconcile { now_ms } => {
                    let window = i64::try_from(shogun_mcp::payments_api::APPROVAL_TIMEOUT_MS).unwrap_or(i64::MAX);
                    pm::reconcile_unapproved_spends(conn, now_ms, window).map(|n| PaymentsReply::Done(n > 0)).map_err(code)
                }
                PaymentsOp::PayeeClass(_) | PaymentsOp::PayReserved { .. } => Err("payment_rail_unavailable".to_string()),
            })
        });
        match answered {
            Ok(reply) => reply,
            Err(_) => Err("payments_unavailable".to_string()),
        }
    }

    /// Apply a human's approval of a mandate grant (`approval_id`, already recorded `approve` in
    /// the principal's ledger). The store refuses a grant without that decision.
    pub fn payments_grant(
        &self,
        mandate_id: &str,
        agent: &str,
        terms: &TermsRow,
        amends: bool,
        approval_id: u64,
    ) -> Result<Granted, String> {
        let now = self.now_ms();
        self.with_conn_mut("payments.grant", |conn| -> Result<Result<Granted, String>, String> {
            Ok(pm::grant(conn, mandate_id, agent, terms, amends, approval_id, now).map_err(code))
        })
        .map_err(|_| "payments_unavailable".to_string())?
    }

    /// Claim a human-approved per-payment spend for execution, once.
    pub fn payments_claim_approved(&self, spend_ref: &str, approval_id: u64) -> Result<Option<SpendRow>, String> {
        self.with_conn("payments.claim", |conn| -> Result<Result<Option<SpendRow>, String>, String> {
            Ok(pm::claim_approved_spend(conn, spend_ref, approval_id).map_err(code))
        })
        .map_err(|_| "payments_unavailable".to_string())?
    }

    /// Settle a spend after the rail answered (or did not).
    pub fn payments_settle(&self, spend_ref: &str, settled: bool, reason_class: Option<&str>, rail_ref: Option<&str>) -> Result<bool, String> {
        let now = self.now_ms();
        let status = if settled { "settled" } else { "failed" };
        self.with_conn("payments.settle", |conn| -> Result<Result<bool, String>, String> {
            Ok(pm::settle_spend(conn, spend_ref, status, reason_class, rail_ref, now).map_err(code))
        })
        .map_err(|_| "payments_unavailable".to_string())?
    }
}
