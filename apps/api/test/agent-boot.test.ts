/** The agent API's boot settings (boot.ts `agentConfigFromEnv`) and its wiring through
 * `createRelay` into the relay app: off by default, names-only refusals, and the mounted routes
 * sharing the relay's logger and the licence ledger. */
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

import { BootError, agentConfigFromEnv, agentLaneLine, createRelay, postgresStore, relayConfigFromEnv } from "../src/boot.js";
import { DEFAULT_CAPS } from "../src/caps.js";
import { DEFAULT_POW_BITS, solvePow } from "../src/pow.js";
import { NoRateLimit } from "../src/ratelimit.js";
import { InMemoryUsageStore } from "../src/usage.js";
import { InMemoryAgentStore } from "../src/agent-store.js";
import releaseKey from "../../../packages/config/license-public-key.json";
import { FakeGateway } from "./helpers.js";
import { AGENT_SQL } from "./agent-store.test.js";
import { sqliteDouble, SHARED_STORE_SQL } from "./sql-double.js";

const RELEASE_PUBKEY = (releaseKey as { ed25519: string }).ed25519;

const relayEnv = {
  ANTHROPIC_API_KEY: "sk-ant-test-not-a-real-key",
  LICENSE_PUBKEY_B64: RELEASE_PUBKEY,
  DAILY_SPEND_CAP_USD: "10",
  BATCH_PROVIDER: "anthropic",
  OPENAI_API_KEY: "sk-test-not-a-real-key",
};

const agentEnv = {
  AGENT_API_ENABLED: "1",
  AGENT_POW_SECRET: "a-sixteen-char-secret!",
  PUBLIC_BASE_URL: "https://api.example.test",
};

function refusal(env: Record<string, string | undefined>): string {
  try {
    agentConfigFromEnv(env);
  } catch (e) {
    expect(e).toBeInstanceOf(BootError);
    return (e as Error).message;
  }
  throw new Error("expected a refusal");
}

