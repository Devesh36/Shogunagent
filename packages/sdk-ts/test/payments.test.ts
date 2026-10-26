import { describe, expect, it } from "vitest";

import { ShogunAgent, ShogunAgentError, ToolError } from "../src/index.js";
import { FakeFetch, rpcResult, toolText } from "./fake-fetch.js";

function client(fake: FakeFetch) {
  return new ShogunAgent({ apiKey: "sa_live_test", fetch: fake.fetch, actor: "agent:test" });
}

const URL_ = "https://api.shogunagent.dev/approve/tok";

/** The MCP result the core sends for a queued L3 (MRTR `approval_required`, text keeps the payload). */
function approvalRequired(payload: Record<string, unknown>, approvalId = 7) {
  return rpcResult({
    resultType: "approval_required",
    approval: { approval_id: approvalId, level: "L3", url: URL_, poll: { tool: "actions.status", arguments: { approval_id: approvalId } }, message: "Hand the URL to the human." },
    content: [{ type: "text", text: JSON.stringify({ pending: true, approval_id: approvalId, url: URL_, next_action: "approve_at_url", ...payload }) }],
    isError: false,
  });
}

const mandate = {
  mandate_id: "pmd_0123456789ab", agent: "agent:test", payee: { kind: "class", value: "cloud" }, currency: "USD",
  max_per_payment_minor: 5000, total_cap_minor: 20000, spent_minor: 0, remaining_minor: 20000, payments: 0, expires_at_ms: 2_000_000_000_000,
  purpose: "Build minutes", single_use: false, status: "active", revision: 1, granted: { approval_id: 7, by: "human:owner", at_ms: 1 }, revoked: null,
};

function spendRow(status: string, extra: Record<string, unknown> = {}) {
  return {
    spend_ref: "psp_0123456789ab", actor: "agent:test", idempotency_key: "inv-1", mandate_id: null, authorization: "approval", approval_id: 9,
    decided_by: "human:owner", payee: "cloud:hetzner", currency: "USD", amount_minor: 1250, status, reason_class: null, rail: "stub", rail_ref: null,
    created_at_ms: 1, settled_at_ms: null, ...extra,
  };
}

describe("payments.mandates", () => {
  it("request maps the terms, generates a key, and answers the approval URL without ever approving", async () => {
    const fake = new FakeFetch()
      .tool("payments.mandates.request", approvalRequired({ tool: "payments.mandates.request", mandate_id: "pmd_0123456789ab" }))
      .tool("actions.status", toolText({ approval_id: 7, status: "pending" }));
    const out = await client(fake).payments.mandates.request({
      payeeClass: "cloud", currency: "USD", maxPerPaymentMinor: 5000, totalCapMinor: 20000, expiresInDays: 30, purpose: "Build minutes",
    });
    expect(out.type).toBe("approval_required");
    if (out.type !== "approval_required") throw new Error("unreachable");
    expect(out.approval.url).toBe(URL_);
    expect(out.mandateId).toBe("pmd_0123456789ab");
    expect(out.idempotencyKey).toMatch(/^mandate_/);
    const args = fake.toolCalls("payments.mandates.request")[0]!.body.params.arguments;
    expect(args).toEqual({ payee_class: "cloud", currency: "USD", max_per_payment_minor: 5000, total_cap_minor: 20000, expires_in_days: 30, purpose: "Build minutes", idempotency_key: out.idempotencyKey });
    expect((await out.status()).status).toBe("pending");
    // Only the request and the status read: no decide, no approval route.
    expect(fake.calls.map((c) => c.headers["mcp-name"])).toEqual(["payments.mandates.request", "actions.status"]);
    expect(fake.calls.some((c) => c.url.includes("/approve") || c.url.includes("/decide"))).toBe(false);
  });

  it("request: narrowing applies at once, a granted retry replays, and the local plain pending envelope is an approval", async () => {
    const fake = new FakeFetch().tool("payments.mandates.request", (c) => {
      const a = c.body.params.arguments;
      if (a.mandate_id) return toolText({ tool: "payments.mandates.request", narrowed: true, mandate: { ...mandate, total_cap_minor: 6000 } });
      if (a.idempotency_key === "again") return toolText({ tool: "payments.mandates.request", replayed: true, mandate });
      return toolText({ pending: true, approval_id: 3, level: "L3", origin: "mcp", mandate_id: "pmd_x" });
    });
    const c = client(fake);
    const narrowed = await c.payments.mandates.request({ mandateId: mandate.mandate_id, payeeClass: "cloud", currency: "USD", maxPerPaymentMinor: 5000, totalCapMinor: 6000, purpose: "Build minutes", idempotencyKey: "n" });
    expect(narrowed).toMatchObject({ type: "narrowed", mandateId: mandate.mandate_id, idempotencyKey: "n" });
    const replayed = await c.payments.mandates.request({ payee: "cloud:hetzner", currency: "USD", maxPerPaymentMinor: 1, totalCapMinor: 1, purpose: "x", idempotencyKey: "again" });
    expect(replayed.type).toBe("replayed");
    const local = await c.payments.mandates.request({ payee: "cloud:hetzner", currency: "USD", maxPerPaymentMinor: 1, totalCapMinor: 1, purpose: "x", idempotencyKey: "k" });
    expect(local).toMatchObject({ type: "approval_required", mandateId: "pmd_x", approval: { approval_id: 3, url: null } });
  });

  it("request refuses an ambiguous payee before any call", async () => {
    const fake = new FakeFetch();
    const c = client(fake);
    await expect(c.payments.mandates.request({ payee: "a", payeeClass: "b", currency: "USD", maxPerPaymentMinor: 1, totalCapMinor: 1, purpose: "x" })).rejects.toBeInstanceOf(ShogunAgentError);
    await expect(c.payments.mandates.request({ currency: "USD", maxPerPaymentMinor: 1, totalCapMinor: 1, purpose: "x" })).rejects.toMatchObject({ code: "invalid_payee" });
    expect(fake.calls).toHaveLength(0);
  });

  it("list and revoke send the core's arguments", async () => {
    const fake = new FakeFetch()
      .tool("payments.mandates.list", (c) => toolText(c.body.params.arguments.mandate_id
        ? { tool: "payments.mandates.list", mandate, spends: [spendRow("settled")] }
        : { tool: "payments.mandates.list", mandates: [mandate] }))
      .tool("payments.mandates.revoke", toolText({ tool: "payments.mandates.revoke", revoked: true, already_revoked: false, mandate: { ...mandate, status: "revoked" } }));
    const c = client(fake);
    expect((await c.payments.mandates.list()).mandates[0]!.remaining_minor).toBe(20000);
    const one = await c.payments.mandates.list({ mandateId: mandate.mandate_id, limit: 5 });
    expect(one.mandate?.mandate_id).toBe(mandate.mandate_id);
    expect(one.spends?.[0]?.status).toBe("settled");
    expect(fake.toolCalls("payments.mandates.list")[1]!.body.params.arguments).toEqual({ mandate_id: mandate.mandate_id, limit: 5 });
    const revoked = await c.payments.mandates.revoke(mandate.mandate_id, { idempotencyKey: "r1" });
    expect(revoked.revoked).toBe(true);
    expect(fake.toolCalls("payments.mandates.revoke")[0]!.body.params.arguments).toEqual({ mandate_id: mandate.mandate_id, idempotency_key: "r1" });
  });
});

