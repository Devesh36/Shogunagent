/** E3-T1: `sa_live_` / `sa_test_` keys — format, hashed storage, the `requireAgentKey` middleware,
 * and the one rule the logger must keep: a key never reaches a log line. */
import { Hono } from "hono";
import { describe, expect, it } from "vitest";

import {
  PREFIX_LENGTH, agentMeterSubject, generateAgentKey, hasScopes, hashAgentKey, parseAgentKey, requireAgentKey, resolveAgentKey, verifyAgentKey,
  type AgentKeyRecord, type AgentVars,
} from "../src/agent-keys.js";
import { InMemoryAgentStore } from "../src/agent-store.js";
import { meterSubjectOf } from "../src/auth.js";
import { logId, redactingLogger } from "../src/logging.js";
import { TEST_KEY_ITERATIONS, authed, makeAgentRig, registerAgent } from "./agent-helpers.js";

async function storedKey(store: InMemoryAgentStore, overrides: Partial<AgentKeyRecord> = {}): Promise<{ key: string; record: AgentKeyRecord }> {
  const { key, prefix } = generateAgentKey(overrides.mode ?? "live");
  const record: AgentKeyRecord = {
    keyId: "ak_1", prefix, hash: await hashAgentKey(key, TEST_KEY_ITERATIONS), mode: "live", principalId: "prn_1", tenantId: "prn_1",
    scopes: ["read", "write"], createdAt: 1_000, ...overrides,
  };
  await store.insertKey(record);
  return { key, record };
}

describe("agent key format and hashing", () => {
  it("mints sa_live_ / sa_test_ keys with 43 base64url characters and a short lookup prefix", () => {
    const live = generateAgentKey("live");
    const test = generateAgentKey("test");
    expect(live.key).toMatch(/^sa_live_[A-Za-z0-9_-]{43}$/);
    expect(test.key).toMatch(/^sa_test_[A-Za-z0-9_-]{43}$/);
    expect(live.prefix).toHaveLength(PREFIX_LENGTH);
    expect(live.key.endsWith(live.prefix)).toBe(false);
    expect(parseAgentKey(live.key)).toEqual({ mode: "live", prefix: live.prefix });
    expect(parseAgentKey(test.key)?.mode).toBe("test");
    expect(parseAgentKey("sa_live_short")).toBeUndefined();
    expect(parseAgentKey(`sk_live_${"a".repeat(43)}`)).toBeUndefined();
    expect(parseAgentKey(`${live.key}=`)).toBeUndefined();
  });

  it("stores a self-describing PBKDF2 hash that verifies the key and nothing else", async () => {
    const { key } = generateAgentKey("live");
    const stored = await hashAgentKey(key, TEST_KEY_ITERATIONS);
    expect(stored.startsWith(`pbkdf2-sha256$${TEST_KEY_ITERATIONS}$`)).toBe(true);
    expect(stored).not.toContain(key.slice(8));
    expect(await verifyAgentKey(key, stored)).toBe(true);
    expect(await verifyAgentKey(generateAgentKey("live").key, stored)).toBe(false);
    expect(await verifyAgentKey(key, "argon2id$whatever")).toBe(false);
    expect(await verifyAgentKey(key, "pbkdf2-sha256$999999999$AA$AA")).toBe(false);
    // Two hashes of the same key differ (fresh salt each time) and both verify.
    const again = await hashAgentKey(key, TEST_KEY_ITERATIONS);
    expect(again).not.toBe(stored);
    expect(await verifyAgentKey(key, again)).toBe(true);
  });

  it("admin implies every scope", () => {
    expect(hasScopes(["admin"], ["read", "write", "run"])).toBe(true);
    expect(hasScopes(["read"], ["write"])).toBe(false);
    expect(hasScopes(["read", "write"], [])).toBe(true);
  });

  it("meters an agent principal in the same subject space as a licence (E3-T4)", () => {
    expect(agentMeterSubject({ principalId: "prn_x" })).toBe("principal:prn_x");
    expect(meterSubjectOf({ principalId: "prn_x" })).toBe("principal:prn_x");
    expect(meterSubjectOf({ licenseId: "lic_1", deviceId: "d", plan: "standard" })).toBe("lic_1");
    expect(meterSubjectOf({ licenseId: "trial_d", deviceId: "d", plan: "trial", accountId: "acc_1" })).toBe("account:acc_1");
  });
});

