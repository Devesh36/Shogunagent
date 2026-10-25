/**
 * Cost accounting for the global daily spend breaker (issue #29 FR-SVC-08, with #286).
 *
 * The per-licence chunk cap bounds how many items one licence can send; it says nothing about
 * dollars. Many licences × large chunks × the more expensive `brief` model is a bill the chunk cap
 * cannot see. The breaker is one global number: micro-USD booked against today's UTC date. When
 * a submission would cross it, the relay answers the cap-reached 429 (`reason: "spend_cap"`) and
 * every device gives that night to its local lane.
 *
 * How the number is kept honest in both directions:
 * - **At submission, an upper bound is reserved** (same transaction as the chunk budget). Input is
 *   reserved from bytes at a conservative 2 bytes ≈ 1 token (`reserveInputTokens`, models.ts;
 *   audit 2026-10-10 M9), output at the
 *   class's full `max_tokens`, and a system prompt carrying a cache breakpoint at the 1-hour cache
 *   *write* rate. Over-reserving only trips the breaker early, which is the safe direction.
 * - **At results, the estimate is replaced by the provider-reported usage**, exactly once per
 *   batch (`UsageStore.settleSpend`), so a night's real spend is what the ledger ends up holding.
 *   Two paths call it and the first wins: the client reading its results through the relay, and
 *   the relay's own settlement sweep (boot.ts `settleOnce`), which polls ended batches so a client
 *   that never reads cannot leave a night's estimate standing.
 *
 * The same estimate is reserved a second time against the **per-licence daily budget**
 * (`LICENSE_DAILY_SPEND_CAP_USD`, `TRIAL_DAILY_SPEND_CAP_USD`; audit H1): the global breaker says
 * how much the company spends in a day, the per-licence budget says how much of it one licence
 * may be. Together a leaked or bought licence is bounded to a few dollars a day instead of to the
 * whole breaker.
 *
 * The provider console's hard limit (runbook §2-5) stays the final wall; this is the gateway's own.
 * Prices are Batch API rates (50% of standard) in USD per million tokens. Keep this table in step
 * with `MODEL_BY_CLASS` (models.ts) and the provider's pricing page; `SPEND_PRICE_TABLE_JSON`
 * changes rows without a code change, merged over this table (`priceTableWithOverride`), and a
 * model in neither is priced at no less than `UNKNOWN_MODEL_FLOOR`.
 */
import { MAX_TOKENS_BY_CLASS, MODEL_BY_CLASS, reserveInputTokens } from "./models.js";
import type { AnthropicBatchRequestItem, ModelClass, RelayUsage } from "./types.js";

/** One rate card: USD per million tokens at Batch rates. */
export interface RateCard {
  /** Uncached input tokens. */
  input: number;
  /** Output tokens (adaptive `thinking` is billed as output and is already in `output_tokens`). */
  output: number;
  /** Cache-read input tokens. */
  cacheRead: number;
  /** Cache-write input tokens at the 1-hour TTL the relay uses. */
  cacheWrite: number;
}

/** A model's price row: its rate card, plus — for a model the provider bills on two cards by
 * prompt length (Haiku 5.5) — the card for prompts over `LOW_RATE_CARD_MAX_PROMPT_TOKENS`.
 * `rateCardFor` picks between them; nothing else reads `longPrompt` directly. */
export interface ModelPrice extends RateCard {
  longPrompt?: RateCard;
}

export type PriceTable = Record<string, ModelPrice>;

/** Haiku 5.5's rate-card boundary, in prompt tokens: a request whose prompt (`input_tokens` +
 * cache reads + cache writes, as the provider counts it) is longer than this is billed on the
 * `longPrompt` card — 10× the base card. The relay does not try to keep requests under it by
 * construction (a 192 KiB meeting transcript in CJK at ~2 bytes/token is a legitimate ~98K-token
 * prompt that a 64 KiB `system` pushes over); it prices whichever card the prompt lands on, at
 * reservation (from the bytes/2 estimate) and at settlement (from the reported counts). */
