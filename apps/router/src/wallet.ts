/**
 * The router's wallet client (E32): every metered run is charged exactly once, by apps/api's wallet
 * (reserve → settle, 402, caps, commits; apps/api/src/router-wallet.ts). The router holds no
 * balance and adds no price of its own:
 *
 *   reserve   outbox.open(draft)                       durable before any money is held
 *             POST /v1/internal/router/reserve         { run_id, tool_id, provider_micro_usd, first_party }
 *                                                      + the caller's bearer, forwarded, never stored
 *             apps/api prices it (provider + RUN_MARKUP_BPS for third-party), checks the key's
 *             `run` scope, caps and balance, and holds it under `rcpt_run_<run_id>` (idempotent).
 *   settle    outbox.put(settlement)                   durable before it is sent
 *             POST /v1/internal/router/settlements     settles that hold once at the provider's
 *                                                      measured price + one take (failures and
 *                                                      empty results at $0) and accrues the provider
 *
 * Only the provider price crosses the wire; the take is applied by apps/api, so there is no second
 * markup anywhere on the path (the Monid passthrough included: src/passthrough/monid.ts). A $0 tool
 * (the first-party state reads, invariant 10) reserves nothing and calls nothing inline: its
 * settlement is queued and delivered by the retry job, so reads stay free and fast.
 *
 * Fails closed: if the outbox cannot record the draft, or apps/api cannot be reached, the run is
 * refused with `wallet_unavailable` before the provider is called. Nothing is charged.
 */
import type { Json } from "../connectors/types.js";
import { shortHash } from "./hash.js";
import type { Logger } from "./logging.js";
import type { Identity, MeteringClient, PaymentRequired, Quote, Reservation, ReserveContext } from "./metering.js";
import type { SettlementDelivery, SettlementOutbox } from "./outbox.js";
import type { Secret } from "./secret.js";
import { receiptIdFor, type RunSettlement } from "./settlement.js";

/** The body of `POST /v1/internal/router/reserve`. Content-free: ids, a price and a flag. */
export interface WalletReserveRequest {
  run_id: string;
  tool_id: string;
  provider_micro_usd: number;
  first_party: boolean;
}

/** The wallet's two routes, as src/transport.ts (the router's only egress) implements them. */
export interface WalletTransport {
  reserve(body: WalletReserveRequest, caller: Secret): Promise<{ status: number; body: Json; retryAfter?: number }>;
}

/** Bearer shapes apps/api accepts: an agent key (`sa_…`) or an OAuth access token (`sat_…`). */
const BEARER_RE = /^sat?_[\x21-\x7e]{4,1020}$/;

export interface ApiWalletOptions {
  wallet: WalletTransport;
  outbox: SettlementOutbox;
  delivery: SettlementDelivery;
  logger: Logger;
  now?: () => number;
}

function paymentOf(body: Json): PaymentRequired | undefined {
  if (!body || typeof body !== "object" || Array.isArray(body)) return undefined;
  const p = (body as Record<string, Json>)["payment"];
  if (!p || typeof p !== "object" || Array.isArray(p)) return undefined;
  const r = p as Record<string, Json>;
  if (typeof r["amount_micro_usd"] !== "number" || !Array.isArray(r["accepts"])) return undefined;
  return p as unknown as PaymentRequired;
}

function errorCodeOf(body: Json): string | undefined {
  if (!body || typeof body !== "object" || Array.isArray(body)) return undefined;
  const e = (body as Record<string, Json>)["error"];
  if (e && typeof e === "object" && !Array.isArray(e)) {
    const code = (e as Record<string, Json>)["code"];
    return typeof code === "string" ? code : undefined;
  }
  return undefined;
}

export class ApiWalletClient implements MeteringClient {
  readonly #wallet: WalletTransport;
  readonly #outbox: SettlementOutbox;
  readonly #delivery: SettlementDelivery;
  readonly #logger: Logger;
  readonly #now: () => number;

  constructor(opts: ApiWalletOptions) {
    this.#wallet = opts.wallet;
    this.#outbox = opts.outbox;
    this.#delivery = opts.delivery;
    this.#logger = opts.logger;
    this.#now = opts.now ?? Date.now;
  }

