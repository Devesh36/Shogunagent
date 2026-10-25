/** The shared bootstrap (boot.ts) both entries use: settings parsed from an env record with
 * names-only refusals, the 2026-10-12 budget defaults (docs/unit-economics.md §3.3), and the
 * wiring of a config + store into the app and the settlement pass. */
import { describe, expect, it } from "vitest";

import { AuthError, licensePublicKeyFromB64, rawPublicKeyBytes } from "../src/auth.js";
import {
  BootError,
  DEFAULT_LICENSE_DAILY_SPEND_CAP_USD,
  DEFAULT_TRIAL_DAILY_SPEND_CAP_USD,
  createRelay,
  postgresStore,
  relayConfigFromEnv,
  trialLaneLine,
  type RelayConfig,
} from "../src/boot.js";
import { NoRateLimit } from "../src/ratelimit.js";
import { InMemoryUsageStore, utcDate } from "../src/usage.js";
import releaseKey from "../../../packages/config/license-public-key.json";
import { FakeGateway, makeKeys, sampleBody, signLicense } from "./helpers.js";
import { sqliteDouble } from "./sql-double.js";

const RELEASE_PUBKEY = (releaseKey as { ed25519: string }).ed25519;

/** The smallest production-shaped environment: the two required values plus the breaker. */
const minimal = {
  ANTHROPIC_API_KEY: "sk-ant-test-not-a-real-key",
  LICENSE_PUBKEY_B64: RELEASE_PUBKEY,
  DAILY_SPEND_CAP_USD: "10",
  // The suites run the Anthropic rows (vitest.config.ts). The hourly review needs an OpenAI key
  // under either provider.
  BATCH_PROVIDER: "anthropic",
  OPENAI_API_KEY: "sk-test-not-a-real-key",
};

function refusal(env: Record<string, string | undefined>): string {
  try {
    relayConfigFromEnv(env);
  } catch (e) {
    expect(e).toBeInstanceOf(BootError);
    return (e as Error).message;
  }
  throw new Error("expected a BootError");
}

