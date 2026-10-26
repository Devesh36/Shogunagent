/**
 * The hosted REST face of `payments.*` (E51, docs/api/payments.md): each route is one tool, relayed
 * to the principal's core at the same path through the `/mcp` relay. The gateway never reads,
 * logs or stores either body; the scope comes from the pinned tool (list: `read`; request, revoke
 * and spend: `run`), and every call is free at the call (reads by invariant 10, the run tools by
 * `FREE_RUN_TOOLS`), so nothing is charged from the wallet.
 */
import { describe, expect, it } from "vitest";

import { hashAgentKey, type AgentScope } from "../src/agent-keys.js";
import { PAYMENT_ROUTES } from "../src/agent-app.js";
import { SIDECAR_PATHS, createMcpRelay } from "../src/mcp-proxy.js";
import { routeTemplate } from "../src/logging.js";
import { BASE, TEST_KEY_ITERATIONS, authed, makeAgentRig, registerAgent, type AgentRig } from "./agent-helpers.js";

const SECRET = "SENTINEL-PAYEE-memo-91c2";

type Seen = { path: string; method: string; body: string; principalId: string; auth: string | null };

function rig(answer: { status?: number; body?: string; headers?: Record<string, string> } = {}) {
  const seen: Seen[] = [];
  const r = makeAgentRig({
    mcpUpstream: async (req, principal) => {
      seen.push({ path: new URL(req.url).pathname, method: req.method, body: req.method === "GET" ? "" : await req.text(), principalId: principal.principalId, auth: req.headers.get("authorization") });
      return new Response(answer.body ?? JSON.stringify({ tool: "payments.mandates.list", mandates: [] }), { status: answer.status ?? 200, headers: { "content-type": "application/json", ...(answer.headers ?? {}) } });
    },
  });
  return { r, seen };
}

/** A key of `principalId` with exactly `scopes`, inserted the way the store keeps every key (hashed). */
async function keyWith(r: AgentRig, principalId: string, scopes: AgentScope[]): Promise<string> {
  const random = "Q".repeat(10) + Math.random().toString(36).slice(2).padEnd(33, "x").slice(0, 33);
  const key = `sa_live_${random}`;
  await r.store.insertKey({ keyId: `ak_${scopes.join("_")}_${random.slice(10, 16)}`, prefix: random.slice(0, 10), hash: await hashAgentKey(key, TEST_KEY_ITERATIONS), mode: "live", principalId, tenantId: principalId, scopes, createdAt: 0 });
  return key;
}

