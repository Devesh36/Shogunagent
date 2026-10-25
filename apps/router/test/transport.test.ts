/**
 * E8-T9 / invariant 7: provider credentials come from the env map at the transport layer; the
 * engine and connector hooks never see them.
 */
import { describe, expect, it } from "vitest";
import { defineEndpoint, z, zodInput } from "../connectors/define.js";
import type { Json } from "../connectors/types.js";
import { compileCatalog } from "../src/catalog.js";
import { run } from "../src/run.js";
import { createFetchTransport, TransportError, type TransportCall } from "../src/transport.js";
import { AGENT_KEY, bearer, makeDeps, synthProvider } from "./helpers.js";

const KEY = "PROVIDER-KEY-SENTINEL-4e2b";

function fakeFetch(seen: { url: string; headers: Record<string, string>; body: string | null }[], answer: Json = { items: [1] }) {
  return async (input: string | URL | Request, init?: RequestInit): Promise<Response> => {
    const h = new Headers(init?.headers);
    const headers: Record<string, string> = {};
    h.forEach((v, k) => (headers[k] = v));
    seen.push({ url: String(input), headers, body: typeof init?.body === "string" ? init.body : null });
    return new Response(JSON.stringify(answer), { status: 200, headers: { "content-type": "application/json" } });
  };
}

describe("transport credential injection", () => {
  it("injects the bearer at fetch time; hooks and the engine receive no credential fields", async () => {
    const hookContexts: unknown[] = [];
    const endpoint = defineEndpoint({
      id: "lookup",
      meta: { summary: "look something up", description: "look something up in a vendor database", categories: ["data"] },
      request: { method: "POST", path: "/v1/lookup" },
      input: {
        schema: zodInput(z.object({ q: z.string() }).strict()),
        toRequest: (ctx) => {
          hookContexts.push(ctx);
          return { method: "POST", path: "/v1/lookup", body: ctx.input };
        },
      },
      output: {
        fromResponse: (ctx) => {
          hookContexts.push(ctx);
          return ctx.response.body;
        },
      },
      usage: { model: "per_unit", unit: "u", cost_micro_usd_per_unit: 10, typical_units: 1, units: (ctx) => { hookContexts.push(ctx); return 2; } },
      scope: { class: "read" },
      health: { p50_ms: 1, p95_ms: 2, success_rate: 1 },
      fixtures: [],
    });
    const catalog = compileCatalog([synthProvider("vendor", [endpoint])]);
    const seen: { url: string; headers: Record<string, string>; body: string | null }[] = [];
    const engineCalls: TransportCall[] = [];
    const inner = createFetchTransport({ env: { VENDOR_KEY: KEY }, fetch: fakeFetch(seen) });
    const deps = makeDeps({ catalog });
    deps.transport = { execute: (call) => { engineCalls.push(call); return inner.execute(call); } };

    const out = await run(deps, { input: { tool_id: "vendor.lookup", input: { q: "hi" }, idempotency_key: "t1" }, bearer: bearer() });
    expect(out.status).toBe(200);
    // 2 units × 10 µ$ from the provider, plus one take (5%, rounded up) for a third-party tool.
    expect(out.body.usage).toEqual({ unit: "u", quantity: 2, cost_micro_usd: 21 });

    // fetch saw the credential…
    expect(seen).toHaveLength(1);
    expect(seen[0]?.headers["authorization"]).toBe(`Bearer ${KEY}`);
    expect(seen[0]?.url).toBe("https://vendor.test/v1/lookup");
    // …the engine's call to the transport did not carry it, nor the agent's bearer for a non-first-party provider…
    const engineView = JSON.stringify(engineCalls);
    expect(engineView).not.toContain(KEY);
    expect(engineView).not.toContain(AGENT_KEY);
    expect(engineCalls[0]?.caller).toBeUndefined();
    expect(engineCalls[0]?.provider.auth).toEqual({ kind: "bearer", credential_env: "VENDOR_KEY" });
    // …and no hook received a credential field or value.
    expect(hookContexts.length).toBe(3);
    const hookView = JSON.stringify(hookContexts);
    expect(hookView).not.toContain(KEY);
    expect(hookView).not.toContain(AGENT_KEY);
    for (const ctx of hookContexts) {
      for (const k of Object.keys(ctx as object)) expect(["input", "request", "response"]).toContain(k);
      expect(ctx).not.toHaveProperty("authorization");
      expect(ctx).not.toHaveProperty("apiKey");
      expect(ctx).not.toHaveProperty("credential");
      expect(ctx).not.toHaveProperty("env");
    }
  });

  it("supports header and query injection and fails closed when the credential is missing", async () => {
    const seen: { url: string; headers: Record<string, string>; body: string | null }[] = [];
    const t = createFetchTransport({ env: { H: "hv", Q: "qv" }, fetch: fakeFetch(seen) });
    await t.execute({ provider: { id: "a", auth: { kind: "header", header: "x-api-key", credential_env: "H" }, baseUrl: "https://a.test/" }, request: { method: "GET", path: "/x", query: { q: "1" } } });
    await t.execute({ provider: { id: "b", auth: { kind: "query", param: "key", credential_env: "Q" }, baseUrl: "https://b.test" }, request: { method: "GET", path: "y" } });
    expect(seen[0]?.headers["x-api-key"]).toBe("hv");
    expect(seen[0]?.url).toBe("https://a.test/x?q=1");
    expect(seen[1]?.url).toBe("https://b.test/y?key=qv");
    await expect(t.execute({ provider: { id: "c", auth: { kind: "bearer", credential_env: "MISSING" }, baseUrl: "https://c.test" }, request: { method: "GET", path: "/" } })).rejects.toBeInstanceOf(TransportError);
    expect(seen).toHaveLength(2);
  });

  it("forwards the caller's bearer only for caller_bearer providers and honours the base-URL env override", async () => {
    const seen: { url: string; headers: Record<string, string>; body: string | null }[] = [];
    const t = createFetchTransport({ env: { SHOGUNAGENT_API_BASE_URL: "http://localhost:8787" }, fetch: fakeFetch(seen, { items: [] }) });
    const provider = { id: "shogunagent", auth: { kind: "caller_bearer" } as const, baseUrl: "https://api.shogunaios.com", baseUrlEnv: "SHOGUNAGENT_API_BASE_URL" };
    await t.execute({ provider, request: { method: "GET", path: "/v1/state/commitments" }, caller: { bearer: bearer() } });
    expect(seen[0]?.url).toBe("http://localhost:8787/v1/state/commitments");
    expect(seen[0]?.headers["authorization"]).toBe(`Bearer ${AGENT_KEY}`);
    await expect(t.execute({ provider, request: { method: "GET", path: "/v1/state/commitments" } })).rejects.toMatchObject({ code: "caller_bearer_missing" });
  });
});
