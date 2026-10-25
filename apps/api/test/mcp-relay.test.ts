/** E34 (gateway half): the hosted `/mcp` relay to the principal's sidecar (mcp-proxy.ts), and the
 * settlement contract between the two — the gateway bills what the core's reply `Mcp-Quantity`
 * reports, never the request's figure and never from the body (metering.ts `settledQuantity`). */
import { createServer, type IncomingMessage, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import type { AgentPrincipal } from "../src/agent-keys.js";
import { BootError, agentLaneLine, createRelay, mcpRelayConfigFromEnv, postgresStore, relayConfigFromEnv } from "../src/boot.js";
import { createMcpRelay, derivedSidecarBearer, fnv1a64, parseShard, parseSidecarPrincipalTokens, shardOf, type FetchFn } from "../src/mcp-proxy.js";
import { settledQuantity } from "../src/metering.js";
import { FREE_ALLOWANCE_MICROS, RATE_MICROS } from "../src/pricing.js";
import { solvePow } from "../src/pow.js";
import releaseKey from "../../../packages/config/license-public-key.json";
import { BASE, authed, makeAgentRig, mcp, registerAgent, type AgentRig } from "./agent-helpers.js";
import { AGENT_SQL } from "./agent-store.test.js";
import { FakeGateway } from "./helpers.js";
import { sqliteDouble } from "./sql-double.js";

interface Err { error: { code: string; message: string; next_action: string | null } }

async function balance(rig: AgentRig, key: string): Promise<{ balance: number; reserved: number }> {
  const w = (await (await authed(rig, key, "/v1/wallet")).json()) as { balance_micro_usd: number; reserved_micro_usd: number };
  return { balance: w.balance_micro_usd, reserved: w.reserved_micro_usd };
}

const ingest = (qty: string) => ({ "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": qty });

describe("settledQuantity: the gateway ↔ core billing contract", () => {
  const reply = (status: number, quantity?: string) => ({ status, headers: new Headers(quantity === undefined ? {} : { "mcp-quantity": quantity }) });
  it("settles the reply's figure, capped at what was reserved, and 0 for anything else", () => {
    expect(settledQuantity(reply(200, "5"), 5)).toBe(5);
    expect(settledQuantity(reply(200, "3"), 5)).toBe(3);
    expect(settledQuantity(reply(200, "50"), 5)).toBe(5);
    expect(settledQuantity(reply(200, "0"), 5)).toBe(0);
    expect(settledQuantity(reply(200), 5)).toBe(0);
    for (const bad of ["", "-1", "1.5", "abc", "1e3", "99999999999999999"]) expect(settledQuantity(reply(200, bad), 5)).toBe(0);
    expect(settledQuantity(reply(202, "5"), 5)).toBe(5);
    for (const status of [400, 401, 405, 500, 503]) expect(settledQuantity(reply(status, "5"), 5)).toBe(0);
  });
});

describe("metered /mcp settles from the core's reply", () => {
  it("normal ingest: bills the events the core took", async () => {
    const rig = makeAgentRig();
    const { key } = await registerAgent(rig);
    const res = await mcp(rig, key, ingest("5"));
    expect(res.status).toBe(200);
    expect(res.headers.get("x-usage-quantity")).toBe("5");
    expect(res.headers.get("x-usage-cost-micro-usd")).toBe(String(5 * RATE_MICROS.ingest));
    expect(await balance(rig, key)).toEqual({ balance: FREE_ALLOWANCE_MICROS - 5 * RATE_MICROS.ingest, reserved: 0 });
  });

  it("a refused call (HTTP 200, isError result, Mcp-Quantity: 0) costs nothing", async () => {
    const rig = makeAgentRig();
    const { key } = await registerAgent(rig);
    rig.upstream.body = JSON.stringify({ jsonrpc: "2.0", id: 1, result: { isError: true, content: [{ type: "text", text: "invalid_argument: events is empty" }] } });
    rig.upstream.quantity = "0";
    const res = await mcp(rig, key, ingest("5"));
    expect(res.status).toBe(200);
    expect(res.headers.get("x-usage-quantity")).toBe("0");
    expect(res.headers.get("x-usage-cost-micro-usd")).toBe("0");
    expect(await balance(rig, key)).toEqual({ balance: FREE_ALLOWANCE_MICROS, reserved: 0 });
    // The body is relayed untouched.
    expect(await res.text()).toBe(rig.upstream.body);
  });

  it("an idempotent replay (usage.quantity 0) costs nothing; a partial replay bills the new events only", async () => {
    const rig = makeAgentRig();
    const { key } = await registerAgent(rig);
    rig.upstream.quantity = "0";
    expect((await mcp(rig, key, ingest("5"))).headers.get("x-usage-cost-micro-usd")).toBe("0");
    rig.upstream.quantity = "2";
    const partial = await mcp(rig, key, ingest("5"));
    expect(partial.headers.get("x-usage-quantity")).toBe("2");
    expect(partial.headers.get("x-usage-cost-micro-usd")).toBe(String(2 * RATE_MICROS.ingest));
    expect(await balance(rig, key)).toEqual({ balance: FREE_ALLOWANCE_MICROS - 2 * RATE_MICROS.ingest, reserved: 0 });
    // A core claiming more than was reserved is capped at the reservation.
    rig.upstream.quantity = "500";
    expect((await mcp(rig, key, ingest("1"))).headers.get("x-usage-cost-micro-usd")).toBe(String(RATE_MICROS.ingest));
  });

  it("a JSON-RPC error (HTTP 200) costs nothing, and so does a billable reply without the header", async () => {
    const rig = makeAgentRig();
    const { key } = await registerAgent(rig);
    rig.upstream.body = JSON.stringify({ jsonrpc: "2.0", id: 1, error: { code: -32602, message: "invalid params" } });
    rig.upstream.quantity = "0";
    expect((await mcp(rig, key, ingest("3"))).headers.get("x-usage-cost-micro-usd")).toBe("0");
    rig.upstream.quantity = null;
    const op = await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "memory.summarize" });
    expect(op.status).toBe(200);
    expect(op.headers.get("x-usage-cost-micro-usd")).toBe("0");
    expect(await balance(rig, key)).toEqual({ balance: FREE_ALLOWANCE_MICROS, reserved: 0 });
    expect(rig.logLines.some((l) => l.startsWith("POST /mcp 200") && l.includes("event=settle_quantity_missing"))).toBe(true);
    const receipts = (await (await authed(rig, key, "/v1/receipts")).json()) as { receipts: Array<{ cost_micro_usd: number; settled: boolean }> };
    expect(receipts.receipts.every((r) => r.cost_micro_usd === 0 && r.settled)).toBe(true);
  });

  it("reads stay free whatever the reply says (invariant 10)", async () => {
    const rig = makeAgentRig();
    const { key } = await registerAgent(rig);
    rig.upstream.quantity = "7";
    const res = await mcp(rig, key, { "mcp-method": "tools/call", "mcp-name": "state.commitments.list" });
    expect(res.headers.get("x-usage-unit")).toBe("read");
    expect(res.headers.get("x-usage-cost-micro-usd")).toBe("0");
    expect(await balance(rig, key)).toEqual({ balance: FREE_ALLOWANCE_MICROS, reserved: 0 });
  });
});

