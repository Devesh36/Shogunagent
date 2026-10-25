//! The payment rail seam (ShogunAgent E46, feature `exec`): where a payment actually moves money,
//! and the one function each payment path goes through.
//!
//! - [`PaymentRail`] is the transport seam. This build ships only [`StubRail`], which moves no
//!   money: it classifies a payee by the `<class>:` prefix of its id and answers a reference
//!   `stub_<spend_ref>`. It is off unless the operator sets `SHOGUN_PAYMENT_RAIL=stub`; without a
//!   rail `payments.spend` answers `payment_rail_unavailable` before anything is recorded.
//! - [`RailTransport`] adapts a rail to [`SendTransport`], so a payment goes through
//!   [`execute_send`] like every other L3 send: the traceability row (destination
//!   `payment_rail`, body digested, never stored) is written before the rail is called, and no row
//!   means no payment.
//! - [`pay_reserved`] runs a spend a mandate pre-approved (its row is already `authorized`);
//!   [`pay_approved`] runs one a human approved per payment (claimed `awaiting_approval` →
//!   `executing` once). Both settle the spend row with the rail's reference or a fixed class.
//!
//! What a real rail needs is in `docs/api/payments.md` ("A real rail"): a credential injected at
//! this transport (never seen by an agent or the router), idempotency on `spend_ref`, payee
//! classification from the rail's own merchant data, a settlement callback, refunds and disputes,
//! and an egress through `shogun-core`'s one HTTP client.

use std::sync::{Arc, Mutex};

use shogun_agents::approval::ConfirmedSend;
use shogun_agents::payment::PaymentOrder;
use shogun_agents::permission::SendAction;

use crate::daemon::Db;
use crate::send_exec::{execute_send, SendExecOutcome, SendTransport};

/// The environment variable that selects the rail. `stub` is the only value this build knows.
pub const RAIL_ENV: &str = "SHOGUN_PAYMENT_RAIL";

/// Where money moves. Errors are fixed classes from
/// [`shogun_agents::approval::EXECUTION_FAILURE_CLASSES`], never a rail message.
pub trait PaymentRail: Send + Sync {
    /// The label recorded on each spend.
    fn name(&self) -> &'static str;
    /// The rail's own class for a payee (a merchant category), or `None` when it has none. A
    /// class mandate is checked against this, never a class the caller asserts.
    fn payee_class(&self, payee: &str) -> Option<String>;
    /// Pay `order` once: `spend_ref` is the idempotency key, so a retried call never pays twice.
    /// Answers the rail's reference.
    fn pay(&self, order: &PaymentOrder) -> Result<String, &'static str>;
}

/// The development rail: moves no money, records nothing outside the principal's store.
#[derive(Debug, Default, Clone, Copy)]
pub struct StubRail;

impl PaymentRail for StubRail {
    fn name(&self) -> &'static str {
        "stub"
    }

    fn payee_class(&self, payee: &str) -> Option<String> {
        payee
            .split_once(':')
            .map(|(class, _)| class)
            .filter(|class| shogun_agents::payment::plausible_class(class))
            .map(str::to_string)
    }

    fn pay(&self, order: &PaymentOrder) -> Result<String, &'static str> {
        Ok(format!("stub_{}", order.spend_ref))
    }
}

static INSTALLED: std::sync::OnceLock<Arc<dyn PaymentRail>> = std::sync::OnceLock::new();

/// Install the rail for this process from its composition root (a real rail, or a test's). The
/// first install wins; `false` when one was already installed.
pub fn install(rail: Arc<dyn PaymentRail>) -> bool {
    INSTALLED.set(rail).is_ok()
}

/// The rail this process is configured with: the installed one, else `SHOGUN_PAYMENT_RAIL`
/// (`stub`), else `None` (payments refused).
pub fn configured() -> Option<Arc<dyn PaymentRail>> {
    if let Some(rail) = INSTALLED.get() {
        return Some(rail.clone());
    }
    match std::env::var(RAIL_ENV).ok()?.trim() {
        "stub" => Some(Arc::new(StubRail)),
        _ => None,
    }
}

/// A rail as a [`SendTransport`]: it accepts only [`SendAction::Pay`] and keeps the reference the
/// rail answered for the caller to record.
pub struct RailTransport {
    rail: Arc<dyn PaymentRail>,
    reference: Mutex<Option<String>>,
    failure: Mutex<Option<&'static str>>,
}

impl RailTransport {
    pub fn new(rail: Arc<dyn PaymentRail>) -> Self {
        Self { rail, reference: Mutex::new(None), failure: Mutex::new(None) }
    }

    /// The rail's reference for the payment, once it answered.
    pub fn reference(&self) -> Option<String> {
        self.reference.lock().ok().and_then(|r| r.clone())
    }

    /// The rail's failure class, when it refused.
    pub fn failure(&self) -> Option<&'static str> {
        self.failure.lock().ok().and_then(|f| *f)
    }
}

