/**
 * Write-only ingest keys for the browser extension (E51, docs/api/ingest-sources.md §3): a
 * principal's `write` key issues one with `POST /v1/keys`; it may call `memory.ingest` over `/mcp`
 * (and the MCP handshake and tool list) and nothing else, enforced from `Mcp-Method` / `Mcp-Name`
 * without reading the body. It is stored hashed and manages only itself.
 */
import { describe, expect, it } from "vitest";

import { scopeRefusal } from "../src/metering.js";
import { hasScopes, isIngestOnly } from "../src/agent-keys.js";
import { authed, makeAgentRig, mcp, registerAgent, type AgentRig, type Registered } from "./agent-helpers.js";

async function issue(r: AgentRig, parent: Registered, body: unknown = {}, headers: Record<string, string> = {}): Promise<Response> {
  return authed(r, parent.key, "/v1/keys", { method: "POST", body, headers });
}

async function ingestKey(r: AgentRig, parent: Registered): Promise<{ key: string; key_id: string; scopes: string[]; expires_at: string }> {
  const res = await issue(r, parent);
  expect(res.status).toBe(201);
  return (await res.json()) as { key: string; key_id: string; scopes: string[]; expires_at: string };
}

const call = (name: string) => ({ "mcp-method": "tools/call", "mcp-name": name });

describe("POST /v1/keys issues a write-only ingest key", () => {
  it("shows the secret once, stores only its hash, and expires in 90 days by default", async () => {
    const r = makeAgentRig();
    const parent = await registerAgent(r);
    const issued = await ingestKey(r, parent);
    expect(issued.key).toMatch(/^sa_live_/);
    expect(issued.scopes).toEqual(["ingest"]);
    expect(Date.parse(issued.expires_at) - r.now().getTime()).toBe(90 * 86_400_000);
    const stored = await r.store.keyById(issued.key_id);
    expect(stored?.principalId).toBe(parent.principalId);
    expect(stored?.hash).toMatch(/^pbkdf2-sha256\$/);
    expect(JSON.stringify(stored)).not.toContain(issued.key.slice(8));
    expect(r.logLines.join("\n")).not.toContain(issued.key);
  });

  it("refuses other scopes, bad expiries, OAuth-less misuse and a key without write", async () => {
    const r = makeAgentRig();
    const parent = await registerAgent(r);
    expect((await issue(r, parent, { scopes: ["read"] })).status).toBe(400);
    expect((await issue(r, parent, { scopes: ["ingest", "write"] })).status).toBe(400);
    expect((await issue(r, parent, { expires_in_days: 0 })).status).toBe(400);
    expect((await issue(r, parent, { expires_in_days: 366 })).status).toBe(400);
    expect((await issue(r, parent, { expires_in_days: 7 })).status).toBe(201);
    // An ingest key cannot mint keys (it lacks write).
    const child = await ingestKey(r, parent);
    const res = await authed(r, child.key, "/v1/keys", { method: "POST", body: {} });
    expect(res.status).toBe(403);
  });

  it("an Idempotency-Key answers the same key reference, never a second key or the secret", async () => {
    const r = makeAgentRig();
    const parent = await registerAgent(r);
    const first = await issue(r, parent, {}, { "idempotency-key": "extension-install-1" });
    expect(first.status).toBe(201);
    const a = (await first.json()) as { key: string; key_id: string };
    const again = await issue(r, parent, {}, { "idempotency-key": "extension-install-1" });
    expect(again.status).toBe(200);
    const b = (await again.json()) as { key: string | null; key_id: string; created: boolean };
    expect(b).toMatchObject({ key: null, key_id: a.key_id, created: false });
    // Another principal's same Idempotency-Key is another key.
    const other = await registerAgent(r, {}, "203.0.113.9");
    const c = (await (await issue(r, other, {}, { "idempotency-key": "extension-install-1" })).json()) as { key_id: string };
    expect(c.key_id).not.toBe(a.key_id);
  });
});

