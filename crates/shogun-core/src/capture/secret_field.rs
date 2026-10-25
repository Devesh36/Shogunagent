//! Which accessibility text fields are secret and must never be read (issue #283 OPEN-283-02).
//!
//! `AXSecureTextField` (as a role, or as the subrole AppKit and browsers put on a password
//! `AXTextField`) was always skipped with its whole subtree. But a plain text field can hold a
//! secret just as well: a card number, a CVC, a one-time code, a PIN, or a password field a web
//! page built without `type=password`. Those announce themselves only through their labelling —
//! the placeholder, title, description or identifier. This module decides from that labelling,
//! purely, so it is unit-tested on Linux; every macOS AX capture path asks it before reading a
//! field's value and treats a "secret" answer exactly like a secure field.
//!
//! The decision is deliberately conservative in one direction only: a false positive loses one
//! field's text from capture, a false negative stores a secret. Hints are still matched as whole
//! words (so "pinned", "spinner", "shipping" and "opinion" never match "pin", and "pass" alone
//! never matches) to keep ordinary fields — a search box, a "Cc" line, a chat composer — readable.
//!
//! The hint text itself is never logged or returned; callers get a boolean.

/// The role and subrole Apple and browsers use for password inputs.
pub const SECURE_TEXT_FIELD: &str = "AXSecureTextField";

/// Labelling attributes consulted for a text-field-like element, in the order worth reading
/// (the placeholder is the most telling and a positive stops the reads early). `AXDOMIdentifier`
/// is the HTML `id` Chromium-based browsers expose; `AXIdentifier` is the native equivalent.
pub const SECRET_HINT_ATTRIBUTES: [&str; 5] = [
    "AXPlaceholderValue",
    "AXTitle",
    "AXDescription",
    "AXIdentifier",
    "AXDOMIdentifier",
];

/// Only this much of each hint is inspected. Labels are short; a long string is content, not a
/// label, and scanning it would both cost time and invite false positives.
const MAX_HINT_CHARS: usize = 256;

/// Roles whose labelling attributes are worth reading. Restricting the extra reads to these keeps
/// the capture walk's cost flat: static text, links and containers never pay for them.
pub fn reads_secret_hints(role: &str) -> bool {
    matches!(
        role,
        "AXTextField" | "AXTextArea" | "AXComboBox" | "AXSearchField"
    )
}

/// Whether an element must be treated like a password field: its value is not read and its whole
/// subtree is skipped.
///
/// `hints` are the element's labelling strings (see [`SECRET_HINT_ATTRIBUTES`]). They are only
/// consulted for text-field-like roles ([`reads_secret_hints`]) and are consumed lazily, so an
/// adapter can pass an iterator that reads each attribute on demand and stops at the first hit.
pub fn is_secret_field<S: AsRef<str>>(
    role: &str,
    subrole: Option<&str>,
    hints: impl IntoIterator<Item = S>,
) -> bool {
    if role == SECURE_TEXT_FIELD || subrole == Some(SECURE_TEXT_FIELD) {
        return true;
    }
    reads_secret_hints(role)
        && hints
            .into_iter()
            .any(|hint| hint_names_secret(hint.as_ref()))
}

/// Long, unambiguous words matched anywhere in the lowercased hint (so identifiers such as
/// `newpassword` or `user-passcode-input` match too). Japanese has no word boundaries, so its
/// hints live here as well; each is a compound specific enough not to occur by accident.
const SUBSTRING_HINTS: &[&str] = &[
    "password",
    "passcode",
    "passphrase",
    "passwort",
    "kennwort",
    "contraseña",
    "contrasena",
    "mot de passe",
    // Japanese
    "パスワード",
    "パスコード",
    "暗証番号",
    "確認コード",
    "認証コード",
    "認証番号",
    "ワンタイムコード",
    "カード番号",
    "セキュリティコード",
    "口座番号",
    "マイナンバー",
    "個人番号",
    "秘密鍵",
];

/// Whole-word hints, as token sequences. A multi-word hint also matches its words run together
/// as one token (`cardnumber`, `securitycode`), which is how form identifiers spell them.
const WORD_HINTS: &[&[&str]] = &[
    &["passwd"],
    &["pin"],
    &["otp"],
    &["totp"],
    &["2", "fa"],
    &["mfa", "code"],
    &["one", "time", "code"],
    &["verification", "code"],
    &["authentication", "code"],
    &["authenticator", "code"],
    &["security", "code"],
    &["recovery", "code"],
    &["backup", "code"],
    &["card", "number"],
    &["credit", "card"],
    &["debit", "card"],
    &["cc", "number"],
    &["cvv"],
    &["cvc"],
    &["csc"],
    &["ssn"],
    &["social", "security"],
    &["iban"],
    &["routing", "number"],
    &["account", "number"],
    &["api", "key"],
    &["secret", "key"],
    &["private", "key"],
    &["client", "secret"],
    &["recovery", "phrase"],
    &["seed", "phrase"],
    &["senha"],
];

