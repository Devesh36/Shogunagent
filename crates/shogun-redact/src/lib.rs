//! Secret redaction — the pure logic shared by the DB write path and the diagnostic-log path.
//!
//! Everything SHOGUN reads gets stored, and screens and AI-tool transcripts routinely contain
//! credentials — a key pasted into a terminal, a token in a config file on screen, an
//! `Authorization` header in a response. Storing those verbatim would turn the memory database
//! into a credential store, so recognisable secrets are masked *before* the row is written (see
//! [`redact`]). The database only ever holds the masked form; there is no "raw" copy to leak later.
//!
//! [`redact_log`] is the sibling used on the log / error-report path: it additionally masks whole
//! email addresses and full URLs, which must NOT be stripped on the DB path (see its docs).
//!
//! Two deliberate limits:
//!
//! * This is **not** a security boundary — it cannot catch a secret with no recognisable shape.
//!   It reduces exposure; encryption at rest is what actually protects the file.
//! * It is tuned to **avoid false positives**. Mangling real text would corrupt the memory the
//!   product is built on, so a pattern is only matched when it is specifically credential-shaped
//!   (a known issuer prefix, or a value that follows a key/token/password label).
//!
//! Pure and dependency-free: hand-rolled scanning rather than a regex engine, so it is exhaustively
//! testable and adds nothing to the build. Lives in its own crate so it can be shared without
//! pulling in rusqlite/sqlcipher (which shogun-memory carries).
//!
//! [`strip_hidden`] is the sibling on the same write path: hidden format / bidi / noncharacters
//! are removed *before* [`redact`] so a secret split by ZWSP still matches, and so those runes
//! never land in `event_log.content`.
//!
//! [`fence_untrusted`] is the prompt-side sibling: untrusted text is wrapped so a model cannot
//! treat it as instructions (Agent drafts, Batch Classify / Summarize, tool_result). Marker-shaped
//! text inside the data is defanged first ([`neutralize_context_markers`]) so it cannot close the
//! fence early.
//!
//! [`instruction_shaped`] is the persist-gate sibling: classifier / summarizer output that is
//! an instruction *to the assistant* is dropped before it becomes a state row, day-summary, or
//! Recap decision. Deterministic. No LLM on this path.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod hidden;
pub use hidden::{is_hidden, strip_hidden, HiddenStrip};

pub mod fence;
pub use fence::{fence_untrusted, neutralize_context_markers};

pub mod instruction;
pub use instruction::{dropped_count, instruction_shaped, reject_instruction_shaped};

mod identifiers;

// The two role halves of `fence_untrusted`, for backends that can separate them (#667).
pub use fence::{fence_data, fence_instruction};

/// What replaces a matched secret. Fixed-width so the shape of the surrounding text survives.
const MASK: &str = "[redacted]";

/// Issuer prefixes that are unambiguous on sight. Each entry is the literal prefix; the run of
/// secret-ish characters that follows it is masked with it.
const ISSUER_PREFIXES: &[&str] = &[
    "sk-ant-",         // Anthropic
    "sk-",             // OpenAI and friends
    "ghp_",            // GitHub personal access token
    "gho_",            // GitHub OAuth
    "ghu_",            //
    "ghs_",            // GitHub server-to-server
    "ghr_",            //
    "github_pat_",     // GitHub fine-grained PAT
    "xoxb-",           // Slack bot
    "xoxp-",           // Slack user
    "xoxa-",           //
    "xoxs-",           //
    "AKIA",            // AWS access key id
    "ASIA",            // AWS temporary
    "AIza",            // Google API key
    "ya29.",           // Google OAuth access token
    "glpat-",          // GitLab PAT
    "sq0atp-",         // Square
    "sq0csp-",         //
    "shpat_",          // Shopify
    "SG.",             // SendGrid
    "GOCSPX-",         // Google OAuth client secret
];

