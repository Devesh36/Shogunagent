//! ローカルなトークン推定と予算充填（Issue #63）。
//!
//! `shogun-fusion` はクラウド/ONNX に依存しない純粋クレートなので、トークン数は言語別の
//! char→token 比のヒューリスティックで見積もる（±10% で予算管理には十分）。正確な
//! トークナイザが必要になったら [`TokenEstimator`] を差し替える。

/// テキストのトークン数を見積もる seam。
pub trait TokenEstimator {
    /// テキストの推定トークン数を返す。
    fn count(&self, text: &str) -> usize;
}

/// 言語別 char→token 比のローカル推定器。CJK は文字あたりのトークンが多く、ラテンは
/// 単語あたり複数文字なので、CJK 文字比率で 2 つの比率を線形補間する。
#[derive(Debug, Clone, Copy)]
pub struct HeuristicEstimator {
    /// ラテン系: 1 トークンあたりの文字数（概ね 4）。
    latin_chars_per_token: f64,
    /// CJK 系: 1 トークンあたりの文字数（概ね 1.5）。
    cjk_chars_per_token: f64,
}

impl Default for HeuristicEstimator {
    fn default() -> Self {
        Self { latin_chars_per_token: 4.0, cjk_chars_per_token: 1.5 }
    }
}

fn is_cjk(c: char) -> bool {
    // Korean Hangul (0xAC00..=0xD7A3) は未対応。必要になれば追加。
    matches!(c as u32,
        0x3040..=0x30FF |   // ひらがな・カタカナ
        0x3400..=0x4DBF |   // CJK 拡張A
        0x4E00..=0x9FFF |   // CJK 統合漢字
        0xFF00..=0xFFEF)    // 全角
}

impl TokenEstimator for HeuristicEstimator {
    fn count(&self, text: &str) -> usize {
        let total = text.chars().count();
        if total == 0 {
            return 0;
        }
        let cjk = text.chars().filter(|c| is_cjk(*c)).count();
        let cjk_ratio = cjk as f64 / total as f64;
        let chars_per_token =
            self.cjk_chars_per_token * cjk_ratio + self.latin_chars_per_token * (1.0 - cjk_ratio);
        (total as f64 / chars_per_token).ceil() as usize
    }
}

use crate::block::{BlockRef, ContextBlock};
use crate::score::source_rank;

/// 予算充填の結果。
#[derive(Debug, Clone, PartialEq)]
pub struct FitResult {
    /// 予算内に採用したブロック（スコア降順で採用）。
    pub kept: Vec<ContextBlock>,
    /// 落としたブロックの参照（計測・再展開用）。
    pub dropped: Vec<BlockRef>,
    /// 充填前の全候補の合計トークン数。
    pub pre_tokens: usize,
    /// 採用後の合計トークン数（`<= budget_tokens`）。
    pub post_tokens: usize,
}

/// `scored`（ブロック, スコア）を**スコア降順**に並べ、累積トークンが `budget_tokens` を
/// 超えない範囲で採用する。超えたブロックは採用せず `dropped` に回す（後続の低スコアで
/// 予算に収まるものは採用する = best-effort な充填）。
///
/// 不変: 返る `post_tokens <= budget_tokens`。高スコアほど優先採用。同点は
/// [`crate::score::source_rank`] が高い方（MCP structured > 要約 > state > AX evidence）。
/// スコアも rank も同じなら入力順を保つ（安定ソート）。provenance は保持。
/// NaN スコアはどの実数スコアより下に並ぶ（[`sort_key`]）。
pub fn fit_to_budget(mut scored: Vec<(ContextBlock, f64)>, budget_tokens: usize) -> FitResult {
    let pre_tokens: usize = scored.iter().map(|(b, _)| b.tokens).sum();
    // スコア降順。同点は source_rank 降順。どちらも同点なら入力順（安定ソート）。
    scored.sort_by(|a, b| {
        sort_key(b.1)
            .total_cmp(&sort_key(a.1))
            .then_with(|| source_rank(b.0.source_kind).cmp(&source_rank(a.0.source_kind)))
    });

    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    let mut used = 0usize;
    for (block, _score) in scored {
        // `used <= budget_tokens` always holds, so the subtraction cannot underflow and a huge
        // `block.tokens` cannot overflow the sum.
        if block.tokens <= budget_tokens - used {
            used += block.tokens;
            kept.push(block);
        } else {
            dropped.push(block.id_ref);
        }
    }
    FitResult { kept, dropped, pre_tokens, post_tokens: used }
}

