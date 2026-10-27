/** E9-T4: Stripe Checkout top-ups with a fake Stripe — session creation, webhook signature
 * verification, idempotent credit by event id, and the end-to-end: 402 → pay → the same call is 200. */
import { createHmac } from "node:crypto";
import { describe, expect, it } from "vitest";

import { MAX_TOPUP_MICROS, MIN_TOPUP_MICROS, centsToMicros, creditFromEvent, microsToCents, verifyStripeSignature, type StripeFetch } from "../src/billing/stripe.js";
import { FREE_ALLOWANCE_MICROS, RATE_MICROS } from "../src/pricing.js";
import { BASE, authed, makeAgentRig, mcp, registerAgent, type AgentRig } from "./agent-helpers.js";

const WEBHOOK_SECRET = "whsec_test";

/** A Stripe double: records the form it was sent (never the key beyond a presence check) and
 * answers a session. */
function fakeStripe(status = 200) {
  const calls: Array<{ url: string; auth: boolean; idempotencyKey?: string; form: URLSearchParams }> = [];
  const fetch: StripeFetch = (url, init) => {
    calls.push({ url, auth: init.headers.authorization === "Bearer sk_test_fake", ...(init.headers["idempotency-key"] ? { idempotencyKey: init.headers["idempotency-key"] } : {}), form: new URLSearchParams(init.body) });
    const responseStatus = (init.headers["idempotency-key"]?.length ?? 0) > 255 ? 400 : status;
    return Promise.resolve({ status: responseStatus, json: () => Promise.resolve(responseStatus === 200 ? { id: `cs_test_${calls.length}`, url: `https://checkout.stripe.test/c/pay/cs_test_${calls.length}` } : { error: { message: "nope" } }) });
  };
  return { calls, fetch };
}

function rigWithStripe(status = 200) {
  const stripe = fakeStripe(status);
  const rig = makeAgentRig({ stripe: { secretKey: "sk_test_fake", webhookSecret: WEBHOOK_SECRET, fetch: stripe.fetch } });
  return { rig, stripe };
}

function signedEvent(rig: AgentRig, event: unknown, secret = WEBHOOK_SECRET, at = Math.floor(rig.now().getTime() / 1000)): { body: string; signature: string } {
  const body = JSON.stringify(event);
  const sig = createHmac("sha256", secret).update(`${at}.${body}`).digest("hex");
  return { body, signature: `t=${at},v1=${sig}` };
}

function checkoutCompleted(eventId: string, principalId: string, cents: number, paymentStatus = "paid"): unknown {
  return { id: eventId, type: "checkout.session.completed", data: { object: { id: "cs_1", object: "checkout.session", payment_status: paymentStatus, amount_total: cents, currency: "usd", client_reference_id: principalId, metadata: { principal_id: principalId } } } };
}

async function webhook(rig: AgentRig, signed: { body: string; signature: string }): Promise<Response> {
  return rig.app.request("/v1/stripe/webhook", { method: "POST", headers: { "content-type": "application/json", "stripe-signature": signed.signature }, body: signed.body });
}

describe("stripe helpers", () => {
  it("converts micro-USD to whole cents rounding up, and back", () => {
    expect(microsToCents(10_000_000)).toBe(1_000);
    expect(microsToCents(10_000_001)).toBe(1_001);
    expect(centsToMicros(1_000)).toBe(10_000_000);
  });

  it("verifies a Stripe-Signature header within tolerance", () => {
    const body = '{"id":"evt_1"}';
    const sig = createHmac("sha256", WEBHOOK_SECRET).update(`1000.${body}`).digest("hex");
    expect(verifyStripeSignature(`t=1000,v1=${sig}`, body, WEBHOOK_SECRET, 1100)).toBe(true);
    expect(verifyStripeSignature(`t=1000,v1=deadbeef,v1=${sig}`, body, WEBHOOK_SECRET, 1100)).toBe(true);
    expect(verifyStripeSignature(`t=1000,v1=${sig}`, body, WEBHOOK_SECRET, 1400)).toBe(false);
    expect(verifyStripeSignature(`t=1000,v1=${sig}`, body, "other", 1100)).toBe(false);
    expect(verifyStripeSignature(`t=1000,v1=${sig}`, body + " ", WEBHOOK_SECRET, 1100)).toBe(false);
    expect(verifyStripeSignature(undefined, body, WEBHOOK_SECRET, 1100)).toBe(false);
    expect(verifyStripeSignature("v1=abc", body, WEBHOOK_SECRET, 1100)).toBe(false);
  });

  it("reads a credit only from a paid checkout.session.completed", () => {
    expect(creditFromEvent(checkoutCompleted("evt_1", "prn_1", 1_500))).toEqual({ eventId: "evt_1", principalId: "prn_1", micros: 15_000_000 });
    expect(creditFromEvent(checkoutCompleted("evt_1", "prn_1", 1_500, "unpaid"))).toBeUndefined();
    expect(creditFromEvent({ id: "evt_2", type: "payment_intent.succeeded", data: { object: {} } })).toBeUndefined();
    expect(creditFromEvent({ id: "nope", type: "checkout.session.completed", data: { object: { payment_status: "paid", amount_total: 1, client_reference_id: "p" } } })).toBeUndefined();
    expect(creditFromEvent(null)).toBeUndefined();
  });
});