/// Whether one labelling string names a secret. Case-insensitive; full-width ASCII (common in
/// Japanese UIs, e.g. `ＰＩＮ`) is folded to ASCII first.
pub fn hint_names_secret(hint: &str) -> bool {
    let folded: String = hint
        .chars()
        .take(MAX_HINT_CHARS)
        .map(fold_full_width)
        .collect();
    let lower = folded.to_lowercase();
    if SUBSTRING_HINTS.iter().any(|needle| lower.contains(needle)) {
        return true;
    }
    let words = ascii_words(&folded);
    WORD_HINTS.iter().any(|hint| {
        let joined = hint.concat();
        words.contains(&joined)
            || words.windows(hint.len()).any(|window| {
                window
                    .iter()
                    .zip(hint.iter())
                    .all(|(word, part)| word == part)
            })
    })
}

/// Full-width ASCII forms (U+FF01–U+FF5E) and the ideographic space become plain ASCII.
fn fold_full_width(c: char) -> char {
    match c {
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        '\u{3000}' => ' ',
        _ => c,
    }
}

/// Lowercased ASCII words. Anything that is not an ASCII letter or digit separates words (so a
/// Japanese label like `PINコード` still yields `pin`), and so do camelCase humps (`cardNumber`),
/// the end of an acronym before a capitalised word (`PINCode` → `pin`, `code`) and letter/digit
/// changes (`cvv2` → `cvv`, `2`).
fn ascii_words(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut words = Vec::new();
    let mut current = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_ascii_alphanumeric() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        if !current.is_empty() {
            let prev = chars[i - 1];
            let next_is_lower = chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
            let boundary = (prev.is_ascii_lowercase() && c.is_ascii_uppercase())
                || (prev.is_ascii_digit() != c.is_ascii_digit())
                || (prev.is_ascii_uppercase() && c.is_ascii_uppercase() && next_is_lower);
            if boundary {
                words.push(std::mem::take(&mut current));
            }
        }
        current.push(c.to_ascii_lowercase());
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret_text_field(hint: &str) -> bool {
        is_secret_field("AXTextField", None, [hint])
    }

    #[test]
    fn secure_role_and_subrole_are_always_secret() {
        assert!(is_secret_field(
            "AXSecureTextField",
            None,
            std::iter::empty::<&str>()
        ));
        // AppKit and browsers expose password inputs as AXTextField + this subrole.
        assert!(is_secret_field(
            "AXTextField",
            Some("AXSecureTextField"),
            std::iter::empty::<&str>()
        ));
        assert!(is_secret_field(
            "AXGroup",
            Some("AXSecureTextField"),
            ["Search"]
        ));
    }

    #[test]
    fn english_password_hints_match_case_insensitively() {
        for hint in [
            "Password",
            "PASSWORD",
            "Enter your password",
            "Current password",
            "Confirm Password",
            "Passcode",
            "Passphrase",
            "passwd",
            "One-time password",
        ] {
            assert!(secret_text_field(hint), "{hint}");
        }
    }

    #[test]
    fn pin_and_one_time_code_hints_match() {
        for hint in [
            "PIN",
            "Pin",
            "Enter PIN",
            "PIN code",
            "pin-code",
            "PINCode",
            "pinCode",
            "pin1",
            "OTP",
            "otp_input",
            "otpInput",
            "TOTP",
            "One-time code",
            "one_time_code",
            "Verification code",
            "verificationCode",
            "Authentication code",
            "Security code",
            "2FA code",
            "Backup code",
        ] {
            assert!(secret_text_field(hint), "{hint}");
        }
    }

    #[test]
    fn payment_and_identity_hints_match() {
        for hint in [
            "Card number",
            "cardNumber",
            "cardnumber",
            "card-number",
            "Credit card",
            "Debit card number",
            "cc-number",
            "CVV",
            "cvv2",
            "CVC",
            "cc-csc",
            "Security code (CVC)",
            "SSN",
            "ssnLast4",
            "Social Security Number",
            "IBAN",
            "Routing number",
            "Account number",
            "accountNumber",
            "API key",
            "apiKey",
            "Client secret",
            "Recovery phrase",
        ] {
            assert!(secret_text_field(hint), "{hint}");
        }
    }

    #[test]
    fn japanese_hints_match() {
        for hint in [
            "パスワード",
            "パスワードを入力",
            "ワンタイムパスワード",
            "暗証番号（4桁）",
            "確認コード",
            "認証コードを入力してください",
            "カード番号",
            "クレジットカード番号",
            "セキュリティコード",
            "PINコード",
            "ＰＩＮ",
            "ＣＶＣ",
            "口座番号",
            "マイナンバー",
        ] {
            assert!(secret_text_field(hint), "{hint}");
        }
    }

    #[test]
    fn a_few_other_languages_match() {
        for hint in [
            "Passwort",
            "Kennwort",
            "Contraseña",
            "Mot de passe",
            "Senha",
        ] {
            assert!(secret_text_field(hint), "{hint}");
        }
    }

    #[test]
    fn ordinary_words_containing_hint_letters_do_not_match() {
        for hint in [
            "Pinned messages",
            "spinner",
            "Shipping address",
            "Your opinion",
            "Pinyin",
            "Search Pinterest",
            "Search pins",
            "pass",
            "Boarding pass",
            "user_pass",
            "Passport",
            "Passenger name",
            "Bypass",
            "Compass",
            "Cc",
            "Bcc",
            "Code",
            "Card title",
            "Account",
            "Account name",
            "Security",
            "Verification",
            "Credit",
            "Topic",
            "Scope",
            "Keyboard",
            "Private notes",
            "Secret Santa wishlist",
            "Message #general",
            "Search",
            "コード",
            "カード名",
            "番号",
            "メッセージを入力",
        ] {
            assert!(!secret_text_field(hint), "{hint}");
        }
    }

    #[test]
    fn hints_are_ignored_for_roles_that_are_not_text_fields() {
        // A static label or a button saying "Password" is ordinary visible text, not a field.
        for role in [
            "AXStaticText",
            "AXButton",
            "AXGroup",
            "AXLink",
            "AXWebArea",
            "",
        ] {
            assert!(!is_secret_field(role, None, ["Password"]), "{role}");
        }
        for role in ["AXTextField", "AXTextArea", "AXComboBox", "AXSearchField"] {
            assert!(is_secret_field(role, None, ["Password"]), "{role}");
        }
    }

    #[test]
    fn any_one_hint_is_enough_and_absent_hints_are_not() {
        assert!(is_secret_field(
            "AXTextField",
            None,
            ["", "Billing", "card-number"]
        ));
        assert!(!is_secret_field(
            "AXTextField",
            None,
            ["Name", "Email", "firstName"]
        ));
        assert!(!is_secret_field(
            "AXTextField",
            None,
            std::iter::empty::<String>()
        ));
    }

    #[test]
    fn hints_are_read_lazily_and_stop_at_the_first_hit() {
        let mut reads = 0;
        let hints = ["Password", "unreached", "unreached"]
            .into_iter()
            .inspect(|_| reads += 1);
        assert!(is_secret_field("AXTextField", None, hints));
        assert_eq!(reads, 1);

        // Non-text-field roles never read a hint at all.
        let mut reads = 0;
        let hints = ["Password"].into_iter().inspect(|_| reads += 1);
        assert!(!is_secret_field("AXStaticText", None, hints));
        assert_eq!(reads, 0);
    }

    #[test]
    fn only_the_head_of_a_long_hint_is_inspected() {
        let long = format!("{} password", "x ".repeat(MAX_HINT_CHARS));
        assert!(!secret_text_field(&long));
        assert!(secret_text_field(&format!(
            "password {}",
            "x ".repeat(MAX_HINT_CHARS)
        )));
    }

    #[test]
    fn hint_attributes_are_labelling_attributes_not_the_value() {
        assert!(!SECRET_HINT_ATTRIBUTES.contains(&"AXValue"));
        assert!(!SECRET_HINT_ATTRIBUTES.contains(&"AXSelectedText"));
        assert_eq!(SECRET_HINT_ATTRIBUTES[0], "AXPlaceholderValue");
    }

    #[test]
    fn word_splitting_follows_camel_case_acronyms_and_digits() {
        assert_eq!(ascii_words("PINCode"), ["pin", "code"]);
        assert_eq!(
            ascii_words("cardNumber cvv2"),
            ["card", "number", "cvv", "2"]
        );
        assert_eq!(ascii_words("PINコード"), ["pin"]);
        assert_eq!(ascii_words("user_pass-field"), ["user", "pass", "field"]);
    }
}
