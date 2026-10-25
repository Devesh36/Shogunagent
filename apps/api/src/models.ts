/**
 * Server-side model choice (docs/batch-relay-design.md §4.4).
 *
 * The device names an intent; the relay names the model. Changing a row here changes unit cost
 * for every customer, which is exactly why the choice cannot live on the device.
 */
import type { ModelClass, SystemTextBlock } from "./types.js";

/** Owner decision 2026-10-13: the background Batch classes run on OpenAI `gpt-6-luna` through
 * the OpenAI Batch API (`openai-gateway.ts`); the brief joined them 2026-10-16. `BATCH_PROVIDER=
 * anthropic` selects the Anthropic rows instead — Claude Haiku 5.5 (2026-10-12,
 * docs/unit-economics.md §6 step 1): Batch $0.05/$0.25 per MTok for prompts ≤100K tokens, 10×
 * below Haiku 4.5 — and Sonnet 5.5 for the brief. */
export const LUNA_MODEL = "gpt-6-luna";
export type BatchProvider = "openai" | "anthropic";

/** `BATCH_PROVIDER` as a provider, or a thrown error naming the variable. Unset = `openai`. */
export function batchProviderFromEnv(raw: string | undefined): BatchProvider {
  const v = raw?.trim() ?? "";
  if (v === "" || v === "openai") return "openai";
  if (v === "anthropic") return "anthropic";
  throw new Error("invalid BATCH_PROVIDER (openai | anthropic)");
}

// The provider in force. Read from `process.env` at import for the Node entry and the test suites
// (vitest sets BATCH_PROVIDER before the import); the Worker's vars are not in `process.env` while
// a module evaluates, so `createRelay` (boot.ts) calls `setBatchProvider` from its validated
// config before the first request. Every table below reads it at access time, never at import.
let provider: BatchProvider = process.env.BATCH_PROVIDER === "anthropic" ? "anthropic" : "openai";
export function batchProvider(): BatchProvider {
  return provider;
}
export function setBatchProvider(next: BatchProvider): void {
  provider = next;
}

const HAIKU = "claude-haiku-5-5";
const SONNET = "claude-sonnet-5-5";
const anthropic = (): boolean => provider === "anthropic";
/** Reasoning tokens count against luna's `max_completion_tokens`; Haiku 5.5's adaptive thinking
 * counts against `max_tokens` the same way, but its caps below were sized for that already. */
const headroom = (): number => (anthropic() ? 1 : 2);

const CLASSES: readonly ModelClass[] = ["classify", "summarize", "brief", "about_profile", "daily_activity_summary"];

/** A `Record<ModelClass, T>` whose values follow the provider in force (enumerable getters, so
 * `Object.keys` / spreading still see five rows). */
function byClass<T>(value: (modelClass: ModelClass) => T): Record<ModelClass, T> {
  const table = {} as Record<ModelClass, T>;
  for (const c of CLASSES) {
    Object.defineProperty(table, c, { enumerable: true, get: () => value(c) });
  }
  return table;
}

export const MODEL_BY_CLASS: Record<ModelClass, string> = byClass((c) => {
  // Every class, the brief included: luna by default. Under `BATCH_PROVIDER=anthropic`, Haiku 5.5
  // (no date suffix exists for it) and Sonnet 5.5 for the once-a-day brief. Adaptive thinking on
  // the 5.5 models counts toward `max_tokens`; `EFFORT_BY_CLASS` is the lever, never
  // `budget_tokens`, `temperature`, `top_p`, `top_k` or an assistant prefill (all 400 there).
  if (anthropic()) return c === "brief" ? SONNET : HAIKU;
  return LUNA_MODEL;
});

/** How much the model thinks, per class. A server decision like the model: the device never
 * chooses, and a device-sent value is never read. On Anthropic it is `output_config.effort` (both
 * Haiku 5.5 and Sonnet 5.5 think adaptively); on luna the gateway maps it to `reasoning_effort`.
 * `low` for classification/extraction over a fixed schema (the model may skip thinking entirely
 * on simple items); `medium` — sent explicitly so a default change upstream does not change our
 * cost — for free-form meeting recaps and for the brief, whose model would otherwise think at its
 * default `high` and spend most of `max_tokens` before the JSON. */
export type Effort = "low" | "medium";
export const EFFORT_BY_CLASS: Record<ModelClass, Effort> = {
  classify: "low",
  summarize: "medium",
  brief: "medium",
  about_profile: "low",
  daily_activity_summary: "low",
};

/** Output ceilings. Thinking / reasoning tokens count toward the cap and the 5.5 tokenizer yields
 * ~30% more tokens for the same text, so each cap leaves room for both; a cap sized only for the
 * answer ends in `stop_reason: "max_tokens"` after a thinking block and before any text (the relay
 * then returns an error entry, app.ts `parseResultLine`). The spend reservation prices the full
 * cap, so these are also what a submission reserves per item (spend.ts). */
