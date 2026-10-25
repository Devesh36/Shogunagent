//! The models the Agent lane can run on, per API-key provider (issue #389).
//!
//! This table is the single source for every face: the desktop's Settings select, the
//! `agent.source.get` answer on MCP / REST / CLI, what `agent.source.set` accepts, and the model a
//! request actually names. The webview keeps no copy of it (invariant 6, H0-5 / #313).
//!
//! - **Curated, not free text.** A model id outside the list is refused on write, and a stored one
//!   (a hand-edited `llm.json`, a value an older build wrote) runs the provider's default model
//!   instead, with a note the Settings screen and `agent.source.get` show. A key pasted where a
//!   model belongs can therefore never reach a request or a log line as a "model".
//!   **Exception (owner decision 2026-09-26): OpenRouter** also takes a free-form model id (an
//!   "advanced" pick): format-checked only ([`custom_model_ok`]), shown with unknown pricing and
//!   `model_choice: custom`. Every other provider keeps the curated list.
//! - **Two tiers.** `budget` models are fit for the high-frequency lanes (⌥-tap drafts, Scribe,
//!   voice cleanup). `deep` models cost several times more per token and are only ever used when
//!   the user picks one explicitly; they are never a default.
//! - **Three defaults per provider.** `default` runs chat and the other user-initiated lanes when
//!   nothing is picked; `budget` runs the budget-only lanes unless the user picked another budget
//!   model; `translate` runs the two translation lanes the same way. They differ only where the
//!   owner chose a cheaper model for translation (OpenAI: `gpt-6-sol` everywhere, `gpt-6-luna` for
//!   Quick Translate and live meeting translation). A deep or free-form pick never reaches a
//!   budget-only lane ([`model_for_tag`], harness R8).
//! - **Stale defaults move forward.** An install that stored a retired default model
//!   (`retired`) runs today's default model, and [`crate::agent_source::migrate_retired_model`]
//!   clears it from `llm.json` so it follows future defaults too. That is a settings-file change
//!   only; no database is touched.
//!
//! The Batch lane is not here: its models are chosen by the relay (invariant 5). Subscription
//! delegates are not here either: the vendor CLI runs whatever its plan allows (issue #110).
//!
//! Prices are USD per million tokens at the provider's standard rate, checked on each vendor's
//! pricing page on [`PRICES_CHECKED_ON`]. They are shown as a cost hint only; nothing bills from
//! them.

use serde_json::{json, Value};

use crate::llm::prompts::{self, PromptTag};

/// When the prices below were read from the vendors' pricing pages.
pub const PRICES_CHECKED_ON: &str = "2026-09-26";

/// The longest free-form model id (OpenRouter's advanced pick).
pub const MAX_CUSTOM_MODEL_CHARS: usize = 100;

/// How a model may be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Fit for every Agent-lane use, including the high-frequency ones.
    Budget,
    /// Long, hard tasks. Several times the price of a budget model: used only when the user picks
    /// it, never by default, never on a budget-only lane.
    Deep,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Budget => "budget",
            Tier::Deep => "deep",
        }
    }
}

/// One selectable model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelInfo {
    /// The id the provider's API takes.
    pub id: &'static str,
    /// Display name for the Settings select.
    pub label: &'static str,
    pub tier: Tier,
    /// USD per million input tokens (standard rate, short context).
    pub input_usd_per_mtok: f64,
    /// USD per million output tokens (standard rate, short context).
    pub output_usd_per_mtok: f64,
}

/// One API-key provider's models.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProviderModels {
    pub provider: &'static str,
    /// What chat and the other user-initiated lanes run when the user has not picked a model.
    /// Always a listed [`Tier::Budget`] model.
    pub default: &'static str,
    /// What the budget-only lanes run unless the user picked another budget model. Always a
    /// listed [`Tier::Budget`] model, never dearer than [`Self::default`].
    pub budget: &'static str,
    /// What Quick Translate and live meeting translation run unless the user picked another
    /// budget model. Always a listed [`Tier::Budget`] model, never dearer than [`Self::budget`].
    pub translate: &'static str,
    /// Selectable models, in the order the Settings select lists them.
    pub models: &'static [ModelInfo],
    /// Retired default ids. A stored one runs [`Self::default`] and is cleared from
    /// `llm.json` at startup.
    pub retired: &'static [&'static str],
    /// Whether a free-form id outside [`Self::models`] may be stored (OpenRouter only, owner
    /// decision 2026-09-26).
    pub custom: bool,
}

