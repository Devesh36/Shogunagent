//! The deep-context cost estimate (issue #398): tokens × the selected model's price, before a
//! deep-context run and in Settings / `agent.source.get`.
//!
//! The token side is `shogun_fusion::budget` (the cap, the estimator, the range); this module adds
//! the price from [`crate::model_catalog`] and the one rule for when the user must confirm the cost
//! first. Every face reads the same JSON: the desktop's model list (`get_model_catalog`) and
//! `agent.source.get` on MCP / REST / CLI both carry it through [`crate::model_catalog::catalog_json`],
//! and `agent.source.get` adds the estimate for the model that actually runs (invariant 6). The
//! webview only formats the numbers.
//!
//! **An estimate, never a bill.** Prices are the catalog's standard rates ([`PRICES_CHECKED_ON`]);
//! the token count is a heuristic that undercounts CJK text, so the range is widened for it and the
//! JSON says `approximate`. A model with no listed price (an OpenRouter free-form id) shows tokens
//! only, with `basis: "unknown"`, and always asks for confirmation. A subscription delegate is
//! billed against the user's plan, not per token: `basis: "plan_quota"`.
//!
//! [`PRICES_CHECKED_ON`]: crate::model_catalog::PRICES_CHECKED_ON

use serde_json::{json, Value};
use shogun_fusion::budget::{
    deep_context_tokens, deep_context_tokens_at_cap, DeepContextBudget, DeepContextTokens,
    DEEP_CONTEXT_OUTPUT_CAP_TOKENS, DEEP_CONTEXT_PREMIUM_LINE_TOKENS,
};

use crate::model_catalog::{self, ModelInfo};

/// A deep-context run whose cost may exceed this (the upper end of the range, USD) asks the user
/// to confirm the cost first, on top of the L2 confirm every deep-context run has. Also asked when
/// the price is unknown. Owner question on issue #398: the value.
pub const DEEP_CONTEXT_CONFIRM_ABOVE_USD: f64 = 1.00;

/// Input above [`DEEP_CONTEXT_PREMIUM_LINE_TOKENS`] may be billed at this multiple of the input
/// rate (`docs/gpt6-astra-implications.md` §5.10). Applied to the upper end only, and only when
/// the range reaches past the line.
pub const PREMIUM_INPUT_MULTIPLIER: f64 = 2.0;
/// The output-rate multiple for a request billed at the long-input premium (same source).
pub const PREMIUM_OUTPUT_MULTIPLIER: f64 = 1.5;

/// How a run's cost is known.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CostBasis {
    /// The catalog lists the model's price: a USD range.
    Price { usd_low: f64, usd_high: f64 },
    /// No listed price (a free-form model id): tokens only.
    Unknown,
    /// A subscription delegate: the run uses the user's plan quota, not a per-token price.
    PlanQuota,
}

/// One deep-context estimate: the token range and how it is billed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeepContextEstimate {
    pub tokens: DeepContextTokens,
    pub basis: CostBasis,
}

impl DeepContextEstimate {
    /// Whether the user must confirm the cost before the run starts: the price is unknown, or
    /// the upper end is above [`DEEP_CONTEXT_CONFIRM_ABOVE_USD`]. A plan-quota run does not (its
    /// L2 confirm still applies).
    pub fn needs_cost_confirm(&self) -> bool {
        match self.basis {
            CostBasis::Price { usd_high, .. } => usd_high > DEEP_CONTEXT_CONFIRM_ABOVE_USD,
            CostBasis::Unknown => true,
            CostBasis::PlanQuota => false,
        }
    }
}

/// Round down / up to the cent, so the range never reads narrower than it is. The nudge keeps a
/// float sum like `0.22 + 0.08` (= 0.30000000000000004) from rounding up a whole cent.
fn cents_down(usd: f64) -> f64 {
    (usd * 100.0 + 1e-9).floor() / 100.0
}
fn cents_up(usd: f64) -> f64 {
    (usd * 100.0 - 1e-9).ceil() / 100.0
}

/// The USD range for `tokens` on `model`. Low: the low input, no output (a run may answer in a
/// line). High: the high input and the full output cap, at the premium rates when the high input
/// reaches past the premium line.
pub fn price_range(tokens: &DeepContextTokens, model: &ModelInfo) -> (f64, f64) {
    let per = |tokens: usize, usd_per_mtok: f64| tokens as f64 * usd_per_mtok / 1_000_000.0;
    let low = per(tokens.input_tokens_low, model.input_usd_per_mtok);
    let (in_mult, out_mult) = if tokens.may_cross_premium_line {
        (PREMIUM_INPUT_MULTIPLIER, PREMIUM_OUTPUT_MULTIPLIER)
    } else {
        (1.0, 1.0)
    };
    let high = per(tokens.input_tokens_high, model.input_usd_per_mtok * in_mult)
        + per(tokens.output_tokens_max, model.output_usd_per_mtok * out_mult);
    (cents_down(low), cents_up(high))
}