describe("POST /v1/wallet/topup", () => {
  it("creates a Checkout session for the principal and returns its URL", async () => {
    const { rig, stripe } = rigWithStripe();
    const { key, principalId } = await registerAgent(rig);
    const res = await authed(rig, key, "/v1/wallet/topup", { method: "POST", body: { amount_micro_usd: 10_000_000 }, headers: { "idempotency-key": "topup-1" } });
    expect(res.status).toBe(201);
    const body = (await res.json()) as Record<string, unknown>;
    expect(body).toEqual(expect.objectContaining({ rail: "card", url: "https://checkout.stripe.test/c/pay/cs_test_1", session_id: "cs_test_1", amount_micro_usd: 10_000_000, next_action: "approve_at_url" }));
    expect(stripe.calls).toHaveLength(1);
    const call = stripe.calls[0]!;
    expect(call.url).toBe("https://api.stripe.com/v1/checkout/sessions");
    expect(call.auth).toBe(true);
    expect(call.idempotencyKey).toBe(`${principalId}:topup-1`);
    expect(call.form.get("mode")).toBe("payment");
    expect(call.form.get("client_reference_id")).toBe(principalId);
    expect(call.form.get("metadata[principal_id]")).toBe(principalId);
    expect(call.form.get("line_items[0][price_data][unit_amount]")).toBe("1000");
    expect(call.form.get("line_items[0][price_data][currency]")).toBe("usd");
    expect(call.form.get("success_url")).toBe(`${BASE}/v1/wallet/topup/done?status=success`);
    // Nothing is credited until Stripe says so.
    expect(((await (await authed(rig, key, "/v1/wallet")).json()) as { balance_micro_usd: number }).balance_micro_usd).toBe(FREE_ALLOWANCE_MICROS);
    expect(rig.logLines.some((l) => l.includes("POST /v1/wallet/topup 201") && l.includes("event=topup_session_created"))).toBe(true);
  });

  it("accepts long idempotency keys within Stripe's limit while preserving existing scoped keys", async () => {
    const { rig, stripe } = rigWithStripe();
    const { key, principalId } = await registerAgent(rig);
    const lastUnchangedLength = 255 - principalId.length - 1;
    for (const length of [lastUnchangedLength, lastUnchangedLength + 1, 255]) {
      const idem = "x".repeat(length);
      const res = await authed(rig, key, "/v1/wallet/topup", { method: "POST", body: { amount_micro_usd: MIN_TOPUP_MICROS }, headers: { "idempotency-key": idem } });
      expect(res.status).toBe(201);
      const upstreamKey = stripe.calls.at(-1)!.idempotencyKey!;
      expect(upstreamKey.length).toBeLessThanOrEqual(255);
      if (length === lastUnchangedLength) expect(upstreamKey).toBe(`${principalId}:${idem}`);
    }
    expect(stripe.calls).toHaveLength(3);
    const invalid = await authed(rig, key, "/v1/wallet/topup", { method: "POST", body: { amount_micro_usd: MIN_TOPUP_MICROS }, headers: { "idempotency-key": "x".repeat(256) } });
    expect(invalid.status).toBe(400);
    expect(stripe.calls).toHaveLength(3);
  });

  it("keeps long idempotency keys stable on retries and distinct across principals and requests", async () => {
    const { rig, stripe } = rigWithStripe();
    const first = await registerAgent(rig);
    const second = await registerAgent(rig);
    const idem = "x".repeat(255);
    for (const [key, idempotencyKey] of [[first.key, idem], [first.key, idem], [second.key, idem], [first.key, `${"x".repeat(254)}y`]] as const) {
      const res = await authed(rig, key, "/v1/wallet/topup", { method: "POST", body: { amount_micro_usd: MIN_TOPUP_MICROS }, headers: { "idempotency-key": idempotencyKey } });
      expect(res.status).toBe(201);
    }
    const keys = stripe.calls.map((call) => call.idempotencyKey);
    expect(keys[0]).toBe(keys[1]);
    expect(new Set([keys[0], keys[2], keys[3]]).size).toBe(3);
  });

  it("validates the amount, needs the write scope, and answers 503 when Stripe fails or is not configured", async () => {
    const { rig } = rigWithStripe();
    const { key } = await registerAgent(rig);
    expect((await authed(rig, key, "/v1/wallet/topup", { method: "POST", body: { amount_micro_usd: MIN_TOPUP_MICROS - 1 } })).status).toBe(400);
    expect((await authed(rig, key, "/v1/wallet/topup", { method: "POST", body: { amount_micro_usd: MAX_TOPUP_MICROS + 1 } })).status).toBe(400);
    expect((await authed(rig, key, "/v1/wallet/topup", { method: "POST", body: { amount_micro_usd: "10" } })).status).toBe(400);
    const { rig: failing } = rigWithStripe(500);
    const f = await registerAgent(failing);
    const down = await authed(failing, f.key, "/v1/wallet/topup", { method: "POST", body: { amount_micro_usd: MIN_TOPUP_MICROS } });
    expect(down.status).toBe(503);
    expect(failing.logLines.some((l) => l.includes("stripe_failure status=500"))).toBe(true);
    const none = makeAgentRig();
    const n = await registerAgent(none);
    expect((await authed(none, n.key, "/v1/wallet/topup", { method: "POST", body: { amount_micro_usd: MIN_TOPUP_MICROS } })).status).toBe(503);
  });
});