export const LOW_RATE_CARD_MAX_PROMPT_TOKENS = 100_000;

/** Batch prices (checked 2026-10-12). Standard list prices: Haiku 4.5 $1/$5, Haiku 5.5 $0.10/$0.50
 * up to 100K prompt tokens and $1/$5 above, Sonnet 5 and 5.5 $2/$10. Batch is 50% of those; cache
 * read is 0.1× input and a 1-hour cache write is 2× input (the 5-minute write would be 1.25×, but
 * the relay only ever sends `ttl: "1h"`, so the table holds the 1-hour figure), applied to the
 * Batch input rate.
 *
 * Haiku 4.5 stays in the table: a batch submitted before the switch, or an older desktop's
 * settlement, still reports results under that model id and must be priced, not fall through to
 * the worst-case row. */
export const DEFAULT_BATCH_PRICES: PriceTable = {
  // OpenAI GPT-6 (developers.openai.com/api/docs/models/gpt-6-luna and -sol, checked 2026-10-13):
  // Standard Luna $0.10 in / $0.01 cached / $0.125 cache write / $0.50 out; Sol $2 / $0.20 / $2.50 /
  // $10. Batch is 50% of Standard (stated on the model pages; no separate Batch table is
  // published). Luna's lane sends no cache marker, so the write rate is only a ceiling.
  // ponytail: no longPrompt card — OpenAI doubles input / 1.5x output above 272K prompt tokens, but
  // `rateCardFor` has one fixed 100K boundary (Haiku 5.5). Relay prompts top out near 131K reserved
  // tokens, so the base card holds.
  "gpt-6-luna": { input: 0.05, output: 0.25, cacheRead: 0.005, cacheWrite: 0.0625 },
  "gpt-6-sol": { input: 1, output: 5, cacheRead: 0.1, cacheWrite: 1.25 },
  "claude-haiku-5-5": {
    input: 0.05, output: 0.25, cacheRead: 0.005, cacheWrite: 0.1,
    longPrompt: { input: 0.5, output: 2.5, cacheRead: 0.05, cacheWrite: 1 },
  },
  "claude-haiku-4-5-20251001": { input: 0.5, output: 2.5, cacheRead: 0.05, cacheWrite: 1 },
  "claude-sonnet-5": { input: 1, output: 5, cacheRead: 0.1, cacheWrite: 2 },
  "claude-sonnet-5-5": { input: 1, output: 5, cacheRead: 0.1, cacheWrite: 2 },
  // TypeSafe Jev form decisions (issue #1144, `/v1/ai/form_decide`): a synchronous call, not a
  // batch, so this is TypeSafe's list price — $0.042 per million input tokens, output free. Jev
  // has no prompt cache; the cache columns repeat the input rate so nothing reads as free.
  "jev-1.13.0": { input: 0.042, output: 0, cacheRead: 0.042, cacheWrite: 0.042 },
};

const CARD_KEYS = ["input", "output", "cacheRead", "cacheWrite"] as const;

/** The floor under the price of a model the table does not know (audit 2026-10-20 L2), whatever
 * the table holds: the most expensive card the relay has ever shipped (Sonnet 5 / 5.5 at Batch
 * rates), hard-coded so that an override table of cheap rows cannot make an unknown model cheap.
 * A test keeps it at or above every row of `DEFAULT_BATCH_PRICES`. */
export const UNKNOWN_MODEL_FLOOR: RateCard = Object.freeze({ input: 1, output: 5, cacheRead: 0.1, cacheWrite: 2 });

/** The most expensive card in the table, field by field and across both cards of every row, and
 * never below `UNKNOWN_MODEL_FLOOR`; used for a model missing from the table: an unknown price
 * must not read as free, or as cheap. */
