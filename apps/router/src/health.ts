/**
 * Live health per tool (p50 / p95 / success rate) for ranking and `inspect`. The declared
 * baseline from the definition is used until enough runs have been observed; after that the
 * observed window takes over. Pluggable so the hosted router can read the shared status store
 * (README §11 `/.well-known/status.json`) instead of per-isolate memory.
 */
import type { HealthBaseline } from "../connectors/types.js";

export interface HealthObservation {
  latency_ms: number;
  ok: boolean;
}

export interface HealthStore {
  get(toolId: string, baseline: HealthBaseline): HealthBaseline;
  observe(toolId: string, obs: HealthObservation): void;
}

export function percentile(sorted: number[], p: number): number {
  if (sorted.length === 0) return 0;
  const idx = Math.min(sorted.length - 1, Math.max(0, Math.ceil((p / 100) * sorted.length) - 1));
  return sorted[idx] ?? 0;
}

export class InMemoryHealthStore implements HealthStore {
  readonly #window: number;
  readonly #minSamples: number;
  readonly #obs = new Map<string, HealthObservation[]>();

  constructor(opts: { window?: number; minSamples?: number } = {}) {
    this.#window = opts.window ?? 200;
    this.#minSamples = opts.minSamples ?? 20;
  }

  get(toolId: string, baseline: HealthBaseline): HealthBaseline {
    const obs = this.#obs.get(toolId);
    if (!obs || obs.length < this.#minSamples) return baseline;
    const latencies = obs.filter((o) => o.ok).map((o) => o.latency_ms).sort((a, b) => a - b);
    const okCount = obs.filter((o) => o.ok).length;
    return {
      p50_ms: latencies.length ? percentile(latencies, 50) : baseline.p50_ms,
      p95_ms: latencies.length ? percentile(latencies, 95) : baseline.p95_ms,
      success_rate: okCount / obs.length,
    };
  }

  observe(toolId: string, obs: HealthObservation): void {
    const list = this.#obs.get(toolId) ?? [];
    list.push(obs);
    if (list.length > this.#window) list.splice(0, list.length - this.#window);
    this.#obs.set(toolId, list);
  }
}

export const noopHealthStore: HealthStore = {
  get: (_id, baseline) => baseline,
  observe() {},
};
