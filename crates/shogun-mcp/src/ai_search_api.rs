//! `memory.search` with `mode: "ai"` on the API faces (issue #940 B2b, invariant 6).
//!
//! The Full UI's AI search — the question widened into more keyword searches and the matches
//! reranked by the user's own model — is the same operation here: an optional `mode` on
//! `memory.search` (`keyword`, the default, or `ai`). The faces cannot run the model step: it
//! needs the Agent-lane credential, which only the desktop holds (the `translate.text` boundary).
//! So the backend queues the question for the desktop and waits; when the app is not running, or
//! the answer is late, the caller still gets the keyword results, labelled with why
//! (`status: "keyword_fallback"`, `reason`). A caller never has to poll.
//!
//! Who may spend the user's model access: the plan must include agent execution (every paid plan
//! and the trial) and the token must be one that may write (the rule `translate.text` follows —
//! a read-only token is answered with keyword results, `reason: "read_only_token"`). Both are
//! decided here, by the face, before anything is queued.

use crate::memory_api::{plan_allows, scope_allows, TokenScope, Tool};

/// The default: today's keyword search, unchanged.
pub const MODE_KEYWORD: &str = "keyword";
/// Widen and rerank with the user's own model.
pub const MODE_AI: &str = "ai";

/// The face's reason codes for keyword results instead of AI (the backend maps them onto the
/// core's fallback reasons).
pub const AI_NOT_IN_PLAN: &str = "ai_not_in_plan";
pub const READ_ONLY_TOKEN: &str = "read_only_token";
/// A connection without meeting notes and screen recall (owner decision 2026-10-20): the AI step
/// runs on the desktop over the whole memory, where nothing can leave them out.
pub const SOURCES_WITHHELD: &str = "sources_withheld";

/// `true` for `mode: "ai"`, `false` for `keyword` or no mode. Anything else is the caller's to fix.
pub fn wants_ai(mode: Option<&str>) -> Result<bool, &'static str> {
    match mode {
        None | Some(MODE_KEYWORD) => Ok(false),
        Some(MODE_AI) => Ok(true),
        Some(_) => Err("invalid_search_mode"),
    }
}

/// AI search reads the recent (Warm) memory and resolves dates from the question itself, so the
/// archive controls do not combine with it. Refused rather than silently ignored.
pub const AI_SEARCH_OPTIONS: &str = "ai_search_options_unsupported";

/// Whether this caller may spend the user's model access on the AI step. `Err` is the reason the
/// answer is keyword results instead (never a refusal: the caller still gets results).
pub fn ai_gate(scope: TokenScope, ent: &shogun_agents::entitlement::Entitlements) -> Result<(), &'static str> {
    // `translate.text` is the Agent-lane L1 tool: its plan and scope rules are exactly "may spend
    // the user's model access".
    if !ent.agent_execution || !plan_allows(Tool::TranslateText, ent) {
        return Err(AI_NOT_IN_PLAN);
    }
    if !scope_allows(scope, Tool::TranslateText) {
        return Err(READ_ONLY_TOKEN);
    }
    Ok(())
}

/// [`ai_gate`] for a session that may not read meeting notes and screen recall: never the AI
/// step, whatever the token, so the answer is the (filtered) keyword results.
pub fn ai_gate_for(
    scope: TokenScope,
    access: crate::source_policy::SensitiveSources,
    ent: &shogun_agents::entitlement::Entitlements,
) -> Result<(), &'static str> {
    ai_gate(scope, ent)?;
    if access.is_withheld() {
        return Err(SOURCES_WITHHELD);
    }
    Ok(())
}

/// A plain-language sentence for each refusal code of an AI search. Never query text.
pub fn failure_message(code: &str) -> &'static str {
    match code {
        "plan_required" => "Searching memory needs a paid plan or an active trial.",
        "empty_query" => "Nothing to search for.",
        "invalid_search_mode" => "mode must be keyword or ai.",
        AI_SEARCH_OPTIONS => "mode ai searches recent memory and reads dates from the question; depth, from_ms, to_ms and source_kind apply to keyword search only.",
        _ => "Memory couldn't be searched right now. Try again.",
    }
}

/// The `error` payload for a refused AI search.
pub fn refusal_json(code: &str) -> String {
    serde_json::json!({ "error": code, "message": failure_message(code) }).to_string()
}

/// The HTTP status for an AI search answer: 200 for results (AI or keyword fallback alike).
pub fn http_status(json: &str) -> u16 {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return 500;
    };
    match value.get("error").and_then(serde_json::Value::as_str) {
        None => 200,
        Some("plan_required") => 403,
        Some("empty_query" | "invalid_search_mode" | AI_SEARCH_OPTIONS | "query_too_long") => 400,
        Some(_) => 500,
    }
}

#[cfg(test)]
mod tests {
    use shogun_agents::entitlement::{entitlements, Entitlements, Plan};

    use super::*;

    #[test]
    fn mode_defaults_to_keyword_and_rejects_anything_else() {
        assert_eq!(wants_ai(None), Ok(false));
        assert_eq!(wants_ai(Some("keyword")), Ok(false));
        assert_eq!(wants_ai(Some("ai")), Ok(true));
        assert_eq!(wants_ai(Some("AI")), Err("invalid_search_mode"));
    }

    #[test]
    fn only_a_writing_token_on_an_agent_plan_spends_model_access() {
        let pro = entitlements(Plan::Pro, 0);
        assert_eq!(ai_gate(TokenScope::Full, &pro), Ok(()));
        assert_eq!(ai_gate(TokenScope::Read, &pro), Err(READ_ONLY_TOKEN));
        assert_eq!(ai_gate(TokenScope::Full, &Entitlements::trial_not_started()), Err(AI_NOT_IN_PLAN));
        // Owner decision 2026-10-20: a withheld connection never runs the AI step.
        use crate::source_policy::SensitiveSources;
        assert_eq!(ai_gate_for(TokenScope::Full, SensitiveSources::Allowed, &pro), Ok(()));
        assert_eq!(ai_gate_for(TokenScope::Full, SensitiveSources::Withheld, &pro), Err(SOURCES_WITHHELD));
        assert_eq!(ai_gate_for(TokenScope::Read, SensitiveSources::Withheld, &pro), Err(READ_ONLY_TOKEN));
    }

    #[test]
    fn answers_are_200_and_refusals_carry_their_status() {
        assert_eq!(http_status(r#"{"status":"keyword_fallback","results":[]}"#), 200);
        assert_eq!(http_status(&refusal_json("plan_required")), 403);
        assert_eq!(http_status(&refusal_json("empty_query")), 400);
        assert_eq!(http_status(&refusal_json(AI_SEARCH_OPTIONS)), 400);
        assert_eq!(http_status(&refusal_json("search_unavailable")), 500);
        assert_eq!(http_status("nope"), 500);
        for code in ["plan_required", "empty_query", "invalid_search_mode", AI_SEARCH_OPTIONS, "x"] {
            assert!(!failure_message(code).is_empty());
        }
    }
}