describe("relayConfigFromEnv", () => {
  it("applies the unit-economics §3.3 defaults: $0.50 per licence, $0.25 per trial device, per day", () => {
    const config = relayConfigFromEnv(minimal);
    expect(DEFAULT_LICENSE_DAILY_SPEND_CAP_USD).toBe(0.5);
    expect(DEFAULT_TRIAL_DAILY_SPEND_CAP_USD).toBe(0.25);
    expect(config.licenseSpendCapMicros).toBe(500_000);
    expect(config.trialSpendCapMicros).toBe(250_000);
    expect(config.dailyChunkCap).toBe(2000);
    expect(config.trialDailyChunkCap).toBe(500);
    expect(config.trialSpendShare).toBe(0.5);
    expect(config.rateBurst).toBe(12);
    expect(config.ratePerMin).toBe(12);
    expect(config.settleEverySec).toBe(900);
    expect(config.spendBreaker?.capMicros).toBe(10_000_000);
    expect(config.metricsToken).toBeUndefined();
    expect(rawPublicKeyBytes(config.licensePublicKey.current).toString("base64")).toBe(RELEASE_PUBKEY);
    expect(trialLaneLine(config)).toBe("trial_lane open");
  });

  it("keeps DAILY_SPEND_CAP_USD mandatory, with the explicit development opt-out closing the trial lane", () => {
    expect(refusal({ ...minimal, DAILY_SPEND_CAP_USD: undefined })).toMatch(/^missing required env: DAILY_SPEND_CAP_USD/);
    const dev = relayConfigFromEnv({ ...minimal, DAILY_SPEND_CAP_USD: undefined, RELAY_ALLOW_NO_SPEND_CAP: "1" });
    expect(dev.spendBreaker).toBeUndefined();
    expect(trialLaneLine(dev)).toContain("trial_lane closed");
  });

  it("refuses missing or wrong required values by name, never by value", () => {
    expect(refusal({ ...minimal, ANTHROPIC_API_KEY: undefined })).toBe("missing required env: ANTHROPIC_API_KEY");
    // Owner decision 2026-10-13: the background classes run on OpenAI by default, so the default
    // provider needs its key at boot — not a 502 on every create once the device is already sending.
    expect(refusal({ ...minimal, BATCH_PROVIDER: undefined, OPENAI_API_KEY: undefined })).toMatch(/^missing required env: OPENAI_API_KEY/);
    // The hourly review runs on OpenAI under either provider: no key, no reviewer, so no boot.
    expect(refusal({ ...minimal, OPENAI_API_KEY: undefined })).toMatch(/^missing required env: OPENAI_API_KEY/);
    expect(relayConfigFromEnv({ ...minimal, BATCH_PROVIDER: undefined })).toMatchObject({ batchProvider: "openai" });
    expect(relayConfigFromEnv(minimal)).toMatchObject({ batchProvider: "anthropic" });
    expect(refusal({ ...minimal, BATCH_PROVIDER: "gemini" })).toBe("invalid BATCH_PROVIDER (openai | anthropic)");
    expect(refusal({ ...minimal, LICENSE_PUBKEY_B64: undefined })).toBe("missing required env: LICENSE_PUBKEY_B64");
    const other = Buffer.alloc(32, 7).toString("base64");
    const message = refusal({ ...minimal, LICENSE_PUBKEY_B64: other });
    expect(message).toContain("LICENSE_PUBKEY_B64");
    expect(message).not.toContain(other);
    expect(message).not.toContain("sk-ant");
    expect(refusal({ ...minimal, LICENSE_PUBKEY_NEXT_B64: other })).toContain("LICENSE_PUBKEY_NEXT_B64");
  });

  it("refuses malformed numbers by name and clamps the trial budgets to the licence ones", () => {
    for (const [name, bad] of [["DAILY_CHUNK_CAP", "2k"], ["RATE_BURST", "0"], ["SETTLE_INTERVAL_SECONDS", "90000"], ["LICENSE_DAILY_SPEND_CAP_USD", "$3"], ["TRIAL_DAILY_SPEND_CAP_USD", "abc"]]) {
      const message = refusal({ ...minimal, [name!]: bad });
      expect(message).toBe(`invalid ${name}`);
    }
    expect(refusal({ ...minimal, TRIAL_SPEND_SHARE: "1.5" })).toBe("invalid TRIAL_SPEND_SHARE");
    expect(refusal({ ...minimal, DAILY_SPEND_CAP_USD: "ten" })).toBe("invalid DAILY_SPEND_CAP_USD or SPEND_PRICE_TABLE_JSON");
    expect(refusal({ ...minimal, SPEND_PRICE_TABLE_JSON: "{" })).toBe("invalid DAILY_SPEND_CAP_USD or SPEND_PRICE_TABLE_JSON");
    const clamped = relayConfigFromEnv({ ...minimal, LICENSE_DAILY_SPEND_CAP_USD: "0.40", TRIAL_DAILY_SPEND_CAP_USD: "9", DAILY_CHUNK_CAP: "100", TRIAL_DAILY_CHUNK_CAP: "500" });
    expect(clamped.trialSpendCapMicros).toBe(400_000);
    expect(clamped.trialDailyChunkCap).toBe(100);
  });

  it("reads the optional settings", () => {
    const config = relayConfigFromEnv({ ...minimal, METRICS_TOKEN: "m", SETTLE_INTERVAL_SECONDS: "0", DAILY_SPEND_CAP_USD: "0" });
    expect(config.metricsToken).toBe("m");
    expect(config.settleEverySec).toBe(0);
    // "0" is the emergency stop, not "unset": the breaker exists with a zero cap.
    expect(config.spendBreaker?.capMicros).toBe(0);
  });
});

