/** E30: x402 acceptance behind X402_ENABLED (off by default). The 402 gains an x402 entry when
 * on; X-PAYMENT is verified through the facilitator seam (a fake here), replays are refused, a
 * paid call is recorded with funding "x402" and answers X-PAYMENT-RESPONSE; a failed call is
 * never settled. Decision record: docs/decisions/0002-x402.md. */
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

import {
  InMemoryX402Store, SqlX402Store, decodePaymentHeader, httpFacilitator, x402ConfigFromEnv, x402Requirement,
  type FacilitatorClient, type X402Config, type X402PaymentPayload, type X402Requirement,
} from "../src/billing/x402.js";
import { authed, makeAgentRig, mcp, registerAgent, type AgentRig } from "./agent-helpers.js";
import { sqliteDouble } from "./sql-double.js";

const X402_SQL = readFileSync(new URL("../sql/x402-payments.sql", import.meta.url), "utf8");
const CFG: X402Config = {
  network: "eip155:84532",
  payTo: "0x1111111111111111111111111111111111111111",
  asset: "0x036CbD53842c5426634e7929541eC2318f3dCF7e",
  facilitatorUrl: "https://facilitator.test",
  maxTimeoutSeconds: 60,
};

function paymentHeader(nonce: string, over: Record<string, unknown> = {}): string {
  const payload = {
    x402Version: 1, scheme: "exact", network: CFG.network,
    payload: { signature: `0xsig${nonce}`, authorization: { from: "0x2222222222222222222222222222222222222222", to: CFG.payTo, value: "1500", validAfter: "0", validBefore: "9999999999", nonce } },
    ...over,
  };
  return Buffer.from(JSON.stringify(payload), "utf8").toString("base64");
}

function fakeFacilitator(opts: { valid?: boolean; settle?: boolean; verifyThrows?: number } = {}) {
  const seen: { verify: X402Requirement[]; settle: X402Requirement[] } = { verify: [], settle: [] };
  let throws = opts.verifyThrows ?? 0;
  const facilitator: FacilitatorClient = {
    async verify(_p: X402PaymentPayload, r: X402Requirement) {
      seen.verify.push(r);
      if (throws > 0) { throws -= 1; throw new Error("down"); }
      return opts.valid === false ? { isValid: false, invalidReason: "insufficient_funds" } : { isValid: true, payer: "0x2222222222222222222222222222222222222222" };
    },
    async settle(_p: X402PaymentPayload, r: X402Requirement) {
      seen.settle.push(r);
      return opts.settle === false ? { success: false, errorReason: "settle_failed" } : { success: true, transaction: "0xabc", network: CFG.network, payer: "0x2222222222222222222222222222222222222222" };
    },
  };
  return { facilitator, seen };
}

function x402Rig(f = fakeFacilitator(), store = new InMemoryX402Store()): { rig: AgentRig; seen: ReturnType<typeof fakeFacilitator>["seen"]; store: InMemoryX402Store } {
  return { rig: makeAgentRig({ x402: { config: CFG, facilitator: f.facilitator, store } }), seen: f.seen, store };
}

const decodeB64 = (s: string | null, enc: BufferEncoding = "base64"): Record<string, unknown> => JSON.parse(Buffer.from(s ?? "", enc).toString("utf8")) as Record<string, unknown>;
const INGEST3 = { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "3" };
const TOO_BIG = { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "20000" };

describe("x402 configuration", () => {
  it("is off unless X402_ENABLED=1, and then requires every setting", () => {
    expect(x402ConfigFromEnv({})).toBeUndefined();
    expect(x402ConfigFromEnv({ X402_ENABLED: "0", X402_NETWORK: "base" })).toBeUndefined();
    expect(() => x402ConfigFromEnv({ X402_ENABLED: "1" })).toThrow(/X402_NETWORK/);
    expect(() => x402ConfigFromEnv({ X402_ENABLED: "1", X402_NETWORK: "base", X402_PAY_TO: CFG.payTo, X402_ASSET: CFG.asset, X402_FACILITATOR_URL: "http://insecure.test" })).toThrow(/X402_FACILITATOR_URL/);
    expect(x402ConfigFromEnv({ X402_ENABLED: "1", X402_NETWORK: "eip155:8453", X402_PAY_TO: CFG.payTo, X402_ASSET: CFG.asset, X402_FACILITATOR_URL: "https://f.test/" }))
      .toEqual({ network: "eip155:8453", payTo: CFG.payTo, asset: CFG.asset, facilitatorUrl: "https://f.test", maxTimeoutSeconds: 60 });
  });

  it("prices in atomic USDC units: one unit is one micro-USD", () => {
    const r = x402Requirement(CFG, 6000, "https://api.test/mcp");
    expect(r).toMatchObject({ rail: "x402", scheme: "exact", network: CFG.network, asset: CFG.asset, payTo: CFG.payTo, amount: "6000", maxAmountRequired: "6000", resource: "https://api.test/mcp" });
  });

  it("decodes v1 and v2 payloads and keys replays on the signed content, not the encoding", () => {
    const a = decodePaymentHeader(paymentHeader("n1"));
    const b = decodePaymentHeader(Buffer.from(Buffer.from(paymentHeader("n1"), "base64").toString("utf8"), "utf8").toString("base64url"));
    expect(a.ok && b.ok && a.hash === b.hash).toBe(true);
    const v2 = decodePaymentHeader(Buffer.from(JSON.stringify({ x402Version: 2, accepted: { scheme: "exact", network: CFG.network }, payload: { signature: "0x" } })).toString("base64"));
    expect(v2.ok && v2.payload.network).toBe(CFG.network);
    expect(decodePaymentHeader("not base64 !!").ok).toBe(false);
    expect(decodePaymentHeader(Buffer.from("[]").toString("base64")).ok).toBe(false);
  });
});