describe("resolveAgentKey", () => {
  it("finds the key by prefix, verifies the hash and refuses revoked or expired keys", async () => {
    const store = new InMemoryAgentStore();
    const { key } = await storedKey(store);
    const ok = await resolveAgentKey(`Bearer ${key}`, store, 2_000);
    expect(ok.ok && ok.principal.keyId).toBe("ak_1");
    expect((await resolveAgentKey(undefined, store, 2_000)).ok).toBe(false);
    expect((await resolveAgentKey("Bearer sa_live_nope", store, 2_000))).toEqual({ ok: false, code: "invalid_key" });
    const other = generateAgentKey("live").key;
    expect(await resolveAgentKey(`Bearer ${other}`, store, 2_000)).toEqual({ ok: false, code: "invalid_key" });
    await store.revokeKey("ak_1", 3_000);
    expect(await resolveAgentKey(`Bearer ${key}`, store, 2_000)).toEqual(expect.objectContaining({ ok: true }));
    expect(await resolveAgentKey(`Bearer ${key}`, store, 3_000)).toEqual({ ok: false, code: "key_revoked" });
    const expiring = new InMemoryAgentStore();
    const { key: k2 } = await storedKey(expiring, { keyId: "ak_2", expiresAt: 5_000 });
    expect(await resolveAgentKey(`Bearer ${k2}`, expiring, 5_000)).toEqual({ ok: false, code: "key_expired" });
    expect((await resolveAgentKey(`Bearer ${k2}`, expiring, 4_999)).ok).toBe(true);
  });

  it("a test-mode key does not log in as a live key of the same prefix", async () => {
    const store = new InMemoryAgentStore();
    const { key } = await storedKey(store);
    const asTest = key.replace("sa_live_", "sa_test_");
    expect(await resolveAgentKey(`Bearer ${asTest}`, store, 2_000)).toEqual({ ok: false, code: "invalid_key" });
  });
});

describe("requireAgentKey middleware", () => {
  async function appWith(scopes: Parameters<typeof requireAgentKey>[1]) {
    const store = new InMemoryAgentStore();
    const { key } = await storedKey(store);
    const lines: string[] = [];
    const app = new Hono<{ Variables: AgentVars }>();
    app.use("*", redactingLogger((l) => lines.push(l)));
    app.get("/v1/wallet", requireAgentKey(store, scopes, () => new Date(2_000)), (c) => c.json({ principal: c.get("agentPrincipal")?.principalId }));
    return { app, key, lines };
  }

  it("attaches the principal, answers 401 without a key and 403 without the scope", async () => {
    const { app, key } = await appWith(["read"]);
    const ok = await app.request("/v1/wallet", { headers: { authorization: `Bearer ${key}` } });
    expect(ok.status).toBe(200);
    expect(await ok.json()).toEqual({ principal: "prn_1" });
    const none = await app.request("/v1/wallet");
    expect(none.status).toBe(401);
    expect(none.headers.get("www-authenticate")).toContain("Bearer");
    expect(((await none.json()) as { error: { code: string } }).error.code).toBe("missing_key");
    const { app: admin, key: readWriteKey } = await appWith(["admin"]);
    const forbidden = await admin.request("/v1/wallet", { headers: { authorization: `Bearer ${readWriteKey}` } });
    expect(forbidden.status).toBe(403);
    expect(((await forbidden.json()) as { error: { code: string } }).error.code).toBe("insufficient_scope");
  });

  it("never puts the key in the log line, only a hash of the opaque key id", async () => {
    const { app, key, lines } = await appWith(["read"]);
    await app.request("/v1/wallet", { headers: { authorization: `Bearer ${key}` } });
    await app.request("/v1/wallet", { headers: { authorization: `Bearer sa_live_${"Z".repeat(43)}` } });
    const all = lines.join("\n");
    expect(all).toContain(`GET /v1/wallet 200`);
    expect(all).toContain(`key=${logId("ak_1")}`);
    expect(all).not.toContain("ak_1 ");
    expect(all).not.toContain(key.slice(8, 20));
    expect(all).not.toContain("Z".repeat(10));
  });
});

describe("agent routes never log the key", () => {
  it("registration and every authenticated route log the hashed key id only", async () => {
    const rig = makeAgentRig();
    const reg = await registerAgent(rig);
    await authed(rig, reg.key, "/v1/wallet");
    await authed(rig, reg.key, "/v1/agents/me");
    await authed(rig, "sa_live_not-a-key", "/v1/wallet");
    const all = rig.logLines.join("\n");
    expect(all).toContain("POST /v1/agents 201");
    expect(all).toContain("GET /v1/wallet 200");
    expect(all).toContain("GET /v1/wallet 401");
    expect(all).toContain(`key=${logId(reg.keyId)}`);
    expect(all).not.toContain(reg.key.slice(8));
    expect(all).not.toContain(reg.keyId);
    expect(all).not.toContain(reg.principalId);
    expect(all).not.toContain("not-a-key");
  });
});