const fn m(
    id: &'static str,
    label: &'static str,
    tier: Tier,
    input_usd_per_mtok: f64,
    output_usd_per_mtok: f64,
) -> ModelInfo {
    ModelInfo { id, label, tier, input_usd_per_mtok, output_usd_per_mtok }
}

const ANTHROPIC: ProviderModels = ProviderModels {
    provider: "anthropic",
    default: "claude-sonnet-5-5",
    budget: "claude-sonnet-5-5",
    translate: "claude-sonnet-5-5",
    models: &[
        m("claude-sonnet-5-5", "Sonnet 5.5", Tier::Budget, 2.0, 10.0),
        m("claude-haiku-4-5", "Haiku 4.5", Tier::Budget, 1.0, 5.0),
        m("claude-opus-5-5", "Opus 5.5", Tier::Deep, 4.0, 20.0),
        m("claude-fable-5-1", "Fable 5.1", Tier::Deep, 10.0, 50.0),
    ],
    retired: &["claude-sonnet-5"],
    custom: false,
};

const OPENROUTER: ProviderModels = ProviderModels {
    provider: "openrouter",
    default: "anthropic/claude-sonnet-5.5",
    budget: "anthropic/claude-sonnet-5.5",
    translate: "anthropic/claude-sonnet-5.5",
    models: &[
        m("anthropic/claude-sonnet-5.5", "Sonnet 5.5", Tier::Budget, 2.0, 10.0),
        m("anthropic/claude-haiku-4.5", "Haiku 4.5", Tier::Budget, 1.0, 5.0),
        m("openai/gpt-6-luna", "GPT-6 Luna", Tier::Budget, 0.1, 0.5),
        m("openai/gpt-6-sol", "GPT-6 Sol", Tier::Budget, 2.0, 10.0),
        m("google/gemini-3.8-flash", "Gemini 3.8 Flash", Tier::Budget, 0.75, 3.75),
        m("anthropic/claude-opus-5.5", "Opus 5.5", Tier::Deep, 4.0, 20.0),
        m("anthropic/claude-fable-5.1", "Fable 5.1", Tier::Deep, 10.0, 50.0),
        m("openai/gpt-6-astra", "GPT-6 Astra", Tier::Deep, 10.0, 50.0),
    ],
    retired: &["anthropic/claude-sonnet-4.5", "anthropic/claude-sonnet-5"],
    custom: true,
};

/// 2026-10-08: chat runs `gpt-6-sol`. #1066 (owner OK 2026-10-12): every budget-only lane (⌥-tap
/// drafts, Scribe, translation, AI memory search, live summary) runs `gpt-6-luna` — a probe showed
/// it accepts reasoning effort `none` and `low`; the inline rewrite sends `low`, the other short
/// lanes `none`.
const OPENAI: ProviderModels = ProviderModels {
    provider: "openai",
    default: "gpt-6-sol",
    budget: "gpt-6-luna",
    translate: "gpt-6-luna",
    models: &[
        m("gpt-6-sol", "GPT-6 Sol", Tier::Budget, 2.0, 10.0),
        m("gpt-6-luna", "GPT-6 Luna", Tier::Budget, 0.1, 0.5),
        m("gpt-6-astra", "GPT-6 Astra", Tier::Deep, 10.0, 50.0),
    ],
    retired: &["gpt-4o-mini"],
    custom: false,
};

const GEMINI: ProviderModels = ProviderModels {
    provider: "gemini",
    default: "gemini-3.8-flash",
    budget: "gemini-3.8-flash",
    translate: "gemini-3.8-flash",
    models: &[
        m("gemini-3.8-flash", "Gemini 3.8 Flash", Tier::Budget, 0.75, 3.75),
        m("gemini-3.5-flash-lite", "Gemini 3.5 Flash-Lite", Tier::Budget, 0.3, 2.5),
    ],
    retired: &["gemini-2.5-flash"],
    custom: false,
};