describe("payments.* over hosted REST", () => {
  it("relays each route to the same path on the principal's core, body untouched and never logged", async () => {
    const { r, seen } = rig();
    const agent = await registerAgent(r);
    const spend = { payee: "cloud:hetzner", currency: "USD", amount_minor: 1250, memo: `invoice ${SECRET}`, idempotency_key: "inv-1" };
    for (const route of PAYMENT_ROUTES) {
      const res = await authed(r, agent.key, route.path, route.method === "get" ? {} : { method: "POST", body: spend });
      expect(res.status, route.path).toBe(200);
    }
    expect(seen.map((s) => [s.method, s.path])).toEqual(PAYMENT_ROUTES.map((p) => [p.method.toUpperCase(), p.path]));
    expect(seen.every((s) => s.principalId === agent.principalId)).toBe(true);
    expect(JSON.parse(seen.find((s) => s.path === "/v1/payments/spend")!.body)).toEqual(spend);
    const log = r.logLines.join("\n");
    expect(log).not.toContain(SECRET);
    expect(log).not.toContain("hetzner");
    expect(r.logLines.some((l) => l.includes("/v1/payments/spend"))).toBe(true);
  });

  it("meters by the pinned tool: list is a free read, request / revoke / spend are free runs", async () => {
    const { r } = rig();
    const agent = await registerAgent(r);
    const before = await r.store.wallet(agent.principalId);
    const list = await authed(r, agent.key, "/v1/payments/mandates");
    expect(list.headers.get("x-usage-unit")).toBe("read");
    expect(list.headers.get("x-usage-cost-micro-usd")).toBe("0");
    for (const path of ["/v1/payments/mandates/request", "/v1/payments/mandates/revoke", "/v1/payments/spend"]) {
      const res = await authed(r, agent.key, path, { method: "POST", body: { idempotency_key: "k" } });
      expect(res.headers.get("x-usage-unit"), path).toBe("run");
      expect(res.headers.get("x-usage-cost-micro-usd"), path).toBe("0");
    }
    // A spoofed Mcp-Name cannot reclassify a REST route: the path pins the tool.
    const spoofed = await authed(r, agent.key, "/v1/payments/spend", { method: "POST", body: {}, headers: { "mcp-method": "tools/call", "mcp-name": "payments.mandates.list" } });
    expect(spoofed.headers.get("x-usage-unit")).toBe("run");
    expect((await r.store.wallet(agent.principalId))?.balanceMicros).toBe(before?.balanceMicros);
  });

  it("needs read to list and run to request, revoke or spend", async () => {
    const { r, seen } = rig();
    const agent = await registerAgent(r);
    const readOnly = await keyWith(r, agent.principalId, ["read"]);
    expect((await authed(r, readOnly, "/v1/payments/mandates")).status).toBe(200);
    for (const path of ["/v1/payments/mandates/request", "/v1/payments/mandates/revoke", "/v1/payments/spend"]) {
      const res = await authed(r, readOnly, path, { method: "POST", body: {} });
      expect(res.status, path).toBe(403);
      expect(((await res.json()) as { error: { code: string } }).error.code).toBe("insufficient_scope");
    }
    const runOnly = await keyWith(r, agent.principalId, ["run"]);
    expect((await authed(r, runOnly, "/v1/payments/mandates")).status).toBe(403);
    expect((await authed(r, runOnly, "/v1/payments/spend", { method: "POST", body: {} })).status).toBe(200);
    expect((await r.app.request("/v1/payments/mandates")).status).toBe(401);
    expect(seen).toHaveLength(2);
  });

  it("passes the core's pending answer through and drops the approval token header", async () => {
    const pending = JSON.stringify({ tool: "payments.spend", pending: true, approval_id: 7, url: `${BASE}/approve/tok`, next_action: "approve_at_url", authorization: "approval", reason: "no_mandate" });
    const { r } = rig({ status: 202, body: pending, headers: { "x-shogun-approval-token": "tok" } });
    const agent = await registerAgent(r);
    const res = await authed(r, agent.key, "/v1/payments/spend", { method: "POST", body: { payee: "x", currency: "USD", amount_minor: 1, idempotency_key: "k" } });
    expect(res.status).toBe(202);
    expect(await res.text()).toBe(pending);
    expect(res.headers.get("x-shogun-approval-token")).toBeNull();
  });

  it("answers 503 without a core, and the sidecar relay keeps each payments path", async () => {
    const none = makeAgentRig({ mcpUpstream: undefined });
    const a = await registerAgent(none);
    const res = await authed(none, a.key, "/v1/payments/mandates");
    expect(res.status).toBe(503);
    for (const route of PAYMENT_ROUTES) {
      expect(SIDECAR_PATHS).toContain(route.path);
      expect(routeTemplate(route.path)).toBe(route.path);
    }
    const urls: string[] = [];
    const auths: Array<string | null> = [];
    const relay = createMcpRelay({ url: "http://127.0.0.1:7464", tokens: new Map([["p1", "sidecar-token-1"]]) }, async (input, init) => {
      urls.push(input);
      auths.push(new Headers(init?.headers).get("authorization"));
      return new Response("{}", { status: 200 });
    });
    const principal = { principalId: "p1", keyId: "k1" } as Parameters<typeof relay.upstream>[1];
    await relay.upstream(new Request(`${BASE}/v1/payments/mandates`, { headers: { authorization: "Bearer sa_live_agent-key" } }), principal);
    // The agent's key never reaches the core: the relay presents the principal's sidecar bearer.
    expect(auths).toEqual(["Bearer sidecar-token-1"]);
    await relay.upstream(new Request(`${BASE}/v1/payments/spend`, { method: "POST", body: "{}" }), principal);
    await relay.upstream(new Request(`${BASE}/v1/payments/other`, { method: "POST", body: "{}" }), principal);
    expect(urls).toEqual(["http://127.0.0.1:7464/v1/payments/mandates", "http://127.0.0.1:7464/v1/payments/spend", "http://127.0.0.1:7464/mcp"]);
  });
});
