/**
 * Personalised `discover` (E20; README §7 decision 4 "the router learns in user-owned state").
 *
 * From the `ToolRunRecord` stream (src/runlog.ts — body-less, `actor` is a hash of the key) this
 * module learns, per principal, a tool × task-cluster outcome model: runs, successes, runs that
 * followed a hint (and how many of those succeeded) and the cost spent. `discover` turns it into
 * `scoreBreakdown.personal`, a number in [0, 1] weighted like every other factor
 * (src/discover.ts `RANKING_WEIGHTS.personal`).
 *
 * Task clusters come from the same BM25 tokenizer `discover` ranks with (src/bm25.ts): the
 * cluster of a task phrase is its `CLUSTER_TERMS` most distinctive stemmed terms (highest catalog
 * IDF, ties alphabetical), sorted and joined. No model call, no embedding, the same for every
 * caller — and recomputable from the `task_hint` column of `tool_runs`.
 *
 * Isolation. `PersonalStore` is keyed by principal and has no method that reads across
 * principals: `record` writes one principal's row, `profile` returns one principal's rows. The
 * in-memory store here is for development and tests. In production the store is the principal's
 * own ShogunAgent state — the `tool_runs` table (V94) in that principal's SQLCipher file, reached
 * through the state sidecar — never a hosted shared database (invariants 2 and 6). The hosted
 * router keeps no personal data of its own.
 */
import type { Bm25Index } from "./bm25.js";
import { tokenize } from "./bm25.js";
import { priceScore } from "./scores.js";
import type { RunLogSink, RunOutcome, ToolRunRecord } from "./runlog.js";

/** Below this many counted runs the personal factor is neutral (no effect on the ranking). */
export const PERSONAL_MIN_RUNS = 5;

/** How many terms name a task cluster. */
export const CLUSTER_TERMS = 3;

/**
 * Outcomes that say something about the tool. Permission stops (input_required,
 * approval_required, denied), the caller's own mistakes (invalid_input, authentication_required,
 * payment_required) and idempotent replays are not evidence about the tool and are not counted.
 */
export const COUNTED_OUTCOMES: ReadonlySet<RunOutcome> = new Set<RunOutcome>(["ok", "empty", "provider_error", "transport_error"]);

/** Components of the personal value (sum to 1). Published with every discover response. */
export const PERSONAL_COMPONENTS = {
  /** successes / runs. */
  success_rate: 0.6,
  /** priceScore(cost per success): what this principal actually paid per useful result. 0 without a success. */
  cost_per_success: 0.25,
  /** Success rate of the runs where the caller followed a hint to this tool (success_rate when none). */
  hint_success: 0.15,
} as const;

export const PERSONAL_FORMULA =
  `personal = ${PERSONAL_COMPONENTS.success_rate}*success_rate + ${PERSONAL_COMPONENTS.cost_per_success}*price(cost_micro_usd / successes) + ${PERSONAL_COMPONENTS.hint_success}*hint_success_rate, ` +
  `over this principal's runs of the tool in the task's cluster (or of the tool across clusters when the cluster has fewer than ${PERSONAL_MIN_RUNS} runs). ` +
  `Counted outcomes: ok (success), empty, provider_error, transport_error. ` +
  `With fewer than ${PERSONAL_MIN_RUNS} counted runs, or without a valid agent key, personal is neutral: it equals the candidate's own price/health/latency mix, so the score is exactly the unpersonalised score.`;

export interface OutcomeStats {
  runs: number;
  successes: number;
  hint_followed_runs: number;
  hint_followed_successes: number;
  cost_micro_usd: number;
}

export interface PersonalObservation {
  tool_id: string;
  /** `taskCluster(...)` of the run's task_hint; null when the caller sent none. */
  cluster: string | null;
  success: boolean;
  hint_followed: boolean;
  cost_micro_usd: number;
}

export interface PersonalRow extends OutcomeStats {
  tool_id: string;
  cluster: string | null;
}

/** One principal's outcome model. Never contains another principal's rows. */
export interface PersonalProfile {
  principal: string;
  rows: readonly PersonalRow[];
}

/**
 * Where per-principal outcome rows live. Every method takes exactly one principal; there is no
 * listing or cross-principal read. Production: the principal's `tool_runs` (V94) via the sidecar.
 */
export interface PersonalStore {
  record(principal: string, obs: PersonalObservation): void | Promise<void>;
  profile(principal: string): PersonalProfile | Promise<PersonalProfile>;
}

const emptyStats = (): OutcomeStats => ({ runs: 0, successes: 0, hint_followed_runs: 0, hint_followed_successes: 0, cost_micro_usd: 0 });

function addTo(s: OutcomeStats, obs: PersonalObservation): void {
  s.runs += 1;
  if (obs.success) s.successes += 1;
  if (obs.hint_followed) {
    s.hint_followed_runs += 1;
    if (obs.success) s.hint_followed_successes += 1;
  }
  s.cost_micro_usd += Math.max(0, obs.cost_micro_usd);
}

