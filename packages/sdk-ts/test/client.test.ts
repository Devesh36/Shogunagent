import { describe, expect, it } from "vitest";

import {
  AuthenticationError, HOSTED_BASE_URL, LOCAL_BASE_URL, PaymentRequiredError, PermissionDeniedError, RateLimitError,
  ServiceUnavailableError, ShogunAgent, ToolError, TransportError,
} from "../src/index.js";
import { FakeFetch, PAYMENT_402, ingestPayload, recallPayload, rpcResult, toolText } from "./fake-fetch.js";

function client(fake: FakeFetch, extra: Record<string, unknown> = {}) {
  return new ShogunAgent({ apiKey: "sa_live_test", fetch: fake.fetch, actor: "agent:test", ...extra });
}

describe("transport", () => {
  it("defaults to the local sidecar and sends MCP Streamable HTTP headers", async () => {
    const fake = new FakeFetch().tool("memory.recall", toolText(recallPayload()));
    const c = new ShogunAgent({ fetch: fake.fetch });
    expect(c.baseUrl).toBe(LOCAL_BASE_URL);
    await c.recall("rollout");
    const call = fake.calls[0]!;
    expect(call.url).toBe("http://127.0.0.1:7464/mcp");
    expect(call.method).toBe("POST");
    expect(call.headers["mcp-method"]).toBe("tools/call");
    expect(call.headers["mcp-name"]).toBe("memory.recall");
    expect(call.headers["mcp-protocol-version"]).toBe("2026-07-28");
    expect(call.headers.authorization).toBeUndefined();
    expect(call.body).toMatchObject({ jsonrpc: "2.0", method: "tools/call", params: { name: "memory.recall", arguments: { query: "rollout" } } });
  });

  it("targets the hosted gateway with the bearer key", async () => {
    const fake = new FakeFetch().tool("memory.recall", toolText(recallPayload()));
    await client(fake, { baseUrl: `${HOSTED_BASE_URL}/` }).recall("x");
    expect(fake.calls[0]!.url).toBe("https://api.shogunagent.dev/mcp");
    expect(fake.calls[0]!.headers.authorization).toBe("Bearer sa_live_test");
  });

  it("reads a single SSE event", async () => {
    const fake = new FakeFetch().tool("memory.recall", {
      headers: { "content-type": "text/event-stream" },
      text: `event: message\ndata: ${JSON.stringify({ jsonrpc: "2.0", id: 1, result: { content: [{ type: "text", text: JSON.stringify(recallPayload()) }] } })}\n\n`,
    });
    const r = await client(fake).recall("x");
    expect(r.items).toHaveLength(3);
  });

  it("maps a fetch failure to TransportError", async () => {
    const c = new ShogunAgent({ fetch: async () => { throw new Error("ECONNREFUSED"); } });
    await expect(c.recall("x")).rejects.toBeInstanceOf(TransportError);
  });
});