/// The score as a sort key with a total order. `partial_cmp` has none once a score is NaN, and
/// since Rust 1.81 `sort_by` may panic on an inconsistent comparator — the daemon must not. NaN
/// maps below every real score, and `-0.0` maps to `0.0` so every non-NaN pair compares exactly
/// as `partial_cmp` did (the normal budget's order is unchanged).
fn sort_key(score: f64) -> f64 {
    if score.is_nan() {
        f64::NEG_INFINITY
    } else if score == 0.0 {
        0.0
    } else {
        score
    }
}

// deep-context budget (issue #398)
//
// An explicit, user-invoked, infrequent run (weekly review, project retrospective, "everything
// with this client since March") may send a large Warm-tier pack instead of the everyday small
// one. The cap is enforced here, in the Rust core, and cannot be raised by a setting. Everyday
// Notch actions keep their own small budgets (harness R9); nothing here changes them.

/// The long-input pricing line (issue #398, `docs/gpt6-astra-implications.md` §5.10): above this
/// many input tokens a request is billed at the premium rate. A deep-context pack stays below it.
pub const DEEP_CONTEXT_PREMIUM_LINE_TOKENS: usize = 272_000;

/// The deep-context cap when the user has not lowered it (issue #398). The gap to
/// [`DEEP_CONTEXT_PREMIUM_LINE_TOKENS`] is headroom for the rest of the prompt and for the
/// estimator's error.
pub const DEEP_CONTEXT_DEFAULT_CAP_TOKENS: usize = 200_000;

// The default must sit below the premium line; a change that breaks this does not compile.
const _: () = assert!(DEEP_CONTEXT_DEFAULT_CAP_TOKENS < DEEP_CONTEXT_PREMIUM_LINE_TOKENS);

/// The input cap for one deep-context run. Only [`Default`] and [`DeepContextBudget::from_setting`]
/// build one, and neither can exceed [`DEEP_CONTEXT_DEFAULT_CAP_TOKENS`]: a setting lowers the
/// cap, never raises it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeepContextBudget {
    cap_tokens: usize,
}

impl Default for DeepContextBudget {
    fn default() -> Self {
        Self { cap_tokens: DEEP_CONTEXT_DEFAULT_CAP_TOKENS }
    }
}

impl DeepContextBudget {
    /// The cap from the user's setting. `None` is the default; a value above the default is
    /// clamped to it (downward only, issue #398). `Some(0)` is honoured: the pack is empty.
    pub fn from_setting(setting_tokens: Option<usize>) -> Self {
        let cap_tokens = setting_tokens
            .map_or(DEEP_CONTEXT_DEFAULT_CAP_TOKENS, |t| t.min(DEEP_CONTEXT_DEFAULT_CAP_TOKENS));
        Self { cap_tokens }
    }

    /// The cap in estimated tokens. Always below [`DEEP_CONTEXT_PREMIUM_LINE_TOKENS`].
    pub fn cap_tokens(self) -> usize {
        self.cap_tokens
    }
}

/// Fill a deep-context pack under `budget`: the same fill as [`fit_to_budget`], so the rules are
/// one set. Highest score first (ties: [`crate::score::source_rank`], then input order); a block
/// is kept whole or dropped whole, never cut mid-way; a block larger than the cap is dropped and
/// smaller, lower-scored ones still fill the space; every candidate ends up in `kept` (with its
/// [`BlockRef`]) or in `dropped`, so provenance survives truncation. `post_tokens <= cap`.
pub fn fit_deep_context(scored: Vec<(ContextBlock, f64)>, budget: DeepContextBudget) -> FitResult {
    fit_to_budget(scored, budget.cap_tokens())
}

/// What the approval sheet shows before a deep-context run starts (issue #398). Tokens only; the
/// billed range is [`deep_context_tokens`] and the dollar figure is `shogun_core::deep_context`
/// (per-model prices; a delegated run shows the plan-quota note instead of a price).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeepContextPreview {
    /// Estimated input tokens for the whole request (pack plus the rest of the prompt), from the
    /// same [`TokenEstimator`] the pack was filled with.
    pub estimated_input_tokens: usize,
    /// The cap in force for this run.
    pub cap_tokens: usize,
    /// [`DEEP_CONTEXT_PREMIUM_LINE_TOKENS`], so the sheet can say what the cap keeps it under.
    pub premium_line_tokens: usize,
    /// `estimated_input_tokens <= cap_tokens`. A run starts only from this state.
    pub within_cap: bool,
}

