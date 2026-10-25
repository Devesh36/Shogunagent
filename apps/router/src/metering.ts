/**
 * The metering seam (E8-T5, E32). The router reserves a quote before a run and settles the measured
 * cost after it. Production wires `ApiWalletClient` (src/wallet.ts): the reservation and the
 * settlement go through apps/api's wallet (reserve → settle, 402, caps), which charges the caller
 * exactly once. The in-memory client below is for development and tests.
 *
 * The bearer is a `Secret`; this module may `expose()` it to identify the actor and never
 * returns it. The actor id it derives is an opaque hash, so run records and logs carry no key.
 */
import { shortHash } from "./hash.js";
import type { Secret } from "./secret.js";
import type { RunSettlement, SettlementSink } from "./settlement.js";

export interface Quote {
  tool_id: string;
  /** What the caller is quoted: the provider's price plus one take (src/take.ts). */
  estimate_micro_usd: number;
  /** The provider's own price for a typical run, before the take. The only figure sent to the
   * wallet; apps/api adds the take itself. */
  provider_micro_usd: number;
  first_party: boolean;
  unit: string;
}

/** The 402 payload (implementation-plan §7.3; x402 `payment-required` field names). apps/api's
 * wallet answers with the same shape (docs/api/402.md); its extra fields ride along. */
export interface PaymentRequired {
  amount_micro_usd: number;
  unit: string;
  accepts: { rail: string; url?: string; [k: string]: unknown }[];
  retry_after: number;
  balance_micro_usd?: number;
  cap?: { scope: string; period: string; limit_micro_usd: number; spent_micro_usd: number };
}

export type Identity = { actor: string; anonymous: boolean } | { error: "invalid_key" };

export type Reservation =
  /** `receipt_id`: the wallet's reservation (apps/api), carried on the settlement; null when
   * nothing was reserved at apps/api (a $0 tool, or development metering). */
  | { ok: true; reservation_id: string; receipt_id: string | null }
  | { ok: false; status: 401; error: "authentication_required" }
  | { ok: false; status: 402; error: "payment_required" | "cap_exceeded"; payment: PaymentRequired }
  | { ok: false; status: 503; error: "wallet_unavailable"; retry_after: number }
  /** apps/api's wallet throttled the reserve (429); `retry_after` is its `Retry-After`. */
  | { ok: false; status: 429; error: "rate_limited"; retry_after: number };

/** What a reservation needs besides the quote. */
export interface ReserveContext {
  /** The run's id: the wallet derives the receipt id from it, so a retried reserve is one hold. */
  runId: string;
  /** The caller's bearer, forwarded to the wallet (never logged, never stored). */
  bearer?: Secret;
  /** The settlement to deliver if the run is abandoned after the hold (outcome transport_error,
   * $0): the durable outbox records it before money is held (src/outbox.ts). */
  draft: RunSettlement;
}

export interface MeteringClient {
  identify(bearer: Secret | undefined): Promise<Identity>;
  reserve(identity: { actor: string; anonymous: boolean }, quote: Quote, ctx: ReserveContext): Promise<Reservation>;
  /**
   * Settles a reservation, once. `charged_micro_usd` is the caller price the router shows (0 for
   * failures and empty results); `settlement` is the content-free record of the run. A client wired
   * to apps/api ignores the figure and sends only the settlement: apps/api prices it from the
   * provider price with its own take, so the take is applied in one place.
   */
  settle(reservationId: string, charged_micro_usd: number, settlement: RunSettlement): Promise<void>;
}

export interface InMemoryMeteringOptions {
  /** Starting balance for any bearer not listed in `balances` (README §11: a free allowance). */
  defaultBalanceMicroUsd?: number;
  balances?: Record<string, number>;
  /** Where a 402 sends the human, once. */
  topUpUrl?: string;
  /** Where run settlements go (E29). Absent = not forwarded (development). */
  settlements?: SettlementSink;
}

export class InMemoryMeteringClient implements MeteringClient {
  readonly #balances = new Map<string, number>();
  readonly #reservations = new Map<string, { actor: string; amount: number }>();
  readonly #defaultBalance: number;
  readonly #topUpUrl: string;
  readonly #settlements: SettlementSink | undefined;
  #seq = 0;

  constructor(opts: InMemoryMeteringOptions = {}) {
    this.#defaultBalance = opts.defaultBalanceMicroUsd ?? 1_000_000;
    this.#topUpUrl = opts.topUpUrl ?? "https://api.shogunaios.com/wallet/topup";
    this.#settlements = opts.settlements;
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
      return { ok: true, reservation_id: `free-${++this.#seq}`, receipt_id: null };
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
    return { ok: true, reservation_id: id, receipt_id: null };
  }

  async settle(reservationId: string, charged_micro_usd: number, settlement: RunSettlement): Promise<void> {
    const r = this.#reservations.get(reservationId);
    if (r) {
      this.#reservations.delete(reservationId);
      const balance = this.#balances.get(r.actor) ?? this.#defaultBalance;
      this.#balances.set(r.actor, balance + r.amount - charged_micro_usd);
    } else if (!reservationId.startsWith("free-")) {
      return;
    }
    // Free reservations settle too: a $0 run still counts on the provider's statement.
    if (this.#settlements) await this.#settlements.accrue(settlement).catch(() => undefined);
  }
}