/// Every API-key provider's models, in [`crate::agent_source::BYOK_PROVIDERS`] order.
pub const CATALOG: [ProviderModels; 4] = [ANTHROPIC, OPENROUTER, OPENAI, GEMINI];

/// A provider's models. `None` for a subscription delegate or an unknown id.
pub fn provider_models(provider: &str) -> Option<&'static ProviderModels> {
    CATALOG.iter().find(|p| p.provider == provider)
}

/// What chat runs when the user has not picked a model. Empty for a subscription delegate (its
/// CLI picks) and for an unknown provider.
pub fn default_model(provider: &str) -> &'static str {
    provider_models(provider).map_or("", |p| p.default)
}

/// What the budget-only lanes run by default. Empty for a delegate or an unknown provider.
pub fn budget_model(provider: &str) -> &'static str {
    provider_models(provider).map_or("", |p| p.budget)
}

/// What the translation lanes run by default. Empty for a delegate or an unknown provider.
pub fn translate_model(provider: &str) -> &'static str {
    provider_models(provider).map_or("", |p| p.translate)
}

/// The entry for `model` on `provider`, if it is listed there.
pub fn find(provider: &str, model: &str) -> Option<&'static ModelInfo> {
    provider_models(provider)?.models.iter().find(|m| m.id == model.trim())
}

/// The format a free-form model id must have (OpenRouter's advanced pick): 1..=100 characters of
/// `A-Z a-z 0-9 - . _ / :` (so no whitespace or control characters), not starting like an API key,
/// and a long one must name a vendor (`vendor/model`) — a bare 32+ character token is far more
/// likely a pasted key than a model (invariant 7).
pub fn custom_model_ok(model: &str) -> bool {
    let m = model.trim();
    let charset = m
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '/' | ':'));
    let key_prefix = ["sk-", "sk_", "AIza", "AQ."].iter().any(|p| m.starts_with(p));
    !m.is_empty()
        && m.chars().count() <= MAX_CUSTOM_MODEL_CHARS
        && charset
        && !key_prefix
        && (m.len() < 32 || m.contains('/'))
}

/// Whether `model` may be stored for `provider`: empty (the default), a listed id, or — on a
/// provider that takes free-form ids — one that passes [`custom_model_ok`]. A subscription
/// delegate takes only empty: its CLI picks the model.
pub fn is_selectable(provider: &str, model: &str) -> bool {
    if model.trim().is_empty() || find(provider, model).is_some() {
        return true;
    }
    provider_models(provider).is_some_and(|p| p.custom) && custom_model_ok(model)
}

/// How the stored model was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Nothing stored: the default model runs.
    Default,
    /// A listed model the user picked.
    Selected,
    /// A free-form id the user typed on a provider that takes one: it runs as typed, pricing
    /// unknown.
    Custom,
    /// A retired default model: the default runs (and startup clears it).
    Retired,
    /// Not a model this build offers for the provider: the default runs, and the user is told so.
    Unknown,
}

impl Choice {
    pub fn as_str(self) -> &'static str {
        match self {
            Choice::Default => "default",
            Choice::Selected => "selected",
            Choice::Custom => "custom",
            Choice::Retired => "retired",
            Choice::Unknown => "unknown",
        }
    }
}

/// The model that runs for a stored `(provider, model)`, and how it was read.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// The id sent to the provider. Empty for a subscription delegate.
    pub model: String,
    pub choice: Choice,
    /// `None` for a subscription delegate and for a free-form id (its cost is unknown).
    pub tier: Option<Tier>,
}

/// Read a stored model. Never fails: anything this build does not offer runs the default model,
/// so a bad `llm.json` degrades to a working request, never to a 400 or to a credential sent as a
/// model id.
pub fn resolve(provider: &str, stored: &str) -> Resolved {
    let Some(p) = provider_models(provider) else {
        return Resolved { model: String::new(), choice: Choice::Default, tier: None };
    };
    let stored = stored.trim();
    let fallback = |choice| Resolved {
        model: p.default.to_string(),
        choice,
        tier: find(provider, p.default).map(|m| m.tier),
    };
    if stored.is_empty() {
        return fallback(Choice::Default);
    }
    if let Some(info) = find(provider, stored) {
        return Resolved { model: info.id.to_string(), choice: Choice::Selected, tier: Some(info.tier) };
    }
    if p.retired.contains(&stored) {
        return fallback(Choice::Retired);
    }
    if p.custom && custom_model_ok(stored) {
        return Resolved { model: stored.to_string(), choice: Choice::Custom, tier: None };
    }
    fallback(Choice::Unknown)
}