/// The pre-run estimate for `estimated_input_tokens` under `budget`. Pure; no pricing.
pub fn deep_context_preview(
    estimated_input_tokens: usize,
    budget: DeepContextBudget,
) -> DeepContextPreview {
    DeepContextPreview {
        estimated_input_tokens,
        cap_tokens: budget.cap_tokens(),
        premium_line_tokens: DEEP_CONTEXT_PREMIUM_LINE_TOKENS,
        within_cap: estimated_input_tokens <= budget.cap_tokens(),
    }
}

// deep-context token range (issue #398)
//
// What the sheet shows before a deep-context run: the input the estimator counts, the range the
// provider may actually bill (the estimator is a heuristic), and the output ceiling. Pricing is
// not here — `shogun_core::deep_context` multiplies this by the selected model's price.

/// The expected output ceiling of one deep-context run (issue #398): a review or a summary, not a
/// book. The cost range's upper end assumes the run writes all of it.
pub const DEEP_CONTEXT_OUTPUT_CAP_TOKENS: usize = 8_000;
/// The estimator's error either way on Latin-script text, percent (the ±10% in the module doc).
pub const ESTIMATE_ERROR_PCT: usize = 10;
/// The extra upper error for CJK text, percent at an all-CJK pack. The heuristic undercounts
/// Japanese and Chinese: a pack estimated at 200K can bill near 300K (#669), so the upper end of
/// the range is 110% + 40% × the CJK share of the characters.
pub const CJK_EXTRA_ERROR_PCT: usize = 40;

/// The token side of a deep-context estimate (issue #398).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeepContextTokens {
    /// Input tokens as the estimator counts them, after the cap: never above
    /// [`DeepContextTokens::cap_tokens`], so never above [`DEEP_CONTEXT_PREMIUM_LINE_TOKENS`].
    pub estimated_input_tokens: usize,
    /// The low end of what the provider may bill for that input.
    pub input_tokens_low: usize,
    /// The high end of what the provider may bill for that input (estimator error, CJK widened).
    pub input_tokens_high: usize,
    /// [`DEEP_CONTEXT_OUTPUT_CAP_TOKENS`].
    pub output_tokens_max: usize,
    pub cap_tokens: usize,
    pub premium_line_tokens: usize,
    /// Share of CJK characters, percent. `None` when the text is not known yet (the at-cap
    /// preview, which assumes the worst case).
    pub cjk_share_pct: Option<u8>,
    /// The range is wide because of CJK text (or an unknown language): say "approximate".
    pub approximate: bool,
    /// The texts were over the cap; the pack will be cut to it by score ([`fit_deep_context`]).
    pub over_cap: bool,
    /// The high end is above the premium line: a provider that bills long input at a premium may
    /// bill this run at it.
    pub may_cross_premium_line: bool,
}

fn token_range(
    estimated: usize,
    cjk_share_pct: Option<u8>,
    budget: DeepContextBudget,
    over_cap: bool,
) -> DeepContextTokens {
    // Unknown language: assume all CJK, the widest range.
    let cjk = usize::from(cjk_share_pct.unwrap_or(100).min(100));
    let high_pct = 100 + ESTIMATE_ERROR_PCT + CJK_EXTRA_ERROR_PCT * cjk / 100;
    let input_tokens_high = estimated.saturating_mul(high_pct).div_ceil(100);
    DeepContextTokens {
        estimated_input_tokens: estimated,
        input_tokens_low: estimated.saturating_mul(100 - ESTIMATE_ERROR_PCT) / 100,
        input_tokens_high,
        output_tokens_max: DEEP_CONTEXT_OUTPUT_CAP_TOKENS,
        cap_tokens: budget.cap_tokens(),
        premium_line_tokens: DEEP_CONTEXT_PREMIUM_LINE_TOKENS,
        cjk_share_pct,
        approximate: cjk > 0,
        over_cap,
        may_cross_premium_line: input_tokens_high > DEEP_CONTEXT_PREMIUM_LINE_TOKENS,
    }
}

