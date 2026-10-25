/**
 * `hints` (README §11 principle 2): a cheaper or better-fitting alternative when one exists,
 * including a competitor's connector. Computed from the catalog alone, so the same hint is given
 * to every caller, and first-party entries receive no special treatment (invariant 13).
 */
import type { CatalogEntry, CompiledCatalog } from "./catalog.js";

export interface Hint {
  tool_id: string;
  reason: "cheaper" | "better_fit" | "free_alternative";
  estimated_cost_micro_usd: number;
  first_party: boolean;
}

function sharesCategory(a: CatalogEntry, b: CatalogEntry): boolean {
  return a.categories.some((c) => b.categories.includes(c));
}

/** Cheaper endpoints in the same category and at the same or a lower permission level. */
export function cheaperAlternatives(catalog: CompiledCatalog, entry: CatalogEntry, limit = 3): Hint[] {
  const levelRank = { L1: 1, L2: 2, L3: 3 } as const;
  return catalog
    .entries()
    .filter(
      (e) =>
        e.tool_id !== entry.tool_id &&
        sharesCategory(e, entry) &&
        e.quote_micro_usd < entry.quote_micro_usd &&
        levelRank[e.scope.level] <= levelRank[entry.scope.level],
    )
    .sort((a, b) => a.quote_micro_usd - b.quote_micro_usd || a.tool_id.localeCompare(b.tool_id))
    .slice(0, limit)
    .map((e) => ({
      tool_id: e.tool_id,
      reason: e.quote_micro_usd === 0 ? "free_alternative" : "cheaper",
      estimated_cost_micro_usd: e.quote_micro_usd,
      first_party: e.first_party,
    }));
}

/** For a task, candidates that fit better than the chosen tool (used by `run` when a task hint is given). */
export function betterFitAlternatives(catalog: CompiledCatalog, entry: CatalogEntry, task: string, limit = 2): Hint[] {
  const own = catalog.index.fit(entry.tool_id, task);
  return catalog
    .entries()
    .filter((e) => e.tool_id !== entry.tool_id)
    .map((e) => ({ e, fit: catalog.index.fit(e.tool_id, task) }))
    .filter(({ fit }) => fit > own + 0.1)
    .sort((a, b) => b.fit - a.fit || a.e.quote_micro_usd - b.e.quote_micro_usd)
    .slice(0, limit)
    .map(({ e }) => ({
      tool_id: e.tool_id,
      reason: "better_fit" as const,
      estimated_cost_micro_usd: e.quote_micro_usd,
      first_party: e.first_party,
    }));
}