describe("POST /v1/stripe/webhook", () => {
  it("credits the wallet once per event id, after verifying the signature", async () => {
    const { rig } = rigWithStripe();
    const { key, principalId } = await registerAgent(rig);
    const event = checkoutCompleted("evt_100", principalId, 2_000);
    const bad = await webhook(rig, signedEvent(rig, event, "whsec_wrong"));
    expect(bad.status).toBe(401);
    const stale = await webhook(rig, signedEvent(rig, event, WEBHOOK_SECRET, Math.floor(rig.now().getTime() / 1000) - 3_600));
    expect(stale.status).toBe(401);
    const ok = await webhook(rig, signedEvent(rig, event));
    expect(ok.status).toBe(200);
    expect(await ok.json()).toEqual({ received: true, credited: true });
    const again = await webhook(rig, signedEvent(rig, event));
    expect(await again.json()).toEqual({ received: true, credited: false });
    expect(((await (await authed(rig, key, "/v1/wallet")).json()) as { balance_micro_usd: number }).balance_micro_usd).toBe(FREE_ALLOWANCE_MICROS + 20_000_000);
    // Other events are acknowledged and ignored.
    const other = await webhook(rig, signedEvent(rig, { id: "evt_101", type: "payment_intent.succeeded", data: { object: {} } }));
    expect(await other.json()).toEqual({ received: true, credited: false });
    const unpaid = await webhook(rig, signedEvent(rig, checkoutCompleted("evt_102", principalId, 2_000, "unpaid")));
    expect(await unpaid.json()).toEqual({ received: true, credited: false });
    expect(rig.logLines.filter((l) => l.includes("event=topup_credited"))).toHaveLength(1);
    expect(rig.logLines.filter((l) => l.includes("event=webhook_rejected"))).toHaveLength(2);
    // The webhook body (the event) never reaches the log.
    expect(rig.logLines.join("\n")).not.toContain("evt_100");
    expect(rig.logLines.join("\n")).not.toContain(principalId);
  });

  it("end to end: a 402'd call succeeds unchanged after the top-up is credited", async () => {
    const { rig } = rigWithStripe();
    const { key, principalId } = await registerAgent(rig);
    // Drain the allowance.
    await rig.store.reserve({ receiptId: "drain", principalId, keyId: "ak_x", method: "tools/call", unit: "model_op", quantity: 1, micros: FREE_ALLOWANCE_MICROS, ts: 0, day: "2026-01-01", month: "2026-01", caps: { key: { day: 1e12, month: 1e12 }, principal: { day: 1e12, month: 1e12 } } });
    await rig.store.settle("drain", FREE_ALLOWANCE_MICROS);
    const headers = { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "4" };
    const refused = await mcp(rig, key, headers);
    expect(refused.status).toBe(402);
    const payment = ((await refused.json()) as { payment: { accepts: Array<{ url: string }>; amount_micro_usd: number } }).payment;
    expect(payment.amount_micro_usd).toBe(4 * RATE_MICROS.ingest);
    // The agent follows the rail: POST the top-up route, hand the URL to a human, Stripe calls back.
    const topup = await authed(rig, key, payment.accepts[0]!.url.replace(BASE, ""), { method: "POST", body: { amount_micro_usd: 10_000_000 } });
    expect(topup.status).toBe(201);
    expect((await webhook(rig, signedEvent(rig, checkoutCompleted("evt_e2e", principalId, 1_000)))).status).toBe(200);
    const retried = await mcp(rig, key, headers);
    expect(retried.status).toBe(200);
    expect(retried.headers.get("x-usage-cost-micro-usd")).toBe(String(4 * RATE_MICROS.ingest));
    expect(((await (await authed(rig, key, "/v1/wallet")).json()) as { balance_micro_usd: number }).balance_micro_usd).toBe(10_000_000 - 4 * RATE_MICROS.ingest);
  });
});
