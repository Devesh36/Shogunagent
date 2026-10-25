/**
 * E48 / E50: `POST /v1/internal/router/connector-token` (connector-tokens.ts, contract
 * docs/api/connector-tokens.md) over its default backend, the principal's sidecar
 * (`sidecarConnectorVault`, here against a fake sidecar that speaks the Rust contract), and the
 * router's real client (apps/router/src/transport.ts) against it in-process. What is proved:
 *
 * - the route answers only the router's service token, resolves the principal from the caller's key
 *   (never from the body), needs the `run` scope, rejects unknown fields and foreign slots;
 * - the gateway asks the sidecar with its admin credential and the caller principal's derived
 *   bearer, with `{slot}` only: never the caller's key, never a principal in the body;
 * - 404 `connector_not_connected` for no grant, a token the router would refuse, an expired one,
 *   and a gateway without a vault; 403 for a purged principal; 503 when the sidecar does not answer,
 *   refuses the gateway's credential, or the principal lives on another shard;
 * - the router injects the principal's token into the vendor request and nothing else, and maps
 *   404 / 401 / 403 to its own errors;
 * - no token and no key reaches a log line.
 */
import { describe, expect, it } from "vitest";

import type { ProviderRef } from "../../router/src/transport.js";
import { createFetchTransport, TransportError } from "../../router/src/transport.js";
import { Secret } from "../../router/src/secret.js";
import { generateAgentKey, hashAgentKey, type AgentScope } from "../src/agent-keys.js";
import { CONNECTOR_TOKEN_PATH, sidecarConnectorVault, type ConnectorVault } from "../src/connector-tokens.js";
import { derivedSidecarBearer, shardOf } from "../src/mcp-proxy.js";
import { TEST_KEY_ITERATIONS, makeAgentRig, type AgentRig } from "./agent-helpers.js";
import { ADMIN_TOKEN, BEARER_SECRET, FakeSidecar, SIDECAR_URL } from "./connector-helpers.js";

const TOKEN = "rst_router_wallet_token_0123456789abcdef";
const ACCESS = "ya29.SENTINEL-access-token-4711";
const NOW = Date.parse("2026-10-25T12:00:00Z");

interface Fixture {
  rig: AgentRig;
  sidecar: FakeSidecar;
  key(principalId: string, scopes?: AgentScope[]): Promise<string>;
  ask(key: string | undefined, body: unknown, service?: string): Promise<Response> | Response;
}

function fixture(opts: { vault?: false | ((sidecar: FakeSidecar) => ConnectorVault) } = {}): Fixture {
  const sidecar = new FakeSidecar();
  const vault = opts.vault === false
    ? undefined
    : (opts.vault ?? ((s: FakeSidecar) => sidecarConnectorVault({ relay: { url: SIDECAR_URL, bearerSecret: BEARER_SECRET }, adminToken: ADMIN_TOKEN }, s.fetch)))(sidecar);
  const rig = makeAgentRig({ routerWallet: { token: TOKEN }, ...(vault ? { connectorTokens: { vault } } : {}) });
  rig.setNow(new Date(NOW));
  return {
    rig, sidecar,
    async key(principalId, scopes = ["read", "write", "run"]) {
      const { key, prefix } = generateAgentKey("live");
      if (!(await rig.store.principal(principalId))) await rig.store.createPrincipal({ principalId, tenantId: principalId, createdAt: NOW });
      await rig.store.insertKey({ keyId: `ak_${principalId}_${scopes.join("")}`, prefix, hash: await hashAgentKey(key, TEST_KEY_ITERATIONS), mode: "live", principalId, tenantId: principalId, scopes, createdAt: NOW });
      return key;
    },
    ask(key, body, service = TOKEN) {
      return rig.app.request(CONNECTOR_TOKEN_PATH, {
        method: "POST",
        headers: { authorization: `Bearer ${service}`, ...(key ? { "x-caller-authorization": `Bearer ${key}` } : {}), "content-type": "application/json" },
        body: typeof body === "string" ? body : JSON.stringify(body),
      });
    },
  };
}

const GMAIL = { slot: "gmail-tokenset", provider_id: "gmail", run_id: "run_1" };
const errorCode = async (res: Response) => ((await res.json()) as { error: { code: string } }).error.code;