/// The token estimate for an assembled deep-context request (`texts`: the pack and the rest of
/// the prompt), counted with the same [`HeuristicEstimator`] the pack is filled with. A request
/// over the cap is reported at the cap (`over_cap`): that is what will be sent.
pub fn deep_context_tokens(texts: &[&str], budget: DeepContextBudget) -> DeepContextTokens {
    let est = HeuristicEstimator::default();
    let counted: usize = texts.iter().map(|t| est.count(t)).fold(0, usize::saturating_add);
    let (chars, cjk) = texts.iter().fold((0usize, 0usize), |(n, c), t| {
        (n + t.chars().count(), c + t.chars().filter(|ch| is_cjk(*ch)).count())
    });
    let cjk_share = if chars == 0 { 0 } else { (cjk * 100).div_ceil(chars) };
    let cjk_share = u8::try_from(cjk_share.min(100)).unwrap_or(100);
    let cap = budget.cap_tokens();
    token_range(counted.min(cap), Some(cjk_share), budget, counted > cap)
}

/// The most one deep-context run can send under `budget`: a pack filled to the cap, language
/// unknown (so the widest range). What Settings and `agent.source.get` show before any run.
pub fn deep_context_tokens_at_cap(budget: DeepContextBudget) -> DeepContextTokens {
    token_range(budget.cap_tokens(), None, budget, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_zero_tokens() {
        assert_eq!(HeuristicEstimator::default().count(""), 0);
    }

    #[test]
    fn latin_uses_about_four_chars_per_token() {
        // 40 文字のラテン → 約 10 トークン。
        let s = "a".repeat(40);
        let n = HeuristicEstimator::default().count(&s);
        assert!((9..=11).contains(&n), "got {n}");
    }

    #[test]
    fn cjk_costs_more_tokens_than_latin_for_same_length() {
        let est = HeuristicEstimator::default();
        let latin = est.count(&"a".repeat(30));
        let cjk = est.count(&"あ".repeat(30));
        assert!(cjk > latin, "cjk={cjk} latin={latin}");
    }

    use crate::block::{BlockRef, ScoreInputs, SourceKind};

    fn blk(id: i64, tokens_text: usize) -> ContextBlock {
        blk_kind(id, tokens_text, SourceKind::Evidence)
    }

    fn blk_kind(id: i64, tokens_text: usize, kind: SourceKind) -> ContextBlock {
        // tokens は text から算出されるので、ラテン文字を tokens_text*4 個並べて概算調整。
        let est = HeuristicEstimator::default();
        ContextBlock::new(
            BlockRef::Event(id),
            kind,
            "a".repeat(tokens_text * 4),
            ScoreInputs { relevance: 0.5, freshness: 0.5, task_link: 0.0, confidence: 1.0 },
            &est,
        )
    }

    #[test]
    fn never_exceeds_budget() {
        let scored = vec![(blk(1, 10), 0.9), (blk(2, 10), 0.8), (blk(3, 10), 0.7)];
        let r = fit_to_budget(scored, 15);
        assert!(r.post_tokens <= 15, "post={}", r.post_tokens);
    }

    #[test]
    fn keeps_higher_score_first() {
        let scored = vec![(blk(1, 10), 0.2), (blk(2, 10), 0.9)];
        let r = fit_to_budget(scored, 10);
        assert_eq!(r.kept.len(), 1);
        assert_eq!(r.kept[0].id_ref, BlockRef::Event(2)); // 高スコアが残る
        assert_eq!(r.dropped, vec![BlockRef::Event(1)]);
    }

    #[test]
    fn all_fit_when_budget_large() {
        let scored = vec![(blk(1, 5), 0.5), (blk(2, 5), 0.5)];
        let r = fit_to_budget(scored, 1000);
        assert_eq!(r.kept.len(), 2);
        assert!(r.dropped.is_empty());
    }

    #[test]
    fn best_effort_skips_oversized_and_takes_smaller() {
        // A=10tok(0.9), B=7tok(0.8), C=3tok(0.7), budget=13 → A,C 採用 / B drop
        let scored = vec![(blk(1, 10), 0.9), (blk(2, 7), 0.8), (blk(3, 3), 0.7)];
        let r = fit_to_budget(scored, 13);
        assert_eq!(r.kept.len(), 2);
        assert_eq!(r.kept[0].id_ref, BlockRef::Event(1));
        assert_eq!(r.kept[1].id_ref, BlockRef::Event(3));
        assert_eq!(r.dropped, vec![BlockRef::Event(2)]);
    }

    #[test]
    fn equal_score_prefers_structured_over_ax_evidence() {
        // Evidence first in input: without the #35 tie-break, stable score-sort would keep it.
        let scored = vec![
            (blk_kind(1, 10, SourceKind::Evidence), 0.5),
            (blk_kind(2, 10, SourceKind::Structured), 0.5),
        ];
        let r = fit_to_budget(scored, 10);
        assert_eq!(r.kept.len(), 1);
        assert_eq!(r.kept[0].id_ref, BlockRef::Event(2));
        assert_eq!(r.dropped, vec![BlockRef::Event(1)]);
    }

    #[test]
    fn higher_score_beats_source_rank() {
        let scored = vec![
            (blk_kind(1, 10, SourceKind::Evidence), 0.9),
            (blk_kind(2, 10, SourceKind::Structured), 0.1),
        ];
        let r = fit_to_budget(scored, 10);
        assert_eq!(r.kept.len(), 1);
        assert_eq!(r.kept[0].id_ref, BlockRef::Event(1));
        assert_eq!(r.dropped, vec![BlockRef::Event(2)]);
    }

    // deep context (issue #398)

    /// A block with an exact token count. Built directly (fields are public) so a 200K-token
    /// candidate costs no 800 KB string; `text` carries the id so "never split" is checkable.
    fn big(id: i64, tokens: usize) -> ContextBlock {
        ContextBlock {
            id_ref: BlockRef::Event(id),
            source_kind: SourceKind::ThreadSummary,
            text: format!("thread pack item {id}"),
            score_inputs: ScoreInputs { relevance: 0.5, freshness: 0.5, task_link: 0.0, confidence: 1.0 },
            tokens,
        }
    }

    fn kept_ids(r: &FitResult) -> Vec<BlockRef> {
        r.kept.iter().map(|b| b.id_ref.clone()).collect()
    }

    #[test]
    fn deep_context_default_cap_is_200k_and_below_the_premium_line() {
        let b = DeepContextBudget::default();
        assert_eq!(b.cap_tokens(), 200_000);
        assert_eq!(DeepContextBudget::from_setting(None), b);
        assert!(b.cap_tokens() < DEEP_CONTEXT_PREMIUM_LINE_TOKENS);
        assert_eq!(DEEP_CONTEXT_PREMIUM_LINE_TOKENS, 272_000);
    }

    #[test]
    fn deep_context_setting_lowers_the_cap_but_never_raises_it() {
        assert_eq!(DeepContextBudget::from_setting(Some(100_000)).cap_tokens(), 100_000);
        assert_eq!(DeepContextBudget::from_setting(Some(199_999)).cap_tokens(), 199_999);
        assert_eq!(DeepContextBudget::from_setting(Some(200_000)).cap_tokens(), 200_000);
        // Above the default, even while still under the premium line: clamped, not raised.
        assert_eq!(DeepContextBudget::from_setting(Some(200_001)).cap_tokens(), 200_000);
        assert_eq!(DeepContextBudget::from_setting(Some(250_000)).cap_tokens(), 200_000);
        // At, above, and far above the premium line.
        for raised in [272_000, 272_001, 1_050_000, usize::MAX] {
            let cap = DeepContextBudget::from_setting(Some(raised)).cap_tokens();
            assert_eq!(cap, 200_000, "setting {raised}");
            assert!(cap < DEEP_CONTEXT_PREMIUM_LINE_TOKENS);
        }
        assert_eq!(DeepContextBudget::from_setting(Some(0)).cap_tokens(), 0);
    }

    #[test]
    fn deep_context_pack_under_the_cap_goes_whole() {
        let scored = vec![(big(1, 80_000), 0.4), (big(2, 60_000), 0.9), (big(3, 50_000), 0.7)];
        let r = fit_deep_context(scored, DeepContextBudget::default());
        assert_eq!(kept_ids(&r), vec![BlockRef::Event(2), BlockRef::Event(3), BlockRef::Event(1)]);
        assert!(r.dropped.is_empty());
        assert_eq!(r.pre_tokens, 190_000);
        assert_eq!(r.post_tokens, 190_000);
    }

    #[test]
    fn deep_context_pack_over_the_cap_is_truncated_by_score_never_sent_whole() {
        // 5 × 60K = 300K candidates against the 200K cap: the top three by score fit (180K), the
        // two lowest are dropped.
        let scored = vec![
            (big(1, 60_000), 0.10),
            (big(2, 60_000), 0.90),
            (big(3, 60_000), 0.50),
            (big(4, 60_000), 0.70),
            (big(5, 60_000), 0.30),
        ];
        let r = fit_deep_context(scored, DeepContextBudget::default());
        assert_eq!(r.pre_tokens, 300_000);
        assert_eq!(r.post_tokens, 180_000);
        assert!(r.post_tokens <= DeepContextBudget::default().cap_tokens());
        assert_eq!(kept_ids(&r), vec![BlockRef::Event(2), BlockRef::Event(4), BlockRef::Event(3)]);
        assert_eq!(r.dropped, vec![BlockRef::Event(5), BlockRef::Event(1)]);
    }

    #[test]
    fn deep_context_lowered_cap_truncates_harder() {
        let scored = vec![(big(1, 60_000), 0.9), (big(2, 60_000), 0.8), (big(3, 60_000), 0.7)];
        let r = fit_deep_context(scored, DeepContextBudget::from_setting(Some(100_000)));
        assert_eq!(kept_ids(&r), vec![BlockRef::Event(1)]);
        assert_eq!(r.dropped, vec![BlockRef::Event(2), BlockRef::Event(3)]);
        assert_eq!(r.post_tokens, 60_000);
    }

    #[test]
    fn deep_context_exactly_at_the_cap_is_kept_and_one_token_over_is_not() {
        let cap = DeepContextBudget::default();
        // Exactly the cap: everything goes.
        let at = fit_deep_context(vec![(big(1, 150_000), 0.9), (big(2, 50_000), 0.1)], cap);
        assert!(at.dropped.is_empty());
        assert_eq!(at.post_tokens, 200_000);
        // One token over: the lower-scored block is the one left out.
        let over = fit_deep_context(vec![(big(1, 150_000), 0.9), (big(2, 50_001), 0.1)], cap);
        assert_eq!(kept_ids(&over), vec![BlockRef::Event(1)]);
        assert_eq!(over.dropped, vec![BlockRef::Event(2)]);
        assert_eq!(over.post_tokens, 150_000);
        // A single block of exactly the cap, and of the cap plus one.
        let one_at = fit_deep_context(vec![(big(1, 200_000), 0.5)], cap);
        assert_eq!(kept_ids(&one_at), vec![BlockRef::Event(1)]);
        let one_over = fit_deep_context(vec![(big(1, 200_001), 0.5)], cap);
        assert!(one_over.kept.is_empty());
        assert_eq!(one_over.dropped, vec![BlockRef::Event(1)]);
        assert_eq!(one_over.post_tokens, 0);
    }

    #[test]
    fn deep_context_empty_candidates_give_an_empty_pack() {
        let r = fit_deep_context(Vec::new(), DeepContextBudget::default());
        assert!(r.kept.is_empty());
        assert!(r.dropped.is_empty());
        assert_eq!((r.pre_tokens, r.post_tokens), (0, 0));
        // A zero cap from the setting also sends nothing, and still accounts for every candidate.
        let zero = fit_deep_context(vec![(big(1, 1), 0.9)], DeepContextBudget::from_setting(Some(0)));
        assert!(zero.kept.is_empty());
        assert_eq!(zero.dropped, vec![BlockRef::Event(1)]);
    }

    #[test]
    fn deep_context_item_larger_than_the_cap_is_dropped_whole_and_the_rest_still_fill() {
        // The top-scored item alone is over the cap. It is not split; the next items fill in.
        let scored = vec![(big(1, 500_000), 0.99), (big(2, 120_000), 0.6), (big(3, 70_000), 0.5)];
        let r = fit_deep_context(scored, DeepContextBudget::default());
        assert_eq!(kept_ids(&r), vec![BlockRef::Event(2), BlockRef::Event(3)]);
        assert_eq!(r.dropped, vec![BlockRef::Event(1)]);
        assert_eq!(r.post_tokens, 190_000);
    }

    #[test]
    fn deep_context_never_splits_a_block_and_keeps_every_provenance_ref() {
        let input: Vec<(ContextBlock, f64)> = (1..=12)
            .map(|i| (big(i, 10_000 * usize::try_from(i).unwrap()), 1.0 / i as f64))
            .collect();
        let originals: Vec<ContextBlock> = input.iter().map(|(b, _)| b.clone()).collect();
        let r = fit_deep_context(input, DeepContextBudget::default());
        assert!(r.post_tokens <= 200_000);
        // Kept blocks are byte-identical to their input (text and token count untouched).
        for kept in &r.kept {
            let original = originals.iter().find(|o| o.id_ref == kept.id_ref).unwrap();
            assert_eq!(kept, original);
        }
        assert_eq!(r.post_tokens, r.kept.iter().map(|b| b.tokens).sum::<usize>());
        // Every candidate is accounted for exactly once: kept or dropped.
        let mut seen: Vec<BlockRef> = kept_ids(&r);
        seen.extend(r.dropped.iter().cloned());
        assert_eq!(seen.len(), originals.len());
        for o in &originals {
            assert_eq!(seen.iter().filter(|s| **s == o.id_ref).count(), 1, "{:?}", o.id_ref);
        }
    }

    #[test]
    fn deep_context_fill_is_deterministic_for_the_same_candidates() {
        let make = || {
            vec![
                (big(1, 90_000), 0.5),
                (big(2, 90_000), 0.5),
                (big(3, 40_000), 0.8),
                (big(4, 90_000), 0.2),
            ]
        };
        let first = fit_deep_context(make(), DeepContextBudget::default());
        for _ in 0..5 {
            assert_eq!(fit_deep_context(make(), DeepContextBudget::default()), first);
        }
        // Equal score and equal source kind: input order breaks the tie.
        assert_eq!(kept_ids(&first), vec![BlockRef::Event(3), BlockRef::Event(1)]);
        assert_eq!(first.dropped, vec![BlockRef::Event(2), BlockRef::Event(4)]);
    }

    #[test]
    fn nan_score_sorts_last_and_does_not_panic() {
        let scored = vec![(big(1, 10), f64::NAN), (big(2, 10), 0.1), (big(3, 10), f64::NAN), (big(4, 10), 0.9)];
        let r = fit_to_budget(scored, 20);
        assert_eq!(kept_ids(&r), vec![BlockRef::Event(4), BlockRef::Event(2)]);
        assert_eq!(r.dropped, vec![BlockRef::Event(1), BlockRef::Event(3)]);
    }

    #[test]
    fn negative_zero_ties_with_zero_as_before() {
        // partial_cmp treated -0.0 == 0.0, so input order decided; the total-order key keeps that.
        let scored = vec![(big(1, 10), -0.0), (big(2, 10), 0.0)];
        let r = fit_to_budget(scored, 10);
        assert_eq!(kept_ids(&r), vec![BlockRef::Event(1)]);
    }

    #[test]
    fn deep_context_preview_reports_tokens_cap_and_premium_line() {
        let cap = DeepContextBudget::default();
        let p = deep_context_preview(150_000, cap);
        assert_eq!(
            p,
            DeepContextPreview {
                estimated_input_tokens: 150_000,
                cap_tokens: 200_000,
                premium_line_tokens: 272_000,
                within_cap: true,
            }
        );
        assert!(deep_context_preview(200_000, cap).within_cap);
        assert!(!deep_context_preview(200_001, cap).within_cap);
        assert!(deep_context_preview(0, cap).within_cap);
        let lowered = deep_context_preview(90_000, DeepContextBudget::from_setting(Some(80_000)));
        assert_eq!(lowered.cap_tokens, 80_000);
        assert!(!lowered.within_cap);
    }

    #[test]
    fn deep_context_preview_of_a_filled_pack_is_within_cap() {
        // A pack filled by fit_deep_context, estimated with the same estimator, always previews
        // as within the cap.
        let est = HeuristicEstimator::default();
        let scored: Vec<(ContextBlock, f64)> = (0..30)
            .map(|i| {
                let b = ContextBlock::new(
                    BlockRef::Thread(format!("t{i}")),
                    SourceKind::ThreadSummary,
                    "word ".repeat(2_000 + i * 97),
                    ScoreInputs { relevance: 0.5, freshness: 0.5, task_link: 0.0, confidence: 1.0 },
                    &est,
                );
                (b, (i % 7) as f64)
            })
            .collect();
        let budget = DeepContextBudget::from_setting(Some(40_000));
        let r = fit_deep_context(scored, budget);
        assert!(!r.dropped.is_empty());
        assert!(deep_context_preview(r.post_tokens, budget).within_cap);
    }

    // deep-context token range (issue #398)

    #[test]
    fn deep_context_tokens_never_exceed_the_cap_or_the_premium_line() {
        let huge = "word ".repeat(1_500_000); // ~1.9M chars, far over any cap
        let jp = "日本語の文章".repeat(200_000);
        for setting in [None, Some(0), Some(50_000), Some(200_000), Some(271_999), Some(272_000), Some(usize::MAX)] {
            let budget = DeepContextBudget::from_setting(setting);
            for texts in [vec![huge.as_str()], vec![jp.as_str()], vec![huge.as_str(), jp.as_str()], vec![]] {
                let t = deep_context_tokens(&texts, budget);
                assert!(t.estimated_input_tokens <= t.cap_tokens);
                assert!(t.cap_tokens <= DEEP_CONTEXT_DEFAULT_CAP_TOKENS);
                assert!(t.estimated_input_tokens < DEEP_CONTEXT_PREMIUM_LINE_TOKENS);
            }
            assert!(deep_context_tokens_at_cap(budget).estimated_input_tokens < DEEP_CONTEXT_PREMIUM_LINE_TOKENS);
        }
        let over = deep_context_tokens(&[huge.as_str()], DeepContextBudget::default());
        assert!(over.over_cap);
        assert_eq!(over.estimated_input_tokens, DEEP_CONTEXT_DEFAULT_CAP_TOKENS);
    }

    #[test]
    fn deep_context_range_is_plus_minus_ten_percent_for_latin_and_wider_for_cjk() {
        let budget = DeepContextBudget::default();
        let latin = deep_context_tokens(&["a".repeat(400_000).as_str()], budget); // 100K tokens
        assert_eq!(latin.estimated_input_tokens, 100_000);
        assert_eq!((latin.input_tokens_low, latin.input_tokens_high), (90_000, 110_000));
        assert_eq!(latin.cjk_share_pct, Some(0));
        assert!(!latin.approximate);
        assert!(!latin.may_cross_premium_line);
        assert_eq!(latin.output_tokens_max, DEEP_CONTEXT_OUTPUT_CAP_TOKENS);

        let jp = deep_context_tokens(&["あ".repeat(150_000).as_str()], budget); // 100K tokens
        assert_eq!(jp.estimated_input_tokens, 100_000);
        assert_eq!((jp.input_tokens_low, jp.input_tokens_high), (90_000, 150_000));
        assert_eq!(jp.cjk_share_pct, Some(100));
        assert!(jp.approximate);

        // Half and half sits in between.
        let mixed = deep_context_tokens(&["a".repeat(1_000).as_str(), "あ".repeat(1_000).as_str()], budget);
        assert_eq!(mixed.cjk_share_pct, Some(50));
        assert!(mixed.input_tokens_high > mixed.estimated_input_tokens * 110 / 100);
        assert!(mixed.input_tokens_high < mixed.estimated_input_tokens * 150 / 100 + 1);
        assert!(mixed.approximate);
    }

    #[test]
    fn deep_context_at_cap_assumes_the_worst_language_and_flags_the_premium_line() {
        let t = deep_context_tokens_at_cap(DeepContextBudget::default());
        assert_eq!(t.estimated_input_tokens, 200_000);
        assert_eq!(t.input_tokens_low, 180_000);
        // 200K estimated can bill ~300K in Japanese — above the 272K line.
        assert_eq!(t.input_tokens_high, 300_000);
        assert_eq!(t.cjk_share_pct, None);
        assert!(t.approximate && t.may_cross_premium_line);
        // A lowered cap keeps even the worst case under the line.
        let low = deep_context_tokens_at_cap(DeepContextBudget::from_setting(Some(150_000)));
        assert_eq!(low.input_tokens_high, 225_000);
        assert!(!low.may_cross_premium_line);
        // Nothing to send, nothing to bill.
        let empty = deep_context_tokens(&[], DeepContextBudget::default());
        assert_eq!((empty.estimated_input_tokens, empty.input_tokens_high), (0, 0));
    }
}