/// Issuer prefixes whose letter case is fixed by the issuer's own format, matched exactly.
///
/// Every other prefix is matched case-insensitively: a user pasting `SK-ANT-...` from a wiki or a
/// shouted header is handing over the same live credential as the lowercase form, and until this
/// list existed the uppercase spelling went to disk verbatim (`pii-bench`, PII-004).
///
/// These six stay exact because case is the only thing separating them from ordinary words.
/// "ASIA" opens a heading and "SG." abbreviates Singapore; matching them loosely would mask
/// running prose. A false mask in the DB path destroys memory the product is built on, which is
/// the more expensive of the two errors.
const CASE_EXACT_PREFIXES: &[&str] = &["AKIA", "ASIA", "AIza", "ya29.", "SG.", "GOCSPX-"];

/// Does `rest` open with `prefix`, at the case sensitivity that prefix demands?
fn prefix_matches(rest: &str, prefix: &str) -> bool {
    if CASE_EXACT_PREFIXES.contains(&prefix) {
        rest.starts_with(prefix)
    } else {
        starts_with_ignore_case(rest, prefix)
    }
}

/// Labels whose following value is a secret (`api_key: …`, `password=…`, `Authorization: Bearer …`).
const LABELS: &[&str] = &[
    "api_key", "api-key", "apikey", "access_token", "access-token", "accesstoken",
    "refresh_token", "refresh-token", "client_secret", "client-secret", "secret_key",
    "secret-key", "password", "passwd", "authorization", "auth_token", "auth-token",
    "private_key", "private-key", "token", "secret",
];

/// Shortest run that counts as a secret value. Below this it is more likely a placeholder or a
/// word than a credential, and masking it would do more harm than good.
const MIN_SECRET_LEN: usize = 12;

/// Characters that can appear inside a token value.
fn is_secret_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '/' | '=' | '~')
}

/// Mask recognisable secrets in `text`.
///
/// Returns the input unchanged (no allocation) when nothing matched, which is the common case for
/// ordinary captured prose.
pub fn redact(text: &str) -> std::borrow::Cow<'_, str> {
    // Cheap pre-check so the ordinary path costs one scan and no allocation.
    if !might_contain_secret(text) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < text.len() {
        if !text.is_char_boundary(i) {
            // Never split a codepoint: copy the byte through and move on.
            out.push_str(&text[i..i + 1]);
            i += 1;
            continue;
        }
        let rest = &text[i..];

        // 1. A known issuer prefix: mask the prefix and the value that follows it. Only at a
        //    token boundary — the "sk-" inside "task-management" is prose, not a key.
        //    Only an ALPHANUMERIC neighbour makes it prose: the separators in `is_secret_char`
        //    (`=`, `/`, `.`, `-`, `_`, `+`) are exactly what precedes a real key in
        //    `MYVAR=sk-…`, `?k=sk-…`, `/keys/sk-…`, so testing those would un-mask live secrets.
        let at_token_boundary = i == 0
            || !text[..i].chars().next_back().map(char::is_alphanumeric).unwrap_or(false);
        if at_token_boundary {
            if let Some(p) = ISSUER_PREFIXES.iter().find(|p| prefix_matches(rest, p)) {
                let tail = &rest[p.len()..];
                // AWS access key ids are strictly uppercase alphanumeric after the prefix; a
                // hyphenated heading like "ASIA-PACIFIC-2026" must not read as a credential.
                let value_len = if *p == "AKIA" || *p == "ASIA" {
                    tail.char_indices()
                        .find(|(_, c)| !c.is_ascii_alphanumeric())
                        .map(|(idx, _)| idx)
                        .unwrap_or(tail.len())
                } else {
                    run_len(tail)
                };
                if p.len() + value_len >= MIN_SECRET_LEN {
                    out.push_str(MASK);
                    i += p.len() + value_len;
                    continue;
                }
            }
        }

        // 2. A labelled value: `token: …`, `password=…`, `Authorization: Bearer …`.
        if let Some(consumed) = match_labelled(rest) {
            out.push_str(&rest[..consumed.label_end]);
            out.push_str(MASK);
            i += consumed.total;
            continue;
        }

        // Not a match: copy this character.
        let ch_len = rest.chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&rest[..ch_len]);
        i += ch_len;
    }
    std::borrow::Cow::Owned(out)
}

