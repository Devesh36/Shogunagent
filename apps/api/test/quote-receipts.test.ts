/** E9-T6 / E9-T7: `POST /v1/quote` (the exact cost, nothing reserved; zero difference from the
 * real settlement) and the receipts / dispute routes. */
import { describe, expect, it } from "vitest";

import { FREE_ALLOWANCE_MICROS, RATE_MICROS } from "../src/pricing.js";
import { authed, makeAgentRig, mcp, registerAgent } from "./agent-helpers.js";

describe("POST /v1/quote", () => {
  it("quotes every unit without reserving, and the real call settles at the same figure", async () => {
    const rig = makeAgentRig();
    const { key } = await registerAgent(rig);
    const cases: Array<[Record<string, unknown>, { unit: string; quantity: number; cost_micro_usd: number }]> = [
      [{ method: "tools/call", name: "memory.ingest", quantity: 250 }, { unit: "ingest", quantity: 250, cost_micro_usd: 250 * RATE_MICROS.ingest }],
      [{ name: "memory.summarize" }, { unit: "model_op", quantity: 1, cost_micro_usd: RATE_MICROS.model_op }],
      [{ name: "memory.search" }, { unit: "read", quantity: 1, cost_micro_usd: 0 }],
      [{ name: "state.people.list", quantity: 7 }, { unit: "read", quantity: 7, cost_micro_usd: 0 }],
      [{ method: "tools/list" }, { unit: "protocol", quantity: 1, cost_micro_usd: 0 }],
      [{ name: "router.run", provider_price_micro_usd: 1_999 }, { unit: "run", quantity: 1, cost_micro_usd: 2_099 }],
    ];
    for (const [body, expected] of cases) {
      const res = await authed(rig, key, "/v1/quote", { method: "POST", body });
      expect(res.status).toBe(200);
      const json = (await res.json()) as { quote: unknown; sufficient: boolean; balance_micro_usd: number; usage: unknown; next_action: unknown };
      expect(json.quote).toEqual(expected);
      expect(json.sufficient).toBe(true);
      expect(json.balance_micro_usd).toBe(FREE_ALLOWANCE_MICROS);
      expect(json.usage).toEqual({ unit: expected.unit, quantity: 0, cost_micro_usd: 0 });
      expect(json.next_action).toBeNull();
    }
    // The metered call for the first case charges exactly the quote.
    const real = await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "250" });
    expect(Number(real.headers.get("x-usage-cost-micro-usd"))).toBe(250 * RATE_MICROS.ingest);
    const receipts = (await (await authed(rig, key, "/v1/receipts")).json()) as { receipts: Array<{ cost_micro_usd: number }> };
    expect(receipts.receipts.map((r) => r.cost_micro_usd)).toEqual([250 * RATE_MICROS.ingest]);
  });

  it("says when the balance is insufficient and validates its input", async () => {
    const rig = makeAgentRig();
    const { key } = await registerAgent(rig);
    const big = (await (await authed(rig, key, "/v1/quote", { method: "POST", body: { name: "memory.ingest", quantity: 1_000_000 } })).json()) as { sufficient: boolean; next_action: string };
    expect(big.sufficient).toBe(false);
    expect(big.next_action).toBe("top_up");
    expect((await authed(rig, key, "/v1/quote", { method: "POST", body: { name: "memory.ingest", quantity: 0 } })).status).toBe(400);
    expect((await authed(rig, key, "/v1/quote", { method: "POST", body: { name: "router.run" } })).status).toBe(400);
    expect((await authed(rig, key, "/v1/quote", { method: "POST", body: { name: "router.run", provider_price_micro_usd: -1 } })).status).toBe(400);
    expect((await authed(rig, "sa_live_x", "/v1/quote", { method: "POST", body: {} })).status).toBe(401);
  });
});

