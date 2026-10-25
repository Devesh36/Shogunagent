//! The test corpus.
//!
//! Hand-written rather than generated. A generator that produced both the payload and its label
//! would only be testing the generator; whether a string is "a credential" or "prose that looks
//! like one" is a human judgement, and the precision cases only mean anything if a person chose
//! them to be genuinely confusable.
//!
//! **Every string is synthetic.** Keys use repeated-letter bodies so no value here could ever be
//! live, while keeping the issuer prefix and the length that the detector actually keys on.

use serde::Serialize;

/// What kind of data a case carries, and therefore how to read its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// A credential the detector is built to catch.
    TargetedSecret,
    /// A credential the audit predicted it misses (PII-003).
    UntargetedSecret,
    /// Personal data. The DB path does not target this **by design** (PII-002).
    PersonalData,
    /// A national ID or payment card: personal data with **no legitimate product use**.
    ///
    /// Split out from [`Category::PersonalData`] because the two get opposite treatment at the
    /// egress boundary. Masking an email on the way out would break nothing but would also
    /// protect nothing the DB has not already stored; masking a card number protects a value
    /// nothing downstream ever needed. The DB path still leaves both alone — see
    /// [`expectation_at`].
    HighRiskIdentifier,
    /// Ordinary text that merely resembles a credential. Masking it is the failure.
    LookAlike,
    /// A targeted secret, obfuscated. Tests robustness rather than coverage.
    Obfuscated,
}

/// What should happen, given the design as documented — not as we might wish it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Expectation {
    /// Must be masked. Surviving is a defect.
    Masked,
    /// Documented as out of scope on the DB path. Surviving is the designed behaviour, and is
    /// reported as a characterisation rather than a failure.
    SurvivesByDesign,
    /// Must pass through untouched. Masking it corrupts real memory.
    Untouched,
}

/// One payload plus the needle that proves whether it survived.
#[derive(Debug, Clone)]
pub struct Case {
    pub id: &'static str,
    pub category: Category,
    pub expectation: Expectation,
    /// The text written into the column under test.
    pub payload: &'static str,
    /// The fragment whose presence means the sensitive value survived.
    ///
    /// Deliberately not the whole payload: partial masking still leaks, and a substring check is
    /// what catches "masked the prefix, kept the body".
    pub needle: &'static str,
}

