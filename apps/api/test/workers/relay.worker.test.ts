/**
 * Runs INSIDE workerd (vitest.workers.config.ts). Proves, in the runtime the deployed Worker uses:
 *
 * 1. `node:crypto` Ed25519 verification (auth.ts `verifyLicense`) accepts a real token and
 *    rejects a tampered one under `nodejs_compat` — so no WebCrypto rewrite is needed.
 * 2. The Worker entry fails closed: a missing required variable or binding makes every request a
 *    503 with the fixed body, logged once; a complete environment serves /healthz without touching
 *    the store; an unreachable store is a 503 on submit, never a provider call.
 * 3. The redacting logger and its `logId` hash work without Node.
 *
 * No database in CI: the store paths run on a `SqlClient` whose every query is refused (what the
 * driver reports for an unreachable Hyperdrive), so the fail-closed behaviour is what is tested.
 * The real Hyperdrive → Supabase round trip is the first-deploy smoke in DEPLOY.md.
 */
import { createExecutionContext, waitOnExecutionContext } from "cloudflare:test";
import { afterEach, describe, expect, it, vi } from "vitest";

import { AuthError, keyId, licensePublicKeyFromB64, verifyLicense } from "../../src/auth.js";
import { createRelay, postgresStore, relayConfigFromEnv } from "../../src/boot.js";
import { logId, routeTemplate } from "../../src/logging.js";
import { postgresClient, type SqlClient } from "../../src/sql.js";
import worker, { MISCONFIGURED_BODY, envRecord, resetBootStateForTests, type WorkerEnv } from "../../src/worker.js";
import releaseKey from "../../../../packages/config/license-public-key.json";

const RELEASE_PUBKEY = (releaseKey as { ed25519: string }).ed25519;
/** Never connected to: the Worker entry only hands it to the driver, which connects on first query,
 * and no test below sends one. */
const UNUSED_DB = "postgres://relay:nope@127.0.0.1:9/relay";

/** A store whose database is unreachable: every statement is refused the way the driver refuses
 * one for a dead connection. */
function refusedClient(): SqlClient {
  const refused = () => Promise.reject(new Error("connection refused"));
  return { query: refused, transaction: refused, close: () => Promise.resolve() };
}

type SubtleKey = Parameters<typeof crypto.subtle.sign>[1];

function b64(bytes: Uint8Array): string {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s);
}
const b64url = (bytes: Uint8Array): string => b64(bytes).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");

/** An Ed25519 pair from WebCrypto (the signer here stands in for the licence API) and a `v1.`
 * token over `payload`, in the exact shape apps/website/src/lib/license.ts issues. */
async function signer() {
  const pair = (await crypto.subtle.generateKey({ name: "Ed25519" }, true, ["sign", "verify"])) as { publicKey: SubtleKey; privateKey: SubtleKey };
  const publicB64 = b64(new Uint8Array(await crypto.subtle.exportKey("raw", pair.publicKey)));
  const sign = async (payload: Record<string, unknown>): Promise<string> => {
    const body = new TextEncoder().encode(JSON.stringify(payload));
    const sig = new Uint8Array(await crypto.subtle.sign("Ed25519", pair.privateKey, body));
    return `v1.${b64url(body)}.${b64url(sig)}`;
  };
  return { publicB64, sign };
}

function claims(now: number): Record<string, unknown> {
  return { v: 1, lic: "lic_worker_1", plan: "standard", status: "active", iat: now, exp: now + 24 * 3600, device: "dev_worker_1" };
}

const fullEnv = (): WorkerEnv => ({
  ANTHROPIC_API_KEY: "sk-ant-test-not-a-real-key",
  LICENSE_PUBKEY_B64: RELEASE_PUBKEY,
  DAILY_SPEND_CAP_USD: "10",
  BATCH_PROVIDER: "anthropic",
  OPENAI_API_KEY: "sk-test-not-a-real-key",
  HYPERDRIVE: { connectionString: UNUSED_DB },
});

afterEach(() => {
  resetBootStateForTests();
  vi.restoreAllMocks();
});

