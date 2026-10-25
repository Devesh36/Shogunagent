//! Agent payment mandates (ShogunAgent E46): the typed pre-approval a human gives an agent to pay
//! within bounds, and the checks that decide whether one payment falls inside them.
//!
//! **A payment is L3 by type.** [`SendAction::Pay`] is a [`SendAction`], so
//! [`crate::permission::Action::required_level`] is L3 and no L1 path can represent it
//! (invariant 3 of ShogunAgent, invariant 4 of the permission model). A mandate does not lower
//! that level. It moves *when* the L3 confirmation is given, the way a standing approval does for
//! an L2 service change (FR-TG-21): the human confirms the bounds once, on the approval page, and
//! each payment inside them carries that confirmation as a [`MandateAuthorization`].
//!
//! - **Granting is L3 and human-only.** Creating a mandate, widening it (a larger cap, a later
//!   expiry, another payee, another currency, a reusable one from a single-use one) or extending
//!   it is a [`SendAction::GrantPaymentMandate`] that waits in the one approval queue. Only a
//!   human decision (signed approval link or the operator's admin credential) releases it, and
//!   the store refuses to write a mandate whose grant has no recorded human `approve` decision.
//! - **Narrowing and revoking are direct.** [`MandateTerms::widens`] is the one comparison: terms
//!   that widen nothing apply at once from any face, like a privacy-narrowing setting.
//! - **Spending.** [`authorize`] is the only constructor of a [`MandateAuthorization`]: the
//!   mandate is live, the caller is the agent it was granted to, the payee and currency are
//!   covered and the amount fits the per-payment and remaining total caps. Anything else is an
//!   [`OutOfBounds`] reason, and the face falls back to a normal per-payment L3 approval.
//!
//! Amounts are integer minor units of an ISO 4217 currency (`1250` USD = $12.50). Pure and
//! dependency-free, so every bound is Linux-testable.

use crate::approval::{ConfirmedSend, Preview, Route};
use crate::permission::SendAction;

/// The longest purpose text a mandate keeps (chars).
pub const MAX_PURPOSE_CHARS: usize = 280;
/// The longest payee id (chars).
pub const MAX_PAYEE_CHARS: usize = 128;
/// The longest payee class (chars).
pub const MAX_CLASS_CHARS: usize = 64;
/// The longest a mandate may run, in days (one year and a day).
pub const MAX_MANDATE_DAYS: i64 = 366;
/// The largest amount any mandate or payment names, in minor units (10^12: ten billion in a
/// two-decimal currency). Keeps every sum inside `i64` in the store.
pub const MAX_AMOUNT_MINOR: u64 = 1_000_000_000_000;

const DAY_MS: i64 = 86_400_000;

/// What a mandate lets the agent pay: one exact payee, or every payee of a class. A class is
/// checked against the class the payment **rail** reports for the payee, never a class the
/// caller asserts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayeeScope {
    Payee(String),
    Class(String),
}

impl PayeeScope {
    /// `payee` / `class`, the wire and storage label.
    pub fn kind(&self) -> &'static str {
        match self {
            PayeeScope::Payee(_) => "payee",
            PayeeScope::Class(_) => "class",
        }
    }

    pub fn value(&self) -> &str {
        match self {
            PayeeScope::Payee(v) | PayeeScope::Class(v) => v,
        }
    }

    /// Rebuild from the stored `(kind, value)`.
    pub fn parse(kind: &str, value: &str) -> Option<Self> {
        match kind {
            "payee" if plausible_payee(value) => Some(PayeeScope::Payee(value.to_string())),
            "class" if plausible_class(value) => Some(PayeeScope::Class(value.to_string())),
            _ => None,
        }
    }

    /// Whether a payment to `payee` (of `payee_class`, as the rail reports it) is covered.
    pub fn covers(&self, payee: &str, payee_class: Option<&str>) -> bool {
        match self {
            PayeeScope::Payee(p) => p == payee,
            PayeeScope::Class(c) => payee_class == Some(c.as_str()),
        }
    }
}

/// A payee id: visible ASCII, no whitespace, at most [`MAX_PAYEE_CHARS`].
pub fn plausible_payee(raw: &str) -> bool {
    !raw.is_empty() && raw.len() <= MAX_PAYEE_CHARS && raw.bytes().all(|b| b.is_ascii_graphic())
}

