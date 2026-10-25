/**
 * Stripe Checkout top-ups (docs/implementation-plan.md E9-T4), without the SDK: two `fetch`
 * calls' worth of API, written against the seam `StripeFetch` so tests run a fake Stripe.
 *
 * - `createCheckoutSession` → `POST https://api.stripe.com/v1/checkout/sessions` (form-encoded,
 *   bearer `STRIPE_SECRET_KEY`). The principal rides in `client_reference_id` and `metadata`, so
 *   the webhook can credit the right wallet without a session table.
 * - `verifyStripeSignature` checks `Stripe-Signature: t=…,v1=…` (HMAC-SHA256 over `${t}.${body}`
 *   with the endpoint secret) within a tolerance window. Constant-time compare.
 * - `creditFromEvent` reads the one event the wallet cares about (`checkout.session.completed`
 *   with `payment_status: "paid"`) and nothing else from the body.
 *
 * The webhook body is read for parsing only and never logged or stored (the event id is the
 * only thing that persists, as the top-up's idempotency key).
 */
import { createHmac, timingSafeEqual } from "node:crypto";

export const STRIPE_API_BASE = "https://api.stripe.com";
export const DEFAULT_SIGNATURE_TOLERANCE_SECS = 300;

/** Smallest and largest top-up (µ$): $5 and $500. */
export const MIN_TOPUP_MICROS = 5_000_000;
export const MAX_TOPUP_MICROS = 500_000_000;

export type StripeFetch = (url: string, init: { method: "POST"; headers: Record<string, string>; body: string }) => Promise<{ status: number; json(): Promise<unknown> }>;

export interface CheckoutInput {
  secretKey: string;
  fetch: StripeFetch;
  principalId: string;
  amountMicros: number;
  successUrl: string;
  cancelUrl: string;
  /** Passed through as Stripe's `Idempotency-Key`, so a retried top-up request creates one session. */
  idempotencyKey?: string;
  apiBase?: string;
}

export interface CheckoutSession {
  sessionId: string;
  url: string;
}

/** An upstream failure, status only (a Stripe error body can echo request fields). */
export class StripeError extends Error {
  constructor(readonly status: number) {
    super("stripe error");
    this.name = "StripeError";
  }
}

/** Micro-USD → whole cents, rounding up so a top-up never credits more than was paid. */
export function microsToCents(micros: number): number {
  return Math.ceil(micros / 10_000);
}

export function centsToMicros(cents: number): number {
  return cents * 10_000;
}

export async function createCheckoutSession(input: CheckoutInput): Promise<CheckoutSession> {
  const form = new URLSearchParams();
  form.set("mode", "payment");
  form.set("client_reference_id", input.principalId);
  form.set("metadata[principal_id]", input.principalId);
  form.set("payment_intent_data[metadata][principal_id]", input.principalId);
  form.set("line_items[0][quantity]", "1");
  form.set("line_items[0][price_data][currency]", "usd");
  form.set("line_items[0][price_data][unit_amount]", String(microsToCents(input.amountMicros)));
  form.set("line_items[0][price_data][product_data][name]", "ShogunAgent wallet top-up");
  form.set("success_url", input.successUrl);
  form.set("cancel_url", input.cancelUrl);
  const headers: Record<string, string> = {
    authorization: `Bearer ${input.secretKey}`,
    "content-type": "application/x-www-form-urlencoded",
  };
  if (input.idempotencyKey) headers["idempotency-key"] = input.idempotencyKey;
  const res = await input.fetch(`${input.apiBase ?? STRIPE_API_BASE}/v1/checkout/sessions`, { method: "POST", headers, body: form.toString() });
  if (res.status < 200 || res.status >= 300) throw new StripeError(res.status);
  const body = (await res.json()) as Record<string, unknown> | null;
  const id = body?.id;
  const url = body?.url;
  if (typeof id !== "string" || typeof url !== "string") throw new StripeError(res.status);
  return { sessionId: id, url };
}

/** Parse `Stripe-Signature` and verify `v1` against the endpoint secret. */
export function verifyStripeSignature(header: string | undefined, rawBody: string, endpointSecret: string, nowSecs: number, toleranceSecs = DEFAULT_SIGNATURE_TOLERANCE_SECS): boolean {
  if (!header) return false;
  let timestamp: string | undefined;
  const signatures: string[] = [];
  for (const part of header.split(",")) {
    const [k, v] = part.trim().split("=", 2);
    if (k === "t" && v) timestamp = v;
    else if (k === "v1" && v) signatures.push(v);
  }
  if (!timestamp || !/^\d+$/.test(timestamp) || signatures.length === 0) return false;
  const t = Number(timestamp);
  if (!Number.isSafeInteger(t) || Math.abs(nowSecs - t) > toleranceSecs) return false;
  const expected = createHmac("sha256", endpointSecret).update(`${timestamp}.${rawBody}`).digest();
  return signatures.some((sig) => {
    if (!/^[0-9a-f]+$/i.test(sig)) return false;
    const presented = Buffer.from(sig, "hex");
    return presented.length === expected.length && timingSafeEqual(presented, expected);
  });
}

/** What a paid Checkout session credits. */
export interface StripeCredit {
  eventId: string;
  principalId: string;
  micros: number;
}

/** The credit a webhook event describes, or `undefined` for every event that is not a paid
 * `checkout.session.completed` with a principal and an amount (those are acknowledged, not acted on). */
export function creditFromEvent(event: unknown): StripeCredit | undefined {
  if (typeof event !== "object" || event === null) return undefined;
  const e = event as Record<string, unknown>;
  if (typeof e.id !== "string" || !e.id.startsWith("evt_") || e.type !== "checkout.session.completed") return undefined;
  const data = (typeof e.data === "object" && e.data !== null ? e.data : {}) as Record<string, unknown>;
  const session = (typeof data.object === "object" && data.object !== null ? data.object : {}) as Record<string, unknown>;
  if (session.payment_status !== "paid") return undefined;
  const metadata = (typeof session.metadata === "object" && session.metadata !== null ? session.metadata : {}) as Record<string, unknown>;
  const principalId = typeof session.client_reference_id === "string" ? session.client_reference_id
    : typeof metadata.principal_id === "string" ? metadata.principal_id : undefined;
  const cents = session.amount_total;
  if (!principalId || typeof cents !== "number" || !Number.isSafeInteger(cents) || cents <= 0) return undefined;
  if (session.currency !== undefined && session.currency !== "usd") return undefined;
  return { eventId: e.id, principalId, micros: centsToMicros(cents) };
}