describe("POST /v1/internal/router/connector-token (sidecar vault)", () => {
  it("returns the caller's own access token, no-store, and asks the sidecar as the caller's principal", async () => {
    const f = fixture();
    const key = await f.key("prn_a");
    f.sidecar.grant("prn_a", "gmail-tokenset", { accessToken: ACCESS, expiresAt: NOW + 3_600_000 });
    const res = await f.ask(key, GMAIL);
    expect(res.status).toBe(200);
    expect(res.headers.get("cache-control")).toBe("no-store");
    expect(await res.json()).toEqual({ access_token: ACCESS, token_type: "Bearer", expires_at: NOW + 3_600_000 });
    expect(f.rig.logLines.some((l) => l.includes("event=connector_token_served"))).toBe(true);
    // What the sidecar saw: the admin credential, the principal's derived bearer, the slot only.
    expect(f.sidecar.calls).toEqual([{
      path: "/v1/internal/connectors/token",
      authorization: `Bearer ${ADMIN_TOKEN}`,
      principalHeader: `Bearer ${derivedSidecarBearer(BEARER_SECRET, { principalId: "prn_a", keyId: "ak_prn_a_readwriterun" })}`,
      body: { slot: "gmail-tokenset" },
    }]);
    expect(JSON.stringify(f.sidecar.calls)).not.toContain(key);
    // Another principal asking for the same slot gets its own (absent) grant, never prn_a's.
    const other = await f.key("prn_b");
    const res2 = await f.ask(other, GMAIL);
    expect(res2.status).toBe(404);
    expect(await res2.json()).toEqual({ error: { code: "connector_not_connected", message: expect.any(String), next_action: "authenticate" } });
    for (const line of f.rig.logLines) {
      expect(line).not.toContain(ACCESS);
      expect(line).not.toContain(key);
      expect(line).not.toContain("prn_a");
    }
  });

  it("answers only the router's service token, before reading the body", async () => {
    const f = fixture();
    const key = await f.key("prn_a");
    const res = await f.ask(key, "{not json", "rst_wrong_token_0123456789abcdef0123");
    expect(res.status).toBe(401);
    expect(await errorCode(res)).toBe("invalid_token");
    expect(f.sidecar.calls).toEqual([]);
  });

  it("refuses a malformed body: unknown fields, a slot of another provider, a bad run id", async () => {
    const f = fixture();
    const key = await f.key("prn_a");
    for (const body of [
      { ...GMAIL, principal_id: "prn_b" },
      { slot: "slack-tokenset", provider_id: "gmail" },
      { slot: "gmail-tokenset", provider_id: "monid" },
      { slot: "batch-relay-bearer", provider_id: "gmail" },
      { ...GMAIL, run_id: "bad id with spaces" },
      [GMAIL],
      "{not json",
    ]) {
      const res = await f.ask(key, body);
      expect(res.status, JSON.stringify(body)).toBe(400);
    }
    const big = await f.ask(key, { ...GMAIL, run_id: "x".repeat(2000) });
    expect(big.status).toBe(413);
    expect(f.sidecar.calls).toEqual([]);
  });

  it("verifies the caller's key and its run scope before asking the sidecar", async () => {
    const f = fixture();
    f.sidecar.grant("prn_a", "gmail-tokenset", { accessToken: ACCESS, expiresAt: NOW + 3_600_000 });
    expect((await f.ask(undefined, GMAIL)).status).toBe(401);
    expect((await f.ask("sa_live_notakey", GMAIL)).status).toBe(401);
    const readOnly = await f.key("prn_a", ["read"]);
    const res = await f.ask(readOnly, GMAIL);
    expect(res.status).toBe(403);
    expect(await errorCode(res)).toBe("insufficient_scope");
    expect(f.sidecar.calls).toEqual([]);
  });

  it("answers 404 without a vault, and for a token the router would refuse or that has expired", async () => {
    const bare = fixture({ vault: false });
    const k = await bare.key("prn_a");
    const res = await bare.ask(k, GMAIL);
    expect(res.status).toBe(404);
    expect(bare.rig.logLines.some((l) => l.includes("event=connector_vault_unconfigured"))).toBe(true);

    const f = fixture();
    const key = await f.key("prn_a");
    f.sidecar.grant("prn_a", "gmail-tokenset", { accessToken: "short", expiresAt: NOW + 3_600_000 });
    expect((await f.ask(key, GMAIL)).status).toBe(404);
    expect(f.rig.logLines.some((l) => l.includes("event=connector_token_unreadable"))).toBe(true);
    f.sidecar.grant("prn_a", "gmail-tokenset", { accessToken: ACCESS, expiresAt: NOW - 1 });
    expect((await f.ask(key, GMAIL)).status).toBe(404);
    expect(f.rig.logLines.some((l) => l.includes("event=connector_token_expired"))).toBe(true);
  });

  it("maps the sidecar's failures: 503 for down, an error or a refused gateway credential; 403 for a purged principal", async () => {
    const f = fixture();
    const key = await f.key("prn_a");
    f.sidecar.down = true;
    let res = await f.ask(key, GMAIL);
    expect(res.status).toBe(503);
    expect(res.headers.get("retry-after")).toBe("5");
    f.sidecar.down = false;
    f.sidecar.forced = { status: 503, body: { error: "connector_unavailable" } };
    expect((await f.ask(key, GMAIL)).status).toBe(503);
    // The sidecar refusing the gateway's own credential is an operator fault: 503, never a 401.
    f.sidecar.forced = { status: 401, body: { error: "unauthorized" } };
    res = await f.ask(key, GMAIL);
    expect(res.status).toBe(503);
    expect(f.rig.logLines.some((l) => l.includes("event=connector_vault_rejected_gateway"))).toBe(true);
    f.sidecar.forced = { status: 410, body: { error: "principal_purged" } };
    res = await f.ask(key, GMAIL);
    expect(res.status).toBe(403);
    expect(await errorCode(res)).toBe("principal_unavailable");
    // A 200 the gateway cannot read is not a token.
    f.sidecar.forced = { status: 200, body: { access_token: 42 } };
    expect((await f.ask(key, GMAIL)).status).toBe(503);
  });

  it("does not ask a sidecar that is not the principal's shard, nor one with a wrong admin credential", async () => {
    const other = shardOf("prn_a", 2) === 0 ? 1 : 0;
    const f = fixture({ vault: (s) => sidecarConnectorVault({ relay: { url: SIDECAR_URL, bearerSecret: BEARER_SECRET, shard: { index: other, count: 2 } }, adminToken: ADMIN_TOKEN }, s.fetch) });
    const key = await f.key("prn_a");
    f.sidecar.grant("prn_a", "gmail-tokenset", { accessToken: ACCESS, expiresAt: NOW + 3_600_000 });
    expect((await f.ask(key, GMAIL)).status).toBe(503);
    expect(f.sidecar.calls).toEqual([]);
    expect(f.rig.logLines.some((l) => l.includes("event=connector_vault_not_provisioned"))).toBe(true);

    const g = fixture({ vault: (s) => sidecarConnectorVault({ relay: { url: SIDECAR_URL, bearerSecret: BEARER_SECRET }, adminToken: "not-the-admin-token-000" }, s.fetch) });
    const k2 = await g.key("prn_a");
    g.sidecar.grant("prn_a", "gmail-tokenset", { accessToken: ACCESS, expiresAt: NOW + 3_600_000 });
    expect((await g.ask(k2, GMAIL)).status).toBe(503);
  });
});

