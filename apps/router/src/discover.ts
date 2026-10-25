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
import { latencyScore, priceScore } from "./scores.js";
import { explainPersonal, PERSONAL_COMPONENTS, PERSONAL_FORMULA, PERSONAL_MIN_RUNS, taskCluster, type PersonalExplanation, type PersonalProfile } from "./personal.js";
import type { Level } from "./scope.js";

export { latencyScore, priceScore };

/**
 * The E8 quality mix (sums to 1): price, health and latency of the tool, the same for every caller.
 */
export const BASE_QUALITY_WEIGHTS = { price: 0.45, health: 0.3, latency: 0.25 } as const;

/** The published weight of the personal factor (E20). The base mix shares the remaining 1 − w. */
export const PERSONAL_WEIGHT = 0.15;

/**
 * Quality weights (sum to 1): the base mix scaled by 1 − PERSONAL_WEIGHT, plus PERSONAL_WEIGHT on
 * `personal`. Task fit is not a weight: it multiplies the quality sum, so a cheap, fast tool that
 * barely fits the task cannot outrank one that fits it well — a weighted sum would let a $0
 * first-party read win on price alone, which is self-preferencing by construction.
 */
export const RANKING_WEIGHTS = {
  price: round4(BASE_QUALITY_WEIGHTS.price * (1 - PERSONAL_WEIGHT)),
  health: round4(BASE_QUALITY_WEIGHTS.health * (1 - PERSONAL_WEIGHT)),
  latency: round4(BASE_QUALITY_WEIGHTS.latency * (1 - PERSONAL_WEIGHT)),
  personal: PERSONAL_WEIGHT,
} as const;

function round4(n: number): number {
  return Math.round(n * 10_000) / 10_000;
}

export const RANKING_FORMULA =
  `score = task_fit × (${RANKING_WEIGHTS.price}*price + ${RANKING_WEIGHTS.health}*health + ${RANKING_WEIGHTS.latency}*latency + ${RANKING_WEIGHTS.personal}*personal). ` +
  "task_fit = BM25(task over id+summary×2+categories×2+description+notes; exclusions not indexed) / query bound, in [0,1). " +
  "price = 1 / (1 + ln(1 + quote_micro_usd / 1000)) — $0 scores 1, strictly decreasing in price. " +
  "health = observed success_rate (declared baseline until 20 runs). " +
  "latency = 1 / (1 + p95_ms / 1000). " +
  `personal = this principal's own outcome history with the tool for this kind of task (see ranking.personal); neutral below ${PERSONAL_MIN_RUNS} runs or without an agent key, where it equals 0.45*price + 0.30*health + 0.25*latency and the score is unchanged. ` +
  "Ties: lower quote first, then tool_id. Candidates with task_fit = 0 are not returned. First-party tools use the same formula and are labeled first_party.";

export function combineScore(b: ScoreBreakdown): number {
  return b.task_fit * (RANKING_WEIGHTS.price * b.price + RANKING_WEIGHTS.health * b.health + RANKING_WEIGHTS.latency * b.latency + RANKING_WEIGHTS.personal * b.personal);
}

/** The base quality mix of a candidate: the neutral personal value (no history → no effect). */
export function baseQuality(b: Pick<ScoreBreakdown, "price" | "health" | "latency">): number {
  return BASE_QUALITY_WEIGHTS.price * b.price + BASE_QUALITY_WEIGHTS.health * b.health + BASE_QUALITY_WEIGHTS.latency * b.latency;
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
  /** Why `scoreBreakdown.personal` has its value (E20). */
  personal: PersonalExplanation;
}

export interface DiscoverInput {
  task: string;
  budget_micro_usd?: number;
  limit?: number;
  categories?: Category[];
}

export interface DiscoverResultBody {
  candidates: DiscoverCandidate[];
  ranking: {
    formula: string;
    weights: typeof RANKING_WEIGHTS;
    personal: { formula: string; components: typeof PERSONAL_COMPONENTS; min_runs: number; basis: "principal" | "none"; cluster: string | null };
  };
  catalog_hash: string;
  /** Number of catalog entries considered before the budget filter. */
  considered: number;
}

export const DEFAULT_LIMIT = 10;
export const MAX_LIMIT = 50;

function round(n: number): number {
  return Math.round(n * 10_000) / 10_000;
}

/** One principal's history, already loaded (src/engine.ts fetches it from the PersonalStore). */
export interface PersonalContext {
  profile: PersonalProfile;
}

export function scoreEntry(catalog: CompiledCatalog, health: HealthStore, entry: CatalogEntry, task: string, personal?: PersonalContext, cluster: string | null = taskCluster(catalog.index, task)): DiscoverCandidate {
  const live = health.get(entry.tool_id, entry.health);
  const base = {
    task_fit: round(catalog.index.fit(entry.tool_id, task)),
    price: round(priceScore(entry.quote_micro_usd)),
    health: round(live.success_rate),
    latency: round(latencyScore(live.p95_ms)),
  };
  const explanation = explainPersonal(personal?.profile, entry.tool_id, cluster, baseQuality(base));
  const breakdown: ScoreBreakdown = { ...base, personal: round(explanation.value) };
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
    personal: { ...explanation, value: round(explanation.value) },
  };
}

/**
 * Ranks the catalog for a task. `personal` is the calling principal's own history (absent for
 * anonymous callers); it can only re-weight that caller's ranking and is never shared.
 */
export function discover(catalog: CompiledCatalog, health: HealthStore, input: DiscoverInput, now: () => number = Date.now, personal?: PersonalContext): Envelope<DiscoverResultBody> {
  const start = now();
  const limit = Math.min(MAX_LIMIT, Math.max(1, input.limit ?? DEFAULT_LIMIT));
  let pool = catalog.entries();
  if (input.categories?.length) pool = pool.filter((e) => e.categories.some((c) => input.categories?.includes(c)));
  const considered = pool.length;
  if (input.budget_micro_usd !== undefined) pool = pool.filter((e) => e.quote_micro_usd <= (input.budget_micro_usd ?? 0));

  const cluster = taskCluster(catalog.index, input.task);
  const candidates = pool
    .map((e) => scoreEntry(catalog, health, e, input.task, personal, cluster))
    .filter((c) => c.scoreBreakdown.task_fit > 0)
    .sort((a, b) => b.score - a.score || a.price.quote_micro_usd - b.price.quote_micro_usd || a.tool_id.localeCompare(b.tool_id))
    .slice(0, limit);

  return {
    result: {
      candidates,
      ranking: {
        formula: RANKING_FORMULA,
        weights: RANKING_WEIGHTS,
        personal: { formula: PERSONAL_FORMULA, components: PERSONAL_COMPONENTS, min_runs: PERSONAL_MIN_RUNS, basis: personal ? "principal" : "none", cluster },
      },
      catalog_hash: catalog.manifest.hash,
      considered,
    },
    usage: FREE_USAGE,
    latency_ms: now() - start,
    hints: [],
    next_action: candidates.length === 0 ? "broaden_query" : null,
  };
}
