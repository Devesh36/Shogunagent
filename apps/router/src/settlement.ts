/**
 * The run-settlement seam (E29): what the router tells the payout ledger about every run it
 * settled, so the provider named in that tool's `provider.json` (`payout_account_ref`) accrues
 * its share. apps/api implements the receiving side (`POST /v1/internal/router/settlements`,
 * apps/api/src/payouts.ts) and computes our take there from its own rate table
 * (`RUN_MARKUP_BPS`, 5%), so the published price and the take have one source.
 *
 * A settlement is content-free by construction (invariant 2): tool id, provider id, the opaque
 * payout reference, an outcome, a unit count and an amount. No input, no output, no actor, no key.
 *
 * Every run that reached the provider is settled exactly once, including failures and empty
 * results at $0 (they count as runs on the provider's statement, never as earnings). The
 * `run_id` is the idempotency key at the receiving end, so a retried delivery changes nothing.
 */
import type { EndpointDefinition, ProviderDefinition } from "../connectors/types.js";

export const SETTLEMENT_FORMAT = "shogunagent-run-settlement/1";

/** The outcomes a settled run can have. Permission stops, input errors and 402s never reserve,
 * so they never settle and never appear here. */
export type SettledOutcome = "ok" | "empty" | "provider_error" | "transport_error";

export interface RunSettlement {
  format: typeof SETTLEMENT_FORMAT;
  /** Unique per run; the receiver's idempotency key. */
  run_id: string;
  /** ms since the epoch, when the run started. */
  ts: number;
  tool_id: string;
  provider_id: string;
  /** First-party revenue is ours: it is ledgered for the statement, never paid out. */
  first_party: boolean;
  /** From `provider.json`; null for a provider without a manifest (an imported passthrough
   * catalog, paid on that marketplace's own invoice, never through Connect). */
  payout_account_ref: string | null;
  outcome: SettledOutcome;
  /** The settled unit count (0 for failures and empty results). */
  quantity: number;
  /** The provider's price for this run under its usage model (µ$). 0 for failures and empty
   * results. Our take is computed by the receiver, never sent. */
  provider_micro_usd: number;
}

export const RUN_SETTLEMENT_KEYS = [
  "format", "run_id", "ts", "tool_id", "provider_id", "first_party", "payout_account_ref", "outcome", "quantity", "provider_micro_usd",
] as const;

export interface SettlementSink {
  /** Deliver one settlement. Must not throw into the run: a sink that cannot deliver retries or
   * reports through its own channel; the run's answer to the agent never depends on it. */
  accrue(settlement: RunSettlement): Promise<void>;
}

/** The payout reference a provider declared in its `provider.json`, when it has one. */
export function payoutRefOf(provider: ProviderDefinition): string | null {
  const m = provider.manifest;
  if (m && typeof m === "object" && !Array.isArray(m)) {
    const ref = (m as Record<string, unknown>)["payout_account_ref"];
    if (typeof ref === "string" && ref.length > 0) return ref;
  }
  return null;
}

export function buildSettlement(args: {
  runId: string;
  ts: number;
  toolId: string;
  provider: ProviderDefinition;
  endpoint: EndpointDefinition;
  outcome: SettledOutcome;
  quantity: number;
  costMicroUsd: number;
}): RunSettlement {
  const paid = args.outcome === "ok" ? Math.max(0, Math.round(args.costMicroUsd)) : 0;
  return {
    format: SETTLEMENT_FORMAT,
    run_id: args.runId,
    ts: args.ts,
    tool_id: args.toolId,
    provider_id: args.provider.id,
    first_party: args.provider.first_party,
    payout_account_ref: payoutRefOf(args.provider),
    outcome: args.outcome,
    quantity: args.outcome === "ok" ? Math.max(0, args.quantity) : 0,
    provider_micro_usd: paid,
  };
}

export class InMemorySettlementSink implements SettlementSink {
  readonly settlements: RunSettlement[] = [];
  async accrue(settlement: RunSettlement): Promise<void> {
    this.settlements.push(settlement);
  }
}

/** What `HttpSettlementSink` posts with. src/transport.ts builds the real one (the router's only
 * egress); tests pass a fake. */
export type SettlementPoster = (settlements: RunSettlement[]) => Promise<{ status: number }>;

/**
 * Delivers settlements to apps/api. Retries a failed POST (the receiver is idempotent by
 * `run_id`), then gives up and counts the loss in `dropped` — the caller's logger reports the
 * count, never the settlement. A durable outbox is the production follow-up
 * (docs/api/payouts.md, "Delivery").
 */
export class HttpSettlementSink implements SettlementSink {
  dropped = 0;
  readonly #post: SettlementPoster;
  readonly #attempts: number;

  constructor(post: SettlementPoster, opts: { attempts?: number } = {}) {
    this.#post = post;
    this.#attempts = Math.max(1, opts.attempts ?? 3);
  }

  async accrue(settlement: RunSettlement): Promise<void> {
    for (let i = 0; i < this.#attempts; i += 1) {
      try {
        const res = await this.#post([settlement]);
        if (res.status >= 200 && res.status < 300) return;
        // A 4xx other than 429 will not get better on retry.
        if (res.status >= 400 && res.status < 500 && res.status !== 429) break;
      } catch {
        // network: retry
      }
    }
    this.dropped += 1;
  }
}