describe("agentConfigFromEnv", () => {
  it("is off unless AGENT_API_ENABLED=1, then needs the secret and the public origin", () => {
    expect(agentConfigFromEnv({})).toBeUndefined();
    expect(agentConfigFromEnv({ AGENT_API_ENABLED: "0", AGENT_POW_SECRET: "x" })).toBeUndefined();
    expect(refusal({ AGENT_API_ENABLED: "1" })).toContain("AGENT_POW_SECRET");
    expect(refusal({ AGENT_API_ENABLED: "1", AGENT_POW_SECRET: "short" })).toContain("AGENT_POW_SECRET");
    expect(refusal({ ...agentEnv, PUBLIC_BASE_URL: "api.example.test" })).toContain("PUBLIC_BASE_URL");
    expect(refusal({ ...agentEnv, PUBLIC_BASE_URL: "https://api.example.test/v1" })).toContain("PUBLIC_BASE_URL");
    // Refusals name variables, never values.
    expect(refusal({ ...agentEnv, PUBLIC_BASE_URL: "not a url" })).not.toContain("not a url");
  });

  it("applies the defaults and parses the optional settings", () => {
    const config = agentConfigFromEnv(agentEnv)!;
    expect(config).toEqual({
      powSecret: agentEnv.AGENT_POW_SECRET, powBits: DEFAULT_POW_BITS, publicBaseUrl: agentEnv.PUBLIC_BASE_URL,
      registerPerHour: 5, keyIterations: 20_000, caps: DEFAULT_CAPS,
    });
    const tuned = agentConfigFromEnv({
      ...agentEnv, AGENT_POW_BITS: "12", AGENT_REGISTER_PER_HOUR: "2", AGENT_KEY_PBKDF2_ITERATIONS: "50000",
      AGENT_KEY_CAP_DAY_USD: "0.25", AGENT_KEY_CAP_MONTH_USD: "3", AGENT_PRINCIPAL_CAP_DAY_USD: "2.5", AGENT_PRINCIPAL_CAP_MONTH_USD: "40",
      STRIPE_SECRET_KEY: "sk_test_x", STRIPE_WEBHOOK_SECRET: "whsec_x",
    })!;
    expect(tuned.powBits).toBe(12);
    expect(tuned.registerPerHour).toBe(2);
    expect(tuned.keyIterations).toBe(50_000);
    expect(tuned.caps).toEqual({ key: { day: 250_000, month: 3_000_000 }, principal: { day: 2_500_000, month: 40_000_000 } });
    expect(tuned.stripe).toEqual({ secretKey: "sk_test_x", webhookSecret: "whsec_x" });
    expect(refusal({ ...agentEnv, AGENT_POW_BITS: "33" })).toBe("invalid AGENT_POW_BITS");
    expect(refusal({ ...agentEnv, AGENT_KEY_PBKDF2_ITERATIONS: "100001" })).toBe("invalid AGENT_KEY_PBKDF2_ITERATIONS");
    expect(refusal({ ...agentEnv, AGENT_KEY_CAP_DAY_USD: "1 dollar" })).toBe("invalid AGENT_KEY_CAP_DAY_USD");
    expect(refusal({ ...agentEnv, STRIPE_SECRET_KEY: "sk_test_x" })).toContain("STRIPE_WEBHOOK_SECRET");
  });

  it("rides along in relayConfigFromEnv and the boot-log line says which way the lane went", () => {
    const off = relayConfigFromEnv(relayEnv);
    expect(off.agent).toBeUndefined();
    expect(agentLaneLine(off)).toBe("agent_api closed (AGENT_API_ENABLED unset)");
    const on = relayConfigFromEnv({ ...relayEnv, ...agentEnv });
    expect(on.agent?.publicBaseUrl).toBe(agentEnv.PUBLIC_BASE_URL);
    expect(agentLaneLine(on)).toContain("agent_api open");
    expect(agentLaneLine(on)).toContain("top-ups off");
  });
});

describe("createRelay with the agent API", () => {
  const relayConfig = () => relayConfigFromEnv({ ...relayEnv, ...agentEnv, AGENT_POW_BITS: "8", AGENT_KEY_PBKDF2_ITERATIONS: "1000" });

  it("does not serve the agent routes when the lane is closed", async () => {
    const relay = createRelay(relayConfigFromEnv(relayEnv), { usage: new InMemoryUsageStore(), rateLimit: new NoRateLimit(), agent: new InMemoryAgentStore(), close: () => Promise.resolve() }, { gateway: new FakeGateway(), log: () => undefined });
    expect((await relay.app.request("/v1/agents/challenge")).status).toBe(404);
    expect((await relay.app.request("/.well-known/pricing.json")).status).toBe(404);
  });

  it("serves them behind the relay's own logger, on the Postgres-shaped store, and /mcp waits for the core", async () => {
    const lines: string[] = [];
    const db = sqliteDouble(AGENT_SQL);
    const store = postgresStore(db, relayConfig(), (l) => lines.push(l));
    await store.agent.checkSchema();
    const relay = createRelay(relayConfig(), store, { gateway: new FakeGateway(), log: (l) => lines.push(l) });
    expect((await relay.app.request("/.well-known/pricing.json")).status).toBe(200);
    const challenge = ((await (await relay.app.request("/v1/agents/challenge")).json()) as { challenge: string }).challenge;
    const nonce = solvePow(challenge, 8)!;
    const res = await relay.app.request("/v1/agents", { method: "POST", headers: { "cf-connecting-ip": "203.0.113.77", "x-agent-pow": `${challenge}:${nonce}` } });
    expect(res.status).toBe(201);
    const { key } = (await res.json()) as { key: string };
    // The licence routes are untouched (E3-T4): an agent key is not a licence token.
    expect((await relay.app.request("/v1/batch", { method: "POST", headers: { authorization: `Bearer ${key}` } })).status).toBe(401);
    // The metered proxy exists but has no core behind it yet: 503 before anything is reserved.
    const mcp = await relay.app.request("/mcp", { method: "POST", headers: { authorization: `Bearer ${key}`, "mcp-method": "tools/call", "mcp-name": "memory.ingest" }, body: "{}" });
    expect(mcp.status).toBe(503);
    expect(((await mcp.json()) as { error: { code: string } }).error.code).toBe("upstream_unavailable");
    const wallet = await relay.app.request("/v1/wallet", { headers: { authorization: `Bearer ${key}` } });
    expect(((await wallet.json()) as { reserved_micro_usd: number; balance_micro_usd: number }).reserved_micro_usd).toBe(0);
    expect(lines.some((l) => l.startsWith("POST /v1/agents 201") && l.includes("key="))).toBe(true);
    expect(lines.some((l) => l.startsWith("POST /mcp 503") && l.includes("event=upstream_unavailable"))).toBe(true);
    expect(lines.join("\n")).not.toContain(key.slice(8));
    // The sweep runs the agent store's maintenance without complaint.
    await relay.sweep();
    expect(lines.filter((l) => l.includes("agent maintenance unavailable"))).toHaveLength(0);
    expect(SHARED_STORE_SQL.length).toBeGreaterThan(0);
  });
});

