/**
 * The metering seam (E8-T5). The router reserves a quote before a run and settles the measured
 * cost after it; apps/api wires the real wallet (E9: reserve-then-settle ledger, 402, caps)
 * behind this interface. The in-memory stub below is for development and tests.
 *
 * The bearer is a `Secret`; this module may `expose()` it to identify the actor and never
 * returns it. The actor id it derives is an opaque hash, so run records and logs carry no key.
 */
import { shortHash } from "./hash.js";
import type { Secret } from "./secret.js";

export interface Quote {
  tool_id: string;
  estimate_micro_usd: number;
  unit: string;
}

/** The 402 payload (implementation-plan §7.3; x402 `payment-required` field names). */
export interface PaymentRequired {
  amount_micro_usd: number;
  unit: string;
  accepts: { rail: "card"; url: string }[];
  retry_after: number;
}

export type Identity = { actor: string; anonymous: boolean } | { error: "invalid_key" };

export type Reservation =
  | { ok: true; reservation_id: string }
  | { ok: false; status: 401; error: "authentication_required" }
  | { ok: false; status: 402; error: "payment_required"; payment: PaymentRequired };

export interface MeteringClient {
  identify(bearer: Secret | undefined): Promise<Identity>;
  reserve(identity: { actor: string; anonymous: boolean }, quote: Quote): Promise<Reservation>;
  /** Settles the measured cost against a reservation; 0 releases it. */
  settle(reservationId: string, cost_micro_usd: number): Promise<void>;
}

export interface InMemoryMeteringOptions {
  /** Starting balance for any bearer not listed in `balances` (README §11: a free allowance). */
  defaultBalanceMicroUsd?: number;
  balances?: Record<string, number>;
  /** Where a 402 sends the human, once. */
  topUpUrl?: string;
}

export class InMemoryMeteringClient implements MeteringClient {
  readonly #balances = new Map<string, number>();
  readonly #reservations = new Map<string, { actor: string; amount: number }>();
  readonly #defaultBalance: number;
  readonly #topUpUrl: string;
  #seq = 0;

  constructor(opts: InMemoryMeteringOptions = {}) {
    this.#defaultBalance = opts.defaultBalanceMicroUsd ?? 1_000_000;
    this.#topUpUrl = opts.topUpUrl ?? "https://api.shogunaios.com/wallet/topup";
    for (const [k, v] of Object.entries(opts.balances ?? {})) this.#balances.set(shortHash(k), v);
  }

  balanceOf(bearerValue: string): number {
    return this.#balances.get(shortHash(bearerValue)) ?? this.#defaultBalance;
  }

  async identify(bearer: Secret | undefined): Promise<Identity> {
    if (!bearer) return { actor: "anonymous", anonymous: true };
    const value = bearer.expose();
    if (!value.startsWith("sa_") || value.length < 8) return { error: "invalid_key" };
    return { actor: shortHash(value), anonymous: false };
  }

  async reserve(identity: { actor: string; anonymous: boolean }, quote: Quote): Promise<Reservation> {
    if (quote.estimate_micro_usd === 0) {
      return { ok: true, reservation_id: `free-${++this.#seq}` };
    }
    if (identity.anonymous) return { ok: false, status: 401, error: "authentication_required" };
    const balance = this.#balances.get(identity.actor) ?? this.#defaultBalance;
    if (balance < quote.estimate_micro_usd) {
      return {
        ok: false,
        status: 402,
        error: "payment_required",
        payment: {
          amount_micro_usd: quote.estimate_micro_usd - balance,
          unit: quote.unit,
          accepts: [{ rail: "card", url: this.#topUpUrl }],
          retry_after: 0,
        },
      };
    }
    const id = `res-${++this.#seq}`;
    this.#balances.set(identity.actor, balance - quote.estimate_micro_usd);
    this.#reservations.set(id, { actor: identity.actor, amount: quote.estimate_micro_usd });
    return { ok: true, reservation_id: id };
  }

  async settle(reservationId: string, cost_micro_usd: number): Promise<void> {
    const r = this.#reservations.get(reservationId);
    if (!r) return;
    this.#reservations.delete(reservationId);
    const balance = this.#balances.get(r.actor) ?? this.#defaultBalance;
    this.#balances.set(r.actor, balance + r.amount - cost_micro_usd);
  }
}
