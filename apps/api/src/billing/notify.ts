/**
 * Billing notifications (docs/implementation-plan.md E19): `wallet.low_balance` when a settlement
 * takes a wallet's spendable balance below the threshold, and `cap.reached` when a hard cap refuses
 * a call. The shapes are documented in docs/api/billing.md.
 *
 * There is no per-principal webhook module in apps/api yet (E15 brings remote approvals over
 * webhooks), so delivery is pluggable: `Notifier.emit` takes the event and the default
 * `InMemoryNotifier` keeps the most recent events per principal in memory — log-free, so an event
 * never becomes a log line. A webhook notifier implements the same interface later, delivering the
 * same JSON. Event ids are deterministic (one low-balance event per principal per UTC day, one
 * cap event per counter per period), so a notifier dedupes on `id` and a delivery retry is safe.
 *
 * Content-free: ids, amounts, scopes and periods. Never a request body, never a key.
 */

export interface WalletLowBalanceEvent {
  type: "wallet.low_balance";
  id: string;
  principal_id: string;
  created_at: string;
  data: { balance_micro_usd: number; threshold_micro_usd: number; topup_url: string };
}

export interface CapReachedEvent {
  type: "cap.reached";
  id: string;
  principal_id: string;
  created_at: string;
  data: { scope: "key" | "principal"; period: "day" | "month"; period_key: string; limit_micro_usd: number; spent_micro_usd: number; key_id: string | null; retry_after: number };
}

export type BillingEvent = WalletLowBalanceEvent | CapReachedEvent;

export interface Notifier {
  /** Deliver one event. Must not throw into the request path; failures are the notifier's own. */
  emit(event: BillingEvent): Promise<void>;
}

/** Spendable balance below this many µ$ ($1) after a settlement emits `wallet.low_balance`. */
export const LOW_BALANCE_THRESHOLD_MICROS = 1_000_000;

export function lowBalanceEvent(principalId: string, balanceMicros: number, thresholdMicros: number, topupUrl: string, nowMs: number): WalletLowBalanceEvent {
  const day = new Date(nowMs).toISOString().slice(0, 10);
  return {
    type: "wallet.low_balance",
    id: `wallet.low_balance:${principalId}:${day}`,
    principal_id: principalId,
    created_at: new Date(nowMs).toISOString(),
    data: { balance_micro_usd: balanceMicros, threshold_micro_usd: thresholdMicros, topup_url: topupUrl },
  };
}

export function capReachedEvent(
  principalId: string,
  keyId: string,
  cap: { scope: "key" | "principal"; period: "day" | "month"; periodKey: string; limitMicros: number; spentMicros: number; retryAfterSecs: number },
  nowMs: number,
): CapReachedEvent {
  const subject = cap.scope === "key" ? keyId : principalId;
  return {
    type: "cap.reached",
    id: `cap.reached:${cap.scope}:${subject}:${cap.periodKey}`,
    principal_id: principalId,
    created_at: new Date(nowMs).toISOString(),
    data: {
      scope: cap.scope, period: cap.period, period_key: cap.periodKey, limit_micro_usd: cap.limitMicros, spent_micro_usd: cap.spentMicros,
      key_id: cap.scope === "key" ? keyId : null, retry_after: cap.retryAfterSecs,
    },
  };
}

/** Whether a settlement crossed the threshold: spendable now below it, and at or above it before
 * this charge. A wallet already below the line does not re-notify on every call. */
export function crossedLowBalance(availableAfter: number, charged: number, thresholdMicros: number): boolean {
  return charged > 0 && availableAfter < thresholdMicros && availableAfter + charged >= thresholdMicros;
}

/** The log-free default: the newest `perPrincipal` events of each principal, deduplicated by id. */
export class InMemoryNotifier implements Notifier {
  private readonly byPrincipal = new Map<string, BillingEvent[]>();
  private readonly seen = new Set<string>();

  constructor(private readonly perPrincipal = 50, private readonly maxIds = 10_000) {}

  emit(event: BillingEvent): Promise<void> {
    if (this.seen.has(event.id)) return Promise.resolve();
    if (this.seen.size >= this.maxIds) this.seen.clear();
    this.seen.add(event.id);
    const list = this.byPrincipal.get(event.principal_id) ?? [];
    list.push(event);
    if (list.length > this.perPrincipal) list.splice(0, list.length - this.perPrincipal);
    this.byPrincipal.set(event.principal_id, list);
    return Promise.resolve();
  }

  /** The principal's retained events, oldest first. */
  events(principalId: string): BillingEvent[] {
    return [...(this.byPrincipal.get(principalId) ?? [])];
  }
}