function worstCase(prices: PriceTable): ModelPrice {
  const cards: RateCard[] = [UNKNOWN_MODEL_FLOOR, ...Object.values(prices).flatMap(r => (r.longPrompt ? [r, r.longPrompt] : [r]))];
  const worst = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 };
  for (const k of CARD_KEYS) worst[k] = Math.max(0, ...cards.map(c => c[k]));
  return worst;
}

/** A provider-reported model id with a dated-snapshot suffix (`gpt-6-luna-2026-10-01`,
 * `…-20261001`) is the same model as its alias for pricing. Only an exact date suffix is
 * stripped: `gpt-6-luna-pro` is a different model and stays unknown. */
const SNAPSHOT_SUFFIX = /-(?:\d{4}-\d{2}-\d{2}|\d{8})$/;

/** The price row of `model` (or of the alias of its dated snapshot), or the worst case for a model
 * the table does not know. */
export function priceFor(prices: PriceTable, model: string | undefined): ModelPrice {
  if (model === undefined) return worstCase(prices);
  return prices[model] ?? (SNAPSHOT_SUFFIX.test(model) ? prices[model.replace(SNAPSHOT_SUFFIX, "")] : undefined) ?? worstCase(prices);
}

/** The card a prompt of `promptTokens` is billed on: the long-prompt card past the boundary when
 * the row has one, the base card otherwise. */
export function rateCardFor(price: ModelPrice, promptTokens: number): RateCard {
  return price.longPrompt !== undefined && promptTokens > LOW_RATE_CARD_MAX_PROMPT_TOKENS ? price.longPrompt : price;
}

/** Parse `SPEND_PRICE_TABLE_JSON` (same shape as `DEFAULT_BATCH_PRICES`; `longPrompt` optional per
 * row, so a table written before it existed still parses as single-card rows). Anything malformed
 * is refused at boot rather than silently pricing a model at zero. */
export function priceTableFromJson(raw: string): PriceTable {
  const parsed: unknown = JSON.parse(raw);
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) throw new Error("price table must be an object");
  const card = (row: unknown, where: string): RateCard => {
    const r = (typeof row === "object" && row !== null ? row : {}) as Record<string, unknown>;
    const n = (k: string): number => {
      const v = r[k];
      if (typeof v !== "number" || !Number.isFinite(v) || v < 0) throw new Error(`price table: ${k} for ${where} is not a non-negative number`);
      return v;
    };
    return { input: n("input"), output: n("output"), cacheRead: n("cacheRead"), cacheWrite: n("cacheWrite") };
  };
  const out: PriceTable = {};
  for (const [model, row] of Object.entries(parsed as Record<string, unknown>)) {
    const base = card(row, "a model");
    const long = (row as Record<string, unknown> | null)?.longPrompt;
    out[model] = long === undefined ? base : { ...base, longPrompt: card(long, "a model's longPrompt card") };
  }
  if (Object.keys(out).length === 0) throw new Error("price table is empty");
  return out;
}

/** The table in force for a `SPEND_PRICE_TABLE_JSON` value (audit 2026-10-20 L2): its rows MERGED
 * OVER `DEFAULT_BATCH_PRICES`, never replacing it. An override written to tighten one model must
 * not drop every other row to the unknown-model price (or, before the floor, to whatever the
 * override's own most expensive row was). Unset or blank = the defaults. */
export function priceTableWithOverride(raw: string | undefined): PriceTable {
  if (raw === undefined || raw.trim() === "") return DEFAULT_BATCH_PRICES;
  return { ...DEFAULT_BATCH_PRICES, ...priceTableFromJson(raw) };
}

/** `DAILY_SPEND_CAP_USD` → micro-USD, or `undefined` (breaker disabled) when unset/blank. Whether
 * "unset" is allowed to boot is decided in boot.ts (`spendCapRequirementError`): production needs
 * the cap, and only `RELAY_ALLOW_NO_SPEND_CAP=1` (development/tests) may run without it. */