/// Redact for the **log / error-report path** (design decision ②). In addition to the DB
/// redactor's issuer-prefix and labelled-value masking, this also masks whole email addresses and
/// full URLs (query string included). It must NOT be used on capture content bound for the DB:
/// `people.emails` and captured prose legitimately contain emails, and masking them there would
/// corrupt the memory the product is built on. Logs are diagnostic, not memory — there, an email
/// or a URL with a token in the query is pure exposure with no upside.
pub fn redact_log(text: &str) -> std::borrow::Cow<'_, str> {
    // Emails and URLs (log-only), then the shared secret redactor, then the national-ID and
    // payment-card backstop. A log is diagnostic rather than memory, so the argument that keeps
    // `redact` from masking personal data does not apply here: nothing downstream reads an ID
    // number out of a log, and leaving one there is pure exposure.
    let stage1 = mask_emails_and_urls(text);
    let stage2 = redact(&stage1);
    let stage3 = identifiers::mask_high_risk_identifiers(&stage2);
    if matches!(stage1, std::borrow::Cow::Borrowed(_))
        && matches!(stage2, std::borrow::Cow::Borrowed(_))
        && matches!(stage3, std::borrow::Cow::Borrowed(_))
    {
        // Nothing changed at any stage: borrow the original and allocate nothing.
        return std::borrow::Cow::Borrowed(text);
    }
    std::borrow::Cow::Owned(stage3.into_owned())
}

/// Redact for text that is about to **leave the device**.
///
/// [`redact`] plus the national-ID and payment-card backstop in [`crate::identifiers`]: everything
/// the DB path masks, and additionally SSNs, Luhn-valid card numbers, and labelled My Number and
/// passport values.
///
/// This is the function for a third-party boundary, not for the capture-to-DB path: `event_log`
/// keeps using [`crate::redact`], because masking personal data on the way into the store would
/// break `people.emails` and `identity::resolve`. Why cards and national IDs are the exception is
/// in the [`crate::identifiers`] module docs.
///
/// Every prompt builder in `shogun-core` that puts stored or captured text in front of a model
/// runs it through here, before the untrusted-text fence:
///
/// - Batch lane: `dreamcycle::jobs::consolidation_data` / `consolidation_prompt` / `brief_prompt`
///   (Dream Cycle consolidation and Morning Brief), `daemon::brief_generation::model_sources`
///   (the durable Morning Brief lane), `mail_batch::triage_prompt` (mail triage),
///   `daemon::daily_activity_summary::prompt` (day summaries) and
///   `daemon::about_profile::about_prompt_input` (About You).
/// - Agent lane, chat and drafts: `screen_context::screen_prompt_text` (live screen block,
///   inline / Scribe context, reply drafts, "looking at" labels),
///   `llm::prompt_cache::untrusted_stable_context` (remembered facts), `chat_prompt` transcript
///   and saved-meeting blocks, `llm::tool_turns::egress_tool_result_content` (tool results fed
///   back to the model on every provider and the delegate bridge), `meeting::minutes` (Recap
///   transcript), `meeting::live_summary::context` / `grounded_context` (live canvas: transcript
///   rows, authored notes, previous summary), `meeting::live_translation::user_message`.
/// - AI search: `ai_search::prompt::sanitize_line` / `sanitize_question`.
/// - Form mode: `forms::jev` refuses a record this would change.
///
/// Deliberately *not* applied to the user's own typed chat turns: those are the person's
/// deliberate words to the model, not captured text, and the model never saw an unmasked value it
/// could echo back because every stored input reached it through one of the paths above.
///
/// Also deliberately *not* applied to `web_fetch::chat::turn_block`: that text is a public web
/// page, caption track or post the user asked chat to read — fetched from the internet moments
/// ago, not stored or captured — so nothing of the user's is in it and masking it would only
/// damage the answer. If that lane ever carries stored text, it joins the list above.
pub fn redact_egress(text: &str) -> std::borrow::Cow<'_, str> {
    // Hidden characters come off first, for the same reason `sanitize::persist_body` does it in
    // that order: a key split by a zero-width space does not match any issuer prefix until the
    // ZWSP is gone. Skipping this step made the egress path strictly weaker than the DB path —
    // `pii-bench` caught exactly that, with `zwsp-split-key` masked on write and surviving here.
    let stripped = hidden::strip_hidden(text);
    let secrets = redact(&stripped.text);
    let masked = identifiers::mask_high_risk_identifiers(&secrets);

    if stripped.removed == 0
        && matches!(secrets, std::borrow::Cow::Borrowed(_))
        && matches!(masked, std::borrow::Cow::Borrowed(_))
    {
        // Nothing at any stage touched the text; hand back the caller's own slice.
        return std::borrow::Cow::Borrowed(text);
    }
    std::borrow::Cow::Owned(masked.into_owned())
}

