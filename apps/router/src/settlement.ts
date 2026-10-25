/**
 * The run-settlement seam (E29, E32, E33): what the router tells apps/api about every run it
 * settled. apps/api implements the receiving side (`POST /v1/internal/router/settlements`,
 * apps/api/src/router-wallet.ts) and does two things with each settlement, both idempotent:
 *
 * - **the caller's wallet** (E32): when `receipt_id` names the reservation the router took through
 *   `POST /v1/internal/router/reserve`, that reservation is settled once, at the provider's price
 *   plus ONE take for third-party runs (`RUN_MARKUP_BPS`, computed there, never sent from here);
 *   failures and empty results settle it at $0;
 * - **the provider's ledger** (E29): the run is added to the provider's daily accrual.
 *
 * A settlement is content-free by construction (invariant 2): tool id, provider id, the opaque
 * payout reference, the marketplace tag, an outcome, a unit count, an amount and the reservation
 * id. No input, no output, no actor, no key. The router writes it to its durable outbox
 * (src/outbox.ts) before sending it; `run_id` is the idempotency key at the receiving end, so a
 * redelivery changes nothing.
 */
import type { EndpointDefinition, ProviderDefinition } from "../connectors/types.js";

/** `/2` adds `receipt_id` and `marketplace` (E32). apps/api still accepts `/1` (accrual only). */
export const SETTLEMENT_FORMAT = "shogunagent-run-settlement/2";

/** The outcomes a settled run can have. Permission stops, input errors and 402s never reserve,
 * so they never settle and never appear here. */
export type SettledOutcome = "ok" | "empty" | "provider_error" | "transport_error";

export interface RunSettlement {
  format: typeof SETTLEMENT_FORMAT;
  /** Unique per run; the receiver's idempotency key. */
  run_id: string;
  /** The caller's wallet reservation this run settles (`rcpt_run_<run_id>`), or null when nothing
   * was reserved (a $0 tool, or development metering). */
  receipt_id: string | null;
  /** ms since the epoch, when the run started. */
  ts: number;
  tool_id: string;
  provider_id: string;
  /** First-party revenue is ours: no take, ledgered for the statement, never paid out. */
  first_party: boolean;
  /** From `provider.json`; null for a provider without a manifest. */
  payout_account_ref: string | null;
  /** The marketplace whose imported catalog this tool came from (`"monid"`), which executes the run
   * and invoices US for it, or null for a provider listed directly. A marketplace's price already
   * includes whatever that marketplace charges; the caller pays it plus one take, and the provider
   * is never paid through Connect. */
  marketplace: string | null;
  outcome: SettledOutcome;
  /** The settled unit count (0 for failures and empty results). */
  quantity: number;
  /** The provider's price for this run under its usage model (µ$). 0 for failures and empty
   * results. Our take is computed by the receiver, never sent. */
  provider_micro_usd: number;
}

export const RUN_SETTLEMENT_KEYS = [
  "format", "run_id", "receipt_id", "ts", "tool_id", "provider_id", "first_party", "payout_account_ref", "marketplace", "outcome", "quantity", "provider_micro_usd",
] as const;

/** The receipt id apps/api gives a router run's reservation: derived from the run id, so a retried
 * reserve finds the same hold and a settlement can only settle its own run. */
export function receiptIdFor(runId: string): string {
  return `rcpt_run_${runId}`;
}

export interface SettlementSink {
  /** Deliver one settlement. Must not throw into the run. */
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

/** The marketplace tag (E32): a third-party provider without a `provider.json` is an imported
 * marketplace catalog (src/passthrough/), named by its provider id. */
export function marketplaceOf(provider: ProviderDefinition): string | null {
  if (provider.first_party || provider.manifest !== undefined) return null;
  return provider.id;
}

export function buildSettlement(args: {
  runId: string;
  receiptId: string | null;
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
    receipt_id: args.receiptId,
    ts: args.ts,
    tool_id: args.toolId,
    provider_id: args.provider.id,
    first_party: args.provider.first_party,
    payout_account_ref: payoutRefOf(args.provider),
    marketplace: marketplaceOf(args.provider),
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

/** What the outbox delivery posts with. src/transport.ts builds the real one (the router's only
 * egress); tests pass a fake. */
export type SettlementPoster = (settlements: RunSettlement[]) => Promise<{ status: number }>;
