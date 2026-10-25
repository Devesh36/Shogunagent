//! Bounded form suggestions and validation. Captured fields are data, never authority.
//! Every plan requires explicit review; this module never writes or submits a form.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const MAX_FIELDS: usize = 80;
const MAX_VALUE: usize = 2_000;
const MAX_INSTRUCTION: usize = 4_000;
const MAX_EVIDENCE: usize = 24;
/// A native dropdown or radio group offers at most this many choices to review and matching.
pub const MAX_OPTIONS: usize = 254;
/// The value a reviewed checkbox answer carries: tick it. Nothing ever unticks a box.
pub const CHECKED: &str = "checked";
pub mod facts;
pub mod jev;
pub use facts::{FactLine, WrittenFacts};

/// Accepted native writes, retained independently of completion or cancellation.
#[derive(Clone, Default)]
pub struct UndoLedger(Vec<(usize, String)>);

impl UndoLedger {
    /// Record immediately after a write; each field can be undone only once.
    pub fn record(&mut self, index: usize, value: &str) {
        if let Some((_, written)) = self.0.iter_mut().find(|(field, _)| *field == index) {
            *written = value.to_owned();
        } else {
            self.0.push((index, value.to_owned()));
        }
    }

    /// The exact subset written so far; never includes unattempted fields.
    pub fn entries(&self) -> &[(usize, String)] {
        &self.0
    }

