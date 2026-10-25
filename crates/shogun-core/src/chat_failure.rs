//! Why a chat turn ended without an answer, as a stable code (issue #778).
//!
//! The desktop shell puts one of these on `chat_done` (`error_code`) and the webview words it from
//! its catalog in the UI language (`chatErrors` in `apps/desktop/src/strings.ts`), so a failed
//! turn never paints English from Rust or text from a provider. The set is a contract with that
//! catalog: a vitest reads the constants below and requires copy for each one in every language.
//!
//! Provider text never becomes a code — it can quote the request, which is the user's own words.

use crate::llm::LlmError;

/// The plan does not include chat (FR-CH-16): no plan, or the trial ended.
pub const PLAN_REQUIRED: &str = "plan_required";
/// No usable Agent-lane credential: no subscription delegate accepted, no key for the provider.
pub const NOT_CONFIGURED: &str = "not_configured";
/// The provider rejected the credential (HTTP 401/403).
pub const CREDENTIAL_REJECTED: &str = "credential_rejected";
/// The provider is rate limiting (HTTP 429).
pub const RATE_LIMITED: &str = "rate_limited";
/// A subscription delegate's plan allowance ran out.
pub const QUOTA_EXHAUSTED: &str = "quota_exhausted";
/// A subscription delegate's CLI is too old for SHOGUN's flags; the user updates it.
pub const NEEDS_UPDATE: &str = "needs_update";
/// The provider could not be reached (connection, TLS, timeout on the wire).
pub const UNREACHABLE: &str = "unreachable";
/// The provider answered with something that could not be read.
pub const INVALID_RESPONSE: &str = "invalid_response";
/// Any other provider-side failure, including a rejected request.
pub const PROVIDER: &str = "provider";
/// The provider stopped the task under its safety monitoring (issue #399).
pub const PROVIDER_POLICY_STOP: &str = "provider_policy_stop";
/// A question about a saved meeting whose recap or transcript could not be read.
pub const SAVED_MEETING_UNREADABLE: &str = "saved_meeting_unreadable";

/// The company live gateway refused for the plan's SHOGUN AI allowance (#1101).
pub const HOSTED_LIMIT: &str = "hosted_limit";
/// The company live gateway is switched off, paused for this account or failing (#1101).
pub const HOSTED_UNAVAILABLE: &str = "hosted_unavailable";

/// An explicit slash command named a skill that is missing or invalid.
pub const SKILL_NOT_FOUND: &str = "skill_not_found";

/// The submitted message exceeds the composer limit.
pub const IMAGE_UNSUPPORTED: &str = "image_unsupported";

pub const MESSAGE_TOO_LONG: &str = "message_too_long";

/// Every code a chat turn can end with.
pub const ALL: &[&str] = &[
    IMAGE_UNSUPPORTED,
    MESSAGE_TOO_LONG,
    SKILL_NOT_FOUND,
    PLAN_REQUIRED,
    NOT_CONFIGURED,
    CREDENTIAL_REJECTED,
    RATE_LIMITED,
    QUOTA_EXHAUSTED,
    NEEDS_UPDATE,
    UNREACHABLE,
    INVALID_RESPONSE,
    PROVIDER,
    PROVIDER_POLICY_STOP,
    SAVED_MEETING_UNREADABLE,
    HOSTED_LIMIT,
    HOSTED_UNAVAILABLE,
];

