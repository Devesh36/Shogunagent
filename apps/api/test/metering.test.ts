/** E9-T5 / E9-T2 / E9-T3 / E9-T6: the metered `/mcp` proxy — reads free, ingest and model ops
 * charged from headers alone, 402 in the plan's shape (and the x402-style header) when the
 * wallet cannot cover a call, `cap_exceeded` when a cap would be crossed, a dry run that quotes
 * without reserving, and refunds when the core fails. */
import { describe, expect, it } from "vitest";

import { capSubjectKey, capSubjectPrincipal } from "../src/agent-store.js";
import { paymentRequiredHeader, paymentRequiredPayload } from "../src/billing/402.js";
import { DEFAULT_CAPS } from "../src/caps.js";
import { logId } from "../src/logging.js";
import { FREE_ALLOWANCE_MICROS, RATE_MICROS } from "../src/pricing.js";
import { DEFAULT_BATCH_PRICES } from "../src/spend.js";
import { InMemoryUsageStore } from "../src/usage.js";
import { BASE, authed, makeAgentRig, mcp, registerAgent, type AgentRig } from "./agent-helpers.js";

interface Err { error: { code: string; message: string; next_action: string | null }; payment?: Record<string, unknown>; usage?: unknown; latency_ms?: number }

async function wallet(rig: AgentRig, key: string): Promise<{ balance: number; reserved: number; available: number }> {
  const w = (await (await authed(rig, key, "/v1/wallet")).json()) as { balance_micro_usd: number; reserved_micro_usd: number; available_micro_usd: number };
  return { balance: w.balance_micro_usd, reserved: w.reserved_micro_usd, available: w.available_micro_usd };
}

