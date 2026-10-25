/**
 * The 402 response (docs/implementation-plan.md E9-T2, §7.3; the contract is docs/api/402.md).
 *
 * Two reasons, one shape:
 * - `payment_required` — the wallet cannot cover the quoted cost. `next_action: "top_up"`;
 *   `payment.accepts` lists the rails (today: `card`, the Stripe Checkout route).
 * - `cap_exceeded` — a hard cap (E9-T3) would be crossed. Money does not help, so `accepts` is
 *   empty, `next_action: "retry_after"` and `retry_after` is the seconds until the period resets.
 *
 * The body carries the plan's envelope; the same payload, base64url-encoded, rides the
 * `PAYMENT-REQUIRED` header (the x402 payment-required convention: `x402Version`, `error`,
 * `accepts[]`, plus the plan's fields), so a client that only looks at headers sees it too.
 */
import type { MeterUnit } from "../pricing.js";
import type { X402Requirement } from "./x402.js";

export const X402_VERSION = 2;

/** Field names follow x402's `accepts` entries where they overlap (`scheme`/`network` are not
 * used for a card rail; `url` is where a human completes the payment). */
export interface CardRail {
  rail: "card";
  url: string;
  /** The HTTP method the rail's `url` expects. */
  method: "POST";
}

/** A rail in `payment.accepts`: the card top-up, and — only with `X402_ENABLED=1` — an x402
 * PaymentRequirements entry (billing/x402.ts). */
export type PaymentRail = CardRail | X402Requirement;

export interface PaymentRequiredPayload {
  error: { code: "payment_required" | "cap_exceeded" | "payment_invalid" | "payment_replayed"; message: string; next_action: "top_up" | "retry_after" };
  payment: {
    amount_micro_usd: number;
    unit: MeterUnit;
    accepts: PaymentRail[];
    retry_after: number;
    /** Spendable balance at the time of the refusal (payment_required only). */
    balance_micro_usd?: number;
    /** Which ceiling refused (cap_exceeded only). */
    cap?: { scope: "key" | "principal"; period: "day" | "month"; limit_micro_usd: number; spent_micro_usd: number };
  };
}

export interface PaymentRequiredInput {
  reason: "payment_required";
  amountMicros: number;
  unit: MeterUnit;
  balanceMicros: number;
  /** The absolute URL of `POST /v1/wallet/topup`. */
  topupUrl: string;
  /** The x402 requirement for this amount, appended after the card rail (E30, flag-gated). */
  x402?: X402Requirement;
  /** A presented x402 payment was refused (E30): the code and the plain-English reason. The
   * accepts list is the same, so the agent can pay again. */
  refused?: { code: "payment_invalid" | "payment_replayed"; message: string };
}

export interface CapExceededInput {
  reason: "cap_exceeded";
  amountMicros: number;
  unit: MeterUnit;
  scope: "key" | "principal";
  period: "day" | "month";
  limitMicros: number;
  spentMicros: number;
  retryAfterSecs: number;
}

export function paymentRequiredPayload(input: PaymentRequiredInput | CapExceededInput): PaymentRequiredPayload {
  if (input.reason === "payment_required") {
    const short = input.amountMicros - input.balanceMicros;
    return {
      error: input.refused ? { ...input.refused, next_action: "top_up" } : {
        code: "payment_required",
        message: `This call costs ${input.amountMicros} micro-USD and the wallet has ${input.balanceMicros} spendable. Top up at least ${Math.max(short, 0)} micro-USD, then retry the same request.${input.x402 ? " Or pay this call with x402: send X-PAYMENT for the x402 entry in payment.accepts." : ""}`,
        next_action: "top_up",
      },
      payment: {
        amount_micro_usd: input.amountMicros,
        unit: input.unit,
        accepts: [{ rail: "card", url: input.topupUrl, method: "POST" }, ...(input.x402 ? [input.x402] : [])],
        retry_after: 0,
        balance_micro_usd: input.balanceMicros,
      },
    };
  }
  return {
    error: {
      code: "cap_exceeded",
      message: `This call would cross the ${input.scope}'s ${input.period === "day" ? "daily" : "monthly"} hard cap (${input.limitMicros} micro-USD; ${input.spentMicros} already booked). Wait for the period to reset or lower what you send; topping up does not raise a cap.`,
      next_action: "retry_after",
    },
    payment: {
      amount_micro_usd: input.amountMicros,
      unit: input.unit,
      accepts: [],
      retry_after: input.retryAfterSecs,
      cap: { scope: input.scope, period: input.period, limit_micro_usd: input.limitMicros, spent_micro_usd: input.spentMicros },
    },
  };
}

/** The `PAYMENT-REQUIRED` header value: base64url of the x402-shaped JSON. */
export function paymentRequiredHeader(payload: PaymentRequiredPayload): string {
  // x402's top level: `x402Version`, a string `error`, `accepts[]`; the plan's `payment` object
  // and `next_action` ride alongside so the header alone carries the full quote.
  const x402 = {
    x402Version: X402_VERSION,
    error: payload.error.code,
    accepts: payload.payment.accepts,
    next_action: payload.error.next_action,
    payment: payload.payment,
  };
  return Buffer.from(JSON.stringify(x402), "utf8").toString("base64url");
}

/** The response. `latency_ms` and the `usage` of the refused (unbilled) call ride along so the
 * envelope matches a 200's (§7.2). */
export function paymentRequiredResponse(input: PaymentRequiredInput | CapExceededInput, latencyMs: number): Response {
  const payload = paymentRequiredPayload(input);
  const headers: Record<string, string> = {
    "content-type": "application/json",
    "cache-control": "no-store",
    "payment-required": paymentRequiredHeader(payload),
  };
  if (payload.payment.retry_after > 0) headers["retry-after"] = String(payload.payment.retry_after);
  const body = {
    ...payload,
    usage: { unit: input.unit, quantity: 0, cost_micro_usd: 0 },
    latency_ms: Math.max(0, Math.round(latencyMs)),
  };
  return new Response(JSON.stringify(body), { status: 402, headers });
}