/// Mask email addresses and full URLs. Hand-rolled (no regex dep, matching this module's style).
fn mask_emails_and_urls(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('@') && !text.contains("://") {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < text.len() {
        if !text.is_char_boundary(i) {
            out.push_str(&text[i..i + 1]);
            i += 1;
            continue;
        }
        let rest = &text[i..];
        // URL: scheme "://" then a run of URL-ish characters (query included).
        if let Some(scheme_len) = url_scheme_len(rest) {
            let url_len = scheme_len + run_len_url(&rest[scheme_len..]);
            out.push_str(MASK);
            i += url_len;
            continue;
        }
        // Email: back up over the local part already emitted, then mask local@domain.
        if let Some(after_at) = rest.strip_prefix('@') {
            if let Some((local_len, domain_len)) = email_span(&out, after_at) {
                out.truncate(out.len() - local_len);
                out.push_str(MASK);
                i += 1 + domain_len; // consume '@' + domain
                continue;
            }
        }
        let ch_len = rest.chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&rest[..ch_len]);
        i += ch_len;
    }
    std::borrow::Cow::Owned(out)
}

/// Length of a `scheme://` prefix at the start of `s`, or `None`.
///
/// Scans only the leading alphabetic run (max 10) rather than `find("://")` over the whole tail —
/// this runs at every byte offset, and a whole-tail search made the scan O(n²) on long text.
fn url_scheme_len(s: &str) -> Option<usize> {
    let scheme = s.bytes().take(11).take_while(|b| b.is_ascii_alphabetic()).count();
    // scheme must be short and alphabetic (http, https, ftp, ...) and immediately at the start.
    if scheme == 0 || scheme > 10 || !s[scheme..].starts_with("://") {
        return None;
    }
    Some(scheme + 3)
}

/// Length of the URL body (host/path/query) after the scheme. Stops at whitespace or quote.
fn run_len_url(s: &str) -> usize {
    s.char_indices()
        .find(|(_, c)| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')'))
        .map(|(idx, _)| idx)
        .unwrap_or(s.len())
}

/// Given the text already emitted (`emitted`) ending in an email local part, and the text after
/// the `@`, return `(local_part_byte_len, domain_byte_len)` when both sides look like an email.
fn email_span(emitted: &str, after_at: &str) -> Option<(usize, usize)> {
    // local part: trailing run of email-local chars in what we've emitted.
    let local: String = emitted
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-'))
        .collect();
    if local.is_empty() {
        return None;
    }
    let local_len = local.len();
    // domain: run of domain chars, must contain at least one dot before a whitespace/end.
    let domain_len = after_at
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-')))
        .map(|(idx, _)| idx)
        .unwrap_or(after_at.len());
    // A trailing '.' or '-' is sentence punctuation, not part of the domain ("…example.com.").
    // Trim it off and mask the address; rejecting here would leave the email in the log verbatim.
    let domain = after_at[..domain_len].trim_end_matches(['.', '-']);
    let domain_len = domain.len();
    if domain_len == 0 || !domain.contains('.') {
        return None;
    }
    Some((local_len, domain_len))
}