pub const CASES: &[Case] = &[
    // Targeted secrets: the detector's declared job
    Case {
        id: "anthropic-key",
        category: Category::TargetedSecret,
        expectation: Expectation::Masked,
        payload: "rotating the key sk-ant-api03-AAAABBBBCCCCDDDDEEEEFFFF before the demo",
        needle: "sk-ant-api03-AAAABBBBCCCCDDDDEEEEFFFF",
    },
    Case {
        id: "openai-key",
        category: Category::TargetedSecret,
        expectation: Expectation::Masked,
        payload: "export OPENAI_API_KEY=sk-AAAABBBBCCCCDDDDEEEEFFFFGGGG",
        needle: "sk-AAAABBBBCCCCDDDDEEEEFFFFGGGG",
    },
    Case {
        id: "github-pat",
        category: Category::TargetedSecret,
        expectation: Expectation::Masked,
        payload: "token ghp_AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHH0000",
        needle: "ghp_AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHH0000",
    },
    Case {
        id: "slack-bot-token",
        category: Category::TargetedSecret,
        expectation: Expectation::Masked,
        payload: "webhook uses xoxb-000011112222-AAAABBBBCCCC",
        needle: "xoxb-000011112222-AAAABBBBCCCC",
    },
    Case {
        id: "aws-access-key-id",
        category: Category::TargetedSecret,
        expectation: Expectation::Masked,
        payload: "AKIAIOSFODNN7EXAMPLE is the access key id",
        needle: "AKIAIOSFODNN7EXAMPLE",
    },
    Case {
        id: "google-api-key",
        category: Category::TargetedSecret,
        expectation: Expectation::Masked,
        payload: "maps key AIzaAAAABBBBCCCCDDDDEEEEFFFFGGGG",
        needle: "AIzaAAAABBBBCCCCDDDDEEEEFFFFGGGG",
    },
    Case {
        id: "labelled-password",
        category: Category::TargetedSecret,
        expectation: Expectation::Masked,
        payload: "password=correcthorsebatterystaple",
        needle: "correcthorsebatterystaple",
    },
    Case {
        id: "authorization-bearer",
        category: Category::TargetedSecret,
        expectation: Expectation::Masked,
        payload: "authorization: Bearer AAAABBBBCCCCDDDDEEEEFFFF",
        needle: "AAAABBBBCCCCDDDDEEEEFFFF",
    },
    Case {
        id: "url-with-labelled-token",
        category: Category::TargetedSecret,
        expectation: Expectation::Masked,
        payload: "callback https://example.test/cb?access_token=AAAABBBBCCCCDDDD",
        needle: "AAAABBBBCCCCDDDD",
    },
    // Secrets the audit predicted are missed (PII-003)
    Case {
        id: "stripe-style-key",
        category: Category::UntargetedSecret,
        expectation: Expectation::Masked,
        // `sk_live_` uses an underscore; the listed prefix is `sk-`, so this is predicted to miss.
        // Split with concat! so hosting-side secret scanners do not read the synthetic fixture as a
        // live key; the bench sees the same joined string.
        payload: concat!("billing key sk_live", "_AAAABBBBCCCCDDDDEEEEFFFF"),
        needle: concat!("sk_live", "_AAAABBBBCCCCDDDDEEEEFFFF"),
    },
    Case {
        id: "unlabelled-high-entropy",
        category: Category::UntargetedSecret,
        expectation: Expectation::Masked,
        payload: "paste this: AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHH",
        needle: "AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHH",
    },
    Case {
        id: "short-password",
        category: Category::UntargetedSecret,
        expectation: Expectation::Masked,
        // Under MIN_SECRET_LEN (12) — predicted to miss, by a documented trade.
        payload: "password=hunter2",
        needle: "hunter2",
    },
    Case {
        id: "private-key-header",
        category: Category::UntargetedSecret,
        expectation: Expectation::Masked,
        payload: "-----BEGIN RSA PRIVATE KEY-----\nAAAABBBBCCCCDDDDEEEE\n-----END RSA PRIVATE KEY-----",
        needle: "AAAABBBBCCCCDDDDEEEE",
    },
    // Personal data: out of scope on the DB path, by design (PII-002)
    Case {
        id: "email",
        category: Category::PersonalData,
        expectation: Expectation::SurvivesByDesign,
        payload: "contact alice.smith@example.test about the renewal",
        needle: "alice.smith@example.test",
    },
    Case {
        id: "phone",
        category: Category::PersonalData,
        expectation: Expectation::SurvivesByDesign,
        payload: "call +1 415 555 0132 tomorrow",
        needle: "415 555 0132",
    },
    Case {
        id: "national-id",
        category: Category::PersonalData,
        expectation: Expectation::SurvivesByDesign,
        payload: "SSN 123-45-6789 on the onboarding form",
        needle: "123-45-6789",
    },
    Case {
        id: "payment-card",
        category: Category::PersonalData,
        expectation: Expectation::SurvivesByDesign,
        payload: "card 4111 1111 1111 1111 exp 03/29",
        needle: "4111 1111 1111 1111",
    },
    Case {
        id: "postal-address",
        category: Category::PersonalData,
        expectation: Expectation::SurvivesByDesign,
        payload: "ship to 221B Baker Street, London NW1 6XE",
        needle: "221B Baker Street",
    },
    // Look-alikes: masking these is the failure
    Case {
        id: "hyphenated-word",
        category: Category::LookAlike,
        expectation: Expectation::Untouched,
        payload: "the task-management board was updated this morning",
        needle: "task-management",
    },
    Case {
        id: "region-heading",
        category: Category::LookAlike,
        expectation: Expectation::Untouched,
        payload: "ASIA-PACIFIC-2026 planning kickoff",
        needle: "ASIA-PACIFIC-2026",
    },
    Case {
        id: "uuid",
        category: Category::LookAlike,
        expectation: Expectation::Untouched,
        payload: "run id 550e8400-e29b-41d4-a716-446655440000 finished",
        needle: "550e8400-e29b-41d4-a716-446655440000",
    },
    Case {
        id: "git-sha",
        category: Category::LookAlike,
        expectation: Expectation::Untouched,
        payload: "merged at d418ef5a72042c966145126386f2e62611c511dd",
        needle: "d418ef5a72042c966145126386f2e62611c511dd",
    },
    Case {
        id: "file-path",
        category: Category::LookAlike,
        expectation: Expectation::Untouched,
        payload: "see /Users/dana/Projects/kestrel/docs/architecture.md",
        needle: "/Users/dana/Projects/kestrel",
    },
    Case {
        id: "prose-with-token-word",
        category: Category::LookAlike,
        expectation: Expectation::Untouched,
        payload: "the design token naming convention needs a decision",
        needle: "naming convention",
    },
    // Obfuscation: robustness of a targeted secret
    Case {
        id: "zwsp-split-key",
        category: Category::Obfuscated,
        expectation: Expectation::Masked,
        // strip_hidden runs before redact specifically so this still matches.
        payload: "key sk-ant-\u{200b}api03-AAAABBBBCCCCDDDD here",
        needle: "api03-AAAABBBBCCCCDDDD",
    },
    Case {
        id: "ocr-corrupted-prefix",
        category: Category::Obfuscated,
        expectation: Expectation::Masked,
        // OCR reads "sk-" as "s k-". Predicted to defeat the prefix match.
        payload: "key s k-ant-api03-AAAABBBBCCCCDDDD",
        needle: "ant-api03-AAAABBBBCCCCDDDD",
    },
    Case {
        id: "uppercased-prefix",
        category: Category::Obfuscated,
        expectation: Expectation::Masked,
        payload: "key SK-ANT-API03-AAAABBBBCCCCDDDD",
        needle: "SK-ANT-API03-AAAABBBBCCCCDDDD",
    },
    Case {
        id: "mixed-case-prefix",
        category: Category::Obfuscated,
        expectation: Expectation::Masked,
        payload: "pasted Sk-Ant-Api03-AAAABBBBCCCCDDDD from the wiki",
        needle: "Sk-Ant-Api03-AAAABBBBCCCCDDDD",
    },
    Case {
        id: "uppercased-github-pat",
        category: Category::Obfuscated,
        expectation: Expectation::Masked,
        payload: "token GHP_AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHH0000",
        needle: "GHP_AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHH0000",
    },

    // National IDs and payment cards
    //
    // `expectation` here is the DB path's behaviour, which is to leave them alone. The egress
    // boundary expects them masked; `expectation_at` applies that split.
    Case {
        id: "payment-card",
        category: Category::HighRiskIdentifier,
        expectation: Expectation::SurvivesByDesign,
        // Luhn-valid test number, never issued to anyone.
        payload: "charged the renewal to 4111111111111111 this morning",
        needle: "4111111111111111",
    },
    Case {
        id: "payment-card-spaced",
        category: Category::HighRiskIdentifier,
        expectation: Expectation::SurvivesByDesign,
        payload: "card on file 4111 1111 1111 1111 expires soon",
        needle: "4111 1111 1111 1111",
    },
    Case {
        id: "amex-card",
        category: Category::HighRiskIdentifier,
        expectation: Expectation::SurvivesByDesign,
        payload: "the amex 378282246310005 was declined",
        needle: "378282246310005",
    },
    Case {
        id: "us-ssn",
        category: Category::HighRiskIdentifier,
        expectation: Expectation::SurvivesByDesign,
        payload: "onboarding form lists 123-45-6789 for payroll",
        needle: "123-45-6789",
    },
    Case {
        id: "my-number",
        category: Category::HighRiskIdentifier,
        expectation: Expectation::SurvivesByDesign,
        payload: "マイナンバー 123456789012 を入力してください",
        needle: "123456789012",
    },
    Case {
        id: "passport-number",
        category: Category::HighRiskIdentifier,
        expectation: Expectation::SurvivesByDesign,
        payload: "passport: X1234567 expires next year",
        needle: "X1234567",
    },
    // Japanese running text puts no space between a label or a number and the words around it.
    // Boundaries are ASCII boundaries, so a kana or kanji neighbour does not hide the next four
    // from the egress detectors.
    Case {
        id: "my-number-cjk-adjacent",
        category: Category::HighRiskIdentifier,
        expectation: Expectation::SurvivesByDesign,
        payload: "私のマイナンバー123456789012です",
        needle: "123456789012",
    },
    Case {
        id: "my-number-after-kanji-label",
        category: Category::HighRiskIdentifier,
        expectation: Expectation::SurvivesByDesign,
        payload: "申請者個人番号：123456789012",
        needle: "123456789012",
    },
    Case {
        id: "payment-card-cjk-adjacent",
        category: Category::HighRiskIdentifier,
        expectation: Expectation::SurvivesByDesign,
        payload: "カード番号4111111111111111です",
        needle: "4111111111111111",
    },
    Case {
        id: "us-ssn-cjk-adjacent",
        category: Category::HighRiskIdentifier,
        expectation: Expectation::SurvivesByDesign,
        payload: "社会保障番号123-45-6789です",
        needle: "123-45-6789",
    },
    Case {
        id: "payment-card-fullwidth",
        category: Category::HighRiskIdentifier,
        expectation: Expectation::SurvivesByDesign,
        // Full-width digits, as a Japanese IME types them.
        payload: "カード番号　４１１１１１１１１１１１１１１１　で決済",
        needle: "４１１１１１１１１１１１１１１１",
    },
    Case {
        id: "my-number-printed-groups",
        category: Category::HighRiskIdentifier,
        expectation: Expectation::SurvivesByDesign,
        // The 4-4-4 grouping on the card itself.
        payload: "マイナンバー 1234 5678 9012 を控えた",
        needle: "1234 5678 9012",
    },
    Case {
        id: "us-ssn-spaced",
        category: Category::HighRiskIdentifier,
        expectation: Expectation::SurvivesByDesign,
        payload: "payroll lists 123 45 6789 for Dana",
        needle: "123 45 6789",
    },

    // Look-alikes for the identifier detectors
    //
    // These are the precision half. A Luhn/SSN detector that masks these is worse than no
    // detector, because it removes text the product legitimately holds.
    Case {
        id: "non-luhn-digit-run",
        category: Category::LookAlike,
        expectation: Expectation::Untouched,
        // One digit off the card above, so the checksum fails.
        payload: "order reference 4111111111111112 shipped",
        needle: "4111111111111112",
    },
    Case {
        id: "epoch-millis",
        category: Category::LookAlike,
        expectation: Expectation::Untouched,
        payload: "build finished at 1727712000000 utc",
        needle: "1727712000000",
    },
    Case {
        id: "reserved-ssn-range",
        category: Category::LookAlike,
        expectation: Expectation::Untouched,
        // 666 is never issued, so this is a part number, not an SSN.
        payload: "part 666-45-6789 is on back order",
        needle: "666-45-6789",
    },
    Case {
        id: "passport-prose",
        category: Category::LookAlike,
        expectation: Expectation::Untouched,
        payload: "renewing my passport application this week",
        needle: "passport application",
    },
    // The precision side of the separator and width rules added with the CJK boundary fix.
    Case {
        id: "float-timestamp",
        category: Category::LookAlike,
        expectation: Expectation::Untouched,
        // 16 Luhn-valid digits once the dot is dropped; no card has a 13-digit group.
        payload: "measured 1727712000000.000 seconds elapsed",
        needle: "1727712000000.000",
    },
    Case {
        id: "number-column",
        category: Category::LookAlike,
        expectation: Expectation::Untouched,
        // A 3-2-4 run inside a row of figures is a table, not an SSN.
        payload: "totals 2026 123 45 6789 10 end of sheet",
        needle: "123 45 6789",
    },
    Case {
        id: "fullwidth-phone",
        category: Category::LookAlike,
        expectation: Expectation::Untouched,
        // Full-width digits that are not an identifier come back exactly as typed.
        payload: "電話　０３－１２３４－５６７８　まで",
        needle: "０３－１２３４－５６７８",
    },
    Case {
        id: "ascii-glued-card-digits",
        category: Category::LookAlike,
        expectation: Expectation::Untouched,
        // An ASCII letter glued to a Luhn-valid run keeps it an identifier.
        payload: "artifact ab4111111111111111cd uploaded",
        needle: "ab4111111111111111cd",
    },
];

/// What a case should do **at a given boundary**.
///
/// The DB path and the egress path do not have the same job, so a single expectation per case
/// cannot describe both. A card number must survive the write to `event_log` — masking personal
/// data on the way into the store is what PII-002 rules out — and must not survive the handoff to
/// Composio or Deepgram. Reading one number for both boundaries would have hidden exactly that.
pub fn expectation_at(case: &Case, egress: bool) -> Expectation {
    match (case.category, egress) {
        (Category::HighRiskIdentifier, true) => Expectation::Masked,
        (Category::HighRiskIdentifier, false) => Expectation::SurvivesByDesign,
        _ => case.expectation,
    }
}

impl Case {
    /// Whether surviving this case should be reported as a defect.
    ///
    /// `SurvivesByDesign` is deliberately excluded: counting documented behaviour as a failure
    /// would inflate the leak rate with cases the product never claimed to handle.
    pub fn survival_is_a_defect(&self) -> bool {
        matches!(self.expectation, Expectation::Masked)
    }
}