describe("an ingest key calls memory.ingest and nothing else", () => {
  it("is relayed and metered as ingest for memory.ingest, and refused for every other tool", async () => {
    const r = makeAgentRig();
    const parent = await registerAgent(r);
    const child = await ingestKey(r, parent);
    const ok = await mcp(r, child.key, { ...call("memory.ingest"), "mcp-quantity": "2" });
    expect(ok.status).toBe(200);
    expect(ok.headers.get("x-usage-unit")).toBe("ingest");
    expect(r.upstreamCalls).toHaveLength(1);
    for (const name of ["memory.ingest_webhook", "memory.append_note", "handoff.accept", "memory.recall", "state.commitments.list", "memory.search", "actions.execute", "payments.spend", "router.run", "translate.text"]) {
      const res = await mcp(r, child.key, call(name));
      expect(res.status, name).toBe(403);
      expect(((await res.json()) as { error: { code: string } }).error.code, name).toBe("insufficient_scope");
    }
    // The handshake and the tool list pass; nothing that reads state does.
    for (const method of ["initialize", "tools/list", "ping"]) expect((await mcp(r, child.key, { "mcp-method": method })).status, method).toBe(200);
    for (const method of ["resources/read", "resources/list", "prompts/get"]) expect((await mcp(r, child.key, { "mcp-method": method })).status, method).toBe(403);
    expect((await mcp(r, child.key, {}, "GET")).status).toBe(403);
    expect(r.upstreamCalls).toHaveLength(4);
  });

  it("is refused by every REST route that needs a scope", async () => {
    const r = makeAgentRig();
    const parent = await registerAgent(r);
    const child = await ingestKey(r, parent);
    for (const [method, path] of [["GET", "/v1/agents/me"], ["GET", "/v1/wallet"], ["GET", "/v1/receipts"], ["POST", "/v1/quote"], ["POST", "/v1/handoff"],
      ["GET", "/v1/payments/mandates"], ["POST", "/v1/payments/spend"], ["POST", "/v1/wallet/topup"]] as const) {
      const res = await authed(r, child.key, path, method === "GET" ? {} : { method, body: {} });
      expect(res.status, path).toBe(403);
    }
    expect(r.upstreamCalls).toHaveLength(0);
  });

  it("manages only itself: it cannot revoke or re-cap its parent, and can revoke itself", async () => {
    const r = makeAgentRig();
    const parent = await registerAgent(r);
    const child = await ingestKey(r, parent);
    expect((await authed(r, child.key, `/v1/keys/${parent.keyId}/revoke`, { method: "POST" })).status).toBe(404);
    expect((await authed(r, child.key, `/v1/keys/${parent.keyId}`, { method: "PATCH", body: { day_micro_usd: 1 } })).status).toBe(404);
    expect((await authed(r, parent.key, "/v1/agents/me")).status).toBe(200);
    // The parent may revoke it, and it may revoke itself.
    expect((await authed(r, child.key, `/v1/keys/${child.key_id}`, { method: "PATCH", body: { day_micro_usd: 1_000 } })).status).toBe(200);
    expect((await authed(r, child.key, `/v1/keys/${child.key_id}/revoke`, { method: "POST" })).status).toBe(200);
    expect((await mcp(r, child.key, call("memory.ingest"))).status).toBe(401);
  });

  it("the scope rule itself: write covers ingest, and ingest covers nothing else", () => {
    expect(isIngestOnly(["ingest"])).toBe(true);
    expect(isIngestOnly(["ingest", "read"])).toBe(false);
    expect(hasScopes(["write"], ["ingest"])).toBe(true);
    expect(hasScopes(["ingest"], ["write"])).toBe(false);
    expect(scopeRefusal(["ingest"], { unit: "ingest", method: "tools/call", name: "memory.ingest" })).toBeUndefined();
    expect(scopeRefusal(["ingest"], { unit: "ingest", method: "tools/call", name: "memory.ingest" }, true)).toBe("ingest_only");
    expect(scopeRefusal(["ingest"], { unit: "ingest", method: "tools/call", name: "memory.ingest_webhook" })).toBe("ingest_only");
    expect(scopeRefusal(["write"], { unit: "ingest", method: "tools/call", name: "memory.ingest_webhook" })).toBeUndefined();
    expect(scopeRefusal(["read"], { unit: "ingest", method: "tools/call", name: "memory.ingest" })).toBe("insufficient_scope");
  });
});