/// True when `text` contains anything worth a full scan.
fn might_contain_secret(text: &str) -> bool {
    // Case-exact prefixes are searched in the original text; every other prefix is searched in the
    // lowercased copy, which is exactly the split `prefix_matches` applies. If this pre-check were
    // stricter than the matcher, the fast path would return early and the matcher would never run.
    if CASE_EXACT_PREFIXES.iter().any(|p| text.contains(p)) {
        return true;
    }
    let lower = text.to_ascii_lowercase();
    if ISSUER_PREFIXES
        .iter()
        .filter(|p| !CASE_EXACT_PREFIXES.contains(*p))
        .any(|p| lower.contains(p))
    {
        return true;
    }
    // Labels are matched case-insensitively, so pre-check the same way.
    LABELS.iter().any(|l| lower.contains(l))
}

/// How long the run of secret-ish characters at the start of `s` is.
fn run_len(s: &str) -> usize {
    s.char_indices()
        .find(|(_, c)| !is_secret_char(*c))
        .map(|(idx, _)| idx)
        .unwrap_or(s.len())
}

struct Labelled {
    /// Bytes up to and including the separator + optional `Bearer `, copied through verbatim.
    label_end: usize,
    /// Total bytes consumed (label + separator + the masked value).
    total: usize,
}