describe("Ed25519 licence verification in workerd (node:crypto under nodejs_compat)", () => {
  it("accepts a real token and rejects a tampered signature, an unknown kid and an expired exp", async () => {
    const { publicB64, sign } = await signer();
    const key = licensePublicKeyFromB64(publicB64);
    const now = Math.floor(Date.now() / 1000);
    const token = await sign(claims(now));
    expect(verifyLicense(`Bearer ${token}`, key, now)).toEqual({ licenseId: "lic_worker_1", deviceId: "dev_worker_1", plan: "standard" });
    // With the matching kid, and against the TrustedKeys pair shape.
    expect(verifyLicense(`Bearer ${await sign({ ...claims(now), kid: keyId(key) })}`, { current: key }, now).plan).toBe("standard");

    const [v, body, sig] = token.split(".") as [string, string, string];
    const flipped = (sig[0] === "A" ? "B" : "A") + sig.slice(1);
    expect(() => verifyLicense(`Bearer ${v}.${body}.${flipped}`, key, now)).toThrow(AuthError);
    const unknownKid = await sign({ ...claims(now), kid: "0123456789abcdef" });
    expect(() => verifyLicense(`Bearer ${unknownKid}`, key, now)).toThrow(AuthError);
    const expired = await sign({ ...claims(now), exp: now - 1 });
    expect(() => verifyLicense(`Bearer ${expired}`, key, now)).toThrow(AuthError);
    // Another key's signature is not this key's.
    const other = await signer();
    expect(() => verifyLicense(`Bearer ${token}`, licensePublicKeyFromB64(other.publicB64), now)).toThrow(AuthError);
  });

  it("runs the redacting logger's hash and route templating without Node", () => {
    expect(logId("lic_worker_1")).toMatch(/^[0-9a-f]{12}$/);
    expect(routeTemplate("/v1/batch/rb_123")).toBe("/v1/batch/:id");
  });
});

describe("Worker entry: fail-closed boot", () => {
  it("answers 503 with the fixed body to every request while a required variable is missing, logging once", async () => {
    const error = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const env: WorkerEnv = { ...fullEnv(), DAILY_SPEND_CAP_USD: undefined };
    for (const path of ["/healthz", "/v1/batch", "/metrics"]) {
      const ctx = createExecutionContext();
      const res = await worker.fetch(new Request(`https://api.shogunaios.com${path}`, { method: path === "/v1/batch" ? "POST" : "GET" }), env, ctx);
      await waitOnExecutionContext(ctx);
      expect(res.status).toBe(503);
      expect(await res.text()).toBe(MISCONFIGURED_BODY);
      expect(res.headers.get("cache-control")).toBe("no-store");
    }
    // The refusal names the variable and is written once per isolate, not once per request.
    const lines = error.mock.calls.map((c) => String(c[0]));
    expect(lines.filter((l) => l.startsWith("missing required env: DAILY_SPEND_CAP_USD"))).toHaveLength(1);
    expect(lines.join("\n")).not.toContain("sk-ant");
  });

  it("treats a missing HYPERDRIVE binding like a missing variable", async () => {
    const error = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const env = fullEnv();
    delete env.HYPERDRIVE;
    const res = await worker.fetch(new Request("https://api.shogunaios.com/healthz"), env, createExecutionContext());
    expect(res.status).toBe(503);
    expect(error.mock.calls.map((c) => String(c[0]))).toContainEqual(expect.stringContaining("HYPERDRIVE"));
  });

  it("boots from bindings and serves /healthz without touching the store; a bad token is 401 before any store or provider call", async () => {
    const log = vi.spyOn(console, "log").mockImplementation(() => undefined);
    const env = fullEnv();
    const health = await worker.fetch(new Request("https://api.shogunaios.com/healthz"), env, createExecutionContext());
    expect(health.status).toBe(200);
    expect(await health.json()).toEqual({ ok: true });
    expect(log.mock.calls.map((c) => String(c[0]))).toContain("trial_lane open");

    const res = await worker.fetch(
      new Request("https://api.shogunaios.com/v1/batch", { method: "POST", headers: { authorization: "Bearer v1.not.real", "content-type": "application/json" }, body: "{}" }),
      env,
      createExecutionContext(),
    );
    expect(res.status).toBe(401);
    const lines = log.mock.calls.map((c) => String(c[0]));
    expect(lines).toContainEqual(expect.stringMatching(/^POST \/v1\/batch 401 \d+ms$/));
    expect(lines.join("\n")).not.toContain("v1.not.real");
    expect(envRecord(env)).not.toHaveProperty("HYPERDRIVE");
  });

  it("fails closed when the store is unreachable: 503 on submit, nothing sent to the provider", async () => {
    const { publicB64, sign } = await signer();
    const base = relayConfigFromEnv(envRecord(fullEnv()));
    const config = { ...base, licensePublicKey: { current: licensePublicKeyFromB64(publicB64) } };
    const lines: string[] = [];
    const store = postgresStore(refusedClient(), config, (line) => lines.push(line));
    const createBatch = vi.fn(() => Promise.reject(new Error("must not be called")));
    const relay = createRelay(config, store, {
      gateway: { createBatch, getBatch: createBatch, streamResults: () => { throw new Error("must not be called"); } } as never,
      log: (line) => lines.push(line),
    });
    const now = Math.floor(Date.now() / 1000);
    const res = await relay.app.request("/v1/batch", {
      method: "POST",
      headers: { authorization: `Bearer ${await sign(claims(now))}`, "content-type": "application/json" },
      body: JSON.stringify({ client_request_id: "w-1", purpose: "consolidation", model_class: "classify", items: [{ custom_id: "1", chunk: "TOP-SECRET" }] }),
    });
    expect(res.status).toBe(503);
    expect(createBatch).not.toHaveBeenCalled();
    // The rate limiter failed open with its fixed line; the ledger failed closed; no content, no id.
    expect(lines).toContain("ratelimit_unavailable (failing open)");
    expect(lines).toContainEqual(expect.stringContaining("event=metering_unavailable"));
    expect(lines.join("\n")).not.toContain("TOP-SECRET");
    expect(lines.join("\n")).not.toContain("lic_worker_1");
    // The settlement pass against the same dead store: a count, never a throw.
    expect(await relay.settle()).toEqual({ checked: 0, settled: 0, abandoned: 0, pending: 0, failed: 1 });
  });

  it("loads the Postgres driver in workerd and constructs a Hyperdrive-shaped client without connecting", async () => {
    const client = postgresClient(UNUSED_DB, { hyperdrive: true });
    await expect(client.close()).resolves.toBeUndefined();
  });

  it("does nothing on the cron while misconfigured or with the sweep switched off", async () => {
    const log = vi.spyOn(console, "log").mockImplementation(() => undefined);
    const off = createExecutionContext();
    await worker.scheduled({ scheduledTime: Date.now(), cron: "*/15 * * * *" }, { ...fullEnv(), SETTLE_INTERVAL_SECONDS: "0" }, off);
    await waitOnExecutionContext(off);
    expect(log.mock.calls.map((c) => String(c[0]))).not.toContainEqual(expect.stringContaining("settlement_sweep"));
    resetBootStateForTests();
    vi.spyOn(console, "error").mockImplementation(() => undefined);
    const ctx = createExecutionContext();
    await worker.scheduled({ scheduledTime: Date.now(), cron: "*/15 * * * *" }, { ...fullEnv(), ANTHROPIC_API_KEY: undefined }, ctx);
    await waitOnExecutionContext(ctx);
  });
});

