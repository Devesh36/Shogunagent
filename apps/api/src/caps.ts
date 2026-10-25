/**
 * Hard spend caps (docs/implementation-plan.md E9-T3): per key and per principal, daily and
 * monthly, integer micro-USD. Conservative by default — a leaked key is bounded to a dollar a
 * day whatever the wallet holds — and configurable per key (never above the principal's).
 *
 * Periods are UTC: a day is `YYYY-MM-DD`, a month `YYYY-MM` (the same keys `live-store.ts` uses).
 * A cap is checked at reservation, in the same transaction as the wallet, on four counters:
 * `key:<id>` × day/month and `principal:<id>` × day/month (`agent-store.ts`).
 */
import { MICRO_USD_PER_USD } from "./pricing.js";

export interface CapPair {
  day: number;
  month: number;
}

export interface CapSet {
  key: CapPair;
  principal: CapPair;
}

/** Key: $1/day, $20/month. Principal: $5/day, $100/month. */
export const DEFAULT_CAPS: Readonly<CapSet> = Object.freeze({
  key: Object.freeze({ day: 1 * MICRO_USD_PER_USD, month: 20 * MICRO_USD_PER_USD }),
  principal: Object.freeze({ day: 5 * MICRO_USD_PER_USD, month: 100 * MICRO_USD_PER_USD }),
});

export type CapScope = keyof CapSet;
export type CapPeriod = keyof CapPair;

/** The UTC period keys of an instant. */
export function periodsOf(nowMs: number): { day: string; month: string } {
  const day = new Date(nowMs).toISOString().slice(0, 10);
  return { day, month: day.slice(0, 7) };
}

/** Seconds until a period's counter resets (the next UTC midnight, or the first of next month). */
export function secsUntilPeriodReset(period: CapPeriod, nowMs: number): number {
  const d = new Date(nowMs);
  const reset = period === "day"
    ? Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate() + 1)
    : Date.UTC(d.getUTCFullYear(), d.getUTCMonth() + 1, 1);
  return Math.max(1, Math.ceil((reset - nowMs) / 1000));
}

/** The caps a call reserves against: the key's own values where set (never above the principal's),
 * else the defaults; the principal's own values where set, else the defaults. */
export function capsFor(
  defaults: CapSet,
  key: { capDayMicros?: number; capMonthMicros?: number },
  principal: { capDayMicros?: number; capMonthMicros?: number } = {},
): CapSet {
  const principalCaps: CapPair = {
    day: principal.capDayMicros ?? defaults.principal.day,
    month: principal.capMonthMicros ?? defaults.principal.month,
  };
  return {
    key: {
      day: Math.min(principalCaps.day, key.capDayMicros ?? defaults.key.day),
      month: Math.min(principalCaps.month, key.capMonthMicros ?? defaults.key.month),
    },
    principal: principalCaps,
  };
}

/** A cap value from a request body: a non-negative safe integer of micro-USD, or `undefined`
 * when absent; `null` (the sentinel for "not a cap") for anything else. */
export function parseCapMicros(v: unknown): number | undefined | null {
  if (v === undefined) return undefined;
  return typeof v === "number" && Number.isSafeInteger(v) && v >= 0 ? v : null;
}