describe("metered /mcp: authentication and headers", () => {
  it("answers 401 with WWW-Authenticate (and the resource metadata) before looking at metering headers", async () => {
    const rig = makeAgentRig({ resourceMetadataUrl: `${BASE}/.well-known/oauth-protected-resource/mcp` });
    for (const headers of [{ "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "nope" }, {}]) {
      const res = await rig.app.request("/mcp", { method: "POST", headers, body: "{}" });
      expect(res.status).toBe(401);
      expect(res.headers.get("www-authenticate")).toBe(`Bearer realm="shogunagent", resource_metadata="${BASE}/.well-known/oauth-protected-resource/mcp"`);
      expect(((await res.json()) as Err).error.next_action).toBe("register");
    }
    expect(rig.upstreamCalls).toHaveLength(0);
  });

  it("requires Mcp-Method on a POST and Mcp-Name on a tools/call, before reserving", async () => {
    const rig = makeAgentRig();
    const { key } = await registerAgent(rig);
    const noMethod = await rig.app.request("/mcp", { method: "POST", headers: { authorization: `Bearer ${key}` }, body: "{}" });
    expect(noMethod.status).toBe(400);
    expect(((await noMethod.json()) as Err).error).toEqual(expect.objectContaining({ code: "missing_header", next_action: "fix_request" }));
    const noName = await mcp(rig, key, { "mcp-method": "tools/call" });
    expect(noName.status).toBe(400);
    expect(rig.upstreamCalls).toHaveLength(0);
    // GET (and DELETE) carry no JSON-RPC method: relayed as protocol traffic.
    expect((await mcp(rig, key, {}, "GET")).status).toBe(200);
  });
});

const principal = (principalId: string, keyId = "ak_1"): AgentPrincipal => ({ keyId, mode: "live", principalId, tenantId: "ten_1", scopes: ["read", "write"] });

describe("createMcpRelay", () => {
  it("parses the token list like the sidecar, refusing by rule", () => {
    const tokens = parseSidecarPrincipalTokens(" alice:tok-alice-1 , bob:tok-bob-22:agent7 ");
    expect([...tokens]).toEqual([["alice", "tok-alice-1"], ["bob", "tok-bob-22"]]);
    expect(() => parseSidecarPrincipalTokens("alice")).toThrow("principal:token");
    expect(() => parseSidecarPrincipalTokens("alice:short")).toThrow("8 characters");
    expect(() => parseSidecarPrincipalTokens("Alice:longenough")).toThrow("principal id");
    expect(() => parseSidecarPrincipalTokens("a:longenough,b:longenough")).toThrow("share a token");
  });

  it("derives a stable per-principal, per-key bearer and the deploy README's shard", () => {
    const secret = "s".repeat(32);
    const a = derivedSidecarBearer(secret, principal("prn_a"))!;
    expect(a).toMatch(/^sgw1\.prn_a\.ak_1\.[A-Za-z0-9_-]{43}$/);
    expect(derivedSidecarBearer(secret, principal("prn_a"))).toBe(a);
    expect(derivedSidecarBearer(secret, principal("prn_a", "ak_2"))).not.toBe(a);
    expect(derivedSidecarBearer("t".repeat(32), principal("prn_a"))).not.toBe(a);
    expect(derivedSidecarBearer(secret, principal("PRN.bad"))).toBeUndefined();
    // FNV-1a 64 reference vectors.
    expect(fnv1a64("").toString(16)).toBe("cbf29ce484222325");
    expect(fnv1a64("a").toString(16)).toBe("af63dc4c8601ec8c");
    expect(shardOf("a", 2)).toBe(Number(0xaf63dc4c8601ec8cn % 2n));
    expect(parseShard("1/3")).toEqual({ index: 1, count: 3 });
    expect(() => parseShard("3/3")).toThrow();
    expect(() => parseShard("x")).toThrow();
  });

  it("admits only principals it can reach: provisioned and on this shard", () => {
    const relay = createMcpRelay({ url: "http://127.0.0.1:7464", tokens: new Map([["alice", "tok-alice-1"]]) });
    expect(relay.admit(principal("alice"))).toBeUndefined();
    expect(relay.admit(principal("bob"))).toEqual(expect.objectContaining({ status: 503, code: "principal_not_provisioned", nextAction: "contact_operator" }));
    const sharded = createMcpRelay({ url: "http://127.0.0.1:7464", bearerSecret: "s".repeat(32), shard: { index: shardOf("a", 2), count: 2 } });
    expect(sharded.admit(principal("a"))).toBeUndefined();
    const other = ["b", "c", "d", "e", "f"].find((id) => shardOf(id, 2) !== shardOf("a", 2))!;
    expect(sharded.admit(principal(other))).toEqual(expect.objectContaining({ status: 421, code: "wrong_shard" }));
  });

  it("forwards allowlisted headers with the principal's sidecar bearer and streams both bodies", async () => {
    const seen: { url?: string; headers?: Headers; body?: string } = {};
    let release!: () => void;
    const gate = new Promise<void>((r) => { release = r; });
    const enc = new TextEncoder();
    const fetchFn: FetchFn = async (url, init) => {
      seen.url = url;
      seen.headers = new Headers(init?.headers);
      seen.body = await new Response(init?.body as ReadableStream).text();
      // A reply whose second half is held back: the relay must hand the first half on at once.
      const body = new ReadableStream<Uint8Array>({
        async start(controller) {
          controller.enqueue(enc.encode("event: message\n"));
          await gate;
          controller.enqueue(enc.encode("data: {}\n\n"));
          controller.close();
        },
      });
      return new Response(body, { status: 200, headers: { "content-type": "text/event-stream", "mcp-quantity": "2", "mcp-method": "tools/call", "set-cookie": "x=1", "x-internal": "1" } });
    };
    const relay = createMcpRelay({ url: "http://127.0.0.1:7464/", tokens: new Map([["alice", "tok-alice-1"]]) }, fetchFn);
    const request = new Request("https://api.test/mcp", {
      method: "POST",
      headers: { authorization: "Bearer sa_live_agentkey", "content-type": "application/json", accept: "text/event-stream", "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "2", cookie: "c=1", "x-forwarded-for": "198.51.100.1", "x-payment": "p" },
      body: '{"jsonrpc":"2.0"}',
    });
    const res = await relay.upstream(request, principal("alice"));
    expect(seen.url).toBe("http://127.0.0.1:7464/mcp");
    expect(seen.headers?.get("authorization")).toBe("Bearer tok-alice-1");
    expect(seen.headers?.get("mcp-method")).toBe("tools/call");
    expect(seen.headers?.get("mcp-name")).toBe("memory.ingest");
    for (const h of ["cookie", "x-forwarded-for", "x-payment", "mcp-quantity"]) expect(seen.headers?.has(h)).toBe(false);
    expect(seen.body).toBe('{"jsonrpc":"2.0"}');
    expect(res.status).toBe(200);
    expect(res.headers.get("mcp-quantity")).toBe("2");
    expect(res.headers.get("set-cookie")).toBeNull();
    expect(res.headers.get("x-internal")).toBeNull();
    const reader = res.body!.getReader();
    expect(new TextDecoder().decode((await reader.read()).value)).toBe("event: message\n");
    release();
    expect(new TextDecoder().decode((await reader.read()).value)).toBe("data: {}\n\n");
    expect((await reader.read()).done).toBe(true);
  });

  it("turns the sidecar refusing the gateway's bearer into a 503, and gives up when reply headers never come", async () => {
    const refused = createMcpRelay({ url: "http://127.0.0.1:7464", bearerSecret: "s".repeat(32) }, () => Promise.resolve(new Response('{"error":"unauthorized"}', { status: 401, headers: { "www-authenticate": "Bearer" } })));
    const res = await refused.upstream(new Request("https://api.test/mcp", { method: "POST", body: "{}" }), principal("alice"));
    expect(res.status).toBe(503);
    expect(res.headers.get("www-authenticate")).toBeNull();
    expect(((await res.json()) as Err).error.code).toBe("core_rejected_gateway");
    const hung = createMcpRelay({ url: "http://127.0.0.1:7464", bearerSecret: "s".repeat(32) }, (_url, init) => new Promise((_resolve, reject) => {
      init?.signal?.addEventListener("abort", () => reject(new Error("aborted")));
    }), 20);
    await expect(hung.upstream(new Request("https://api.test/mcp", { method: "POST", body: "{}" }), principal("alice"))).rejects.toThrow();
    await expect(createMcpRelay({ url: "http://x", tokens: new Map() }).upstream(new Request("https://api.test/mcp"), principal("alice"))).rejects.toThrow("principal_not_provisioned");
  });
});

describe("boot: the /mcp relay settings", () => {
  const refusal = (env: Record<string, string>): string => {
    try {
      mcpRelayConfigFromEnv(env);
    } catch (e) {
      expect(e).toBeInstanceOf(BootError);
      return (e as Error).message;
    }
    throw new Error("expected a refusal");
  };
  it("is off without a bearer source, and refuses by name", () => {
    expect(mcpRelayConfigFromEnv({})).toBeUndefined();
    expect(mcpRelayConfigFromEnv({ SHOGUN_SIDECAR_URL: "http://127.0.0.1:7464" })).toBeUndefined();
    expect(mcpRelayConfigFromEnv({ SHOGUN_SIDECAR_URL: "http://127.0.0.1:7464", SHOGUN_SIDECAR_BEARER_SECRET: "s".repeat(32), SHOGUN_SHARD: "0/2" }))
      .toEqual({ url: "http://127.0.0.1:7464", bearerSecret: "s".repeat(32), shard: { index: 0, count: 2 } });
    expect(refusal({ SHOGUN_SIDECAR_BEARER_SECRET: "s".repeat(32) })).toContain("SHOGUN_SIDECAR_URL");
    expect(refusal({ SHOGUN_SIDECAR_URL: "http://127.0.0.1:7464", SHOGUN_SIDECAR_BEARER_SECRET: "short" })).toContain("SHOGUN_SIDECAR_BEARER_SECRET");
    expect(refusal({ SHOGUN_SIDECAR_URL: "http://127.0.0.1:7464", SHOGUN_SIDECAR_PRINCIPAL_TOKENS: "alice:secret-value-x:a:b,alice:other-token" })).not.toContain("secret-value-x");
    expect(refusal({ SHOGUN_SHARD: "0/2" })).toContain("SHOGUN_SHARD");
    expect(refusal({ SHOGUN_SIDECAR_URL: "http://127.0.0.1:7464", SHOGUN_SIDECAR_BEARER_SECRET: "s".repeat(32), SHOGUN_SHARD: "2/2" })).toContain("SHOGUN_SHARD");
  });
});

/** A stand-in sidecar on loopback: checks the bearer, reads the body (it is the core), answers
 * like `shogun-api`'s transport and reports the quantity it took. Records headers only. */
describe("createRelay → a sidecar on loopback", () => {
  const secret = "b".repeat(40);
  const calls: Array<{ authorization: string | undefined; method: string | undefined }> = [];
  let server: Server;
  let url = "";
  beforeAll(async () => {
    server = createServer((req: IncomingMessage, res) => {
      const chunks: Buffer[] = [];
      req.on("data", (c: Buffer) => chunks.push(c));
      req.on("end", () => {
        calls.push({ authorization: req.headers.authorization, method: req.headers["mcp-method"] as string | undefined });
        if (!req.headers.authorization?.startsWith("Bearer sgw1.")) {
          res.writeHead(401, { "content-type": "application/json" }).end('{"error":"unauthorized"}');
          return;
        }
        const msg = JSON.parse(Buffer.concat(chunks).toString("utf8")) as { id: number; params?: { arguments?: { events?: unknown[] } } };
        const taken = msg.params?.arguments?.events?.length ?? 0;
        res.writeHead(200, { "content-type": "application/json", "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": String(taken) })
          .end(JSON.stringify({ jsonrpc: "2.0", id: msg.id, result: { content: [{ type: "text", text: JSON.stringify({ usage: { quantity: taken } }) }] } }));
      });
    });
    await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
    url = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
  });
  afterAll(() => new Promise<void>((r) => server.close(() => r())));

  it("serves /mcp through the sidecar instead of 503, billing what the sidecar took", async () => {
    const env = {
      ANTHROPIC_API_KEY: "sk-ant-test-not-a-real-key", LICENSE_PUBKEY_B64: (releaseKey as { ed25519: string }).ed25519, DAILY_SPEND_CAP_USD: "10", BATCH_PROVIDER: "anthropic", OPENAI_API_KEY: "sk-test-not-a-real-key",
      AGENT_API_ENABLED: "1", AGENT_POW_SECRET: "a-sixteen-char-secret!", PUBLIC_BASE_URL: "https://api.example.test", AGENT_POW_BITS: "8", AGENT_KEY_PBKDF2_ITERATIONS: "1000",
      SHOGUN_SIDECAR_URL: url, SHOGUN_SIDECAR_BEARER_SECRET: secret,
    };
    const config = relayConfigFromEnv(env);
    expect(agentLaneLine(config)).toContain("mcp relay on");
    const lines: string[] = [];
    const store = postgresStore(sqliteDouble(AGENT_SQL), config, (l) => lines.push(l));
    await store.agent.checkSchema();
    const relay = createRelay(config, store, { gateway: new FakeGateway(), log: (l) => lines.push(l) });
    const challenge = ((await (await relay.app.request("/v1/agents/challenge")).json()) as { challenge: string }).challenge;
    const reg = await relay.app.request("/v1/agents", { method: "POST", headers: { "cf-connecting-ip": "203.0.113.78", "x-agent-pow": `${challenge}:${solvePow(challenge, 8)!}` } });
    const { key, principal_id: principalId, key_id: keyId } = (await reg.json()) as { key: string; principal_id: string; key_id: string };
    const body = JSON.stringify({ jsonrpc: "2.0", id: 7, method: "tools/call", params: { name: "memory.ingest", arguments: { events: [{}, {}] } } });
    const res = await relay.app.request("/mcp", { method: "POST", headers: { authorization: `Bearer ${key}`, "content-type": "application/json", "mcp-method": "tools/call", "mcp-name": "memory.ingest", "mcp-quantity": "5" }, body });
    expect(res.status).toBe(200);
    expect(res.headers.get("x-usage-quantity")).toBe("2");
    expect(res.headers.get("x-usage-cost-micro-usd")).toBe(String(2 * RATE_MICROS.ingest));
    expect(((await res.json()) as { id: number }).id).toBe(7);
    expect(calls.at(-1)?.authorization).toBe(`Bearer ${derivedSidecarBearer(secret, { principalId, keyId })}`);
    expect(calls.some((c) => c.authorization?.includes(key))).toBe(false);
    const wallet = (await (await relay.app.request("/v1/wallet", { headers: { authorization: `Bearer ${key}` } })).json()) as { balance_micro_usd: number; reserved_micro_usd: number };
    expect(wallet).toEqual(expect.objectContaining({ balance_micro_usd: FREE_ALLOWANCE_MICROS - 2 * RATE_MICROS.ingest, reserved_micro_usd: 0 }));
    // No body and no key in the gateway's log lines.
    const log = lines.join("\n");
    expect(log).not.toContain("jsonrpc");
    expect(log).not.toContain(key.slice(8));
    expect(log).not.toContain(principalId);
  });
});