/// Case-insensitive ASCII prefix test on raw bytes — no allocation. Byte slicing is safe here
/// because a byte-wise case-insensitive match against an ASCII `prefix` can only succeed on
/// ASCII bytes.
fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// Match `label` `sep` `value` at the start of `s`, where sep is `:` or `=` with optional spaces
/// and quotes. Returns `None` unless the value is long enough to be a real credential.
///
/// Matches labels byte-wise instead of lowercasing `s`: this is called at every byte offset of
/// the scan, and `to_ascii_lowercase()` of the whole remaining text allocated O(n) per offset —
/// O(n²) time AND allocation on the DB persist path for any text that merely contains "token".
fn match_labelled(s: &str) -> Option<Labelled> {
    let label = LABELS.iter().find(|l| starts_with_ignore_case(s, l))?;
    let mut p = label.len();

    // Optional closing quote of a quoted key ("api_key": …) then the separator. Every byte
    // consumed below is ASCII, so `p` always lands on a char boundary.
    let b = s.as_bytes();
    while p < b.len() && (b[p] == b'"' || b[p] == b'\'' || b[p] == b' ') {
        p += 1;
    }
    if p >= b.len() || (b[p] != b':' && b[p] != b'=') {
        return None;
    }
    p += 1;
    while p < b.len() && (b[p] == b' ' || b[p] == b'"' || b[p] == b'\'') {
        p += 1;
    }
    // `Authorization: Bearer <token>` — keep the scheme, mask the token.
    for scheme in ["bearer ", "basic ", "token "] {
        if starts_with_ignore_case(&s[p..], scheme) {
            p += scheme.len();
            break;
        }
    }
    let value_len = run_len(&s[p..]);
    if value_len < MIN_SECRET_LEN {
        return None;
    }
    Some(Labelled { label_end: p, total: p + value_len })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> String {
        redact(s).into_owned()
    }

    #[test]
    fn ordinary_text_is_untouched_and_not_reallocated() {
        let text = "Weekly sync notes. I'll send Alice the deck tomorrow. Waiting on legal.";
        assert!(matches!(redact(text), std::borrow::Cow::Borrowed(_)), "no copy for clean text");
        assert_eq!(r(text), text);
    }

    #[test]
    fn issuer_prefixed_keys_are_masked() {
        assert_eq!(r("key sk-ant-api03-abcdefghijklmnop here"), "key [redacted] here");
        assert_eq!(r("ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ012345"), "[redacted]");
        assert_eq!(r("use AKIAIOSFODNN7EXAMPLE now"), "use [redacted] now");
        assert_eq!(r("xoxb-123456789012-abcdefghijkl"), "[redacted]");
        assert_eq!(r("GOCSPX-abcdefghijklmnopqrst"), "[redacted]");
    }

    #[test]
    fn labelled_values_are_masked_but_the_label_survives() {
        assert_eq!(r("api_key: abcdefghijklmnopqrst"), "api_key: [redacted]");
        assert_eq!(r("password=hunter2hunter2hunter2"), "password=[redacted]");
        assert_eq!(r(r#""client_secret": "abcdefghijklmnop""#), r#""client_secret": "[redacted]""#);
    }

    #[test]
    fn an_authorization_header_keeps_its_scheme() {
        assert_eq!(
            r("Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"),
            "Authorization: Bearer [redacted]"
        );
    }

    #[test]
    fn short_values_are_left_alone_so_real_text_is_not_mangled() {
        // A "token" in prose, and a placeholder — masking these would corrupt the memory.
        assert_eq!(r("the token: abc"), "the token: abc");
        assert_eq!(r("password: TODO"), "password: TODO");
        assert_eq!(r("sk-short"), "sk-short");
    }

    #[test]
    fn issuer_prefixes_inside_ordinary_words_are_not_matched() {
        // "sk-", "SG.", "ASIA" and friends appear mid-word in real prose constantly; matching
        // them there shreds captured text permanently (the DB only holds the masked form).
        for s in [
            "task-management-workflow is due Friday",
            "the disk-cleanup-scheduled-job ran overnight",
            "MSG.Sent.Confirmation.Number.1234567890",
            "ASIA-PACIFIC-2026-PLANNING notes from the offsite",
            "kiosk-mode-restart-required on the demo box",
        ] {
            assert_eq!(r(s), s, "prose must survive redaction untouched");
        }
        // Still masked when the prefix starts its own token.
        assert_eq!(r("key sk-abcdefghijklmnop here"), "key [redacted] here");
    }

    #[test]
    fn a_secret_after_a_separator_is_still_masked() {
        // The shapes a key actually arrives in on a captured screen: an env assignment, a query
        // parameter, a path. The preceding char is a separator, not prose — masking must hold,
        // or the live key is written verbatim into event_log.
        assert_eq!(r("export MYVAR=sk-abcdefghijklmnop"), "export MYVAR=[redacted]");
        assert_eq!(r("https://x.test/v1?k=sk-abcdefghijklmnop"), "https://x.test/v1?k=[redacted]");
        assert_eq!(r("cat /keys/sk-abcdefghijklmnop"), "cat /keys/[redacted]");
        assert_eq!(r("(sk-abcdefghijklmnop)"), "([redacted])");
    }

    #[test]
    fn words_containing_a_label_are_not_treated_as_one() {
        // No separator follows, so nothing is masked.
        let s = "tokenising the passwordless flow takes a while honestly";
        assert_eq!(r(s), s);
    }

    #[test]
    fn multi_byte_text_survives_redaction_intact() {
        let s = "資料はこちら api_key: abcdefghijklmnopqrst です。日本語も壊れない。";
        let got = r(s);
        assert!(got.contains("資料はこちら"), "prefix intact: {got}");
        assert!(got.contains("です。日本語も壊れない。"), "suffix intact: {got}");
        assert!(got.contains("[redacted]"));
        assert!(!got.contains("abcdefghijklmnopqrst"));
    }

    #[test]
    fn several_secrets_in_one_blob_are_all_masked() {
        let got = r("first ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ012345 then api_key: zyxwvutsrqponmlkjihg end");
        assert!(!got.contains("ghp_ABCDEF"), "{got}");
        assert!(!got.contains("zyxwvutsrqponmlkjihg"), "{got}");
        assert!(got.starts_with("first ") && got.ends_with(" end"), "{got}");
    }

    fn rl(s: &str) -> String {
        redact_log(s).into_owned()
    }

    #[test]
    fn log_redactor_masks_emails() {
        assert_eq!(rl("user alice@example.com logged in"), "user [redacted] logged in");
        assert_eq!(rl("to: bob.smith+tag@sub.example.co.jp done"), "to: [redacted] done");
    }

    #[test]
    fn log_redactor_masks_an_email_at_the_end_of_a_sentence() {
        // The sentence period must not be absorbed into the domain: that fails the
        // ends_with('.') check and the whole address survives into the log.
        assert_eq!(rl("Sent to alice@example.com."), "Sent to [redacted].");
        assert_eq!(rl("failed for bob@sub.example.co.jp. Retrying"), "failed for [redacted]. Retrying");
    }

    #[test]
    fn log_redactor_masks_full_urls_including_query() {
        assert_eq!(
            rl("GET https://api.example.com/v1/x?token=abc123&u=alice now"),
            "GET [redacted] now",
        );
        assert_eq!(rl("open http://localhost:3000/cb?code=xyz"), "open [redacted]");
    }

    #[test]
    fn log_redactor_still_masks_issuer_keys_and_labels() {
        assert_eq!(rl("key sk-ant-api03-abcdefghijklmnop"), "key [redacted]");
        assert_eq!(rl("api_key: abcdefghijklmnopqrst"), "api_key: [redacted]");
    }

    #[test]
    fn log_redactor_leaves_ordinary_prose_untouched() {
        let s = "Expanding notch panel in 92ms; cache updated.";
        assert_eq!(rl(s), s);
    }

    #[test]
    fn log_redactor_preserves_multibyte_around_matches() {
        let got = rl("送信先 alice@example.com へ通知しました");
        assert!(got.starts_with("送信先 "), "{got}");
        assert!(got.ends_with(" へ通知しました"), "{got}");
        assert!(got.contains("[redacted]") && !got.contains("alice@example.com"), "{got}");
    }

    // regression: risky email/URL backtrack edge cases (locked in)

    #[test]
    fn log_redactor_email_backtrack_stops_at_a_prior_mask() {
        // A masked URL leaves a `[redacted]` whose trailing `]` is NOT an email-local char, so the
        // following bare `@domain` (no valid local part) must be left alone without corrupting the
        // earlier mask.
        assert_eq!(
            rl("see https://a.com then @example.com"),
            "see [redacted] then @example.com",
        );
    }

    #[test]
    fn log_redactor_masks_adjacent_emails() {
        assert_eq!(rl("a@x.com,b@y.com"), "[redacted],[redacted]");
    }

    #[test]
    fn log_redactor_leaves_at_without_local_or_domain_without_dot_untouched() {
        // `@` at string start: no local part → not an email.
        assert_eq!(rl("@example.com"), "@example.com");
        // domain has no dot: not an email.
        assert_eq!(rl("alice@localhost"), "alice@localhost");
    }

    #[test]
    fn log_redactor_masks_email_with_issuer_shaped_local_part() {
        // The local part looks like an issuer key; the whole email region is still masked and the
        // raw address never survives.
        let got = rl("sk-ant-foo@example.com");
        assert_eq!(got, "[redacted]");
        assert!(!got.contains("sk-ant-foo") && !got.contains("example.com"), "{got}");
    }

    #[test]
    fn log_redactor_keeps_a_multibyte_prefix_before_the_local_part() {
        // The multibyte run before the ASCII local part is preserved; the address is gone.
        assert_eq!(rl("名前ab@example.com"), "名前[redacted]");
    }
}

#[cfg(test)]
mod egress_tests {
    use super::*;

    fn r(s: &str) -> String {
        redact(s).into_owned()
    }
    fn e(s: &str) -> String {
        redact_egress(s).into_owned()
    }
    fn rl(s: &str) -> String {
        redact_log(s).into_owned()
    }

    #[test]
    fn issuer_prefixes_match_whatever_case_the_key_was_pasted_in() {
        // PII-004: an uppercase spelling must not reach disk verbatim.
        assert_eq!(r("key SK-ANT-API03-ABCDEFGHIJKLMNOP here"), "key [redacted] here");
        assert_eq!(r("key Sk-Ant-Api03-abcdefghijklmnop here"), "key [redacted] here");
        assert_eq!(r("token GHP_ABCDEFGHIJKLMNOPQRST x"), "token [redacted] x");
        assert_eq!(r("slack XOXB-1234567890-ABCDEFGH x"), "slack [redacted] x");
        // The lowercase forms mask the same way.
        assert_eq!(r("key sk-ant-api03-abcdefghijklmnop here"), "key [redacted] here");
    }

    #[test]
    fn case_exact_prefixes_do_not_swallow_ordinary_words() {
        // The reason CASE_EXACT_PREFIXES exists: these read as prose, not credentials.
        let asia = "asiapacific2026conference agenda";
        assert_eq!(r(asia), asia, "lowercase 'asia' must stay prose");
        let sg = "sg.example-region-identifier ok";
        assert_eq!(r(sg), sg, "lowercase 'sg.' must stay prose");
        // The real uppercase AWS key id is still masked.
        assert_eq!(r("id AKIAIOSFODNN7EXAMPLE x"), "id [redacted] x");
    }

    #[test]
    fn a_prefix_inside_a_word_is_still_not_a_key_in_either_case() {
        let lower = "task-management-system-notes";
        assert_eq!(r(lower), lower);
        let upper = "TASK-MANAGEMENT-SYSTEM-NOTES";
        assert_eq!(r(upper), upper);
    }

    #[test]
    fn egress_masks_cards_and_national_ids_that_the_db_path_deliberately_keeps() {
        // The DB path leaves these alone by design (PII-002).
        let card = "invoice card 4111111111111111 paid";
        assert_eq!(r(card), card, "DB path must not mask");
        assert_eq!(e(card), "invoice card [redacted] paid", "egress must mask");

        let ssn = "his ssn 123-45-6789 on file";
        assert_eq!(r(ssn), ssn);
        // `ssn` is itself an ID label, so the labelled detector fires first and keeps the label.
        assert!(!e(ssn).contains("123-45-6789"), "{}", e(ssn));
    }

    #[test]
    fn egress_still_masks_everything_the_db_path_masks() {
        assert_eq!(e("key sk-ant-api03-abcdefghijklmnop x"), "key [redacted] x");
        assert_eq!(e("password=hunter2-hunter2-hunter2"), "password=[redacted]");
    }

    #[test]
    fn egress_handles_a_secret_and_an_identifier_in_one_string() {
        let got = e("key sk-ant-api03-abcdefghijklmnop card 4111111111111111");
        assert_eq!(got.matches("[redacted]").count(), 2, "{got}");
        assert!(!got.contains("4111"), "{got}");
        assert!(!got.contains("abcdefghijklmnop"), "{got}");
    }

    #[test]
    fn ordinary_prose_passes_through_egress_untouched_and_unallocated() {
        let plain = "the meeting moved to Thursday and Dana will bring the deck";
        assert!(matches!(redact_egress(plain), std::borrow::Cow::Borrowed(_)));
        assert_eq!(e(plain), plain);
    }

    #[test]
    fn the_log_path_now_removes_identifiers_too() {
        let got = rl("charge to 4111111111111111 for dana@example.com");
        assert!(!got.contains("4111"), "{got}");
        assert!(!got.contains("dana@example.com"), "{got}");
    }

    #[test]
    fn a_key_split_by_a_zero_width_space_does_not_survive_egress() {
        // The DB path strips hidden characters before redacting; egress must do the same, or it
        // is strictly weaker than the path it is supposed to backstop.
        let zwsp = "key sk-ant-\u{200b}api03-abcdefghijklmnop end";
        let got = e(zwsp);
        assert!(!got.contains("api03-abcdefghijklmnop"), "{got}");
        assert!(got.contains("[redacted]"), "{got}");
    }

    #[test]
    fn stripping_hidden_characters_alone_still_returns_owned_text() {
        // No secret and no identifier, but a hidden scalar was removed: the result differs from
        // the input, so borrowing it would hand back the unstripped original.
        let sneaky = "ordinary prose with a \u{200b} zero width space";
        assert!(matches!(redact_egress(sneaky), std::borrow::Cow::Owned(_)));
        assert!(!e(sneaky).contains('\u{200b}'));
    }

    #[test]
    fn a_clean_log_line_is_still_borrowed() {
        let plain = "capture loop started";
        assert!(matches!(redact_log(plain), std::borrow::Cow::Borrowed(_)));
    }
}