describe("metered /mcp", () => {
  it("records reads as free receipts: counted, never charged, and forwarded", async () => {
    const rig = makeAgentRig();
    const { key, principalId } = await registerAgent(rig);
    for (const name of ["memory.search", "memory.recall", "memory.get_context_pack", "state.commitments.list", "approvals.decide"]) {
      const res = await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": name });
      expect(res.status).toBe(200);
      expect(res.headers.get("x-usage-unit")).toBe("read");
      expect(res.headers.get("x-usage-cost-micro-usd")).toBe("0");
      expect(res.headers.get("x-receipt-id")).toMatch(/^rcpt_/);
      expect(await res.json()).toEqual({ jsonrpc: "2.0", id: 1, result: { ok: true } });
    }
    // Protocol traffic (initialize, tools/list, the GET stream) is free too.
    expect((await mcp(rig, key, { "mcp-method": "initialize" })).headers.get("x-usage-unit")).toBe("protocol");
    expect((await mcp(rig, key, {}, "GET")).headers.get("x-usage-unit")).toBe("protocol");
    expect(rig.upstreamCalls).toHaveLength(7);
    expect(rig.upstreamCalls[0]?.principalId).toBe(principalId);
    expect(await wallet(rig, key)).toEqual({ balance: FREE_ALLOWANCE_MICROS, reserved: 0, available: FREE_ALLOWANCE_MICROS });
    const receipts = (await (await authed(rig, key, "/v1/receipts")).json()) as { receipts: Array<{ unit: string; cost_micro_usd: number; quantity: number }> };
    expect(receipts.receipts).toHaveLength(7);
    expect(receipts.receipts.every((r) => r.cost_micro_usd === 0)).toBe(true);
  });

  it("charges ingest per event from Mcp-Quantity and model ops per call, settling to the quote", async () => {
    const rig = makeAgentRig();
    const { key } = await registerAgent(rig);
    const ingest = await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "12" });
    expect(ingest.status).toBe(200);
    expect(ingest.headers.get("x-usage-unit")).toBe("ingest");
    expect(ingest.headers.get("x-usage-quantity")).toBe("12");
    expect(ingest.headers.get("x-usage-cost-micro-usd")).toBe(String(12 * RATE_MICROS.ingest));
    const op = await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.summarize" });
    expect(op.headers.get("x-usage-unit")).toBe("model_op");
    expect(op.headers.get("x-usage-cost-micro-usd")).toBe(String(RATE_MICROS.model_op));
    const spent = 12 * RATE_MICROS.ingest + RATE_MICROS.model_op;
    expect(await wallet(rig, key)).toEqual({ balance: FREE_ALLOWANCE_MICROS - spent, reserved: 0, available: FREE_ALLOWANCE_MICROS - spent });
    expect((await (await authed(rig, key, "/v1/wallet")).json())).toEqual(expect.objectContaining({ spend: { key: { day_micro_usd: spent, month_micro_usd: spent }, principal: { day_micro_usd: spent, month_micro_usd: spent } } }));
    expect((await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "0" })).status).toBe(400);
    expect((await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "12.5" })).status).toBe(400);
  });

  it("answers 402 payment_required in the plan's shape, with the PAYMENT-REQUIRED header, when the wallet cannot cover the call", async () => {
    const rig = makeAgentRig();
    const { key, principalId } = await registerAgent(rig);
    // Drain the allowance to 1,000 µ$ with a direct ledger write (a settled charge).
    await rig.store.reserve({ receiptId: "drain", principalId, keyId: "ak_x", method: "tools/call", unit: "model_op", quantity: 1, micros: FREE_ALLOWANCE_MICROS - 1_000, ts: 0, day: "2026-01-01", month: "2026-01", caps: { key: { day: 1e12, month: 1e12 }, principal: { day: 1e12, month: 1e12 } } });
    await rig.store.settle("drain", FREE_ALLOWANCE_MICROS - 1_000);
    const res = await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "10" });
    expect(res.status).toBe(402);
    expect(res.headers.get("cache-control")).toBe("no-store");
    const body = (await res.json()) as Err;
    expect(body.error).toEqual({ code: "payment_required", message: expect.stringContaining("Top up"), next_action: "top_up" });
    expect(body.payment).toEqual({
      amount_micro_usd: 10 * RATE_MICROS.ingest, unit: "ingest", balance_micro_usd: 1_000,
      accepts: [{ rail: "card", url: `${BASE}/v1/wallet/topup`, method: "POST" }], retry_after: 0,
    });
    expect(body.usage).toEqual({ unit: "ingest", quantity: 0, cost_micro_usd: 0 });
    expect(typeof body.latency_ms).toBe("number");
    // The header is the same payload, x402-shaped, base64url.
    const header = res.headers.get("payment-required")!;
    const decoded = JSON.parse(Buffer.from(header, "base64url").toString("utf8")) as Record<string, unknown>;
    expect(decoded).toEqual(expect.objectContaining({ x402Version: 2, error: "payment_required", next_action: "top_up", accepts: body.payment!.accepts, payment: body.payment }));
    expect(header).toBe(paymentRequiredHeader(paymentRequiredPayload({ reason: "payment_required", amountMicros: 5_000, unit: "ingest", balanceMicros: 1_000, topupUrl: `${BASE}/v1/wallet/topup` })));
    expect(res.headers.get("retry-after")).toBeNull();
    // Nothing was forwarded, nothing held, and a free read still works with an empty wallet.
    expect(rig.upstreamCalls).toHaveLength(0);
    expect(await wallet(rig, key)).toEqual({ balance: 1_000, reserved: 0, available: 1_000 });
    expect((await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.search" })).status).toBe(200);
    expect(rig.logLines.some((l) => l.includes("POST /mcp 402") && l.includes("event=payment_required"))).toBe(true);
  });

  it("answers 402 cap_exceeded with retry_after and no rails when a key cap would be crossed", async () => {
    const rig = makeAgentRig({ caps: { key: { day: 1_200, month: 100_000 }, principal: DEFAULT_CAPS.principal } });
    const { key, keyId } = await registerAgent(rig);
    expect((await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "2" })).status).toBe(200);
    const res = await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "1" });
    expect(res.status).toBe(402);
    const body = (await res.json()) as Err;
    expect(body.error.code).toBe("cap_exceeded");
    expect(body.error.next_action).toBe("retry_after");
    expect(body.payment).toEqual(expect.objectContaining({
      amount_micro_usd: RATE_MICROS.ingest, unit: "ingest", accepts: [],
      cap: { scope: "key", period: "day", limit_micro_usd: 1_200, spent_micro_usd: 1_000 },
    }));
    // 12:00 UTC → 43,200 s to midnight.
    expect(body.payment!.retry_after).toBe(43_200);
    expect(res.headers.get("retry-after")).toBe("43200");
    expect(JSON.parse(Buffer.from(res.headers.get("payment-required")!, "base64url").toString("utf8"))).toEqual(expect.objectContaining({ error: "cap_exceeded" }));
    expect(await rig.store.periodSpend(capSubjectKey(keyId), "2026-10-25")).toBe(1_000);
    // The next UTC day refills the cap.
    rig.setNow(new Date("2026-10-26T00:00:01Z"));
    expect((await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "2" })).status).toBe(200);
    expect(rig.logLines.filter((l) => l.includes("event=cap_exceeded"))).toHaveLength(1);
  });

  it("applies the principal's monthly cap across keys and lets a key lower its own cap", async () => {
    const rig = makeAgentRig({ caps: { key: { day: 10_000, month: 10_000 }, principal: { day: 10_000, month: 1_500 } } });
    const a = await registerAgent(rig, {}, "203.0.113.20");
    expect((await mcp(rig, a.key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "2" })).status).toBe(200);
    await authed(rig, a.key, `/v1/keys/${a.keyId}`, { method: "PATCH", body: { day_micro_usd: 1_200 } });
    const capped = (await (await mcp(rig, a.key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "1" })).json()) as Err;
    expect(capped.payment!.cap).toEqual({ scope: "key", period: "day", limit_micro_usd: 1_200, spent_micro_usd: 1_000 });
    expect(await rig.store.periodSpend(capSubjectPrincipal(a.principalId), "2026-10")).toBe(1_000);
  });

  it("a dry run returns the exact quote and reserves nothing", async () => {
    const rig = makeAgentRig();
    const { key } = await registerAgent(rig);
    const res = await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "40", "mcp-dry-run": "true" });
    expect(res.status).toBe(200);
    const body = (await res.json()) as { dry_run: boolean; quote: { unit: string; quantity: number; cost_micro_usd: number }; sufficient: boolean; balance_micro_usd: number; usage: unknown };
    expect(body.dry_run).toBe(true);
    expect(body.quote).toEqual({ unit: "ingest", quantity: 40, cost_micro_usd: 40 * RATE_MICROS.ingest });
    expect(body.sufficient).toBe(true);
    expect(body.usage).toEqual({ unit: "ingest", quantity: 0, cost_micro_usd: 0 });
    expect(rig.upstreamCalls).toHaveLength(0);
    expect(await wallet(rig, key)).toEqual({ balance: FREE_ALLOWANCE_MICROS, reserved: 0, available: FREE_ALLOWANCE_MICROS });
    // The real call settles at exactly the dry run's figure (E9-T6: difference 0).
    const real = await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "40" });
    expect(Number(real.headers.get("x-usage-cost-micro-usd"))).toBe(body.quote.cost_micro_usd);
    expect((await wallet(rig, key)).balance).toBe(FREE_ALLOWANCE_MICROS - body.quote.cost_micro_usd);
  });

  it("refunds the reservation when the core fails or answers an error, and holds nothing twice", async () => {
    const rig = makeAgentRig();
    const { key } = await registerAgent(rig);
    rig.upstream.status = 500;
    const failed = await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "5" });
    expect(failed.status).toBe(500);
    expect(failed.headers.get("x-usage-cost-micro-usd")).toBe("0");
    expect(await wallet(rig, key)).toEqual({ balance: FREE_ALLOWANCE_MICROS, reserved: 0, available: FREE_ALLOWANCE_MICROS });
    const thrower = makeAgentRig({ mcpUpstream: () => Promise.reject(new Error("down")) });
    const t = await registerAgent(thrower);
    const down = await mcp(thrower, t.key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest" });
    expect(down.status).toBe(503);
    expect(((await down.json()) as Err).error.code).toBe("upstream_unavailable");
    expect(await wallet(thrower, t.key)).toEqual({ balance: FREE_ALLOWANCE_MICROS, reserved: 0, available: FREE_ALLOWANCE_MICROS });
    const receipts = (await (await authed(thrower, t.key, "/v1/receipts")).json()) as { receipts: Array<{ cost_micro_usd: number; settled: boolean }> };
    expect(receipts.receipts).toEqual([expect.objectContaining({ cost_micro_usd: 0, settled: true })]);
  });

  it("answers 503 before reserving when no core is wired, and refuses the scope a unit needs", async () => {
    const rig = makeAgentRig({ mcpUpstream: undefined });
    const { key } = await registerAgent(rig);
    const res = await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest" });
    expect(res.status).toBe(503);
    expect(await wallet(rig, key)).toEqual({ balance: FREE_ALLOWANCE_MICROS, reserved: 0, available: FREE_ALLOWANCE_MICROS });
    expect((await mcp(rig, "sa_live_nope", { "mcp-method": "tools/call", "mcp-name": "memory.search" })).status).toBe(401);
  });

  it("prices a run at the router's provider price plus 5%, and refuses a run it cannot price", async () => {
    const unpriced = makeAgentRig();
    const u = await registerAgent(unpriced);
    const refused = await mcp(unpriced, u.key, { "mcp-method": "tools/call", "mcp-name": "router.run", "mcp-tool-id": "acme.search" });
    expect(refused.status).toBe(400);
    expect(((await refused.json()) as Err).error.code).toBe("run_price_unavailable");
    const priced = makeAgentRig({ runPricer: (toolId) => Promise.resolve(toolId === "acme.search" ? 2_000 : undefined) });
    const p = await registerAgent(priced);
    const res = await mcp(priced, p.key, { "mcp-method": "tools/call", "mcp-name": "router.run", "mcp-tool-id": "acme.search" });
    expect(res.status).toBe(200);
    expect(res.headers.get("x-usage-unit")).toBe("run");
    expect(res.headers.get("x-usage-cost-micro-usd")).toBe("2100");
  });

  it("relays actions.execute free: queueing an L3 action is never billed, and needs no router price (E34)", async () => {
    const rig = makeAgentRig();
    const { key } = await registerAgent(rig);
    const res = await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "actions.execute" });
    expect(res.status).toBe(200);
    expect(res.headers.get("x-usage-unit")).toBe("run");
    expect(res.headers.get("x-usage-cost-micro-usd")).toBe("0");
    expect(await wallet(rig, key)).toEqual({ balance: FREE_ALLOWANCE_MICROS, reserved: 0, available: FREE_ALLOWANCE_MICROS });
    // A router run is still priced by the router, and refused without a price.
    expect((await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "router.run", "mcp-tool-id": "acme.search" })).status).toBe(400);
  });

  it("books cost-bearing calls against the licence ledger's global breaker too (E3-T4)", async () => {
    const usage = new InMemoryUsageStore();
    const rig = makeAgentRig({ usage, spendBreaker: { capMicros: 1_200, prices: DEFAULT_BATCH_PRICES } });
    const { key, principalId } = await registerAgent(rig);
    expect((await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "2" })).status).toBe(200);
    expect(await usage.spentOn("2026-10-25")).toBe(1_000);
    expect(await usage.licenseSpentOn(`principal:${principalId}`, "2026-10-25")).toBe(1_000);
    // The breaker refuses the next call (company capacity), the wallet hold is released.
    const res = await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "1" });
    expect(res.status).toBe(503);
    expect(((await res.json()) as Err).error.code).toBe("capacity_exhausted");
    expect(res.headers.get("retry-after")).toBe("43200");
    expect(await wallet(rig, key)).toEqual({ balance: FREE_ALLOWANCE_MICROS - 1_000, reserved: 0, available: FREE_ALLOWANCE_MICROS - 1_000 });
    // Reads never touch the breaker.
    expect((await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.search" })).status).toBe(200);
    expect(await usage.spentOn("2026-10-25")).toBe(1_000);
  });

  it("logs the route template, the status, the event and a hashed key id — never the key, a name or a body", async () => {
    const rig = makeAgentRig();
    const { key, keyId } = await registerAgent(rig);
    await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "3" });
    const line = rig.logLines.find((l) => l.startsWith("POST /mcp 200"))!;
    expect(line).toContain(`key=${logId(keyId)}`);
    expect(line).not.toContain(key.slice(8, 20));
    expect(line).not.toContain("memory.ingest");
    expect(line).not.toContain("jsonrpc");
  });
});