const NOT_A_PAYMENT: &str = "the payment rail carries payments only";

impl SendTransport for RailTransport {
    fn precheck(&self, action: &SendAction) -> Result<(), String> {
        match action {
            SendAction::Pay { order } => order.validate().map_err(str::to_string),
            _ => Err(NOT_A_PAYMENT.to_string()),
        }
    }

    fn send(&self, action: &SendAction, _body: &str) -> Result<(), String> {
        let SendAction::Pay { order } = action else {
            return Err(NOT_A_PAYMENT.to_string());
        };
        match self.rail.pay(order) {
            Ok(reference) => {
                if let Ok(mut slot) = self.reference.lock() {
                    *slot = Some(reference);
                }
                Ok(())
            }
            Err(class) => {
                if let Ok(mut slot) = self.failure.lock() {
                    *slot = Some(class);
                }
                Err(format!("payment rail refused: {class}"))
            }
        }
    }
}

/// The settled outcome of one payment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Paid {
    Settled { rail_ref: String },
    Failed(&'static str),
}

fn run(db: &Db, rail: Arc<dyn PaymentRail>, confirmed: &ConfirmedSend) -> Paid {
    let SendAction::Pay { order } = &confirmed.action else {
        return Paid::Failed("not_executable");
    };
    let transport = RailTransport::new(rail);
    let sink = db.traceability_sink();
    let outcome = execute_send(confirmed, &transport, &sink);
    let paid = match (outcome, transport.reference()) {
        (SendExecOutcome::Sent, Some(rail_ref)) => Paid::Settled { rail_ref },
        (SendExecOutcome::Sent, None) => Paid::Failed("provider_error"),
        (SendExecOutcome::Failed(error), _) => Paid::Failed(match transport.failure() {
            Some(class) if shogun_agents::approval::EXECUTION_FAILURE_CLASSES.contains(&class) => class,
            Some(_) => "provider_error",
            None => crate::approval_exec::classify(&error),
        }),
    };
    let settled = match &paid {
        Paid::Settled { rail_ref } => db.payments_settle(&order.spend_ref, true, None, Some(rail_ref)),
        Paid::Failed(class) => db.payments_settle(&order.spend_ref, false, Some(class), None),
    };
    if settled.is_err() {
        // The rail answered but the row could not be settled: it stays `executing` /
        // `authorized` (counted against the cap, outcome visible as unknown), never re-run.
        eprintln!("shogun-core: a payment's spend row could not be settled");
    }
    paid
}

/// Run a spend a mandate pre-approved (its row is `authorized`): traced, paid, settled.
pub fn pay_reserved(db: &Db, rail: Arc<dyn PaymentRail>, confirmed: &ConfirmedSend) -> Paid {
    run(db, rail, confirmed)
}

/// Run a payment a human approved (`approval_id`, recorded `approve` in the ledger): the spend
/// row is claimed `awaiting_approval` → `executing` once, so a second decision or a second
/// process never pays it twice; a payment without its waiting row is never paid.
pub fn pay_approved(db: &Db, rail: Arc<dyn PaymentRail>, confirmed: &ConfirmedSend, approval_id: u64) -> Paid {
    let SendAction::Pay { order } = &confirmed.action else {
        return Paid::Failed("not_executable");
    };
    match db.payments_claim_approved(&order.spend_ref, approval_id) {
        Ok(Some(row)) if row.payee == order.payee && row.amount_minor == order.amount_minor && row.currency == order.currency => {
            run(db, rail, confirmed)
        }
        Ok(Some(_)) => {
            let _ = db.payments_settle(&order.spend_ref, false, Some("invalid_target"), None);
            Paid::Failed("invalid_target")
        }
        Ok(None) => Paid::Failed("not_executable"),
        Err(_) => Paid::Failed("ledger_unavailable"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn the_stub_classifies_by_prefix_and_never_names_a_real_reference() {
        assert_eq!(StubRail.payee_class("cloud:hetzner").as_deref(), Some("cloud"));
        assert_eq!(StubRail.payee_class("hetzner"), None);
        assert_eq!(StubRail.payee_class("Bad Class:x"), None);
        let order = PaymentOrder {
            spend_ref: "psp_0123456789ab".into(),
            payee: "cloud:hetzner".into(),
            currency: "USD".into(),
            amount_minor: 100,
            mandate_id: None,
        };
        assert_eq!(StubRail.pay(&order).unwrap(), "stub_psp_0123456789ab");
        let transport = RailTransport::new(Arc::new(StubRail));
        assert!(transport.precheck(&SendAction::SendEmail { to: "a@b.co".into() }).is_err());
        assert!(transport.send(&SendAction::Pay { order: Box::new(order) }, "").is_ok());
        assert_eq!(transport.reference().as_deref(), Some("stub_psp_0123456789ab"));
    }
}