  /** Local and cheap: the shape of the bearer and an opaque actor hash. apps/api verifies the key
   * itself on reserve; a $0 run never needs the wallet. */
  async identify(bearer: Secret | undefined): Promise<Identity> {
    if (!bearer) return { actor: "anonymous", anonymous: true };
    const value = bearer.expose();
    if (!BEARER_RE.test(value)) return { error: "invalid_key" };
    return { actor: shortHash(value), anonymous: false };
  }

  async reserve(identity: { actor: string; anonymous: boolean }, quote: Quote, ctx: ReserveContext): Promise<Reservation> {
    // Free (invariant 10): nothing to hold, no call to apps/api. The settlement still counts the run.
    if (quote.provider_micro_usd === 0) return { ok: true, reservation_id: `free:${ctx.runId}`, receipt_id: null };
    if (identity.anonymous || !ctx.bearer) return { ok: false, status: 401, error: "authentication_required" };
    const receiptId = receiptIdFor(ctx.runId);
    const unavailable = (word: string): Reservation => {
      this.#logger.log({ kind: "metering", outcome: word, tool_id: quote.tool_id });
      return { ok: false, status: 503, error: "wallet_unavailable", retry_after: 5 };
    };
    // 1. Durable first: if the router dies after apps/api holds the money, the retry job finds this
    // draft and releases the hold at $0.
    try {
      await this.#outbox.open({ ...ctx.draft, receipt_id: receiptId }, this.#now());
    } catch {
      return unavailable("outbox_unavailable");
    }
    // 2. The hold, priced and capped by apps/api.
    let res: Awaited<ReturnType<WalletTransport["reserve"]>>;
    try {
      res = await this.#wallet.reserve({ run_id: ctx.runId, tool_id: quote.tool_id, provider_micro_usd: quote.provider_micro_usd, first_party: quote.first_party }, ctx.bearer);
    } catch {
      // Unknown whether apps/api holds money: keep the draft; the retry job releases any hold.
      return unavailable("wallet_unreachable");
    }
    if (res.status >= 200 && res.status < 300) return { ok: true, reservation_id: receiptId, receipt_id: receiptId };
    if (res.status >= 500) return unavailable("wallet_unavailable");
    // A definite refusal: nothing is held, so nothing will settle.
    await this.#outbox.discard(ctx.runId).catch(() => undefined);
    if (res.status === 402) {
      const payment = paymentOf(res.body);
      if (payment) return { ok: false, status: 402, error: errorCodeOf(res.body) === "cap_exceeded" ? "cap_exceeded" : "payment_required", payment };
      return unavailable("wallet_answer_invalid");
    }
    if (res.status === 401 || res.status === 403) return { ok: false, status: 401, error: "authentication_required" };
    return unavailable("wallet_refused");
  }

  /** Durable first, then one inline delivery for a run that held money (the caller's wallet is
   * settled promptly); $0 runs are left to the retry job. Never throws into the run. */
  async settle(_reservationId: string, _charged: number, settlement: RunSettlement): Promise<void> {
    try {
      await this.#outbox.put(settlement, this.#now());
    } catch {
      // The draft (if any) is still in the outbox and will release the hold at $0: an under-charge,
      // never an over-charge. A $0 run's count is lost for this run only.
      this.#logger.log({ kind: "settlement", outcome: "outbox_unavailable", count: 1 });
      return;
    }
    if (settlement.receipt_id !== null) await this.#delivery.deliverNow(settlement);
  }
}

/** A misconfigured production router (wallet URL set, no outbox database): metered runs are refused
 * rather than charged without a durable settlement; $0 runs proceed and are not counted. */
export class UnavailableWalletClient implements MeteringClient {
  constructor(readonly logger: Logger) {}
  async identify(bearer: Secret | undefined): Promise<Identity> {
    if (!bearer) return { actor: "anonymous", anonymous: true };
    const value = bearer.expose();
    return BEARER_RE.test(value) ? { actor: shortHash(value), anonymous: false } : { error: "invalid_key" };
  }
  async reserve(_identity: { actor: string; anonymous: boolean }, quote: Quote, ctx: ReserveContext): Promise<Reservation> {
    if (quote.provider_micro_usd === 0) return { ok: true, reservation_id: `free:${ctx.runId}`, receipt_id: null };
    this.logger.log({ kind: "metering", outcome: "outbox_not_configured", tool_id: quote.tool_id });
    return { ok: false, status: 503, error: "wallet_unavailable", retry_after: 60 };
  }
  async settle(): Promise<void> {}
}