/// Whether `provider` is billed per token (an API-key provider in the catalog).
fn per_token(provider: &str) -> bool {
    model_catalog::provider_models(provider).is_some()
}

fn estimate_with(tokens: DeepContextTokens, provider: &str, model: &str) -> DeepContextEstimate {
    let basis = if !per_token(provider) {
        CostBasis::PlanQuota
    } else {
        match model_catalog::find(provider, model) {
            Some(info) => {
                let (usd_low, usd_high) = price_range(&tokens, info);
                CostBasis::Price { usd_low, usd_high }
            }
            None => CostBasis::Unknown,
        }
    };
    DeepContextEstimate { tokens, basis }
}

/// The pre-run estimate for an assembled deep-context request (`texts`: the pack and the rest of
/// the prompt) on the model that runs for the stored `(provider, model)`.
pub fn estimate(texts: &[&str], budget: DeepContextBudget, provider: &str, stored_model: &str) -> DeepContextEstimate {
    let resolved = model_catalog::resolve(provider, stored_model);
    estimate_with(deep_context_tokens(texts, budget), provider, &resolved.model)
}

/// The most one run can cost under `budget` on the model that runs for `(provider, model)`: a
/// pack filled to the cap, language unknown.
pub fn estimate_at_cap(budget: DeepContextBudget, provider: &str, stored_model: &str) -> DeepContextEstimate {
    let resolved = model_catalog::resolve(provider, stored_model);
    estimate_with(deep_context_tokens_at_cap(budget), provider, &resolved.model)
}

/// The shared JSON for one estimate.
pub fn estimate_json(e: &DeepContextEstimate, model: &str) -> Value {
    let t = &e.tokens;
    let cost = match e.basis {
        CostBasis::Price { usd_low, usd_high } => json!({ "basis": "price", "model": model, "usd_low": usd_low, "usd_high": usd_high }),
        CostBasis::Unknown => json!({ "basis": "unknown", "model": model }),
        CostBasis::PlanQuota => json!({ "basis": "plan_quota" }),
    };
    json!({
        "input_tokens": t.estimated_input_tokens,
        "input_tokens_low": t.input_tokens_low,
        "input_tokens_high": t.input_tokens_high,
        "output_tokens_max": t.output_tokens_max,
        "cap_tokens": t.cap_tokens,
        "premium_line_tokens": t.premium_line_tokens,
        "cjk_share_pct": t.cjk_share_pct,
        "approximate": t.approximate,
        "over_cap": t.over_cap,
        "may_cross_premium_line": t.may_cross_premium_line,
        "cost": cost,
        "needs_cost_confirm": e.needs_cost_confirm(),
    })
}

/// The rules every deep-context run is held to, for the catalog's top level.
pub fn policy_json(budget: DeepContextBudget) -> Value {
    json!({
        "cap_tokens": budget.cap_tokens(),
        "premium_line_tokens": DEEP_CONTEXT_PREMIUM_LINE_TOKENS,
        "output_cap_tokens": DEEP_CONTEXT_OUTPUT_CAP_TOKENS,
        "confirm_above_usd": DEEP_CONTEXT_CONFIRM_ABOVE_USD,
        // The token count is a heuristic; CJK text can bill up to ~1.5× the estimate.
        "cjk_estimate_approximate": true,
        "at_cap": {
            "input_tokens": deep_context_tokens_at_cap(budget).estimated_input_tokens,
            "input_tokens_high": deep_context_tokens_at_cap(budget).input_tokens_high,
        },
    })
}