describe("E19 / E3-T2 settings", () => {
  it("parses the overage price and the OAuth secret, refusing by name", () => {
    const on = agentConfigFromEnv({ ...agentEnv, STRIPE_SECRET_KEY: "sk_test_x", STRIPE_WEBHOOK_SECRET: "whsec_x", STRIPE_COMMIT_OVERAGE_PRICE_ID: "price_123", AGENT_OAUTH_SECRET: "o".repeat(32) })!;
    expect(on.stripe).toEqual({ secretKey: "sk_test_x", webhookSecret: "whsec_x", overagePriceId: "price_123" });
    expect(on.oauthSecret).toBe("o".repeat(32));
    expect(agentLaneLine({ agent: on })).toContain("oauth on");
    expect(refusal({ ...agentEnv, STRIPE_COMMIT_OVERAGE_PRICE_ID: "price_123" })).toBe("STRIPE_COMMIT_OVERAGE_PRICE_ID needs STRIPE_SECRET_KEY");
    expect(refusal({ ...agentEnv, STRIPE_SECRET_KEY: "sk_test_x", STRIPE_WEBHOOK_SECRET: "whsec_x", STRIPE_COMMIT_OVERAGE_PRICE_ID: "nope" })).toContain("STRIPE_COMMIT_OVERAGE_PRICE_ID");
    expect(refusal({ ...agentEnv, AGENT_OAUTH_SECRET: "short" })).toBe("invalid AGENT_OAUTH_SECRET (at least 32 characters)");
  });

  it("serves OAuth on the Postgres-shaped store when the secret is set", async () => {
    const config = relayConfigFromEnv({ ...relayEnv, ...agentEnv, AGENT_OAUTH_SECRET: "o".repeat(32) });
    const db = sqliteDouble(AGENT_SQL + "\n" + readFileSync(new URL("../sql/agent-oauth.sql", import.meta.url), "utf8"));
    const store = postgresStore(db, config, () => undefined);
    await store.agent.checkSchema();
    await store.oauth.checkSchema();
    const relay = createRelay(config, store, { gateway: new FakeGateway(), log: () => undefined });
    const as = (await (await relay.app.request("/.well-known/oauth-authorization-server")).json()) as { issuer: string };
    expect(as.issuer).toBe(agentEnv.PUBLIC_BASE_URL);
    await relay.sweep();
    const closed = createRelay(relayConfigFromEnv({ ...relayEnv, ...agentEnv }), store, { gateway: new FakeGateway(), log: () => undefined });
    expect((await closed.app.request("/.well-known/oauth-authorization-server")).status).toBe(404);
  });
});