/// The code for a generation failure on the Agent lane.
pub fn for_error(e: &LlmError) -> &'static str {
    match e {
        LlmError::QuotaExhausted(m) if m == crate::llm::HOSTED_LIMIT => HOSTED_LIMIT,
        LlmError::Provider(m) if m == crate::llm::HOSTED_UNAVAILABLE => HOSTED_UNAVAILABLE,
        LlmError::Unauthorized(..) => CREDENTIAL_REJECTED,
        LlmError::NotConfigured => NOT_CONFIGURED,
        LlmError::RateLimited(..) => RATE_LIMITED,
        LlmError::QuotaExhausted(..) => QUOTA_EXHAUSTED,
        LlmError::NeedsUpdate(..) => NEEDS_UPDATE,
        LlmError::Transport(..) => UNREACHABLE,
        LlmError::Parse(..) => INVALID_RESPONSE,
        LlmError::Provider(..) | LlmError::InvalidRequest => PROVIDER,
        LlmError::PolicyStopped => PROVIDER_POLICY_STOP,
        // The send was refused locally because the traceability ledger would not take its row.
        // Reported under the generic provider code: the catalog contract (one copy per code in
        // every language, in the desktop strings) would need a new entry for a code of its own,
        // and the user-facing effect is the same — this turn did not get an answer.
        LlmError::TraceUnrecorded => PROVIDER,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_failure_has_its_own_code() {
        assert_eq!(
            for_error(&LlmError::Unauthorized(401, String::new())),
            CREDENTIAL_REJECTED
        );
        assert_eq!(
            for_error(&LlmError::Unauthorized(403, String::new())),
            CREDENTIAL_REJECTED
        );
        assert_eq!(for_error(&LlmError::NotConfigured), NOT_CONFIGURED);
        assert_eq!(
            for_error(&LlmError::RateLimited(429, String::new())),
            RATE_LIMITED
        );
        assert_eq!(
            for_error(&LlmError::QuotaExhausted("plan".into())),
            QUOTA_EXHAUSTED
        );
        assert_eq!(
            for_error(&LlmError::Transport(
                crate::llm::transport::TransportError::Unreachable("dns".into())
            )),
            UNREACHABLE
        );
        assert_eq!(
            for_error(&LlmError::NeedsUpdate("update it".into())),
            NEEDS_UPDATE
        );
        assert_eq!(for_error(&LlmError::Parse("eof".into())), INVALID_RESPONSE);
        assert_eq!(for_error(&LlmError::InvalidRequest), PROVIDER);
        assert_eq!(for_error(&LlmError::PolicyStopped), PROVIDER_POLICY_STOP);
    }

    #[test]
    fn gateway_refusals_get_their_own_codes() {
        use crate::llm::hosted_status_error;
        let limit = hosted_status_error(429, r#"{"error":"budget_exceeded","resets_at":"2026-10-15T00:00:00Z"}"#);
        assert_eq!(for_error(&limit), HOSTED_LIMIT);
        assert_eq!(for_error(&hosted_status_error(402, r#"{"error":"plan_required"}"#)), HOSTED_LIMIT);
        for body in [r#"{"error":"blocked"}"#, r#"{"error":"live_not_enabled"}"#,
            r#"{"error":"provider_budget_exceeded"}"#, r#"{"error":"upstream_failed"}"#, "<html>"] {
            assert_eq!(for_error(&hosted_status_error(503, body)), HOSTED_UNAVAILABLE, "{body}");
        }
        assert_eq!(for_error(&hosted_status_error(429, r#"{"error":"rate_limited"}"#)), RATE_LIMITED);
        assert_eq!(for_error(&hosted_status_error(401, r#"{"error":"invalid_token"}"#)), CREDENTIAL_REJECTED);
        // A delegate's own quota text is not the gateway's limit.
        assert_eq!(for_error(&LlmError::QuotaExhausted("plan".into())), QUOTA_EXHAUSTED);
    }

    #[test]
    fn provider_text_never_becomes_the_code() {
        let leaky = LlmError::Provider("invalid request: <the user's question>".into());
        assert_eq!(for_error(&leaky), PROVIDER);
        let quota = LlmError::QuotaExhausted("<vendor text naming the account>".into());
        assert_eq!(for_error(&quota), QUOTA_EXHAUSTED);
        let key = LlmError::Unauthorized(401, "invalid key sk-ant-123".into());
        assert_eq!(for_error(&key), CREDENTIAL_REJECTED);
    }

    #[test]
    fn codes_are_distinct_snake_case_words_the_webview_can_parse() {
        let mut seen = std::collections::HashSet::new();
        for code in ALL {
            assert!(seen.insert(*code), "duplicate code {code}");
            assert!(!code.is_empty());
            assert!(code.starts_with(|c: char| c.is_ascii_lowercase()), "{code}");
            assert!(
                code.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "{code}"
            );
        }
        // Every code `for_error` can return is in the list the catalog is checked against.
        for e in [
            LlmError::Unauthorized(401, String::new()),
            LlmError::NotConfigured,
            LlmError::RateLimited(429, String::new()),
            LlmError::QuotaExhausted(String::new()),
            LlmError::NeedsUpdate(String::new()),
            LlmError::Transport(crate::llm::transport::TransportError::Io(String::new())),
            LlmError::Parse(String::new()),
            LlmError::Provider(String::new()),
            LlmError::InvalidRequest,
            LlmError::PolicyStopped,
        ] {
            assert!(ALL.contains(&for_error(&e)), "{e}");
        }
    }
}