describe("memory", () => {
  it("recall passes budget and scratch and returns the typed result with the envelope", async () => {
    const fake = new FakeFetch().tool("memory.recall", toolText(recallPayload()));
    const r = await client(fake).recall({ query: "rollout", budgetTokens: 500, includeScratch: true });
    expect(fake.calls[0]!.body.params.arguments).toEqual({ query: "rollout", budget_tokens: 500, include_scratch: true });
    expect(r.items[0]!.band).toBe("high");
    expect(r.possibly[0]).toMatch(/^possibly:/);
    expect(r.envelope.usage).toEqual({ unit: "read", quantity: 1, cost_micro_usd: 0 });
    expect(r.envelope.latency_ms).toBe(4);
  });

  it("ingest generates an idempotency key, sends actor/source/namespace and Mcp-Quantity", async () => {
    const fake = new FakeFetch().tool("memory.ingest", toolText(ingestPayload()));
    const r = await client(fake).ingest({ turns: [{ role: "user", content: "hi" }, { role: "assistant", content: "hello" }], threadKey: "t1" });
    const args = fake.calls[0]!.body.params.arguments;
    expect(args.idempotency_key).toMatch(/^sdk_/);
    expect(r.idempotency_key).toBe(args.idempotency_key);
    expect(args).toMatchObject({ actor: "agent:test", source: "shogunagent-sdk", namespace: "world", thread_key: "t1" });
    expect(fake.calls[0]!.headers["mcp-quantity"]).toBe("2");
    expect(r.inserted).toBe(2);
  });

  it("ingest keeps a caller key and refuses empty or oversized batches before the network", async () => {
    const fake = new FakeFetch().tool("memory.ingest", toolText(ingestPayload()));
    const c = client(fake);
    await c.ingest({ events: [{ content: "x" }], idempotencyKey: "mine", namespace: "agent_scratch" });
    expect(fake.calls[0]!.body.params.arguments).toMatchObject({ idempotency_key: "mine", namespace: "agent_scratch", events: [{ content: "x" }] });
    await expect(c.ingest({ events: [] })).rejects.toMatchObject({ code: "invalid_events" });
    await expect(c.ingest({ events: Array.from({ length: 201 }, () => ({ content: "x" })) })).rejects.toMatchObject({ code: "invalid_events" });
    expect(fake.calls).toHaveLength(1);
  });

  it("contextPack calls memory.get_context_pack", async () => {
    const fake = new FakeFetch().tool("memory.get_context_pack", toolText({ result: { pack: "…" }, usage: { unit: "read", quantity: 1, cost_micro_usd: 0 }, latency_ms: 2 }));
    const r = await client(fake).contextPack("Heron");
    expect(fake.calls[0]!.body.params.arguments).toEqual({ query: "Heron" });
    expect(r.result).toEqual({ pack: "…" });
  });
});

describe("state reads", () => {
  const rows = { tool: "x", results: [{ text: "send plan", confidence: 0.9, possibly: false, table: "commitments", id: 7 }, { text: "maybe", confidence: 0.6, possibly: true }] };
  it.each([
    ["commitments", "state.commitments.list"],
    ["openLoops", "state.open_loops.list"],
    ["people", "state.people.list"],
    ["projects", "state.projects.list"],
  ] as const)("state.%s.list → %s", async (noun, tool) => {
    const fake = new FakeFetch().tool(tool, toolText(rows));
    const r = await client(fake).state[noun].list({ includeLow: false, limit: 5, offset: 10 });
    expect(fake.calls[0]!.headers["mcp-name"]).toBe(tool);
    expect(fake.calls[0]!.body.params.arguments).toEqual({ include_low: false, limit: 5, offset: 10 });
    expect(r.results[1]!.possibly).toBe(true);
  });

  it("people.get / projects.get pass the id", async () => {
    const fake = new FakeFetch().tool("state.people.get", toolText(rows)).tool("state.projects.get", toolText(rows));
    const c = client(fake);
    await c.state.people.get(12);
    await c.state.projects.get(3);
    expect(fake.calls.map((x) => [x.headers["mcp-name"], x.body.params.arguments])).toEqual([["state.people.get", { id: 12 }], ["state.projects.get", { id: 3 }]]);
  });

  it("a tool-level refusal is a ToolError with the code", async () => {
    const fake = new FakeFetch().tool("state.people.get", toolText({ error: "not_found" }, true));
    await expect(client(fake).state.people.get(99)).rejects.toMatchObject({ name: "ToolError", code: "not_found" });
  });

  it("a JSON-RPC error is a ToolError with the code from the message", async () => {
    const fake = new FakeFetch().tool("memory.recall", { body: { jsonrpc: "2.0", id: 1, error: { code: -32602, message: "missing_query: pass query" } } });
    await expect(client(fake).recall("")).rejects.toMatchObject({ code: "missing_query" });
  });
});