/// Lanes that run on every keystroke-scale action. They always run a budget model: a deep or
/// free-form pick applies to chat and the other user-initiated lanes, not here (issue #389,
/// harness R8).
///
/// The translation and live meeting lanes run on the user's own credential (#782): a caption per
/// spoken line, a summary refresh every 45 s and a hotkey translation are latency-bound and
/// high-volume, and running each on a deep pick would make them slow and spend the user's quota
/// at a rate nobody chose.
pub fn budget_only(tag: PromptTag) -> bool {
    [
        prompts::INLINE_DRAFT,
        prompts::SCRIBE_EDIT,
        prompts::VOICE_EDIT,
        prompts::AGENT_TRANSLATE,
        prompts::MEETING_LIVE_TRANSLATE,
        prompts::MEETING_LIVE_SUMMARY,
        // #940: AI memory search refines keyword results already on screen — two short calls
        // per search, on the user's own quota.
        prompts::MEMORY_SEARCH_EXPAND,
        prompts::MEMORY_SEARCH_RERANK,
        prompts::MEMORY_SEARCH_SUMMARY,
        // #1066: the hourly style-note mining call runs on the user's own credential.
        prompts::STYLE_NOTE_MINE,
        prompts::CHAT_MEMORY,
    ]
    .iter()
    .any(|t| t.id == tag.id)
}

/// The output cap (max tokens) a lane needs where the provider client's default (1,024) is wrong
/// for it, or `None` to keep the default. A translation can run longer than its source (3,000
/// characters of Japanese is well over 1,024 tokens of English) and a caption line is short by
/// design; a smaller cap is a bound on what a runaway answer spends of the user's quota.
///
/// The live summary needs 2,048: cited bullets, plus the reasoning tokens a reasoning model
/// spends inside the same cap, overrun 512 a minute into a meeting, and a cut-off refresh fails
/// outright, freezing the canvas on a stale summary. 2,048 fits a long meeting's canvas with room
/// to spare.
pub fn lane_max_tokens(tag: PromptTag) -> Option<u32> {
    // The streamed memory-search answer is short, but a cut answer would show half a sentence.
    if tag.id == prompts::AGENT_TRANSLATE.id
        || tag.id == prompts::MEETING_LIVE_SUMMARY.id
        || tag.id == prompts::MEMORY_SEARCH_SUMMARY.id
    {
        Some(2_048)
    } else if tag.id == prompts::MEETING_LIVE_TRANSLATE.id {
        Some(256)
    } else if tag.id == prompts::MEMORY_SEARCH_RERANK.id {
        // Bounded source-backed answer paragraphs plus candidate ranking.
        Some(2_048)
    } else if tag.id == prompts::CHAT_MEMORY.id {
        // A detailed note and its evidence quote, including languages with more tokens per character.
        Some(4_096)
    } else if tag.id == prompts::MEMORY_SEARCH_EXPAND.id {
        // Small structured extraction; truncated or malformed output is rejected by the caller.
        Some(512)
    } else {
        None
    }
}

/// The model a request for `tag` names. The user's pick (or the default) — except on a
/// budget-only lane, which runs a budget model the user picked explicitly and otherwise the
/// provider's [`ProviderModels::translate`] model on the translation lanes and its
/// [`ProviderModels::budget`] model on the rest.
pub fn model_for_tag(provider: &str, stored: &str, tag: PromptTag) -> String {
    let resolved = resolve(provider, stored);
    if budget_only(tag) {
        let explicit_budget = resolved.choice == Choice::Selected && resolved.tier == Some(Tier::Budget);
        if !explicit_budget {
            let translation = tag.id == prompts::AGENT_TRANSLATE.id
                || tag.id == prompts::MEETING_LIVE_TRANSLATE.id;
            let model = if translation { translate_model(provider) } else { budget_model(provider) };
            return model.to_string();
        }
    }
    resolved.model
}