export const MAX_TOKENS_BY_CLASS: Record<ModelClass, number> = byClass((c) => {
  switch (c) {
    // A consolidation verdict is ~150 tokens of JSON (≈200 on the new tokenizer); 1,024 was sized
    // for that alone. 2,048 leaves ~1,800 tokens for low-effort thinking on a dense item.
    case "classify": return 2048;
    // A recap with its evidence ran ~1,300 output tokens on a three-minute meeting, so 2,048 cut
    // long meetings mid-JSON (the device then kept the degraded recap). ×1.3 ≈ 1,700 for that
    // meeting, a few thousand for a long one, plus medium-effort thinking: 8,192 holds on Haiku;
    // luna's reasoning counts against the same cap, so it gets double.
    case "summarize": return 8192 * headroom();
    // ~1,500 tokens of brief JSON (≈2,000 on the new tokenizer) plus medium-effort thinking over a
    // whole day's events: at 2,048 the thinking alone could end the result before the JSON.
    case "brief": return 8192 * headroom();
    // ~800 output tokens (≈1,050 on the new tokenizer) plus low-effort thinking.
    case "about_profile": return 2048 * headroom();
    // ~500 output tokens (≈650) plus low-effort thinking.
    case "daily_activity_summary": return 2048 * headroom();
  }
});

/**
 * The provider's minimum cacheable prompt length for each class's model, in tokens (Anthropic
 * prompt-caching docs, "Cache limitations", checked 2026-10-12). A shorter prefix marked with
 * `cache_control` is processed uncached with no error. luna caches automatically upstream and has
 * no marker: `Infinity` keeps `cache_control` off its requests. Keep in step with `MODEL_BY_CLASS`
 * above and with the Rust copy for the direct dev path (`min_cacheable_prompt_tokens` in
 * crates/shogun-core/src/llm/anthropic.rs).
 */
export const MIN_CACHEABLE_TOKENS_BY_CLASS: Record<ModelClass, number> = byClass((c) => {
  return anthropic() ? 512 : Infinity; // Haiku/Sonnet 5.5 (Haiku 4.5 was 4,096) | luna
});

/** Estimated tokens of a text: 4 UTF-8 bytes ≈ 1 token, the same divisor the device's usage
 * estimate uses (`system_prefix_is_cacheable` in crates/shogun-core/src/llm/anthropic.rs). Used
 * ONLY to decide the cache-breakpoint threshold below, where it must agree with the device; spend
 * reservation uses `reserveInputTokens`. Never for billing. */
export function estimateTokens(text: string): number {
  return Math.ceil(Buffer.byteLength(text, "utf8") / 4);
}

/** Tokens to RESERVE for a text against the spend budgets (audit 2026-10-10 M9): 2 UTF-8 bytes
 * per token. Real text tokenizes near 4 bytes/token, but adversarial input (dense punctuation,
 * code, CJK, base64) reaches 2 or below, so a 4-byte divisor would under-reserve by 2–3× exactly
 * when it matters. Over-reserving only trips a breaker early, which is the safe direction; the
 * settlement sweep (boot.ts) replaces the reservation with the provider-reported usage. */
export function reserveInputTokens(text: string): number {
  return Math.ceil(Buffer.byteLength(text, "utf8") / RESERVE_BYTES_PER_TOKEN);
}

export const RESERVE_BYTES_PER_TOKEN = 2;

/**
 * The `system` blocks for a shared instruction (#667). A 1-hour `cache_control` breakpoint is
 * added only when the instruction reaches the class model's cacheable minimum: below it the
 * marker is silently ignored upstream, so sending it would only make the request claim a cache
 * that never engages. One hour rather than the 5-minute default because a batch's requests run
 * concurrently over minutes to an hour; the provider recommends it for batches with shared
 * context. The relay — not the device — decides this, because the TTL changes the write price.
 *
 * Today's consolidation instruction is ~2.9 KB ≈ 716 estimated tokens, above Haiku 5.5's 512, so
 * classify requests carry the breakpoint on the Anthropic rows (the first request of a batch
 * writes it at 2× input, the rest read it at 0.1×; the reservation prices every item at the write
 * rate, which is the upper bound). On luna the marker is never added (the gateway drops
 * `cache_control` anyway; the provider caches on its own).
 */
export function systemBlocksFor(modelClass: ModelClass, system: string): SystemTextBlock[] {
  if (estimateTokens(system) >= MIN_CACHEABLE_TOKENS_BY_CLASS[modelClass]) {
    return [{ type: "text", text: system, cache_control: { type: "ephemeral", ttl: "1h" } }];
  }
  return [{ type: "text", text: system }];
}