describe("payments.spend", () => {
  it("settles inside a mandate and replays with the same key", async () => {
    const fake = new FakeFetch().tool("payments.spend", (c) => toolText({
      tool: "payments.spend", status: "settled", authorization: "mandate", mandate_id: mandate.mandate_id,
      ...(fake.toolCalls("payments.spend").length > 1 ? { replayed: true } : {}),
      spend: spendRow("settled", { authorization: "mandate", mandate_id: mandate.mandate_id, idempotency_key: c.body.params.arguments.idempotency_key, rail_ref: "stub_psp_1" }),
    }));
    const c = client(fake);
    const paid = await c.payments.spend({ payee: "cloud:hetzner", currency: "USD", amountMinor: 1250, memo: "June", idempotencyKey: "inv-1" });
    expect(paid).toMatchObject({ type: "settled", replayed: false, idempotencyKey: "inv-1", spend: { rail_ref: "stub_psp_1" } });
    expect(fake.toolCalls("payments.spend")[0]!.body.params.arguments).toEqual({ payee: "cloud:hetzner", currency: "USD", amount_minor: 1250, memo: "June", idempotency_key: "inv-1" });
    expect(fake.toolCalls("payments.spend")[0]!.headers["mcp-name"]).toBe("payments.spend");
    const again = await c.payments.spend({ payee: "cloud:hetzner", currency: "USD", amountMinor: 1250, idempotencyKey: "inv-1" });
    expect(again).toMatchObject({ type: "settled", replayed: true });
  });

  it("outside every mandate answers the approval for a human, and a rejected one reads not_settled", async () => {
    let decided = false;
    const fake = new FakeFetch().tool("payments.spend", () => decided
      ? toolText({ tool: "payments.spend", replayed: true, status: "rejected", next_action: "ask_human", spend: spendRow("rejected", { reason_class: "rejected" }) })
      : approvalRequired({ tool: "payments.spend", spend_ref: "psp_0123456789ab", authorization: "approval", reason: "over_total_cap" }, 9));
    const c = client(fake);
    const first = await c.payments.spend({ payee: "cloud:hetzner", currency: "USD", amountMinor: 1250 });
    expect(first).toMatchObject({ type: "approval_required", spendRef: "psp_0123456789ab", reason: "over_total_cap", approval: { approval_id: 9, url: URL_ } });
    expect(first.idempotencyKey).toMatch(/^pay_/);
    decided = true;
    const after = await c.payments.spend({ payee: "cloud:hetzner", currency: "USD", amountMinor: 1250, idempotencyKey: first.idempotencyKey });
    expect(after).toMatchObject({ type: "not_settled", replayed: true, spend: { status: "rejected", decided_by: "human:owner" } });
  });

  it("a payment that failed at the rail is a ToolError carrying the spend", async () => {
    const fake = new FakeFetch().tool("payments.spend", toolText({ tool: "payments.spend", error: "payment_failed", message: "The payment did not settle.", next_action: "check_status", spend: spendRow("failed") }, true));
    await expect(client(fake).payments.spend({ payee: "x", currency: "USD", amountMinor: 1, idempotencyKey: "k" })).rejects.toMatchObject({ code: "payment_failed", nextAction: "check_status" });
    await expect(client(fake).payments.spend({ payee: "x", currency: "USD", amountMinor: 1, idempotencyKey: "k" })).rejects.toBeInstanceOf(ToolError);
  });
});

describe("connections", () => {
  it("status reads connections.list and disconnect sends the service", async () => {
    const fake = new FakeFetch()
      .tool("connections.list", toolText({ hosted: true, connections: [{ service: "gcal", state: "connected" }] }))
      .tool("connections.disconnect", toolText({ disconnected: true, service: "gcal" }));
    const c = client(fake);
    expect((await c.connections.status()).connections?.[0]).toMatchObject({ service: "gcal" });
    expect(await c.connections.disconnect("gcal")).toMatchObject({ disconnected: true });
    expect(fake.toolCalls("connections.disconnect")[0]!.body.params.arguments).toEqual({ service: "gcal" });
  });
});