fn model_json(provider: &str, info: &ModelInfo) -> Value {
    json!({
        "id": info.id,
        "label": info.label,
        "tier": info.tier.as_str(),
        "input_usd_per_mtok": info.input_usd_per_mtok,
        "output_usd_per_mtok": info.output_usd_per_mtok,
        // Issue #398: the most one deep-context run can cost on this model (`usd_low`,
        // `usd_high`, `needs_cost_confirm`), from `crate::deep_context`.
        "deep_context_at_cap": crate::deep_context::model_at_cap_json(provider, info),
    })
}

/// One provider's entry: `default` (chat when nothing is picked), `budget` (the budget-only
/// lanes), `translate` (the translation lanes), `deep` (the first deep model, or `null`),
/// `custom` (whether a free-form id is taken) and every listed model.
pub fn provider_json(p: &ProviderModels) -> Value {
    json!({
        "default": p.default,
        "budget": p.budget,
        "translate": p.translate,
        "deep": p.models.iter().find(|m| m.tier == Tier::Deep).map(|m| m.id),
        "custom": p.custom,
        "models": p.models.iter().map(|m| model_json(p.provider, m)).collect::<Vec<_>>(),
    })
}

/// `{"providers": {provider: {default, budget, deep, custom, models: [...]}}, "prices_checked_on",
/// "deep_context"}` for every API-key provider: what the Settings select lists and
/// `agent.source.get` returns. `deep_context` is the deep-context run's cap and cost rules
/// (issue #398, `crate::deep_context::policy_json`).
pub fn catalog_json() -> Value {
    let mut out = serde_json::Map::new();
    for p in &CATALOG {
        out.insert(p.provider.to_string(), provider_json(p));
    }
    json!({
        "providers": Value::Object(out),
        "prices_checked_on": PRICES_CHECKED_ON,
        "deep_context": crate::deep_context::policy_json(shogun_fusion::budget::DeepContextBudget::default()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_api_key_provider_has_listed_budget_defaults() {
        for provider in crate::agent_source::BYOK_PROVIDERS {
            let p = provider_models(provider).expect(provider);
            let default = find(provider, p.default).expect("default model is listed");
            let budget = find(provider, p.budget).expect("budget model is listed");
            let translate = find(provider, p.translate).expect("translate model is listed");
            assert_eq!(default.tier, Tier::Budget, "{provider}: the default must be a budget model");
            assert_eq!(budget.tier, Tier::Budget, "{provider}");
            assert_eq!(translate.tier, Tier::Budget, "{provider}");
            assert!(budget.output_usd_per_mtok <= default.output_usd_per_mtok, "{provider}");
            assert!(translate.output_usd_per_mtok <= budget.output_usd_per_mtok, "{provider}");
        }
        assert_eq!(CATALOG.len(), crate::agent_source::BYOK_PROVIDERS.len());
    }

    #[test]
    fn astra_is_selectable_but_never_a_default_and_always_deep() {
        for p in &CATALOG {
            assert!(!p.default.contains("astra") && !p.budget.contains("astra"), "{}", p.provider);
            for info in p.models.iter().filter(|m| m.id.contains("astra")) {
                assert_eq!(info.tier, Tier::Deep, "{}", info.id);
            }
        }
        assert!(is_selectable("openai", "gpt-6-astra"));
        assert!(is_selectable("openrouter", "openai/gpt-6-astra"));
        let json = catalog_json();
        assert_eq!(json["providers"]["openai"]["default"], "gpt-6-sol");
        assert_eq!(json["providers"]["openai"]["budget"], "gpt-6-luna");
        assert_eq!(json["providers"]["openai"]["translate"], "gpt-6-luna");
        assert_eq!(json["providers"]["openai"]["deep"], "gpt-6-astra");
        assert_ne!(json["providers"]["openai"]["budget"], "gpt-6-astra");
    }

    #[test]
    fn defaults_are_the_current_generation() {
        assert_eq!(default_model("anthropic"), "claude-sonnet-5-5");
        assert_eq!(default_model("openrouter"), "anthropic/claude-sonnet-5.5");
        assert_eq!((default_model("openai"), budget_model("openai")), ("gpt-6-sol", "gpt-6-luna"));
        assert_eq!(translate_model("openai"), "gpt-6-luna");
        assert_eq!(default_model("gemini"), "gemini-3.8-flash");
        // A subscription delegate names no model: its CLI picks.
        assert_eq!(default_model("claude-code"), "");
        assert_eq!(resolve("codex", "").model, "");
    }

    #[test]
    fn deep_models_cost_more_than_the_default_they_sit_beside() {
        for p in &CATALOG {
            let default = find(p.provider, p.default).unwrap();
            for deep in p.models.iter().filter(|m| m.tier == Tier::Deep) {
                assert!(deep.output_usd_per_mtok > default.output_usd_per_mtok, "{}", deep.id);
            }
        }
    }

    #[test]
    fn retired_defaults_run_todays_default() {
        let r = resolve("openai", "gpt-4o-mini");
        assert_eq!((r.model.as_str(), r.choice), ("gpt-6-sol", Choice::Retired));
        assert_eq!(model_for_tag("openai", "gpt-4o-mini", prompts::INLINE_DRAFT), "gpt-6-luna");
        assert_eq!(model_for_tag("openai", "gpt-4o-mini", prompts::AGENT_TRANSLATE), "gpt-6-luna");
        let r = resolve("openrouter", "anthropic/claude-sonnet-4.5");
        assert_eq!((r.model.as_str(), r.choice), ("anthropic/claude-sonnet-5.5", Choice::Retired));
        let r = resolve("anthropic", "claude-sonnet-5");
        assert_eq!((r.model.as_str(), r.choice), ("claude-sonnet-5-5", Choice::Retired));
        let r = resolve("gemini", " gemini-2.5-flash ");
        assert_eq!((r.model.as_str(), r.choice), ("gemini-3.8-flash", Choice::Retired));
        for p in &CATALOG {
            for id in p.retired {
                assert!(find(p.provider, id).is_none(), "{id}");
                assert_eq!(resolve(p.provider, id).choice, Choice::Retired, "{id}");
            }
        }
        assert!(!is_selectable("openai", "gpt-4o-mini"));
    }

    #[test]
    fn an_unknown_model_fails_safe_to_the_default_and_says_so() {
        for stored in ["gpt-4o", "claude-sonnet-5", "sk-live-pasted-key-0123456789abcdef0123", "../x"] {
            let r = resolve("openai", stored);
            assert_eq!(r.model, "gpt-6-sol", "{stored}");
            assert_eq!(r.choice, Choice::Unknown, "{stored}");
            assert!(!is_selectable("openai", stored));
        }
        // A model is per provider: an OpenAI id is not an Anthropic one.
        assert_eq!(resolve("anthropic", "gpt-6-astra").choice, Choice::Unknown);
        assert!(!is_selectable("anthropic", "meta-llama/llama-4"));
        assert!(!is_selectable("gemini", "gemini-9-ultra"));
        let r = resolve("anthropic", "");
        assert_eq!((r.model.as_str(), r.choice), ("claude-sonnet-5-5", Choice::Default));
        let r = resolve("anthropic", " claude-haiku-4-5 ");
        assert_eq!((r.model.as_str(), r.choice, r.tier), ("claude-haiku-4-5", Choice::Selected, Some(Tier::Budget)));
    }

    #[test]
    fn openrouter_takes_a_free_form_id_checked_for_format_only() {
        for id in ["meta-llama/llama-4-maverick", "mistralai/mistral-large-2511:free", "qwen/qwen3-235b-a22b-instruct-2507", "x"] {
            assert!(custom_model_ok(id), "{id}");
            assert!(is_selectable("openrouter", id), "{id}");
            let r = resolve("openrouter", id);
            assert_eq!((r.model.as_str(), r.choice, r.tier), (id, Choice::Custom, None), "{id}");
        }
        for bad in [
            "", "   ", "two words", "tab\there", "new\nline", "semi;colon", "quote\"", "é-model",
            "sk-or-v1-0123456789abcdef", "AIzaSyD-0123", "abcdefghijklmnopqrstuvwxyz0123456789",
        ] {
            assert!(!custom_model_ok(bad), "{bad:?}");
            assert!(!is_selectable("openrouter", bad) || bad.trim().is_empty(), "{bad:?}");
        }
        assert!(!custom_model_ok(&format!("v/{}", "m".repeat(MAX_CUSTOM_MODEL_CHARS))));
        assert!(custom_model_ok(&format!("v/{}", "m".repeat(MAX_CUSTOM_MODEL_CHARS - 2))));
        // A stored bad id on OpenRouter still fails safe.
        assert_eq!(resolve("openrouter", "two words").choice, Choice::Unknown);
        assert_eq!(catalog_json()["providers"]["openrouter"]["custom"], true);
        assert_eq!(catalog_json()["providers"]["openai"]["custom"], false);
    }

    #[test]
    fn lanes_moved_off_the_company_key_keep_their_output_caps() {
        assert_eq!(lane_max_tokens(prompts::AGENT_TRANSLATE), Some(2_048));
        assert_eq!(lane_max_tokens(prompts::MEETING_LIVE_SUMMARY), Some(2_048));
        assert_eq!(lane_max_tokens(prompts::MEETING_LIVE_TRANSLATE), Some(256));
        assert_eq!(lane_max_tokens(prompts::MEMORY_SEARCH_EXPAND), Some(512));
        assert_eq!(lane_max_tokens(prompts::MEMORY_SEARCH_RERANK), Some(2_048));
        assert_eq!(lane_max_tokens(prompts::MEMORY_SEARCH_SUMMARY), Some(2_048));
        assert_eq!(lane_max_tokens(prompts::CHAT_MEMORY), Some(4_096));
        // Every other lane keeps its client's own cap.
        for tag in [prompts::INLINE_CHAT, prompts::REPLY_DRAFT, prompts::INLINE_DRAFT] {
            assert_eq!(lane_max_tokens(tag), None);
        }
    }

    #[test]
    fn budget_only_lanes_never_run_a_deep_or_free_form_model() {
        for tag in [
            prompts::INLINE_DRAFT,
            prompts::SCRIBE_EDIT,
            prompts::VOICE_EDIT,
            // #782: the translation and live meeting lanes, on the user's own credential.
            prompts::AGENT_TRANSLATE,
            prompts::MEETING_LIVE_TRANSLATE,
            prompts::MEETING_LIVE_SUMMARY,
            // #940: AI memory search.
            prompts::MEMORY_SEARCH_EXPAND,
            prompts::MEMORY_SEARCH_RERANK,
            prompts::MEMORY_SEARCH_SUMMARY,
            prompts::CHAT_MEMORY,
        ] {
            assert!(budget_only(tag));
            let translation = tag == prompts::AGENT_TRANSLATE || tag == prompts::MEETING_LIVE_TRANSLATE;
            let _ = translation;
            let lane = "gpt-6-luna";
            assert_eq!(model_for_tag("openai", "gpt-6-astra", tag), lane);
            assert_eq!(model_for_tag("openai", "", tag), lane, "the default is the lane's");
            assert_eq!(model_for_tag("anthropic", "claude-fable-5-1", tag), "claude-sonnet-5-5");
            assert_eq!(model_for_tag("openrouter", "meta-llama/llama-4-maverick", tag), "anthropic/claude-sonnet-5.5");
            // An explicit budget pick is honoured on them.
            assert_eq!(model_for_tag("anthropic", "claude-haiku-4-5", tag), "claude-haiku-4-5");
            assert_eq!(model_for_tag("openai", "gpt-6-sol", tag), "gpt-6-sol");
        }
        // Chat and the other user-initiated lanes run the pick, deep and free-form included.
        for tag in [prompts::INLINE_CHAT, prompts::MEETING_CHAT, prompts::REPLY_DRAFT, prompts::USER_SKILL] {
            assert!(!budget_only(tag));
            assert_eq!(model_for_tag("openai", "gpt-6-astra", tag), "gpt-6-astra");
            assert_eq!(model_for_tag("openai", "", tag), "gpt-6-sol");
            assert_eq!(model_for_tag("openrouter", "meta-llama/llama-4-maverick", tag), "meta-llama/llama-4-maverick");
        }
        // Delegates name nothing on any lane.
        assert_eq!(model_for_tag("codex", "", prompts::INLINE_CHAT), "");
        assert_eq!(model_for_tag("codex", "", prompts::INLINE_DRAFT), "");
    }
}
