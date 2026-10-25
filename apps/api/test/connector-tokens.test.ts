/**
 * E48: `POST /v1/internal/router/connector-token` (connector-tokens.ts, contract
 * docs/api/connector-tokens.md), and the router's real client (apps/router/src/transport.ts)
 * against it in-process. What is proved:
 *
 * - the envelope is the Rust `KmsSealer` format: bound to (principal, slot) at the KMS and in the
 *   AEAD, so a row moved to another principal or slot, or altered, does not open;
 * - the route answers only the router's service token, resolves the principal from the caller's key
 *   (never from the body), needs the `run` scope, rejects unknown fields and foreign slots;
 * - 404 `connector_not_connected` for no row, a row that does not open, an expired grant, and a
 *   runtime without a KMS key wrapper; 503 when the store or the KMS does not answer;
 * - an expiring token is refreshed and written back sealed; the refresh token never leaves;
 * - the router injects the principal's token into the vendor request and nothing else, and maps
 *   404 / 401 / 403 to its own errors;
 * - no token and no key reaches a log line.
 */
import { createHash } from "node:crypto";

import { describe, expect, it } from "vitest";

import type { ProviderRef } from "../../router/src/transport.js";
import { createFetchTransport, TransportError } from "../../router/src/transport.js";
import { Secret } from "../../router/src/secret.js";
import { generateAgentKey, hashAgentKey, type AgentScope } from "../src/agent-keys.js";
import {
  CONNECTOR_TOKEN_PATH, InMemoryConnectorRows, KmsSealer, KmsUnavailable, RowUnreadable, SqlConnectorRows, parseTokenset, serializeTokenset, tokenContext,
  type ConnectorRefresher, type KeyWrapper, type RefreshOutcome, type StoredTokenset, type WrappedDataKey,
} from "../src/connector-tokens.js";
import { TEST_KEY_ITERATIONS, makeAgentRig, type AgentRig } from "./agent-helpers.js";
import { sqliteDouble } from "./sql-double.js";
import { readFileSync } from "node:fs";

const TOKEN = "rst_router_wallet_token_0123456789abcdef";
const ACCESS = "ya29.SENTINEL-access-token-4711";
const REFRESH = "1//SENTINEL-refresh-token-0815";
const NOW = Date.parse("2026-10-25T12:00:00Z");

/** The Rust `kms::fake::FakeWrapper`, byte for byte: the data key XOR a pad derived from the key
 * version and the context, the version in the first four bytes. Not encryption; tests only. */
class FakeWrapper implements KeyWrapper {
  readonly provider = "fake";
  primary = 1;
  down = false;
  calls = 0;

  private static pad(version: number, context: string): Buffer {
    const v = Buffer.alloc(4);
    v.writeUInt32BE(version);
    return createHash("sha256").update("fake-kms-pad").update(v).update(context).digest();
  }

  private enter(): void {
    this.calls += 1;
    if (this.down) throw new KmsUnavailable();
  }

  generate(context: string): Promise<{ key: Uint8Array; wrapped: WrappedDataKey }> {
    this.enter();
    const key = new Uint8Array(32).map((_, i) => (i * 7 + 3) & 0xff);
    const pad = FakeWrapper.pad(this.primary, context);
    const blob = Buffer.alloc(36);
    blob.writeUInt32BE(this.primary);
    for (let i = 0; i < 32; i++) blob[4 + i] = (key[i] ?? 0) ^ (pad[i] ?? 0);
    return Promise.resolve({ key, wrapped: { keyVersion: `fake/v${this.primary}`, blob: new Uint8Array(blob) } });
  }

  unwrap(wrapped: WrappedDataKey, context: string): Promise<Uint8Array> {
    this.enter();
    if (wrapped.blob.length !== 36) return Promise.reject(new Error("rejected"));
    const blob = Buffer.from(wrapped.blob);
    const pad = FakeWrapper.pad(blob.readUInt32BE(0), context);
    return Promise.resolve(new Uint8Array(32).map((_, i) => (blob[4 + i] ?? 0) ^ (pad[i] ?? 0)));
  }
}

function tokenset(overrides: Partial<StoredTokenset> = {}): StoredTokenset {
  return { accessToken: ACCESS, refreshToken: REFRESH, expiresAtMs: NOW + 3_600_000, accountLabel: "someone@example.test", ...overrides };
}