/** Development and test store. One map per principal; rows are copied out, never shared. */
export class InMemoryPersonalStore implements PersonalStore {
  readonly #byPrincipal = new Map<string, Map<string, PersonalRow>>();

  record(principal: string, obs: PersonalObservation): void {
    let rows = this.#byPrincipal.get(principal);
    if (!rows) {
      rows = new Map();
      this.#byPrincipal.set(principal, rows);
    }
    const key = `${obs.tool_id}\u0000${obs.cluster ?? ""}`;
    let row = rows.get(key);
    if (!row) {
      row = { tool_id: obs.tool_id, cluster: obs.cluster, ...emptyStats() };
      rows.set(key, row);
    }
    addTo(row, obs);
  }

  profile(principal: string): PersonalProfile {
    const rows = this.#byPrincipal.get(principal);
    return { principal, rows: rows ? [...rows.values()].map((r) => ({ ...r })) : [] };
  }
}

/** The cluster key of a task phrase: its most distinctive stemmed terms, sorted. Null when none. */
export function taskCluster(index: Bm25Index, text: string | null | undefined): string | null {
  if (!text) return null;
  const terms = [...new Set(tokenize(text))];
  if (terms.length === 0) return null;
  return terms
    .map((t) => ({ t, idf: index.idf(t) }))
    .sort((a, b) => b.idf - a.idf || (a.t < b.t ? -1 : a.t > b.t ? 1 : 0))
    .slice(0, CLUSTER_TERMS)
    .map((x) => x.t)
    .sort()
    .join(" ");
}

export type PersonalBasis = "principal" | "none";

/** What every discover candidate carries to explain its personal factor. */
export interface PersonalExplanation {
  /** The value used in the score, in [0, 1]. Neutral (the candidate's own quality mix) when basis is "none". */
  value: number;
  /** Counted runs behind the value (when basis is "none": the runs seen so far, below the minimum). */
  runs: number;
  successes: number;
  basis: PersonalBasis;
  /** Which rows the value came from: the task's cluster, the tool across clusters, or none. */
  level: "task_cluster" | "tool" | null;
  /** The task's cluster key. */
  cluster: string | null;
}

/** The personal value of a set of runs, in [0, 1]. */
export function personalValue(s: OutcomeStats): number {
  if (s.runs <= 0) return 0;
  const successRate = s.successes / s.runs;
  const costScore = s.successes > 0 ? priceScore(s.cost_micro_usd / s.successes) : 0;
  const hint = s.hint_followed_runs > 0 ? s.hint_followed_successes / s.hint_followed_runs : successRate;
  const v = PERSONAL_COMPONENTS.success_rate * successRate + PERSONAL_COMPONENTS.cost_per_success * costScore + PERSONAL_COMPONENTS.hint_success * hint;
  return Math.min(1, Math.max(0, v));
}

/**
 * Explains the personal factor of one tool for one task under one principal's profile.
 * `neutral` is the value used when there is not enough evidence.
 */
export function explainPersonal(profile: PersonalProfile | undefined, toolId: string, cluster: string | null, neutral: number): PersonalExplanation {
  const none: PersonalExplanation = { value: neutral, runs: 0, successes: 0, basis: "none", level: null, cluster };
  if (!profile) return none;
  const total = emptyStats();
  let inCluster: OutcomeStats | undefined;
  for (const r of profile.rows) {
    if (r.tool_id !== toolId) continue;
    total.runs += r.runs;
    total.successes += r.successes;
    total.hint_followed_runs += r.hint_followed_runs;
    total.hint_followed_successes += r.hint_followed_successes;
    total.cost_micro_usd += r.cost_micro_usd;
    if (cluster !== null && r.cluster === cluster) inCluster = r;
  }
  if (inCluster && inCluster.runs >= PERSONAL_MIN_RUNS) {
    return { value: personalValue(inCluster), runs: inCluster.runs, successes: inCluster.successes, basis: "principal", level: "task_cluster", cluster };
  }
  if (total.runs >= PERSONAL_MIN_RUNS) {
    return { value: personalValue(total), runs: total.runs, successes: total.successes, basis: "principal", level: "tool", cluster };
  }
  return { ...none, runs: total.runs, successes: total.successes };
}

/**
 * The learner: a `RunLogSink` that folds each counted run into the principal's store. Anonymous
 * runs are not learned (there is no principal to keep them for).
 */
export class PersonalRanker implements RunLogSink {
  constructor(
    readonly store: PersonalStore,
    readonly index: Bm25Index,
  ) {}

  async record(r: ToolRunRecord): Promise<void> {
    if (r.actor === "anonymous" || !COUNTED_OUTCOMES.has(r.outcome)) return;
    await this.store.record(r.actor, {
      tool_id: r.tool_id,
      cluster: taskCluster(this.index, r.task_hint),
      success: r.outcome === "ok",
      hint_followed: r.hint_followed,
      cost_micro_usd: r.cost_micro_usd,
    });
  }

  profile(principal: string): PersonalProfile | Promise<PersonalProfile> {
    return this.store.profile(principal);
  }
}
