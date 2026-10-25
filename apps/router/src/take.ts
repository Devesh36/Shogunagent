/**
 * The take on a third-party run (E32). The figure lives in ONE place,
 * `packages/config/pricing.json` (`run_markup_bps`), which apps/api's rate table
 * (`RUN_MARKUP_BPS`: the wallet that actually charges, and `/.well-known/pricing.json`) reads too.
 *
 * Every price in the router catalog is the provider's own price (for an imported marketplace
 * catalog such as Monid, the price that marketplace invoices us). The caller's price is that
 * figure plus ONE take: shown here (discover, inspect, dryRun, the open catalog, a run's `usage`)
 * and computed again by apps/api when it reserves and settles. The router sends the wallet the
 * provider price only, never a caller price, so the take cannot be applied twice. First-party
 * runs carry no take (their listed price is ours).
 */
import sharedPricing from "../../../packages/config/pricing.json";

export const RUN_MARKUP_BPS: number = sharedPricing.run_markup_bps;

function wholeMicros(v: number): number {
  return Number.isFinite(v) && v > 0 ? Math.round(v) : 0;
}

/** Our take on a provider price: `RUN_MARKUP_BPS`, rounded up. 0 for first-party runs. */
export function takeOf(providerMicroUsd: number, firstParty: boolean): number {
  const base = wholeMicros(providerMicroUsd);
  if (firstParty || base === 0) return 0;
  return Math.ceil((base * RUN_MARKUP_BPS) / 10_000);
}

/** What the caller pays for a run whose provider price is `providerMicroUsd`: provider + one take.
 * For a third-party run this equals apps/api's `quoteCost("run", 1, provider)`. */
export function callerPrice(providerMicroUsd: number, firstParty: boolean): number {
  const base = wholeMicros(providerMicroUsd);
  return base + takeOf(base, firstParty);
}