export function spendCapMicrosFromEnv(raw: string | undefined): number | undefined {
  if (raw === undefined || raw.trim() === "") return undefined;
  return usdToMicros(raw, "DAILY_SPEND_CAP_USD");
}

/** A USD amount from the environment as micro-USD: plain decimal (`3`, `3.00`, `0.5`), never
 * negative, never a unit suffix. `fallbackUsd` when unset or blank. Throws naming the variable. */
export function usdCapMicrosFromEnv(raw: string | undefined, name: string, fallbackUsd: number): number {
  if (raw === undefined || raw.trim() === "") return Math.round(fallbackUsd * 1_000_000);
  return usdToMicros(raw, name);
}

function usdToMicros(raw: string, name: string): number {
  const text = raw.trim();
  const usd = /^\d+(\.\d+)?$/.test(text) ? Number(text) : Number.NaN;
  if (!Number.isFinite(usd) || usd < 0) throw new Error(`${name} must be a non-negative decimal number of USD`);
  const micros = Math.round(usd * 1_000_000);
  if (!Number.isSafeInteger(micros)) throw new Error(`${name} is too large`);
  return micros;
}

// The per-licence and per-trial budget defaults live in boot.ts (DEFAULT_*_DAILY_SPEND_CAP_USD),
// next to the parsing that applies them; docs/unit-economics.md §3.3 derives the values.

/** USD/MTok × tokens = micro-USD, because 1 USD / 1M tokens is exactly 1 micro-USD per token. */
function micros(tokens: number, usdPerMTok: number): number {
  return tokens * usdPerMTok;
}

/** The prompt length the provider's rate-card boundary is measured against. */
function promptTokensOf(usage: RelayUsage): number {
  return usage.input_tokens + usage.cache_read_input_tokens + usage.cache_creation_input_tokens;
}

/** Actual cost of one result, from the provider-reported token counts, on the card its prompt
 * length lands on. `output_tokens` already includes any `thinking` blocks (Haiku 5.5), so nothing
 * is added for them here. */
export function usageMicros(usage: RelayUsage, price: ModelPrice): number {
  const card = rateCardFor(price, promptTokensOf(usage));
  return micros(usage.input_tokens, card.input) + micros(usage.output_tokens, card.output) +
    micros(usage.cache_read_input_tokens, card.cacheRead) + micros(usage.cache_creation_input_tokens, card.cacheWrite);
}

/** Upper-bound cost of a submission (see the module header). Integer micro-USD, rounded up. Each
 * request is priced on the card its own reserved prompt length selects, so a long transcript
 * reserves at the long-prompt card and a short item next to it does not. */
export function estimateSubmissionMicros(modelClass: ModelClass, requests: AnthropicBatchRequestItem[], prices: PriceTable): number {
  const price = priceFor(prices, MODEL_BY_CLASS[modelClass]);
  let total = 0;
  for (const r of requests) {
    // Reserved prompt tokens by kind: a cache breakpoint bills its prefix at the write rate at
    // worst; without one, plain input. The structured-output schema is prompt input too.
    let cacheWrite = 0;
    let input = 0;
    for (const block of r.params.system ?? []) {
      if (block.cache_control) cacheWrite += reserveInputTokens(block.text);
      else input += reserveInputTokens(block.text);
    }
    for (const m of r.params.messages) input += reserveInputTokens(m.content);
    if (r.params.output_config?.format) input += reserveInputTokens(JSON.stringify(r.params.output_config.format.schema));
    const card = rateCardFor(price, cacheWrite + input);
    total += micros(cacheWrite, Math.max(card.cacheWrite, card.input)) + micros(input, card.input);
    // The full cap: adaptive thinking is billed as output and counts toward it.
    total += micros(MAX_TOKENS_BY_CLASS[modelClass], card.output);
  }
  return Math.ceil(total);
}

/** The breaker's configuration. Absent = disabled (the owner has not set a cap, §8-4). */
export interface SpendBreaker {
  capMicros: number;
  prices: PriceTable;
}