describe("the 402 with x402 off and on", () => {
  it("off (default): accepts holds the card rail only, and X-PAYMENT is ignored (wallet-funded)", async () => {
    const rig = makeAgentRig();
    const { key } = await registerAgent(rig);
    const res = await mcp(rig, key, TOO_BIG);
    expect(res.status).toBe(402);
    const body = (await res.json()) as { payment: { accepts: Array<{ rail: string }> } };
    expect(body.payment.accepts.map((a) => a.rail)).toEqual(["card"]);
    const paid = await mcp(rig, key, { ...INGEST3, "x-payment": paymentHeader("ignored") });
    expect(paid.status).toBe(200);
    expect(paid.headers.get("x-payment-response")).toBeNull();
  });

  it("on: accepts appends an x402 entry for exactly the quoted amount, in the body and the PAYMENT-REQUIRED header", async () => {
    const { rig } = x402Rig();
    const { key } = await registerAgent(rig);
    const res = await mcp(rig, key, TOO_BIG);
    expect(res.status).toBe(402);
    const body = (await res.json()) as { error: { code: string }; payment: { amount_micro_usd: number; accepts: Array<Record<string, unknown>> } };
    expect(body.error.code).toBe("payment_required");
    expect(body.payment.accepts.map((a) => a.rail)).toEqual(["card", "x402"]);
    const x = body.payment.accepts[1] ?? {};
    expect(x).toMatchObject({ scheme: "exact", network: CFG.network, asset: CFG.asset, payTo: CFG.payTo, amount: String(body.payment.amount_micro_usd), resource: "https://api.test/mcp" });
    const header = decodeB64(res.headers.get("payment-required"), "base64url") as { accepts: Array<{ rail: string }> };
    expect(header.accepts.map((a) => a.rail)).toEqual(["card", "x402"]);
  });
});