    /// Retire only a verified restoration; failed fields remain available for retry.
    pub fn restored(&mut self, index: usize) {
        self.0.retain(|(field, _)| *field != index);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FormField {
    pub id: String,
    pub label: String,
    pub section: String,
    pub kind: FieldKind,
    pub has_value: bool,
    pub supported: bool,
    /// The choices of a native dropdown or radio group, in page order. Empty for other kinds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    Text,
    Email,
    Phone,
    Url,
    Number,
    Date,
    Textarea,
    /// A native dropdown (AXPopUpButton). The answer is one of `options`.
    Select,
    /// A checkbox. The only answer is [`CHECKED`].
    Checkbox,
    /// A radio group, one field for the whole group. The answer is one of `options`.
    Radio,
    Unsupported,
}

impl FieldKind {
    /// Choices the page constrains: a value can only be one of the page's own options.
    pub fn is_choice(&self) -> bool {
        matches!(self, Self::Select | Self::Checkbox | Self::Radio)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FormSnapshot {
    pub app: String,
    pub title: String,
    pub website_host: String,
    pub fields: Vec<FormField>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FormEvidence {
    pub id: String,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AnswerKind {
    Exact,
    Draft,
    User,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FormAnswer {
    pub field_id: String,
    pub value: String,
    pub source_ids: Vec<String>,
    pub kind: AnswerKind,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FormPlan {
    pub purpose: String,
    pub subject: String,
    pub answers: Vec<FormAnswer>,
    #[serde(default)]
    pub decisions: Vec<FormDecision>,
}

/// Provider confidence and selected probability are distinct from each other.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FormDecision {
    pub field_id: String,
    pub confidence: f64,
    pub probability: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FormWriteResult {
    pub field_id: String,
    pub status: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FormStatus {
    /// Remaining native Undo window, if this session owns the last fill.
    #[serde(default)]
    pub undo_remaining_ms: Option<u64>,
    /// The Memory API app that asked for this fill, so the review window can name it.
    #[serde(default)]
    pub requester: Option<String>,
    pub session_id: u64,
    pub revision: u64,
    pub phase: String,
    pub snapshot: FormSnapshot,
    pub plan: Option<FormPlan>,
    pub evidence: Vec<FormEvidence>,
    pub results: Vec<FormWriteResult>,
    pub error: Option<String>,
}

fn invalid(code: &str) -> String {
    code.to_owned()
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn valid_source_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':'))
}

fn bounded(text: &str, max: usize) -> bool {
    text.chars().take(max + 1).count() <= max
}

fn safe_text(text: &str) -> bool {
    !text.chars().any(shogun_redact::is_hidden)
        && shogun_redact::redact(text).as_ref() == text
        && !text.contains("[redacted]")
}

/// A captured dropdown or radio choice that may be shown, matched and written.
pub fn valid_option(text: &str) -> bool {
    !text.trim().is_empty() && bounded(text, 256) && safe_text(text)
}

/// Validate captured metadata without changing field identities or availability flags.
/// Errors are fixed codes and contain no captured content.
pub fn validate_snapshot(snapshot: &FormSnapshot) -> Result<(), String> {
    if snapshot.fields.len() > MAX_FIELDS {
        return Err(invalid("form_snapshot_invalid"));
    }
    for text in [&snapshot.app, &snapshot.title, &snapshot.website_host] {
        if !bounded(text, 512) || !safe_text(text) {
            return Err(invalid("form_snapshot_invalid"));
        }
    }
    let mut ids = HashSet::new();
    for field in &snapshot.fields {
        if !valid_id(&field.id)
            || !ids.insert(&field.id)
            || !bounded(&field.label, 256)
            || !bounded(&field.section, 256)
            || !safe_text(&field.label)
            || !safe_text(&field.section)
            || (field.supported
                && (field.label.trim().is_empty() || field.kind == FieldKind::Unsupported))
            || field.options.len() > MAX_OPTIONS
            || !field.options.iter().all(|option| valid_option(option))
            || (!matches!(field.kind, FieldKind::Select | FieldKind::Radio)
                && !field.options.is_empty())
            || (field.supported
                && matches!(field.kind, FieldKind::Select | FieldKind::Radio)
                && field.options.is_empty())
        {
            return Err(invalid("form_snapshot_invalid"));
        }
    }
    Ok(())
}

/// Validate edited answers against the captured targets before native application.
/// Only the explicit human apply flow may accept `User`; its edited values have no source claims.
/// Decision candidate membership is checked before reaching this boundary.
pub fn validate_answers(snapshot: &FormSnapshot, answers: &[FormAnswer]) -> Result<(), String> {
    validate_snapshot(snapshot)?;
    if answers.len() > MAX_FIELDS {
        return Err(invalid("form_answers_invalid"));
    }
    let mut ids = HashSet::new();
    for answer in answers {
        if !valid_id(&answer.field_id) || !ids.insert(&answer.field_id) {
            return Err(invalid("form_target_invalid"));
        }
        let field = snapshot
            .fields
            .iter()
            .find(|field| field.id == answer.field_id)
            .ok_or_else(|| invalid("form_target_invalid"))?;
        if field.has_value || !field.supported || field.kind == FieldKind::Unsupported {
            return Err(invalid("form_target_invalid"));
        }
        if sensitive_field(field) || !safe_text(&answer.value) {
            return Err(invalid("form_sensitive_data"));
        }
        if answer.value.trim().is_empty()
            || !bounded(&answer.value, MAX_VALUE)
            || !valid_value(field, &answer.value)
            || (answer.kind == AnswerKind::Draft && !draft_allowed(&field.kind))
        {
            return Err(invalid("form_value_invalid"));
        }
        if answer.kind == AnswerKind::User {
            if !answer.source_ids.is_empty() {
                return Err(invalid("form_source_invalid"));
            }
        } else {
            let mut sources = HashSet::new();
            if answer.source_ids.is_empty()
                || answer.source_ids.len() > MAX_EVIDENCE + 1
                || answer
                    .source_ids
                    .iter()
                    .any(|id| !valid_source_id(id) || !sources.insert(id))
            {
                return Err(invalid("form_source_invalid"));
            }
        }
    }
    Ok(())
}

/// Free text (a fact, a memory excerpt) checked with the same words as a field label.
pub(crate) fn sensitive_text(text: &str) -> bool {
    sensitive_field(&FormField {
        id: String::new(),
        label: text.to_owned(),
        section: String::new(),
        kind: FieldKind::Text,
        has_value: false,
        supported: true,
        options: Vec::new(),
    })
}

/// Drafts (reworded or derived values) may go into free text and into the page's own choices;
/// typed fields (email, phone, URL, number, date) only take a value copied from a source.
pub(crate) fn draft_allowed(kind: &FieldKind) -> bool {
    matches!(kind, FieldKind::Text | FieldKind::Textarea) || kind.is_choice()
}

/// Whether captured labels identify a credential, payment or government-ID field.
/// Native capture must check this before reading an accessibility value.
pub fn sensitive_field(field: &FormField) -> bool {
    let text = format!("{} {}", field.section, field.label).to_lowercase();
    let compact: String = text.chars().filter(|c| c.is_alphanumeric()).collect();
    [
        "consent",
        "agree",
        "terms",
        "privacyacceptance",
        "password",
        "passwd",
        "passcode",
        "privatekey",
        "apikey",
        "accesstoken",
        "refreshtoken",
        "clientsecret",
        "creditcard",
        "payment",
        "carddetails",
        "cardexpiry",
        "debitcard",
        "cardnumber",
        "cardholder",
        "securitycode",
        "verificationcode",
        "onetimecode",
        "bankaccount",
        "routingnumber",
        "socialsecurity",
        "passport",
        "taxidentification",
        "taxid",
        "nationalid",
        "governmentid",
        "drivinglicense",
        "driverlicense",
        "cvv",
        "cvc",
        "iban",
    ]
    .iter()
    .any(|label| compact.contains(label))
        || text
            .split(|c: char| !c.is_alphanumeric())
            .any(|word| matches!(word, "pin" | "ssn" | "otp" | "token" | "secret"))
}

fn valid_value(field: &FormField, value: &str) -> bool {
    let kind = &field.kind;
    if *kind != FieldKind::Textarea && value.chars().any(char::is_control) {
        return false;
    }
    match kind {
        FieldKind::Text | FieldKind::Textarea => true,
        FieldKind::Select | FieldKind::Radio => field.options.iter().any(|option| option == value),
        FieldKind::Checkbox => value == CHECKED,
        FieldKind::Email => valid_email(value),
        FieldKind::Phone => {
            let digits = value.bytes().filter(u8::is_ascii_digit).count();
            (7..=20).contains(&digits)
                && value.bytes().enumerate().all(|(i, b)| {
                    b.is_ascii_digit()
                        || matches!(b, b' ' | b'-' | b'(' | b')' | b'.')
                        || (b == b'+' && i == 0)
                })
        }
        FieldKind::Url => url::Url::parse(value).is_ok_and(|url| {
            (value.starts_with("https://") || value.starts_with("http://"))
                && matches!(url.scheme(), "https" | "http")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && !value.chars().any(char::is_whitespace)
        }),
        FieldKind::Number => {
            value.trim() == value && value.parse::<f64>().is_ok_and(f64::is_finite)
        }
        FieldKind::Date => valid_date(value),
        FieldKind::Unsupported => false,
    }
}

fn valid_email(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && local.len() <= 64
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && local
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".!#$%&'*+-/=?^_`{|}~".contains(&b))
        && domain.contains('.')
        && domain.len() <= 253
        && domain.split('.').all(|part| {
            !part.is_empty()
                && part.len() <= 63
                && !part.starts_with('-')
                && !part.ends_with('-')
                && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

fn valid_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes
            .iter()
            .enumerate()
            .any(|(i, b)| i != 4 && i != 7 && !b.is_ascii_digit())
    {
        return false;
    }
    let (Ok(year), Ok(month), Ok(day)) = (
        value[..4].parse::<u32>(),
        value[5..7].parse::<u32>(),
        value[8..].parse::<u32>(),
    ) else {
        return false;
    };
    let days = match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return false,
    };
    year > 0 && (1..=days).contains(&day)
}

#[cfg(test)]
mod tests {
    #[test]
    fn partial_undo_retries_only_fields_not_yet_restored() {
        let mut ledger = super::UndoLedger::default();
        ledger.record(0, "Ada");
        ledger.record(1, "checked");
        ledger.record(2, "Canada");
        ledger.restored(0);
        ledger.restored(2);
        assert_eq!(ledger.entries(), &[(1, "checked".to_owned())]);
        ledger.restored(0); // A repeated completion cannot consume a different field.
        assert_eq!(ledger.entries().len(), 1);
        ledger.restored(1);
        assert!(ledger.entries().is_empty());
    }

    #[test]
    fn undo_survives_stop_after_any_write_without_including_remaining_fields() {
        for stop_after in 0..=3 {
            let mut ledger = super::UndoLedger::default();
            for (index, value) in ["Ada", "checked", "Canada"]
                .iter()
                .enumerate()
                .take(stop_after)
            {
                ledger.record(index, value);
            }
            let stopped = ledger.clone();
            assert_eq!(stopped.entries().len(), stop_after);
            assert_eq!(
                stopped
                    .entries()
                    .iter()
                    .map(|(index, _)| *index)
                    .collect::<Vec<_>>(),
                (0..stop_after).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn one_field_is_never_undone_twice() {
        let mut ledger = super::UndoLedger::default();
        ledger.record(2, "first");
        ledger.record(2, "last");
        assert_eq!(ledger.entries(), &[(2, "last".to_owned())]);
    }

    use super::*;

    fn snapshot(kind: FieldKind) -> FormSnapshot {
        FormSnapshot {
            app: "Browser".into(),
            title: "Registration".into(),
            website_host: "example.com".into(),
            fields: vec![FormField {
                id: "field-1".into(),
                label: "Name".into(),
                section: "Applicant".into(),
                kind,
                has_value: false,
                supported: true,
                options: Vec::new(),
            }],
        }
    }

    fn answer(value: &str) -> FormAnswer {
        FormAnswer {
            field_id: "field-1".into(),
            value: value.into(),
            source_ids: vec!["instruction".into()],
            kind: AnswerKind::Exact,
        }
    }

    #[test]
    fn rejects_existing_unsupported_and_sensitive_targets() {
        for mode in 0..4 {
            let mut form = snapshot(FieldKind::Text);
            match mode {
                0 => form.fields[0].has_value = true,
                1 => form.fields[0].supported = false,
                2 => {
                    form.fields[0].supported = false;
                    form.fields[0].kind = FieldKind::Unsupported;
                }
                _ => form.fields[0].label = "Credit card number".into(),
            }
            assert!(validate_answers(&form, &[answer("Mira")]).is_err());
        }
    }

    #[test]
    fn simple_value_types_reject_unsafe_or_invalid_values() {
        for (kind, good, bad) in [
            (FieldKind::Email, "mira+form@example.com", "a@@example.com"),
            (
                FieldKind::Url,
                "https://example.com/profile",
                "javascript:alert(1)",
            ),
            (
                FieldKind::Url,
                "http://example.com",
                "https://user:pass@example.com",
            ),
            (FieldKind::Url, "https://example.com", "https:example.com"),
            (FieldKind::Phone, "+91 98765 43210", "call me tomorrow"),
            (FieldKind::Number, "42.5", "NaN"),
            (FieldKind::Number, "-2", "inf"),
            (FieldKind::Date, "2024-02-29", "2025-02-29"),
            (FieldKind::Date, "2026-10-09", "2026-13-01"),
        ] {
            assert!(
                validate_answers(&snapshot(kind.clone()), &[answer(good)]).is_ok(),
                "{kind:?}"
            );
            assert_eq!(
                validate_answers(&snapshot(kind), &[answer(bad)]),
                Err("form_value_invalid".into())
            );
        }
    }

    #[test]
    fn choices_take_only_the_page_options_or_a_tick() {
        let mut form = snapshot(FieldKind::Select);
        assert_eq!(
            validate_snapshot(&form),
            Err("form_snapshot_invalid".into()),
            "a supported dropdown needs its options"
        );
        form.fields[0].options = vec!["India".into(), "Japan".into()];
        assert_eq!(validate_answers(&form, &[answer("Japan")]), Ok(()));
        let draft = FormAnswer {
            kind: AnswerKind::Draft,
            ..answer("India")
        };
        assert_eq!(validate_answers(&form, &[draft]), Ok(()));
        assert_eq!(
            validate_answers(&form, &[answer("Nippon")]),
            Err("form_value_invalid".into())
        );
        form.fields[0].kind = FieldKind::Radio;
        assert_eq!(validate_answers(&form, &[answer("India")]), Ok(()));
        form.fields[0].kind = FieldKind::Text;
        assert_eq!(
            validate_snapshot(&form),
            Err("form_snapshot_invalid".into()),
            "only dropdowns and radio groups carry options"
        );
        let mut tick = snapshot(FieldKind::Checkbox);
        assert_eq!(validate_answers(&tick, &[answer(CHECKED)]), Ok(()));
        for bad in ["yes", "true", "unchecked"] {
            assert_eq!(
                validate_answers(&tick, &[answer(bad)]),
                Err("form_value_invalid".into())
            );
        }
        tick.fields[0].kind = FieldKind::Select;
        tick.fields[0].options = vec!["x".repeat(257)];
        assert_eq!(
            validate_snapshot(&tick),
            Err("form_snapshot_invalid".into())
        );
        tick.fields[0].options = vec!["ok".into(); MAX_OPTIONS + 1];
        assert_eq!(
            validate_snapshot(&tick),
            Err("form_snapshot_invalid".into())
        );
        // A typed field still refuses drafts.
        let email = FormAnswer {
            kind: AnswerKind::Draft,
            ..answer("mira@example.com")
        };
        assert_eq!(
            validate_answers(&snapshot(FieldKind::Email), &[email]),
            Err("form_value_invalid".into())
        );
    }

    #[test]
    fn eighty_field_forms_and_answers_fit_the_shared_bound() {
        let mut form = snapshot(FieldKind::Text);
        form.fields = (0..80)
            .map(|i| FormField {
                id: format!("field-{i}"),
                ..form.fields[0].clone()
            })
            .collect();
        let answers: Vec<_> = form
            .fields
            .iter()
            .map(|field| FormAnswer {
                field_id: field.id.clone(),
                ..answer("Mira")
            })
            .collect();
        assert_eq!(validate_snapshot(&form), Ok(()));
        assert_eq!(validate_answers(&form, &answers), Ok(()));
    }

    #[test]
    fn snapshot_bounds_and_conflicting_availability_are_rejected() {
        let mut form = snapshot(FieldKind::Text);
        form.fields[0].kind = FieldKind::Unsupported;
        assert_eq!(
            validate_snapshot(&form),
            Err("form_snapshot_invalid".into())
        );
        form = snapshot(FieldKind::Text);
        form.fields = vec![form.fields[0].clone(); MAX_FIELDS + 1];
        assert_eq!(
            validate_snapshot(&form),
            Err("form_snapshot_invalid".into())
        );
    }
}
