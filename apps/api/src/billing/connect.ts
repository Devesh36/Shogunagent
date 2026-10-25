/**
 * Stripe Connect Express for provider payouts (E29), without the SDK: four calls, written against
 * the `ConnectFetch` seam so tests run a fake Stripe.
 *
 * - `createExpressAccount` → `POST /v1/accounts` (`type=express`, transfers capability), with
 *   `Idempotency-Key: connect:account:<provider_id>` so a retried onboarding creates one account.
 * - `createAccountLink`    → `POST /v1/account_links` (`type=account_onboarding`): the hosted
 *   onboarding URL with our refresh and return pages.
 * - `retrieveAccount`      → `GET /v1/accounts/{id}`: whether details were submitted and payouts
 *   are enabled (what binds `payout_account_ref`).
 * - `createTransfer`       → `POST /v1/transfers` (`currency=usd`, `destination=<acct>`,
 *   `transfer_group=<payout key>`), with the provider-month's `Idempotency-Key`.
 *
 * Responses are read for the few fields named here and never logged; a failure is a
 * `StripeError` carrying the status only (a Stripe error body can echo request fields).
 */
import { STRIPE_API_BASE, StripeError } from "./stripe.js";

export type ConnectFetch = (url: string, init: { method: "GET" | "POST"; headers: Record<string, string>; body?: string }) => Promise<{ status: number; json(): Promise<unknown> }>;

export interface ConnectClient {
  secretKey: string;
  fetch: ConnectFetch;
  apiBase?: string;
}

const obj = (v: unknown): Record<string, unknown> => (typeof v === "object" && v !== null && !Array.isArray(v) ? (v as Record<string, unknown>) : {});
const str = (v: unknown): string | undefined => (typeof v === "string" && v.length > 0 ? v : undefined);

async function call(c: ConnectClient, method: "GET" | "POST", path: string, form?: URLSearchParams, idempotencyKey?: string): Promise<Record<string, unknown>> {
  const headers: Record<string, string> = { authorization: `Bearer ${c.secretKey}` };
  if (method === "POST") headers["content-type"] = "application/x-www-form-urlencoded";
  if (idempotencyKey) headers["idempotency-key"] = idempotencyKey;
  const res = await c.fetch(`${c.apiBase ?? STRIPE_API_BASE}${path}`, { method, headers, ...(form ? { body: form.toString() } : {}) });
  if (res.status < 200 || res.status >= 300) throw new StripeError(res.status);
  const body = (await res.json()) as unknown;
  if (typeof body !== "object" || body === null) throw new StripeError(res.status);
  return body as Record<string, unknown>;
}

export const ACCOUNT_ID_RE = /^acct_[A-Za-z0-9]{1,64}$/;

export async function createExpressAccount(c: ConnectClient, providerId: string): Promise<{ accountId: string }> {
  const form = new URLSearchParams();
  form.set("type", "express");
  form.set("capabilities[transfers][requested]", "true");
  form.set("metadata[provider_id]", providerId);
  const body = await call(c, "POST", "/v1/accounts", form, `connect:account:${providerId}`);
  const id = str(body.id);
  if (!id || !ACCOUNT_ID_RE.test(id)) throw new StripeError(502);
  return { accountId: id };
}

export async function createAccountLink(c: ConnectClient, input: { accountId: string; refreshUrl: string; returnUrl: string }): Promise<{ url: string; expiresAt?: number }> {
  const form = new URLSearchParams();
  form.set("account", input.accountId);
  form.set("refresh_url", input.refreshUrl);
  form.set("return_url", input.returnUrl);
  form.set("type", "account_onboarding");
  const body = await call(c, "POST", "/v1/account_links", form);
  const url = str(body.url);
  if (!url || !url.startsWith("https://")) throw new StripeError(502);
  const exp = body.expires_at;
  return { url, ...(typeof exp === "number" && Number.isSafeInteger(exp) ? { expiresAt: exp * 1000 } : {}) };
}

export interface ConnectAccountState {
  detailsSubmitted: boolean;
  payoutsEnabled: boolean;
  /** Stripe has requirements past due: the account cannot receive transfers until they are met. */
  disabled: boolean;
}

export async function retrieveAccount(c: ConnectClient, accountId: string): Promise<ConnectAccountState> {
  if (!ACCOUNT_ID_RE.test(accountId)) throw new StripeError(400);
  const body = await call(c, "GET", `/v1/accounts/${encodeURIComponent(accountId)}`);
  const req = obj(body.requirements);
  return {
    detailsSubmitted: body.details_submitted === true,
    payoutsEnabled: body.payouts_enabled === true,
    disabled: typeof req.disabled_reason === "string" && req.disabled_reason.length > 0,
  };
}

export async function createTransfer(c: ConnectClient, input: { amountCents: number; destination: string; providerId: string; month: string; idempotencyKey: string }): Promise<{ transferId: string }> {
  if (!Number.isSafeInteger(input.amountCents) || input.amountCents <= 0) throw new StripeError(400);
  if (!ACCOUNT_ID_RE.test(input.destination)) throw new StripeError(400);
  const form = new URLSearchParams();
  form.set("amount", String(input.amountCents));
  form.set("currency", "usd");
  form.set("destination", input.destination);
  form.set("transfer_group", input.idempotencyKey);
  form.set("metadata[provider_id]", input.providerId);
  form.set("metadata[month]", input.month);
  const body = await call(c, "POST", "/v1/transfers", form, input.idempotencyKey);
  const id = str(body.id);
  if (!id) throw new StripeError(502);
  return { transferId: id };
}

/** The account status a Stripe account state maps to. */
export function statusOf(s: ConnectAccountState): "onboarding" | "active" | "restricted" {
  if (!s.detailsSubmitted) return "onboarding";
  return s.payoutsEnabled && !s.disabled ? "active" : "restricted";
}