class ScriptedRefresher implements ConnectorRefresher {
  calls: Array<{ slot: string; refreshToken: string | null }> = [];
  constructor(public next: RefreshOutcome) {}
  refresh(slot: string, current: StoredTokenset): Promise<RefreshOutcome> {
    this.calls.push({ slot, refreshToken: current.refreshToken });
    return Promise.resolve(this.next);
  }
}

interface Fixture {
  rig: AgentRig;
  rows: InMemoryConnectorRows;
  wrapper: FakeWrapper;
  sealer: KmsSealer;
  key(principalId: string, scopes?: AgentScope[]): Promise<string>;
  store(principalId: string, slot: string, t: StoredTokenset): Promise<void>;
  ask(key: string | undefined, body: unknown, service?: string): Promise<Response> | Response;
}

function fixture(opts: { refresher?: ConnectorRefresher; vault?: boolean } = {}): Fixture {
  const rows = new InMemoryConnectorRows();
  const wrapper = new FakeWrapper();
  const sealer = new KmsSealer(wrapper);
  const rig = makeAgentRig({
    routerWallet: { token: TOKEN },
    ...(opts.vault === false ? {} : { connectorTokens: { rows, sealer, ...(opts.refresher ? { refresher: opts.refresher } : {}) } }),
  });
  rig.setNow(new Date(NOW));
  return {
    rig, rows, wrapper, sealer,
    async key(principalId, scopes = ["read", "write", "run"]) {
      const { key, prefix } = generateAgentKey("live");
      if (!(await rig.store.principal(principalId))) await rig.store.createPrincipal({ principalId, tenantId: principalId, createdAt: NOW });
      await rig.store.insertKey({ keyId: `ak_${principalId}_${scopes.join("")}`, prefix, hash: await hashAgentKey(key, TEST_KEY_ITERATIONS), mode: "live", principalId, tenantId: principalId, scopes, createdAt: NOW });
      return key;
    },
    async store(principalId, slot, t) {
      await rows.upsert(principalId, slot, await sealer.seal(principalId, slot, serializeTokenset(t)), NOW);
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

describe("KmsSealer (the Rust KmsSealer row format)", () => {
  it("round-trips a value and lays the row out as [1][u16 len][wrapped][nonce][ct‖tag]", async () => {
    const sealer = new KmsSealer(new FakeWrapper());
    const row = await sealer.seal("prn_a", "gmail-tokenset", "hello");
    expect(row.keyId).toBe("fake/v1");
    expect(row.ciphertext[0]).toBe(1);
    expect(Buffer.from(row.ciphertext).readUInt16BE(1)).toBe(36);
    expect(row.ciphertext.length).toBe(3 + 36 + 12 + 5 + 16);
    expect(await sealer.open("prn_a", "gmail-tokenset", row)).toBe("hello");
    expect(tokenContext("prn_a", "gmail-tokenset")).toBe("shogun/token/v1/prn_a/gmail-tokenset");
  });

  it("does not open a row moved to another principal or slot, or altered", async () => {
    const sealer = new KmsSealer(new FakeWrapper());
    const row = await sealer.seal("prn_a", "gmail-tokenset", "hello");
    await expect(sealer.open("prn_b", "gmail-tokenset", row)).rejects.toBeInstanceOf(RowUnreadable);
    await expect(sealer.open("prn_a", "slack-tokenset", row)).rejects.toBeInstanceOf(RowUnreadable);
    const altered = new Uint8Array(row.ciphertext);
    altered[altered.length - 1] = (altered[altered.length - 1] ?? 0) ^ 1;
    await expect(sealer.open("prn_a", "gmail-tokenset", { ...row, ciphertext: altered })).rejects.toBeInstanceOf(RowUnreadable);
    await expect(sealer.open("prn_a", "gmail-tokenset", { ...row, ciphertext: new Uint8Array([2, 0, 0]) })).rejects.toBeInstanceOf(RowUnreadable);
  });

  it("reports an unreachable KMS as KmsUnavailable, not as an unreadable row", async () => {
    const wrapper = new FakeWrapper();
    const sealer = new KmsSealer(wrapper);
    const row = await sealer.seal("prn_a", "gmail-tokenset", "hello");
    wrapper.down = true;
    await expect(sealer.open("prn_a", "gmail-tokenset", row)).rejects.toBeInstanceOf(KmsUnavailable);
  });

  it("reads the tokenset JSON the Rust token store writes", () => {
    expect(parseTokenset('{"access_token":"secret","refresh_token":null,"expires_at_ms":123}')).toEqual({ accessToken: "secret", refreshToken: null, expiresAtMs: 123, accountLabel: null });
    expect(parseTokenset('{"refresh_token":"r","expires_at_ms":1}')).toBeUndefined();
    expect(parseTokenset("not json")).toBeUndefined();
    const t = tokenset();
    expect(parseTokenset(serializeTokenset(t))).toEqual(t);
  });

  it("stores rows in connector_tokens over SQL (the real sql/connector-tokens.sql)", async () => {
    const db = sqliteDouble(readFileSync(new URL("../sql/connector-tokens.sql", import.meta.url), "utf8"));
    const rows = new SqlConnectorRows(db);
    await rows.checkSchema();
    const sealer = new KmsSealer(new FakeWrapper());
    expect(await rows.fetch("prn_a", "gmail-tokenset")).toBeUndefined();
    await rows.upsert("prn_a", "gmail-tokenset", await sealer.seal("prn_a", "gmail-tokenset", "v1"), NOW);
    await rows.upsert("prn_a", "gmail-tokenset", await sealer.seal("prn_a", "gmail-tokenset", "v2"), NOW + 1);
    const row = await rows.fetch("prn_a", "gmail-tokenset");
    expect(row && (await sealer.open("prn_a", "gmail-tokenset", row))).toBe("v2");
    expect(await rows.fetch("prn_b", "gmail-tokenset")).toBeUndefined();
  });
});

describe("POST /v1/internal/router/connector-token", () => {
  it("returns the caller's own access token, no-store, and nothing else", async () => {
    const f = fixture();
    const key = await f.key("prn_a");
    await f.store("prn_a", "gmail-tokenset", tokenset());
    const res = await f.ask(key, GMAIL);
    expect(res.status).toBe(200);
    expect(res.headers.get("cache-control")).toBe("no-store");
    expect(await res.json()).toEqual({ access_token: ACCESS, token_type: "Bearer", expires_at: NOW + 3_600_000 });
    expect(f.rig.logLines.some((l) => l.includes("event=connector_token_served"))).toBe(true);
    // Another principal asking for the same slot gets its own (absent) row, never prn_a's.
    const other = await f.key("prn_b");
    const res2 = await f.ask(other, GMAIL);
    expect(res2.status).toBe(404);
    expect(await res2.json()).toEqual({ error: { code: "connector_not_connected", message: expect.any(String), next_action: "authenticate" } });
    for (const line of f.rig.logLines) {
      expect(line).not.toContain(ACCESS);
      expect(line).not.toContain(REFRESH);
      expect(line).not.toContain(key);
      expect(line).not.toContain("prn_a");
    }
  });

  it("answers only the router's service token, before reading the body", async () => {
    const f = fixture();
    const key = await f.key("prn_a");
    const res = await f.ask(key, "{not json", "rst_wrong_token_0123456789abcdef0123");
    expect(res.status).toBe(401);
    expect(((await res.json()) as { error: { code: string } }).error.code).toBe("invalid_token");
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
  });

  it("verifies the caller's key and its run scope", async () => {
    const f = fixture();
    await f.store("prn_a", "gmail-tokenset", tokenset());
    expect((await f.ask(undefined, GMAIL)).status).toBe(401);
    expect((await f.ask("sa_live_notakey", GMAIL)).status).toBe(401);
    const readOnly = await f.key("prn_a", ["read"]);
    const res = await f.ask(readOnly, GMAIL);
    expect(res.status).toBe(403);
    expect(((await res.json()) as { error: { code: string } }).error.code).toBe("insufficient_scope");
  });

  it("answers 404 for a row that does not open, and for a runtime with no KMS key wrapper", async () => {
    const f = fixture();
    const key = await f.key("prn_a");
    // A row copied from another principal: its binding fails.
    await f.rows.upsert("prn_a", "gmail-tokenset", await f.sealer.seal("prn_x", "gmail-tokenset", serializeTokenset(tokenset())), NOW);
    expect((await f.ask(key, GMAIL)).status).toBe(404);
    expect(f.rig.logLines.some((l) => l.includes("event=connector_token_unreadable"))).toBe(true);
    // A row that opens but is not a tokenset.
    await f.rows.upsert("prn_a", "gmail-tokenset", await f.sealer.seal("prn_a", "gmail-tokenset", "garbage"), NOW);
    expect((await f.ask(key, GMAIL)).status).toBe(404);
    // A token the router would refuse to send is not handed out.
    await f.store("prn_a", "gmail-tokenset", tokenset({ accessToken: "short" }));
    expect((await f.ask(key, GMAIL)).status).toBe(404);

    const bare = fixture({ vault: false });
    const k2 = await bare.key("prn_a");
    const res = await bare.ask(k2, GMAIL);
    expect(res.status).toBe(404);
    expect(bare.rig.logLines.some((l) => l.includes("event=connector_vault_unconfigured"))).toBe(true);
  });

  it("answers 503 when the KMS or the store does not answer", async () => {
    const f = fixture();
    const key = await f.key("prn_a");
    await f.store("prn_a", "gmail-tokenset", tokenset());
    f.wrapper.down = true;
    const res = await f.ask(key, GMAIL);
    expect(res.status).toBe(503);
    expect(res.headers.get("retry-after")).toBe("5");
    f.wrapper.down = false;
    f.rows.fetch = () => Promise.reject(new Error("down"));
    expect((await f.ask(key, GMAIL)).status).toBe(503);
  });

  it("without a refresher: hands out a still-valid token as is, refuses an expired one", async () => {
    const f = fixture();
    const key = await f.key("prn_a");
    await f.store("prn_a", "gmail-tokenset", tokenset({ expiresAtMs: NOW + 30_000 }));
    expect(await (await f.ask(key, GMAIL)).json()).toEqual({ access_token: ACCESS, token_type: "Bearer", expires_at: NOW + 30_000 });
    await f.store("prn_a", "gmail-tokenset", tokenset({ expiresAtMs: NOW - 1 }));
    expect((await f.ask(key, GMAIL)).status).toBe(404);
    expect(f.rig.logLines.some((l) => l.includes("event=connector_token_expired"))).toBe(true);
  });

  it("refreshes a token expiring within 60 s and writes the new tokenset back, sealed", async () => {
    const refresher = new ScriptedRefresher({ kind: "refreshed", tokenset: { accessToken: "ya29.fresh-access-token", refreshToken: null, expiresAtMs: NOW + 3_600_000, accountLabel: null } });
    const f = fixture({ refresher });
    const key = await f.key("prn_a");
    await f.store("prn_a", "gmail-tokenset", tokenset({ expiresAtMs: NOW + 59_000 }));
    const res = await f.ask(key, GMAIL);
    expect(await res.json()).toEqual({ access_token: "ya29.fresh-access-token", token_type: "Bearer", expires_at: NOW + 3_600_000 });
    expect(refresher.calls).toEqual([{ slot: "gmail-tokenset", refreshToken: REFRESH }]);
    const row = await f.rows.fetch("prn_a", "gmail-tokenset");
    // The refresh response omitted the refresh token: the stored one is kept, the label too.
    expect(row && parseTokenset(await f.sealer.open("prn_a", "gmail-tokenset", row))).toEqual({ accessToken: "ya29.fresh-access-token", refreshToken: REFRESH, expiresAtMs: NOW + 3_600_000, accountLabel: "someone@example.test" });
    expect(f.rig.logLines.some((l) => l.includes("event=connector_token_refreshed"))).toBe(true);
    // A valid token is not refreshed.
    expect((await f.ask(key, GMAIL)).status).toBe(200);
    expect(refresher.calls).toHaveLength(1);
  });

  it("a revoked grant is 404; a refresh that fails keeps a still-valid token and is 503 for an expired one", async () => {
    const refresher = new ScriptedRefresher({ kind: "revoked" });
    const f = fixture({ refresher });
    const key = await f.key("prn_a");
    await f.store("prn_a", "gmail-tokenset", tokenset({ expiresAtMs: NOW + 10_000 }));
    expect((await f.ask(key, GMAIL)).status).toBe(404);
    refresher.next = { kind: "unavailable" };
    expect(await (await f.ask(key, GMAIL)).json()).toEqual({ access_token: ACCESS, token_type: "Bearer", expires_at: NOW + 10_000 });
    await f.store("prn_a", "gmail-tokenset", tokenset({ expiresAtMs: NOW - 1 }));
    expect((await f.ask(key, GMAIL)).status).toBe(503);
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
    await f.store("prn_a", "gmail-tokenset", tokenset({ expiresAtMs: Math.max(Date.now(), NOW) + 3_600_000 }));
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