describe("company speech in workerd", () => {
  it("leaves speech disabled by default", async () => {
    const config = relayConfigFromEnv(envRecord(fullEnv()));
    const relay = createRelay(config, postgresStore(refusedClient(), config));
    for (const route of ["grant", "refresh", "finish"]) {
      const response = await relay.app.request(`/v1/asr/${route}`, { method: "POST" });
      expect(response.status).toBe(503);
      expect(response.headers.get("cache-control")).toBe("no-store");
    }
  });
  it("never mints against missing authentication or an unavailable ledger", async () => {
    const { publicB64, sign } = await signer();
    const config = { ...relayConfigFromEnv(envRecord({ ...fullEnv(), ASR_ENABLED: "1", DEEPGRAM_API_KEY: "speech-test-only" })), licensePublicKey: { current: licensePublicKeyFromB64(publicB64) } };
    const relay = createRelay(config, postgresStore(refusedClient(), config));
    const request = vi.spyOn(globalThis, "fetch");
    expect((await relay.app.request("/v1/asr/grant", { method: "POST" })).status).toBe(401);
    const token = await sign(claims(Math.floor(Date.now() / 1000)));
    const response = await relay.app.request("/v1/asr/grant", { method: "POST", headers: { authorization: `Bearer ${token}` } });
    expect(response.status).toBe(503);
    expect(request).not.toHaveBeenCalled();
  });
});

it("mints with the native Workers fetch binding", async () => {
  const { fetchMock } = await import("cloudflare:test");
  const { FetchDeepgramGateway } = await import("../../src/deepgram.js");
  fetchMock.activate(); fetchMock.disableNetConnect();
  try {
    fetchMock.get("https://api.deepgram.com").intercept({ path: "/v1/auth/grant", method: "POST" })
      .reply(200, JSON.stringify({ access_token: "test.jwt.signature", expires_in: 60 }));
    await expect(new FetchDeepgramGateway("test-only").grant()).resolves.toEqual({ access_token: "test.jwt.signature", expires_in: 60 });
    fetchMock.assertNoPendingInterceptors();
  } finally { fetchMock.deactivate(); }
});