describe("actions", () => {
  it("L1 returns the result", async () => {
    const fake = new FakeFetch().tool("actions.execute", toolText({ result: { hits: [1] }, usage: { unit: "run", quantity: 1, cost_micro_usd: 0 }, latency_ms: 1 }));
    const out = await client(fake).actions.execute({ kind: "local_search", query: "plan" });
    expect(out.type).toBe("result");
    if (out.type === "result") expect(out.result).toEqual({ hits: [1] });
  });

  it("L2 input_required → respond(true) resends with inputResponses", async () => {
    const fake = new FakeFetch().tool("actions.execute", (c) => c.body.params.inputResponses
      ? toolText({ result: { ok: true } })
      : rpcResult({ resultType: "input_required", inputRequests: [{ id: "l2_confirm", mode: "form", message: "Confirm", requestedSchema: {} }], content: [{ type: "text", text: "{}" }] }));
    const out = await client(fake).actions.execute({ kind: "edit_note", text: "x" });
    expect(out.type).toBe("input_required");
    if (out.type !== "input_required") return;
    expect(out.inputRequests[0]!.id).toBe("l2_confirm");
    const second = await out.respond(true);
    expect(second.type).toBe("result");
    expect(fake.calls[1]!.body.params.inputResponses).toEqual([{ id: "l2_confirm", action: "accept", content: { confirm: true } }]);
    expect(fake.calls[1]!.body.params.arguments).toEqual({ kind: "edit_note", text: "x" });
  });

  it("L2 declined answers type declined", async () => {
    const fake = new FakeFetch().tool("actions.execute", (c) => c.body.params.inputResponses
      ? toolText({ ran: false, declined: true, tool: "x", level: "L2" })
      : rpcResult({ resultType: "input_required", inputRequests: [{ id: "l2_confirm", mode: "form", message: "Confirm" }] }));
    const out = await client(fake).actions.execute({ kind: "edit_note" });
    if (out.type !== "input_required") throw new Error("expected input_required");
    expect((await out.respond(false)).type).toBe("declined");
  });

  it("L3 approval_required exposes the approval and polls actions.status until it leaves pending", async () => {
    let polls = 0;
    const fake = new FakeFetch()
      .tool("actions.execute", rpcResult({
        resultType: "approval_required",
        approval: { approval_id: 42, level: "L3", url: "https://approve.test/approvals/42", poll: { tool: "actions.status", arguments: { approval_id: 42 } }, message: "Hand the URL to the human" },
        content: [{ type: "text", text: "{\"pending\":true,\"approval_id\":42}" }],
      }))
      .tool("actions.status", () => toolText({ approval_id: 42, status: ++polls < 3 ? "pending" : "sent" }));
    const out = await client(fake).actions.execute({ kind: "send_email", to: "a@b.c", subject: "s", body: "b" });
    expect(out.type).toBe("approval_required");
    if (out.type !== "approval_required") return;
    expect(out.approval.url).toBe("https://approve.test/approvals/42");
    expect((await out.status()).status).toBe("pending");
    const final = await out.wait({ intervalMs: 1 });
    expect(final.status).toBe("sent");
    expect(fake.toolCalls("actions.status").every((c) => c.body.params.arguments.approval_id === 42)).toBe(true);
  });

  it("an un-reshaped pending envelope is still an approval", async () => {
    const fake = new FakeFetch().tool("actions.execute", toolText({ pending: true, approval_id: 9, level: "L3" })).tool("actions.status", toolText({ approval_id: 9, status: "rejected" }));
    const out = await client(fake).actions.execute({ kind: "post_message" });
    if (out.type !== "approval_required") throw new Error("expected approval_required");
    expect(out.approval.approval_id).toBe(9);
    expect((await client(fake).actions.waitForApproval(9, { intervalMs: 1 })).status).toBe("rejected");
  });
});

describe("router, quote", () => {
  it("discover / inspect / run go to the router base over REST; run gets an idempotency key", async () => {
    const fake = new FakeFetch()
      .route("/v1/router/discover", { body: { result: { candidates: [{ tool_id: "a" }] }, usage: { unit: "call", quantity: 1, cost_micro_usd: 0 }, latency_ms: 1, hints: [], next_action: null } })
      .route("/v1/router/inspect", { body: { result: { tool_id: "a" }, usage: null, latency_ms: 1, hints: [{ tool_id: "b", reason: "cheaper" }] } })
      .route("/v1/router/run", (c) => ({ body: { resultType: "approval_required", approval: { url: "https://r.test/a" }, next_action: "approve_at_url", echo: c.body.idempotency_key } }));
    const c = client(fake, { routerBaseUrl: "https://router.test" });
    const d = await c.router.discover({ task: "read a page", limit: 3 });
    expect(d.result.candidates[0]!.tool_id).toBe("a");
    const i = await c.router.inspect("a");
    expect(i.hints[0]!.tool_id).toBe("b");
    const r = await c.router.run({ tool_id: "a", input: { q: 1 } });
    expect(r.resultType).toBe("approval_required");
    expect(r.idempotency_key).toMatch(/^run_/);
    expect(fake.calls.map((x) => x.url)).toEqual(["https://router.test/v1/router/discover", "https://router.test/v1/router/inspect", "https://router.test/v1/router/run"]);
    expect(fake.calls[1]!.body).toEqual({ tool_id: "a" });
  });

  it("quote posts to /v1/quote", async () => {
    const fake = new FakeFetch().route("/v1/quote", (c) => ({ body: { quote: { unit: "ingest", quantity: c.body.quantity, cost_micro_usd: 125000 }, method: c.body.method, name: c.body.name, balance_micro_usd: 1, sufficient: false, usage: { unit: "ingest", quantity: 0, cost_micro_usd: 0 }, next_action: "top_up" } }));
    const q = await client(fake).quote({ name: "memory.ingest", quantity: 250 });
    expect(fake.calls[0]!.body).toEqual({ method: "tools/call", name: "memory.ingest", quantity: 250 });
    expect(q.quote.cost_micro_usd).toBe(125000);
    expect(q.next_action).toBe("top_up");
  });
});

