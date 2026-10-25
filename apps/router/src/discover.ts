/**
 * `discover(task, budget_micro_usd?, limit?)` (E8-T3; implementation-plan §7.4). Ranks the
 * catalog by task fit, price, health and latency with one published formula. No model call.
 *
 * Invariant 13 (no self-preferencing): first-party entries go through the same formula, are
 * labeled `first_party`, and price is a component of the score — not a filter applied afterwards
 * and not a tie-breaker only. The weights are returned with every response so an agent can
 * re-derive the ranking.
 */
import type { Category } from "../connectors/categories.js";
import type { CatalogEntry, CompiledCatalog, PublicUsage } from "./catalog.js";
import type { HealthStore } from "./health.js";
import { FREE_USAGE, type Envelope } from "./responses.js";
import type { Level } from "./scope.js";

/**
 * Quality weights (sum to 1). Task fit is not a weight: it multiplies the quality sum, so a cheap,
 * fast tool that barely fits the task cannot outrank one that fits it well — a weighted sum would
 * let a $0 first-party read win on price alone, which is self-preferencing by construction.
 */
export const RANKING_WEIGHTS = {
  price: 0.45,
  health: 0.3,
  latency: 0.25,
  /** Phase 2: per-user tool × task × outcome from the state layer (V94). Always 0 until then. */
  personal: 0,
} as const;

export const RANKING_FORMULA =
  "score = task_fit × (0.45*price + 0.30*health + 0.25*latency + 0*personal). " +
  "task_fit = BM25(task over id+summary×2+categories×2+description+notes; exclusions not indexed) / query bound, in [0,1). " +
  "price = 1 / (1 + ln(1 + quote_micro_usd / 1000)) — $0 scores 1, strictly decreasing in price. " +
  "health = observed success_rate (declared baseline until 20 runs). " +
  "latency = 1 / (1 + p95_ms / 1000). personal = 0 (Phase 2). " +
  "Ties: lower quote first, then tool_id. Candidates with task_fit = 0 are not returned. First-party tools use the same formula and are labeled first_party.";

export function combineScore(b: ScoreBreakdown): number {
  return b.task_fit * (RANKING_WEIGHTS.price * b.price + RANKING_WEIGHTS.health * b.health + RANKING_WEIGHTS.latency * b.latency + RANKING_WEIGHTS.personal * b.personal);
}

export function priceScore(quoteMicroUsd: number): number {
  return 1 / (1 + Math.log(1 + Math.max(0, quoteMicroUsd) / 1000));
}

export function latencyScore(p95Ms: number): number {
  return 1 / (1 + Math.max(0, p95Ms) / 1000);
}

export interface ScoreBreakdown {
  task_fit: number;
  price: number;
  health: number;
  latency: number;
  personal: number;
}

export interface DiscoverCandidate {
  tool_id: string;
  provider_id: string;
  first_party: boolean;
  summary: string;
  categories: Category[];
  permission_level: Level;
  score: number;
  scoreBreakdown: ScoreBreakdown;
  price: PublicUsage & { quote_micro_usd: number };
  p50_ms: number;
  p95_ms: number;
}

export interface DiscoverInput {
  task: string;
  budget_micro_usd?: number;
  limit?: number;
  categories?: Category[];
}

export interface DiscoverResultBody {
  candidates: DiscoverCandidate[];
  ranking: { formula: string; weights: typeof RANKING_WEIGHTS };
  catalog_hash: string;
  /** Number of catalog entries considered before the budget filter. */
  considered: number;
}

export const DEFAULT_LIMIT = 10;
export const MAX_LIMIT = 50;

function round(n: number): number {
  return Math.round(n * 10_000) / 10_000;
}

export function scoreEntry(catalog: CompiledCatalog, health: HealthStore, entry: CatalogEntry, task: string): DiscoverCandidate {
  const live = health.get(entry.tool_id, entry.health);
  const breakdown: ScoreBreakdown = {
    task_fit: round(catalog.index.fit(entry.tool_id, task)),
    price: round(priceScore(entry.quote_micro_usd)),
    health: round(live.success_rate),
    latency: round(latencyScore(live.p95_ms)),
    personal: 0,
  };
  const score = combineScore(breakdown);
  return {
    tool_id: entry.tool_id,
    provider_id: entry.provider_id,
    first_party: entry.first_party,
    summary: entry.summary,
    categories: entry.categories,
    permission_level: entry.scope.level,
    score: round(score),
    scoreBreakdown: breakdown,
    price: { ...entry.usage, quote_micro_usd: entry.quote_micro_usd },
    p50_ms: live.p50_ms,
    p95_ms: live.p95_ms,
  };
}

export function discover(catalog: CompiledCatalog, health: HealthStore, input: DiscoverInput, now: () => number = Date.now): Envelope<DiscoverResultBody> {
  const start = now();
  const limit = Math.min(MAX_LIMIT, Math.max(1, input.limit ?? DEFAULT_LIMIT));
  let pool = catalog.entries();
  if (input.categories?.length) pool = pool.filter((e) => e.categories.some((c) => input.categories?.includes(c)));
  const considered = pool.length;
  if (input.budget_micro_usd !== undefined) pool = pool.filter((e) => e.quote_micro_usd <= (input.budget_micro_usd ?? 0));

  const candidates = pool
    .map((e) => scoreEntry(catalog, health, e, input.task))
    .filter((c) => c.scoreBreakdown.task_fit > 0)
    .sort((a, b) => b.score - a.score || a.price.quote_micro_usd - b.price.quote_micro_usd || a.tool_id.localeCompare(b.tool_id))
    .slice(0, limit);

  return {
    result: { candidates, ranking: { formula: RANKING_FORMULA, weights: RANKING_WEIGHTS }, catalog_hash: catalog.manifest.hash, considered },
    usage: FREE_USAGE,
    latency_ms: now() - start,
    hints: [],
    next_action: candidates.length === 0 ? "broaden_query" : null,
  };
}