/// A listed model's at-cap range, for its catalog entry: `{usd_low, usd_high, needs_cost_confirm}`.
pub fn model_at_cap_json(provider: &str, model: &ModelInfo) -> Value {
    let e = estimate_with(deep_context_tokens_at_cap(DeepContextBudget::default()), provider, model.id);
    match e.basis {
        CostBasis::Price { usd_low, usd_high } => {
            json!({ "usd_low": usd_low, "usd_high": usd_high, "needs_cost_confirm": e.needs_cost_confirm() })
        }
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usd(e: &DeepContextEstimate) -> (f64, f64) {
        match e.basis {
            CostBasis::Price { usd_low, usd_high } => (usd_low, usd_high),
            other => panic!("expected a price, got {other:?}"),
        }
    }

    #[test]
    fn a_latin_pack_is_priced_from_its_token_range_and_the_output_cap() {
        // 100K estimated tokens of Latin text on Sonnet 5 ($2 in / $10 out).
        let text = "a".repeat(400_000);
        let e = estimate(&[text.as_str()], DeepContextBudget::default(), "anthropic", "");
        assert_eq!(e.tokens.estimated_input_tokens, 100_000);
        // Low: 90K in, no output = $0.18. High: 110K in + 8K out = $0.22 + $0.08 = $0.30.
        assert_eq!(usd(&e), (0.18, 0.30));
        assert!(!e.tokens.may_cross_premium_line);
        assert!(!e.needs_cost_confirm());
    }

    #[test]
    fn the_worst_case_at_the_cap_is_priced_at_the_premium_and_asks_first() {
        let e = estimate_at_cap(DeepContextBudget::default(), "anthropic", "");
        // Low: 180K × $2 = $0.36. High: 300K × $4 + 8K × $15 = $1.20 + $0.12 = $1.32.
        assert_eq!(usd(&e), (0.36, 1.32));
        assert!(e.tokens.may_cross_premium_line);
        assert!(e.needs_cost_confirm(), "above the $1.00 line");
        // A deep model at the cap costs more and asks too.
        let deep = estimate_at_cap(DeepContextBudget::default(), "openai", "gpt-6-astra");
        let (lo, hi) = usd(&deep);
        assert!(lo > 1.0 && hi > lo);
        // A budget model stays under the line.
        let luna = estimate_at_cap(DeepContextBudget::default(), "openai", "gpt-6-luna");
        assert!(!luna.needs_cost_confirm());
    }

    #[test]
    fn a_model_without_a_listed_price_shows_tokens_only_and_asks_first() {
        let e = estimate(&["hello"], DeepContextBudget::default(), "openrouter", "mistral/some-new-model");
        assert_eq!(e.basis, CostBasis::Unknown);
        assert!(e.needs_cost_confirm());
        let v = estimate_json(&e, "mistral/some-new-model");
        assert_eq!(v["cost"]["basis"], "unknown");
        assert!(v["cost"].get("usd_high").is_none());
        assert!(v["input_tokens"].as_u64().unwrap() > 0);
        assert_eq!(v["needs_cost_confirm"], true);
    }

    #[test]
    fn a_subscription_delegate_uses_the_plan_quota() {
        let e = estimate_at_cap(DeepContextBudget::default(), "claude-code", "");
        assert_eq!(e.basis, CostBasis::PlanQuota);
        assert!(!e.needs_cost_confirm());
        assert_eq!(estimate_json(&e, "")["cost"], json!({ "basis": "plan_quota" }));
    }

    #[test]
    fn the_estimate_never_reports_more_than_the_cap_in_or_raises_it() {
        let text = "日本語".repeat(1_000_000);
        for setting in [None, Some(10_000), Some(272_000), Some(1_000_000)] {
            let budget = DeepContextBudget::from_setting(setting);
            let v = estimate_json(&estimate(&[text.as_str()], budget, "anthropic", ""), "claude-sonnet-5-5");
            let input = v["input_tokens"].as_u64().unwrap();
            assert!(input <= v["cap_tokens"].as_u64().unwrap());
            assert!(input < DEEP_CONTEXT_PREMIUM_LINE_TOKENS as u64);
            assert_eq!(v["over_cap"], true);
            assert_eq!(v["approximate"], true);
        }
    }

    #[test]
    fn the_catalog_and_agent_source_carry_the_same_numbers() {
        // Symmetry: the desktop model list and `agent.source.get` read one catalog.
        let catalog = model_catalog::catalog_json();
        assert_eq!(catalog["deep_context"]["cap_tokens"], 200_000);
        assert_eq!(catalog["deep_context"]["premium_line_tokens"], 272_000);
        assert_eq!(catalog["deep_context"]["confirm_above_usd"], DEEP_CONTEXT_CONFIRM_ABOVE_USD);
        let sonnet = &catalog["providers"]["anthropic"]["models"][0];
        assert_eq!(sonnet["id"], "claude-sonnet-5-5");
        assert_eq!(sonnet["deep_context_at_cap"]["usd_high"], 1.32);
        let at_cap = estimate_json(&estimate_at_cap(DeepContextBudget::default(), "anthropic", ""), "claude-sonnet-5-5");
        assert_eq!(sonnet["deep_context_at_cap"]["usd_low"], at_cap["cost"]["usd_low"]);
        assert_eq!(sonnet["deep_context_at_cap"]["usd_high"], at_cap["cost"]["usd_high"]);
    }
}