/// A payee class: lower-case letters, digits, `_` and `-`, at most [`MAX_CLASS_CHARS`].
pub fn plausible_class(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= MAX_CLASS_CHARS
        && raw.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// An ISO 4217 alphabetic code: exactly three upper-case ASCII letters.
pub fn plausible_currency(raw: &str) -> bool {
    raw.len() == 3 && raw.bytes().all(|b| b.is_ascii_uppercase())
}

/// The bounds a human grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MandateTerms {
    pub payee: PayeeScope,
    pub currency: String,
    pub max_per_payment_minor: u64,
    pub total_cap_minor: u64,
    pub expires_at_ms: i64,
    /// What the payments are for, shown to the human on the approval page.
    pub purpose: String,
    /// One payment at most.
    pub single_use: bool,
}

impl MandateTerms {
    /// Why these terms cannot be granted at `now_ms`, or `Ok`.
    pub fn validate(&self, now_ms: i64) -> Result<(), &'static str> {
        match &self.payee {
            PayeeScope::Payee(p) if !plausible_payee(p) => return Err("invalid_payee"),
            PayeeScope::Class(c) if !plausible_class(c) => return Err("invalid_payee_class"),
            _ => {}
        }
        if !plausible_currency(&self.currency) {
            return Err("invalid_currency");
        }
        if self.max_per_payment_minor == 0 || self.max_per_payment_minor > MAX_AMOUNT_MINOR {
            return Err("invalid_max_per_payment");
        }
        if self.total_cap_minor < self.max_per_payment_minor || self.total_cap_minor > MAX_AMOUNT_MINOR {
            return Err("invalid_total_cap");
        }
        if self.expires_at_ms <= now_ms || self.expires_at_ms > now_ms.saturating_add(MAX_MANDATE_DAYS * DAY_MS) {
            return Err("invalid_expiry");
        }
        let purpose = self.purpose.trim();
        if purpose.is_empty() || purpose.chars().count() > MAX_PURPOSE_CHARS || purpose.chars().any(char::is_control) {
            return Err("invalid_purpose");
        }
        Ok(())
    }

    /// Whether replacing `current` with `self` lets the agent pay anything it could not pay
    /// before: a different payee scope or currency, a higher per-payment or total cap, a later
    /// expiry, a reusable mandate from a single-use one, or a changed purpose (the purpose is
    /// what the human read when approving). Terms that widen nothing are a narrowing and apply
    /// directly; anything else needs a human's L3 approval.
    pub fn widens(&self, current: &MandateTerms) -> bool {
        self.payee != current.payee
            || self.currency != current.currency
            || self.max_per_payment_minor > current.max_per_payment_minor
            || self.total_cap_minor > current.total_cap_minor
            || self.expires_at_ms > current.expires_at_ms
            || (current.single_use && !self.single_use)
            || self.purpose.trim() != current.purpose.trim()
    }

    /// The approval page's full text of these terms (FR-AG-03: the whole content, not a summary).
    pub fn describe(&self) -> String {
        let payee = match &self.payee {
            PayeeScope::Payee(p) => format!("payee {p}"),
            PayeeScope::Class(c) => format!("any payee of class {c}"),
        };
        format!(
            "Let the agent pay {payee} in {cur}: at most {per} per payment, {total} in total, until {exp} (unix ms){once}. Purpose: {purpose}",
            cur = self.currency,
            per = format_minor(self.max_per_payment_minor, &self.currency),
            total = format_minor(self.total_cap_minor, &self.currency),
            exp = self.expires_at_ms,
            once = if self.single_use { ", one payment only" } else { "" },
            purpose = self.purpose.trim(),
        )
    }
}

/// `1250 USD` → `12.50 USD`, assuming two minor digits (the display only; amounts stay integer).
pub fn format_minor(amount_minor: u64, currency: &str) -> String {
    format!("{}.{:02} {currency}", amount_minor / 100, amount_minor % 100)
}

/// A mandate as the approval queue carries it: the id it will be stored under, the agent that
/// asked (and alone may spend), the terms, and whether it amends an existing mandate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MandateGrant {
    pub mandate_id: String,
    pub agent: String,
    pub terms: MandateTerms,
    pub amends: bool,
}

/// One payment: what [`SendAction::Pay`] carries. `spend_ref` names its row in the principal's
/// spend ledger; `mandate_id` the mandate it was checked against, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentOrder {
    pub spend_ref: String,
    pub payee: String,
    pub currency: String,
    pub amount_minor: u64,
    pub mandate_id: Option<String>,
}