describe("typed HTTP errors", () => {
  it("402 exposes the payment payload, the decoded PAYMENT-REQUIRED header and the top-up URL", async () => {
    const fake = new FakeFetch().tool("memory.ingest", PAYMENT_402);
    const err = await client(fake).ingest({ events: [{ content: "x" }] }).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(PaymentRequiredError);
    const e = err as PaymentRequiredError;
    expect(e.status).toBe(402);
    expect(e.code).toBe("payment_required");
    expect(e.nextAction).toBe("top_up");
    expect(e.payment?.amount_micro_usd).toBe(6000);
    expect(e.shortfallMicroUsd).toBe(5000);
    expect(e.topUpUrl).toBe("https://api.test/v1/wallet/topup");
    expect(e.paymentRequiredHeader).toMatchObject({ x402Version: 2, error: "payment_required" });
  });

  it("402 cap_exceeded carries retry_after", async () => {
    const fake = new FakeFetch().tool("memory.ingest", {
      status: 402, headers: { "retry-after": "43200" },
      body: { error: { code: "cap_exceeded", message: "cap", next_action: "retry_after" }, payment: { amount_micro_usd: 500, unit: "ingest", accepts: [], retry_after: 43200, cap: { scope: "key", period: "day", limit_micro_usd: 1, spent_micro_usd: 1 } } },
    });
    const e = (await client(fake).ingest({ events: [{ content: "x" }] }).catch((x: unknown) => x)) as PaymentRequiredError;
    expect(e.code).toBe("cap_exceeded");
    expect(e.retryAfter).toBe(43200);
    expect(e.topUpUrl).toBeNull();
    expect(e.payment?.cap?.scope).toBe("key");
  });

  it.each([
    [401, AuthenticationError, "invalid_key", "authenticate"],
    [403, PermissionDeniedError, "insufficient_scope", "not_allowed"],
    [429, RateLimitError, "rate_limited", "retry_after"],
    [503, ServiceUnavailableError, "upstream_unavailable", "retry_after"],
  ] as const)("%i → %s with next_action", async (status, cls, code, next) => {
    const fake = new FakeFetch().tool("memory.recall", { status, headers: { "retry-after": "7" }, body: { error: { code, message: "m", next_action: next } } });
    const e = (await client(fake).recall("x").catch((x: unknown) => x)) as InstanceType<typeof cls>;
    expect(e).toBeInstanceOf(cls);
    expect(e.code).toBe(code);
    expect(e.nextAction).toBe(next);
    expect(e.retryAfter).toBe(7);
  });

  it("understands the local sidecar's flat error body", async () => {
    const fake = new FakeFetch().tool("memory.recall", { status: 401, body: { error: "unauthorized" } });
    const e = (await client(fake).recall("x").catch((x: unknown) => x)) as AuthenticationError;
    expect(e).toBeInstanceOf(AuthenticationError);
    expect(e.code).toBe("unauthorized");
    expect(e.nextAction).toBe("authenticate");
  });

  it("callTool refuses an MRTR branch for a plain read", async () => {
    const fake = new FakeFetch().tool("memory.recall", rpcResult({ resultType: "input_required", inputRequests: [] }));
    await expect(client(fake).recall("x")).rejects.toBeInstanceOf(ToolError);
  });
});
