//! Carrying out a remotely approved action (ShogunAgent E15 follow-up).
//!
//! A human's decision on the approval page, the Slack app or the operator's CLI moves an L2 / L3
//! request to `approved` ([`crate::remote_approvals::apply_with_send`]). This module is the seam
//! between that decision and the code that actually performs the action: the REST server holds an
//! optional [`ApprovalExecutor`], and the decide route hands it the confirmed action once.
//!
//! - **Exactly once.** The queue releases a confirmed action to one decision only (the queue lock),
//!   and the principal's ledger claims it `pending` → `executing` with a compare-and-set (V103)
//!   before the executor is called, so two decisions racing on one approval id, or a second
//!   process, never run it twice.
//! - **Never without a human.** Only the decide route calls an executor, and only with the action
//!   a human's approval released; a rejected, expired or unknown approval has no action to hand
//!   over. No agent-facing face reaches this module.
//! - **Outcome, never content.** The executor answers [`ExecOutcome`]: sent, or failed with a fixed
//!   [`shogun_agents::approval::EXECUTION_FAILURE_CLASSES`] class. The class is what the queue, the
//!   ledger and the audit log record.
//!
//! The implementation lives where the egress does: `shogun_core::approval_exec` (feature `exec`)
//! runs the action through `send_exec::execute_send`, which writes the traceability row before
//! the request leaves. A build without it (this crate's own `server` feature alone) attaches no
//! executor: decided approvals stay `approved` and answer `next_action: "execute_unavailable"`.

use shogun_agents::approval::ConfirmedSend;

/// One approved action handed to the executor.
pub struct ExecRequest<'a> {
    pub approval_id: u64,
    /// The principal whose store and credentials the action uses (multi-principal mode).
    pub principal: Option<&'a str>,
    /// The action and its preview exactly as the human approved them.
    pub confirmed: &'a ConfirmedSend,
}

/// What came of running an approved action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecOutcome {
    /// The action left the device (its traceability row was written first).
    Sent,
    /// It did not complete; the class is one of
    /// [`shogun_agents::approval::EXECUTION_FAILURE_CLASSES`].
    Failed(&'static str),
}

impl ExecOutcome {
    /// The failure class, or `None` when sent.
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Self::Sent => None,
            Self::Failed(class) => Some(class),
        }
    }
}

/// Carries out approved actions. Called once per approval, outside every lock, after the
/// principal's ledger recorded the claim.
pub trait ApprovalExecutor: Send + Sync {
    fn execute(&self, request: &ExecRequest<'_>) -> ExecOutcome;
}

/// Normalize an executor's answer: a failure class outside the allowlist is recorded as
/// `provider_error`, so nothing but a fixed label ever reaches the ledger or the audit log.
pub fn allowlisted(outcome: ExecOutcome) -> ExecOutcome {
    match outcome {
        ExecOutcome::Failed(class) if !shogun_agents::approval::EXECUTION_FAILURE_CLASSES.contains(&class) => {
            ExecOutcome::Failed("provider_error")
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_allowlisted_classes_survive() {
        assert_eq!(allowlisted(ExecOutcome::Sent), ExecOutcome::Sent);
        assert_eq!(allowlisted(ExecOutcome::Failed("not_connected")), ExecOutcome::Failed("not_connected"));
        assert_eq!(allowlisted(ExecOutcome::Failed("mailbox full")), ExecOutcome::Failed("provider_error"));
        assert_eq!(ExecOutcome::Failed("timeout").reason(), Some("timeout"));
        assert_eq!(ExecOutcome::Sent.reason(), None);
    }
}