impl PaymentOrder {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !plausible_payee(&self.payee) {
            return Err("invalid_payee");
        }
        if !plausible_currency(&self.currency) {
            return Err("invalid_currency");
        }
        if self.amount_minor == 0 || self.amount_minor > MAX_AMOUNT_MINOR {
            return Err("invalid_amount");
        }
        if !plausible_ref(&self.spend_ref) {
            return Err("invalid_spend_ref");
        }
        Ok(())
    }

    /// The approval page's destination line.
    pub fn destination(&self) -> String {
        format!("{} ({})", self.payee, format_minor(self.amount_minor, &self.currency))
    }
}

/// A generated id (`pmd_…` / `psp_…`): ASCII letters, digits and `_`, 8–64 chars.
pub fn plausible_ref(raw: &str) -> bool {
    (8..=64).contains(&raw.len()) && raw.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// A mandate as the store knows it at the moment of a spend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MandateStanding {
    pub mandate_id: String,
    pub agent: String,
    pub terms: MandateTerms,
    pub revoked: bool,
    /// Sum of the payments that count against the cap (authorized, executing or settled).
    pub spent_minor: u64,
    /// How many such payments.
    pub payments: u64,
    /// The human decision that granted (or last widened) it.
    pub grant_approval_id: u64,
    pub granted_by: String,
}

/// Why a payment is not covered by a mandate. The face then asks for a per-payment approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutOfBounds {
    Revoked,
    Expired,
    NotYourMandate,
    PayeeNotCovered,
    CurrencyMismatch,
    OverPerPayment,
    OverTotalCap,
    SingleUseSpent,
}

impl OutOfBounds {
    pub fn as_str(self) -> &'static str {
        match self {
            OutOfBounds::Revoked => "mandate_revoked",
            OutOfBounds::Expired => "mandate_expired",
            OutOfBounds::NotYourMandate => "mandate_not_yours",
            OutOfBounds::PayeeNotCovered => "payee_not_covered",
            OutOfBounds::CurrencyMismatch => "currency_mismatch",
            OutOfBounds::OverPerPayment => "over_per_payment",
            OutOfBounds::OverTotalCap => "over_total_cap",
            OutOfBounds::SingleUseSpent => "single_use_spent",
        }
    }
}

/// Proof that a payment is inside a mandate a human granted: the L3 confirmation given in
/// advance. Only [`authorize`] builds one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MandateAuthorization {
    mandate_id: String,
    grant_approval_id: u64,
    granted_by: String,
}

impl MandateAuthorization {
    pub fn mandate_id(&self) -> &str {
        &self.mandate_id
    }
    /// The approval whose human decision granted the mandate (the spend's provenance).
    pub fn grant_approval_id(&self) -> u64 {
        self.grant_approval_id
    }
    /// Who decided it (V92 actor id of a human).
    pub fn granted_by(&self) -> &str {
        &self.granted_by
    }
}

/// Whether `agent` may pay `order` (to a payee the rail classifies as `payee_class`) under
/// `standing` at `now_ms`. The checks run in a fixed order so the reason is deterministic.
pub fn authorize(
    standing: &MandateStanding,
    agent: &str,
    order: &PaymentOrder,
    payee_class: Option<&str>,
    now_ms: i64,
) -> Result<MandateAuthorization, OutOfBounds> {
    let terms = &standing.terms;
    if standing.revoked {
        return Err(OutOfBounds::Revoked);
    }
    if now_ms >= terms.expires_at_ms {
        return Err(OutOfBounds::Expired);
    }
    if standing.agent != agent {
        return Err(OutOfBounds::NotYourMandate);
    }
    if !terms.payee.covers(&order.payee, payee_class) {
        return Err(OutOfBounds::PayeeNotCovered);
    }
    if terms.currency != order.currency {
        return Err(OutOfBounds::CurrencyMismatch);
    }
    if order.amount_minor > terms.max_per_payment_minor {
        return Err(OutOfBounds::OverPerPayment);
    }
    if terms.single_use && standing.payments > 0 {
        return Err(OutOfBounds::SingleUseSpent);
    }
    if standing.spent_minor.saturating_add(order.amount_minor) > terms.total_cap_minor {
        return Err(OutOfBounds::OverTotalCap);
    }
    Ok(MandateAuthorization {
        mandate_id: standing.mandate_id.clone(),
        grant_approval_id: standing.grant_approval_id,
        granted_by: standing.granted_by.clone(),
    })
}

/// The approval request for a mandate (new, widened or extended): L3, applied locally on
/// confirm (nothing leaves the device when a mandate is granted).
pub fn mandate_request(grant: MandateGrant) -> (SendAction, Preview) {
    let body = format!(
        "{}{}\nRequested by {}. Mandate {}.",
        if grant.amends { "Widen or extend an existing payment mandate. " } else { "" },
        grant.terms.describe(),
        grant.agent,
        grant.mandate_id
    );
    let action = SendAction::GrantPaymentMandate { grant: Box::new(grant) };
    let preview = Preview::for_send(&action, body, Route::DirectMcp);
    (action, preview)
}