describe("the router's connector-token client against this route (in-process)", () => {
  function router(f: Fixture) {
    const vendorCalls: Array<{ url: string; authorization: string | null }> = [];
    const doFetch = (async (input: string | URL | Request, init?: RequestInit) => {
      const url = new URL(String(input));
      if (url.origin === "https://api.test") return await f.rig.app.request(url.pathname, init);
      vendorCalls.push({ url: url.toString(), authorization: new Headers(init?.headers).get("authorization") });
      return new Response(JSON.stringify({ emailAddress: "someone@example.test" }), { status: 200, headers: { "content-type": "application/json" } });
    }) as typeof fetch;
    const transport = createFetchTransport({
      env: { ROUTER_SETTLEMENT_TOKEN: TOKEN, ROUTER_WALLET_URL: "https://api.test", GOOGLE_OAUTH_ACCESS_TOKEN: "ya29.SHARED-must-not-be-used" },
      fetch: doFetch,
      connectors: { mode: "per_caller", baseUrl: "https://api.test" },
    });
    const provider: ProviderRef = { id: "gmail", auth: { kind: "caller_connector", slot: "gmail-tokenset", credential_env: "GOOGLE_OAUTH_ACCESS_TOKEN" }, baseUrl: "https://gmail.googleapis.test" };
    const call = (key: string) => transport.execute({ provider, request: { method: "GET", path: "/gmail/v1/users/me/profile" }, caller: { bearer: new Secret(key) }, runId: "run_42" });
    return { vendorCalls, call };
  }

  it("injects the calling principal's own token into the vendor request", async () => {
    const f = fixture();
    const key = await f.key("prn_a");
    // The router checks `expires_at` against its own (real) clock.
    f.sidecar.grant("prn_a", "gmail-tokenset", { accessToken: ACCESS, expiresAt: Math.max(Date.now(), NOW) + 3_600_000 });
    const r = router(f);
    const out = await r.call(key);
    expect(out.status).toBe(200);
    expect(r.vendorCalls).toEqual([{ url: "https://gmail.googleapis.test/gmail/v1/users/me/profile", authorization: `Bearer ${ACCESS}` }]);
    for (const line of f.rig.logLines) expect(line).not.toContain(ACCESS);
  });

  it("maps 404 to connector_not_connected and a key without run to caller_unauthorized, with no vendor call", async () => {
    const f = fixture();
    const r = router(f);
    const unconnected = await f.key("prn_b");
    await expect(r.call(unconnected)).rejects.toMatchObject({ code: "connector_not_connected" });
    const readOnly = await f.key("prn_c", ["read"]);
    const err = await r.call(readOnly).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(TransportError);
    expect((err as TransportError).code).toBe("caller_unauthorized");
    expect(r.vendorCalls).toEqual([]);
  });
});