/** A config with the test rig's own signing key in place of the release key. */
async function testConfig(): Promise<{ config: RelayConfig; token: string }> {
  const keys = await makeKeys();
  const base = relayConfigFromEnv(minimal);
  const current = licensePublicKeyFromB64(rawPublicKeyBytes(keys.publicKey).toString("base64"));
  return { config: { ...base, licensePublicKey: { current } }, token: await signLicense(keys.privateKey) };
}

describe("createRelay", () => {
  it("wires a store into the app: /healthz and a request through the real SQL store", async () => {
    const { config, token } = await testConfig();
    const lines: string[] = [];
    const gateway = new FakeGateway();
    const db = sqliteDouble();
    const store = postgresStore(db, config, (line) => lines.push(line));
    const relay = createRelay(config, store, { gateway, log: (line) => lines.push(line) });
    try {
      expect(await (await relay.app.request("/healthz")).json()).toEqual({ ok: true });
      const res = await relay.app.request("/v1/batch", {
        method: "POST",
        headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
        body: JSON.stringify({ client_request_id: "boot-1", ...sampleBody }),
      });
      expect(res.status).toBe(202);
      expect(gateway.createCalls).toHaveLength(1);
      // Booked on the shared store, and metered through the one metrics window.
      expect(await store.usage.usedOn("lic_test_1", utcDate())).toBe(2);
      expect(relay.metrics.snapshot(Date.now()).batch.plans.standard?.chunks_reserved).toBe(2);
      expect(lines.some((l) => l.startsWith("POST /v1/batch 202"))).toBe(true);
      expect(lines.join("\n")).not.toContain("TOP-SECRET");
      // The periodic tick (maintenance, then settlement) runs against the same store and gateway.
      expect(await relay.sweep()).toEqual({ checked: 1, settled: 0, abandoned: 0, pending: 1, failed: 0 });
      expect(gateway.getCalls).toEqual(["rb_fake_1"]);
      expect(db.statements.filter((q) => /^DELETE/i.test(q)).length).toBeGreaterThan(0);
    } finally {
      await store.close();
    }
  });

  it("answers an unavailable store with 503, never with a provider call", async () => {
    const { config, token } = await testConfig();
    const gateway = new FakeGateway();
    const usage = new InMemoryUsageStore();
    usage.claimIdempotency = () => Promise.resolve({ kind: "unavailable" as const });
    const relay = createRelay(config, { usage, rateLimit: new NoRateLimit(), close: () => Promise.resolve() }, { gateway, log: () => undefined });
    expect(await (await relay.app.request("/healthz")).json()).toEqual({ ok: true });
    const res = await relay.app.request("/v1/batch", {
      method: "POST",
      headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      body: JSON.stringify({ client_request_id: "boot-2", ...sampleBody }),
    });
    expect(res.status).toBe(503);
    expect(gateway.createCalls).toHaveLength(0);
  });

  it("verifies tokens against the config's trusted keys", async () => {
    const { config } = await testConfig();
    const relay = createRelay(config, { usage: new InMemoryUsageStore(), rateLimit: new NoRateLimit(), close: () => Promise.resolve() }, { gateway: new FakeGateway(), log: () => undefined });
    const res = await relay.app.request("/v1/batch", { method: "POST", headers: { authorization: "Bearer v1.x.y" }, body: "{}" });
    expect(res.status).toBe(401);
    expect(new AuthError("x", 401).status).toBe(401);
  });
});

describe("SPEND_PRICE_TABLE_JSON at boot (audit 2026-10-20 L2)", () => {
  it("merges the override over the shipped table instead of replacing it", () => {
    const config = relayConfigFromEnv({ ...minimal, SPEND_PRICE_TABLE_JSON: '{"gpt-6-luna":{"input":0.06,"output":0.3,"cacheRead":0.006,"cacheWrite":0.07}}' });
    const prices = config.spendBreaker!.prices;
    expect(prices["gpt-6-luna"]?.output).toBe(0.3);
    expect(prices["claude-sonnet-5-5"]).toBeDefined();
    expect(prices["claude-haiku-5-5"]?.longPrompt).toBeDefined();
  });
});