describe("paying a call with X-PAYMENT", () => {
  it("verifies, proceeds without touching the wallet, records funding x402, settles and answers X-PAYMENT-RESPONSE", async () => {
    const { rig, seen } = x402Rig();
    const { key } = await registerAgent(rig);
    const before = (await (await authed(rig, key, "/v1/wallet")).json()) as { balance_micro_usd: number };
    const res = await mcp(rig, key, { ...INGEST3, "x-payment": paymentHeader("ok-1") });
    expect(res.status).toBe(200);
    expect(rig.upstreamCalls).toHaveLength(1);
    expect(seen.verify[0]).toMatchObject({ amount: "1500", payTo: CFG.payTo, scheme: "exact" });
    expect(seen.settle).toHaveLength(1);
    expect(res.headers.get("x-usage-funding")).toBe("x402");
    expect(res.headers.get("x-usage-cost-micro-usd")).toBe("1500");
    expect(decodeB64(res.headers.get("x-payment-response"))).toMatchObject({ success: true, transaction: "0xabc", network: CFG.network });
    const after = (await (await authed(rig, key, "/v1/wallet")).json()) as { balance_micro_usd: number; reserved_micro_usd: number };
    expect(after.balance_micro_usd).toBe(before.balance_micro_usd);
    expect(after.reserved_micro_usd).toBe(0);
    const receipts = (await (await authed(rig, key, "/v1/receipts")).json()) as { receipts: Array<Record<string, unknown>> };
    expect(receipts.receipts.at(-1)).toMatchObject({ funding: "x402", cost_micro_usd: 1500, settled: true });
    // The payment payload never reaches a log line.
    expect(rig.logLines.join("\n")).not.toContain("0xsigok-1");
  });

  it("refuses a replayed payment payload without calling the core again", async () => {
    const { rig, seen } = x402Rig();
    const { key } = await registerAgent(rig);
    const header = paymentHeader("replay-1");
    expect((await mcp(rig, key, { ...INGEST3, "x-payment": header })).status).toBe(200);
    const again = await mcp(rig, key, { ...INGEST3, "x-payment": header });
    expect(again.status).toBe(402);
    expect(((await again.json()) as { error: { code: string } }).error.code).toBe("payment_replayed");
    expect(rig.upstreamCalls).toHaveLength(1);
    expect(seen.verify).toHaveLength(1);
    // Also refused under the v2 header name.
    expect((await mcp(rig, key, { ...INGEST3, "payment-signature": header })).status).toBe(402);
  });

  it("refuses a payment the facilitator rejects, or one for another network, with a fresh accepts list", async () => {
    const { rig } = x402Rig(fakeFacilitator({ valid: false }));
    const { key } = await registerAgent(rig);
    const bad = await mcp(rig, key, { ...INGEST3, "x-payment": paymentHeader("bad-1") });
    expect(bad.status).toBe(402);
    const body = (await bad.json()) as { error: { code: string; message: string }; payment: { accepts: Array<{ rail: string }> } };
    expect(body.error.code).toBe("payment_invalid");
    expect(body.error.message).toContain("insufficient_funds");
    expect(body.payment.accepts.map((a) => a.rail)).toEqual(["card", "x402"]);
    const wrong = await mcp(rig, key, { ...INGEST3, "x-payment": paymentHeader("net-1", { network: "eip155:1" }) });
    expect(((await wrong.json()) as { error: { code: string } }).error.code).toBe("payment_invalid");
    expect(rig.upstreamCalls).toHaveLength(0);
  });

  it("never settles a failed call: the payment is not captured and the receipt costs 0", async () => {
    const { rig, seen } = x402Rig();
    const { key } = await registerAgent(rig);
    rig.upstream.status = 500;
    const res = await mcp(rig, key, { ...INGEST3, "x-payment": paymentHeader("fail-1") });
    expect(res.status).toBe(500);
    expect(seen.settle).toHaveLength(0);
    expect(decodeB64(res.headers.get("x-payment-response"))).toMatchObject({ success: false });
    const receipts = (await (await authed(rig, key, "/v1/receipts")).json()) as { receipts: Array<Record<string, unknown>> };
    expect(receipts.receipts.at(-1)).toMatchObject({ funding: "x402", cost_micro_usd: 0, settled: true });
  });

  it("a facilitator outage before verification lets the same payment be retried", async () => {
    const { rig } = x402Rig(fakeFacilitator({ verifyThrows: 1 }));
    const { key } = await registerAgent(rig);
    const header = paymentHeader("retry-1");
    expect((await mcp(rig, key, { ...INGEST3, "x-payment": header })).status).toBe(503);
    expect((await mcp(rig, key, { ...INGEST3, "x-payment": header })).status).toBe(200);
  });

  it("hard caps still bound x402-funded calls", async () => {
    const { rig } = x402Rig();
    const { key } = await registerAgent(rig, { caps: { day_micro_usd: 1000 } });
    const res = await mcp(rig, key, { ...INGEST3, "x-payment": paymentHeader("cap-1") });
    expect(res.status).toBe(402);
    expect(((await res.json()) as { error: { code: string } }).error.code).toBe("cap_exceeded");
    expect(rig.upstreamCalls).toHaveLength(0);
  });
});

describe("the replay guard store and the HTTP facilitator", () => {
  it("SqlX402Store: first claim wins, a replay loses, a retryable payload can be claimed again", async () => {
    const store = new SqlX402Store(sqliteDouble(X402_SQL));
    await store.checkSchema();
    expect(await store.claim("h1", "rcpt_1", 1)).toBe(true);
    expect(await store.claim("h1", "rcpt_2", 2)).toBe(false);
    await store.mark("h1", "retryable", 3);
    expect(await store.claim("h1", "rcpt_3", 4)).toBe(true);
    await store.mark("h1", "settled", 5);
    expect(await store.claim("h1", "rcpt_4", 6)).toBe(false);
  });

  it("posts { x402Version, paymentPayload, paymentRequirements } to /verify and /settle without our private fields", async () => {
    const posted: Array<{ url: string; body: Record<string, unknown> }> = [];
    const f = httpFacilitator("https://facilitator.test", async (url, init) => {
      posted.push({ url, body: JSON.parse(init.body) as Record<string, unknown> });
      return { status: 200, json: async () => (url.endsWith("/verify") ? { isValid: true, payer: "0xp" } : { success: true, transaction: "0xt", network: CFG.network }) };
    });
    const decoded = decodePaymentHeader(paymentHeader("h"));
    if (!decoded.ok) throw new Error("decode");
    const req = x402Requirement(CFG, 1500, "https://api.test/mcp");
    expect(await f.verify(decoded.payload, req)).toEqual({ isValid: true, payer: "0xp" });
    expect(await f.settle(decoded.payload, req)).toMatchObject({ success: true, transaction: "0xt" });
    expect(posted.map((p) => p.url)).toEqual(["https://facilitator.test/verify", "https://facilitator.test/settle"]);
    const requirements = posted[0]?.body.paymentRequirements as Record<string, unknown>;
    expect(requirements.rail).toBeUndefined();
    expect(requirements).toMatchObject({ scheme: "exact", amount: "1500", maxAmountRequired: "1500", extra: { name: "USDC", version: "2" } });
  });
});