describe("GET /v1/receipts and disputes", () => {
  it("lists machine-readable receipts since an instant, oldest first, with a cursor", async () => {
    const rig = makeAgentRig();
    const { key, keyId } = await registerAgent(rig);
    await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.search" });
    rig.setNow(new Date("2026-10-25T12:00:05Z"));
    await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "3" });
    rig.setNow(new Date("2026-10-25T12:00:10Z"));
    await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.summarize" });
    const all = (await (await authed(rig, key, "/v1/receipts")).json()) as { receipts: Array<Record<string, unknown>>; next_since: string };
    expect(all.receipts).toHaveLength(3);
    expect(all.receipts[1]).toEqual({
      id: expect.stringMatching(/^rcpt_/), key_id: keyId, method: "tools/call", name: "memory.ingest", unit: "ingest", quantity: 3,
      reserved_quantity: 3, settled_quantity: 3, cost_micro_usd: 3 * RATE_MICROS.ingest, reserved_micro_usd: null, settled: true, funding: "wallet", ts: "2026-10-25T12:00:05.000Z",
    });
    expect(all.next_since).toBe("2026-10-25T12:00:10.001Z");
    const since = (await (await authed(rig, key, "/v1/receipts?since=2026-10-25T12:00:05Z")).json()) as { receipts: Array<{ name: string }> };
    expect(since.receipts.map((r) => r.name)).toEqual(["memory.ingest", "memory.summarize"]);
    const paged = (await (await authed(rig, key, `/v1/receipts?since=${Date.parse("2026-10-25T12:00:00Z")}&limit=1`)).json()) as { receipts: Array<{ name: string }>; next_since: string };
    expect(paged.receipts.map((r) => r.name)).toEqual(["memory.search"]);
    const next = (await (await authed(rig, key, `/v1/receipts?since=${paged.next_since}`)).json()) as { receipts: Array<{ name: string }> };
    expect(next.receipts.map((r) => r.name)).toEqual(["memory.ingest", "memory.summarize"]);
    expect((await authed(rig, key, "/v1/receipts?since=yesterday")).status).toBe(400);
    expect((await authed(rig, key, "/v1/receipts?limit=0")).status).toBe(400);
    // Another principal sees none of them.
    const other = await registerAgent(rig, {}, "203.0.113.50");
    expect(((await (await authed(rig, other.key, "/v1/receipts")).json()) as { receipts: unknown[] }).receipts).toEqual([]);
  });

  it("shows the reserved and the settled quantity (E48): what the core reported, 0 on a failure, all of a free call", async () => {
    const rig = makeAgentRig();
    const { key } = await registerAgent(rig);
    // The core took 2 of the 5 reserved events.
    rig.upstream.quantity = "2";
    await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "5" });
    // A core that claims more than was reserved is capped at the reservation.
    rig.setNow(new Date("2026-10-25T12:00:01Z"));
    rig.upstream.quantity = "9";
    await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "4" });
    // A non-2xx settles nothing.
    rig.setNow(new Date("2026-10-25T12:00:02Z"));
    rig.upstream.status = 500;
    rig.upstream.quantity = "echo";
    await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "3" });
    // A free read is recorded at its reserved quantity.
    rig.setNow(new Date("2026-10-25T12:00:03Z"));
    rig.upstream.status = 200;
    await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.search" });
    const { receipts } = (await (await authed(rig, key, "/v1/receipts")).json()) as { receipts: Array<Record<string, unknown>> };
    expect(receipts.map((r) => [r.name, r.reserved_quantity, r.settled_quantity, r.cost_micro_usd])).toEqual([
      ["memory.ingest", 5, 2, 2 * RATE_MICROS.ingest],
      ["memory.ingest", 4, 4, 4 * RATE_MICROS.ingest],
      ["memory.ingest", 3, 0, 0],
      ["memory.search", 1, 1, 0],
    ]);
  });

  it("opens one dispute per receipt, on the principal's own receipts only", async () => {
    const rig = makeAgentRig();
    const { key } = await registerAgent(rig);
    const call = await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "2" });
    const receiptId = call.headers.get("x-receipt-id")!;
    const opened = await authed(rig, key, `/v1/receipts/${receiptId}/dispute`, { method: "POST", body: { reason: "ingest counted 2 events, I sent 1" } });
    expect(opened.status).toBe(201);
    const first = (await opened.json()) as { dispute_id: string; receipt_id: string; status: string; ts: string };
    expect(first).toEqual({ dispute_id: expect.stringMatching(/^dsp_/), receipt_id: receiptId, status: "open", ts: "2026-10-25T12:00:00.000Z" });
    const again = await authed(rig, key, `/v1/receipts/${receiptId}/dispute`, { method: "POST", body: { reason: "again" } });
    expect(again.status).toBe(200);
    expect(((await again.json()) as { dispute_id: string }).dispute_id).toBe(first.dispute_id);
    expect((await authed(rig, key, `/v1/receipts/${receiptId}/dispute`, { method: "POST", body: { reason: "" } })).status).toBe(400);
    expect((await authed(rig, key, "/v1/receipts/rcpt_nope/dispute", { method: "POST", body: { reason: "x" } })).status).toBe(404);
    const other = await registerAgent(rig, {}, "203.0.113.51");
    expect((await authed(rig, other.key, `/v1/receipts/${receiptId}/dispute`, { method: "POST", body: { reason: "not mine" } })).status).toBe(404);
    expect(rig.logLines.some((l) => l.startsWith("POST /v1/receipts/:id/dispute 201"))).toBe(true);
    expect(rig.logLines.join("\n")).not.toContain(receiptId);
  });
});