/// The approval request for one payment outside every mandate: L3, executed through the payment
/// rail once a human approves it.
pub fn payment_request(order: PaymentOrder, memo: &str) -> (SendAction, Preview) {
    let body = payment_body(&order, memo);
    let action = SendAction::Pay { order: Box::new(order) };
    let preview = Preview::for_send(&action, body, Route::DirectMcp);
    (action, preview)
}

/// The confirmed payment a mandate pre-approved: what [`crate::approval::ApprovalQueue`] would
/// hand back after a human's confirm, built from the authorization instead. The order must name
/// the authorizing mandate.
pub fn confirmed_under_mandate(authorization: &MandateAuthorization, mut order: PaymentOrder, memo: &str) -> ConfirmedSend {
    order.mandate_id = Some(authorization.mandate_id.clone());
    let body = format!(
        "{}\nPre-approved by mandate {} (approval {}, decided by {}).",
        payment_body(&order, memo),
        authorization.mandate_id,
        authorization.grant_approval_id,
        authorization.granted_by
    );
    let action = SendAction::Pay { order: Box::new(order) };
    let preview = Preview::for_send(&action, body, Route::DirectMcp);
    ConfirmedSend { action, preview }
}

fn payment_body(order: &PaymentOrder, memo: &str) -> String {
    let memo = memo.trim();
    format!(
        "Pay {} to {}.{}",
        format_minor(order.amount_minor, &order.currency),
        order.payee,
        if memo.is_empty() { String::new() } else { format!(" Memo: {memo}") }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::{Action, Level};

    const NOW: i64 = 1_760_000_000_000;

    fn terms() -> MandateTerms {
        MandateTerms {
            payee: PayeeScope::Class("cloud".into()),
            currency: "USD".into(),
            max_per_payment_minor: 5_000,
            total_cap_minor: 20_000,
            expires_at_ms: NOW + 30 * DAY_MS,
            purpose: "Pay for build minutes".into(),
            single_use: false,
        }
    }

    fn standing() -> MandateStanding {
        MandateStanding {
            mandate_id: "pmd_0123456789ab".into(),
            agent: "agent:builder".into(),
            terms: terms(),
            revoked: false,
            spent_minor: 0,
            payments: 0,
            grant_approval_id: 7,
            granted_by: "human:owner".into(),
        }
    }

    fn order(amount: u64) -> PaymentOrder {
        PaymentOrder {
            spend_ref: "psp_0123456789ab".into(),
            payee: "cloud:hetzner".into(),
            currency: "USD".into(),
            amount_minor: amount,
            mandate_id: None,
        }
    }

    #[test]
    fn payments_and_grants_are_l3_by_type() {
        let (pay, _) = payment_request(order(100), "");
        let (grant, preview) = mandate_request(MandateGrant {
            mandate_id: "pmd_0123456789ab".into(),
            agent: "agent:builder".into(),
            terms: terms(),
            amends: false,
        });
        for action in [pay, grant] {
            let action = Action::Send(action);
            assert_eq!(action.required_level(), Level::L3);
            assert!(!action.is_l1_eligible());
        }
        assert!(preview.full_body.contains("at most 50.00 USD per payment"));
    }

    #[test]
    fn terms_validate() {
        assert_eq!(terms().validate(NOW), Ok(()));
        let mut t = terms();
        t.total_cap_minor = 10;
        assert_eq!(t.validate(NOW), Err("invalid_total_cap"));
        let mut t = terms();
        t.expires_at_ms = NOW + 400 * DAY_MS;
        assert_eq!(t.validate(NOW), Err("invalid_expiry"));
        let mut t = terms();
        t.currency = "usd".into();
        assert_eq!(t.validate(NOW), Err("invalid_currency"));
        let mut t = terms();
        t.payee = PayeeScope::Class("Cloud Stuff".into());
        assert_eq!(t.validate(NOW), Err("invalid_payee_class"));
        let mut t = terms();
        t.purpose = " ".into();
        assert_eq!(t.validate(NOW), Err("invalid_purpose"));
    }

    #[test]
    fn widening_and_narrowing() {
        let current = terms();
        let mut narrower = current.clone();
        narrower.total_cap_minor = 10_000;
        narrower.expires_at_ms -= DAY_MS;
        narrower.single_use = true;
        assert!(!narrower.widens(&current));
        assert!(!current.widens(&current));
        for widen in [
            |t: &mut MandateTerms| t.total_cap_minor += 1,
            |t: &mut MandateTerms| t.max_per_payment_minor += 1,
            |t: &mut MandateTerms| t.expires_at_ms += 1,
            |t: &mut MandateTerms| t.currency = "EUR".into(),
            |t: &mut MandateTerms| t.payee = PayeeScope::Payee("cloud:hetzner".into()),
            |t: &mut MandateTerms| t.purpose = "Anything".into(),
        ] {
            let mut t = current.clone();
            widen(&mut t);
            assert!(t.widens(&current), "{t:?}");
        }
        let mut once = current.clone();
        once.single_use = true;
        assert!(current.widens(&once), "single-use to reusable widens");
    }

    #[test]
    fn authorize_checks_every_bound() {
        let ok = authorize(&standing(), "agent:builder", &order(5_000), Some("cloud"), NOW).unwrap();
        assert_eq!((ok.mandate_id(), ok.grant_approval_id(), ok.granted_by()), ("pmd_0123456789ab", 7, "human:owner"));
        let check = |s: &MandateStanding, agent: &str, o: &PaymentOrder, class: Option<&str>, now: i64| {
            authorize(s, agent, o, class, now).err()
        };
        assert_eq!(check(&standing(), "agent:other", &order(1), Some("cloud"), NOW), Some(OutOfBounds::NotYourMandate));
        assert_eq!(check(&standing(), "agent:builder", &order(1), Some("saas"), NOW), Some(OutOfBounds::PayeeNotCovered));
        assert_eq!(check(&standing(), "agent:builder", &order(1), None, NOW), Some(OutOfBounds::PayeeNotCovered));
        assert_eq!(check(&standing(), "agent:builder", &order(5_001), Some("cloud"), NOW), Some(OutOfBounds::OverPerPayment));
        let mut eur = order(1);
        eur.currency = "EUR".into();
        assert_eq!(check(&standing(), "agent:builder", &eur, Some("cloud"), NOW), Some(OutOfBounds::CurrencyMismatch));
        let mut spent = standing();
        spent.spent_minor = 16_000;
        spent.payments = 4;
        assert_eq!(check(&spent, "agent:builder", &order(4_001), Some("cloud"), NOW), Some(OutOfBounds::OverTotalCap));
        assert!(check(&spent, "agent:builder", &order(4_000), Some("cloud"), NOW).is_none());
        let mut once = standing();
        once.terms.single_use = true;
        once.payments = 1;
        once.spent_minor = 1;
        assert_eq!(check(&once, "agent:builder", &order(1), Some("cloud"), NOW), Some(OutOfBounds::SingleUseSpent));
        let mut revoked = standing();
        revoked.revoked = true;
        assert_eq!(check(&revoked, "agent:builder", &order(1), Some("cloud"), NOW), Some(OutOfBounds::Revoked));
        assert_eq!(
            check(&standing(), "agent:builder", &order(1), Some("cloud"), NOW + 30 * DAY_MS),
            Some(OutOfBounds::Expired)
        );
        let mut exact = standing();
        exact.terms.payee = PayeeScope::Payee("cloud:hetzner".into());
        assert!(check(&exact, "agent:builder", &order(1), None, NOW).is_none());
    }

    #[test]
    fn a_mandate_confirmed_payment_names_its_provenance() {
        let auth = authorize(&standing(), "agent:builder", &order(1_250), Some("cloud"), NOW).unwrap();
        let confirmed = confirmed_under_mandate(&auth, order(1_250), "June invoice");
        let SendAction::Pay { order } = &confirmed.action else { panic!("a payment") };
        assert_eq!(order.mandate_id.as_deref(), Some("pmd_0123456789ab"));
        assert!(confirmed.preview.full_body.contains("12.50 USD"));
        assert!(confirmed.preview.full_body.contains("approval 7, decided by human:owner"));
        assert_eq!(confirmed.preview.key_kind, crate::approval::KeyKind::PaymentRail);
    }

    #[test]
    fn orders_and_refs_validate() {
        assert_eq!(order(1).validate(), Ok(()));
        assert_eq!(order(0).validate(), Err("invalid_amount"));
        let mut bad = order(1);
        bad.payee = "has space".into();
        assert_eq!(bad.validate(), Err("invalid_payee"));
        assert!(!plausible_ref("short"));
        assert!(plausible_ref("psp_0123456789ab"));
        assert_eq!(PayeeScope::parse("class", "cloud"), Some(PayeeScope::Class("cloud".into())));
        assert_eq!(PayeeScope::parse("other", "x"), None);
    }
}
