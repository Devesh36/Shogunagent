/** Audit P1 (plan 1-2): the guards that keep one leaked licence from burning the operator's
 * Anthropic key — an atomic cap under concurrency, bounded input, a metering failure that
 * refuses rather than spends, and a per-licence rate limit. */
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

import { MAX_BODY_BYTES } from "../src/app.js";
import { UpstreamError } from "../src/gateway.js";
import { SqlUsageStore } from "../src/pg-store.js";
import { TokenBucketLimiter, type RateLimiter } from "../src/ratelimit.js";
import { MAX_CHUNK_BYTES, MAX_ITEMS } from "../src/types.js";
import { InMemoryUsageStore, type UsageStore } from "../src/usage.js";
import { makeRig, postBatch, sampleBody } from "./helpers.js";
import { sqliteDouble } from "./sql-double.js";

function bodyWithChunks(n: number): unknown {
  return {
    purpose: "consolidation",
    model_class: "classify",
    items: Array.from({ length: n }, (_, i) => ({ custom_id: String(i), chunk: `c${i}` })),
  };
}

/** The two stores a relay can run on: the in-memory one (development) and the SQL one over the
 * Postgres double (what production runs, minus the server). */
const stores: Array<[string, () => UsageStore]> = [
  ["in-memory store", () => new InMemoryUsageStore()],
  ["SQL store", () => new SqlUsageStore(sqliteDouble())],
];

describe.each(stores)("daily cap under concurrency (§2.2) on the %s", (_name, open) => {
  it("never lets simultaneous submissions cross the cap, even by one chunk", async () => {
    // 50 submissions of 1 chunk against a cap of 10 exercise the transactional budget.
    const usage = open();
    const rig = await makeRig({ usage, dailyChunkCap: 10 });
    const results = await Promise.all(Array.from({ length: 50 }, (_, i) =>
      postBatch(rig, { ...(bodyWithChunks(1) as object), client_request_id: `concurrent-cap-${i}` })));
    expect(results.filter((r) => r.status === 202)).toHaveLength(10);
    expect(results.filter((r) => r.status === 429)).toHaveLength(40);
    // And the ledger agrees with what was actually forwarded.
    expect(rig.gateway.createCalls).toHaveLength(10);
    expect(await usage.usedOn("lic_test_1", new Date().toISOString().slice(0, 10))).toBe(10);
  });

  it("releases the reservation after a definite upstream rejection", async () => {
    const usage = open();
    const rig = await makeRig({ usage, dailyChunkCap: 10 });
    const date = new Date().toISOString().slice(0, 10);
    const realCreate = rig.gateway.createBatch.bind(rig.gateway);

    rig.gateway.createBatch = () => Promise.reject(new UpstreamError("create", 400));
    const failed = await postBatch(rig, bodyWithChunks(6));
    expect(failed.status).toBe(502);
    expect(await usage.usedOn("lic_test_1", date)).toBe(0);

    // The whole cap is still available once Anthropic recovers — a rejected submission must not
    // eat six chunks of the user's day.
    rig.gateway.createBatch = realCreate;
    expect((await postBatch(rig, bodyWithChunks(10))).status).toBe(202);
    expect(await usage.usedOn("lic_test_1", date)).toBe(10);
  });
});

describe("bounded input", () => {
  it("refuses more than MAX_ITEMS entries before touching the gateway", async () => {
    const rig = await makeRig({ dailyChunkCap: 10_000_000 });
    const res = await postBatch(rig, bodyWithChunks(MAX_ITEMS + 1));
    expect(res.status).toBe(400);
    expect(rig.gateway.createCalls).toHaveLength(0);
  });

  it("refuses an oversized chunk", async () => {
    const rig = await makeRig();
    const res = await postBatch(rig, {
      purpose: "consolidation",
      model_class: "classify",
      items: [{ custom_id: "1", chunk: "x".repeat(MAX_CHUNK_BYTES + 1) }],
    });
    expect(res.status).toBe(400);
    expect(rig.gateway.createCalls).toHaveLength(0);
    // ...and accepts one exactly at the ceiling.
    const ok = await makeRig();
    expect((await postBatch(ok, { purpose: "consolidation", model_class: "classify", items: [{ custom_id: "1", chunk: "x".repeat(MAX_CHUNK_BYTES) }] })).status).toBe(202);
  });

  it("refuses a body past the hard ceiling with 413", async () => {
    const rig = await makeRig();
    const huge = JSON.stringify({
      purpose: "consolidation",
      model_class: "classify",
      items: [{ custom_id: "1", chunk: "x".repeat(MAX_BODY_BYTES + 1024) }],
    });
    const res = await rig.app.request("/v1/batch", {
      method: "POST",
      headers: {
        authorization: `Bearer ${rig.token}`,
        "content-type": "application/json",
        "content-length": String(Buffer.byteLength(huge)),
      },
      body: huge,
    });
    expect(res.status).toBe(413);
    expect(rig.gateway.createCalls).toHaveLength(0);
  });
});

describe("metering failure fails closed (§4.5)", () => {
  it("answers 503 — not 429, not success — when the ledger cannot be read", async () => {
    const db = sqliteDouble();
    const rig = await makeRig({ usage: new SqlUsageStore(db) });
    db.failNext(1);
    const res = await postBatch(rig, sampleBody);
    expect(res.status).toBe(503);
    expect(res.headers.get("retry-after")).toBe("1");
    expect(rig.gateway.createCalls).toHaveLength(0);
  });

  it("treats an empty store as a fresh day, not a failure", async () => {
    const usage = new SqlUsageStore(sqliteDouble());
    const rig = await makeRig({ usage });
    const res = await postBatch(rig, sampleBody);
    expect(res.status).toBe(202);
    expect(await usage.usedOn("lic_test_1", new Date().toISOString().slice(0, 10))).toBe(2);
  });
});

describe("per-licence rate limit", () => {
  it("refuses a burst past the bucket and lets a different licence through", async () => {
    let nowMs = 1_700_000_000_000;
    const rig = await makeRig({
      dailyChunkCap: 10_000,
      rateLimit: new TokenBucketLimiter(3, 3),
      now: () => new Date(nowMs),
    });

    expect((await postBatch(rig, bodyWithChunks(1))).status).toBe(202);
    expect((await postBatch(rig, bodyWithChunks(1))).status).toBe(202);
    expect((await postBatch(rig, bodyWithChunks(1))).status).toBe(202);
    const over = await postBatch(rig, bodyWithChunks(1));
    expect(over.status).toBe(429);
    expect(await over.json()).toEqual({ error: "too many requests", reason: "rate_limited" });
    expect(rig.gateway.createCalls).toHaveLength(3);

    // A minute later the bucket has refilled.
    nowMs += 60_000;
    expect((await postBatch(rig, bodyWithChunks(1))).status).toBe(202);
  });

  it("meters the bucket per licence, not globally", async () => {
    const { signLicense } = await import("./helpers.js");
    let nowMs = 1_700_000_000_000;
    const rig = await makeRig({
      dailyChunkCap: 10_000,
      rateLimit: new TokenBucketLimiter(1, 1),
      now: () => new Date(nowMs),
    });
    const other = await signLicense(rig.keys.privateKey, { sub: "lic_other", iat: nowMs / 1000 });

    expect((await postBatch(rig, bodyWithChunks(1))).status).toBe(202);
    expect((await postBatch(rig, bodyWithChunks(1))).status).toBe(429);
    expect((await postBatch(rig, bodyWithChunks(1), other)).status).toBe(202);
  });

  it("the limiter itself refills at the configured rate", () => {
    const limiter: RateLimiter = new TokenBucketLimiter(2, 60); // 1/sec sustained
    expect(limiter.take("a", 0)).toBe(true);
    expect(limiter.take("a", 0)).toBe(true);
    expect(limiter.take("a", 0)).toBe(false);
    expect(limiter.take("a", 999)).toBe(false); // not quite a second
    expect(limiter.take("a", 1000)).toBe(true);
  });
});

describe("bucket scope and Retry-After (#334)", () => {
  it("keys the bucket per device, so a copied token cannot multiply the allowance", async () => {
    const { signLicense } = await import("./helpers.js");
    const rig = await makeRig({
      dailyChunkCap: 10_000,
      rateLimit: new TokenBucketLimiter(1, 1),
      now: () => new Date(1_700_000_000_000),
    });
    // Same licence, a token minted for a second Mac. Separate budget…
    const otherDevice = await signLicense(rig.keys.privateKey, { device: "dev_other", iat: 1_700_000_000 });

    expect((await postBatch(rig, bodyWithChunks(1))).status).toBe(202);
    expect((await postBatch(rig, bodyWithChunks(1))).status).toBe(429);
    expect((await postBatch(rig, bodyWithChunks(1), otherDevice)).status).toBe(202);
    // …but a *copy* of the first token is the first device's budget, not a fresh one. That is
    // the whole containment property: the claim travels with the token it was minted for.
    expect((await postBatch(rig, bodyWithChunks(1))).status).toBe(429);
  });

  it("polling does not starve submission — separate buckets per route", async () => {
    const rig = await makeRig({
      dailyChunkCap: 10_000,
      rateLimit: new TokenBucketLimiter(2, 2),
      now: () => new Date(1_700_000_000_000),
    });
    const get = async (): Promise<number> =>
      (
        await rig.app.request("/v1/batch/rb_fake_1", {
          headers: { authorization: `Bearer ${rig.token}` },
        })
      ).status;

    // Drain the poll bucket…
    await get();
    await get();
    expect(await get()).toBe(429);
    // …and submission still has its own full allowance.
    expect((await postBatch(rig, bodyWithChunks(1))).status).toBe(202);
  });

  it("sends Retry-After with a rate-limit 429", async () => {
    const rig = await makeRig({
      dailyChunkCap: 10_000,
      rateLimit: new TokenBucketLimiter(1, 60), // 1/sec sustained
      now: () => new Date(1_700_000_000_000),
    });
    expect((await postBatch(rig, bodyWithChunks(1))).status).toBe(202);
    const over = await postBatch(rig, bodyWithChunks(1));
    expect(over.status).toBe(429);
    expect(over.headers.get("retry-after")).toBe("1");
    // Transient: the reason says so, and the header is what the device keys on (§4.5).
    expect(await over.json()).toEqual({ error: "too many requests", reason: "rate_limited" });
  });

  it("answers the daily cap with a cap-reached 429: reason code and NO Retry-After (#29)", async () => {
    // 1_700_000_000_000 = 2023-11-14T22:13:20Z → refills at 2023-11-15T00:00:00Z.
    const rig = await makeRig({ dailyChunkCap: 1, now: () => new Date(1_700_000_000_000) });
    expect((await postBatch(rig, bodyWithChunks(1))).status).toBe(202);
    const capped = await postBatch(rig, bodyWithChunks(1));
    expect(capped.status).toBe(429);
    // The absence of Retry-After is the contract: nothing is retryable tonight, so the device
    // gives the night to the local lane on the first one instead of sleeping to UTC midnight.
    expect(capped.headers.get("retry-after")).toBeNull();
    expect(await capped.json()).toEqual({
      error: "daily chunk cap reached",
      reason: "daily_cap",
      resets_at: "2023-11-15T00:00:00.000Z",
    });
  });

  it("keeps every 503 retryable: Retry-After is always present", async () => {
    const rig = await makeRig({
      usage: Object.assign(new InMemoryUsageStore(), {
        claimIdempotency: () => Promise.resolve({ kind: "unavailable" as const }),
      }),
    });
    const res = await postBatch(rig, bodyWithChunks(1));
    expect(res.status).toBe(503);
    expect(res.headers.get("retry-after")).toBe("1");
  });
});

describe("health endpoint (#334)", () => {
  it("answers without a licence and without touching a provider", async () => {
    const rig = await makeRig();
    const res = await rig.app.request("/healthz");
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({ ok: true });
    // The probe that restarts the container must never spend the operator's key.
    expect(rig.gateway.createCalls).toHaveLength(0);
    expect(rig.gateway.getCalls).toHaveLength(0);
  });

  it("says nothing about how the gateway is configured", async () => {
    const rig = await makeRig({ dailyChunkCap: 7 });
    const text = JSON.stringify(await (await rig.app.request("/healthz")).json());
    // An unauthenticated endpoint that describes the deployment is reconnaissance.
    expect(text).not.toMatch(/key|model|cap|budget|limit/i);
    expect(text).not.toContain("7");
  });
});

describe("SQL permissions files cover every relay table (defence in depth)", () => {
  const read = (name: string) => readFileSync(new URL(`../sql/${name}`, import.meta.url), "utf8").replace(/--[^\n]*/g, "");
  const tables = (sql: string) => [...sql.matchAll(/CREATE TABLE IF NOT EXISTS (relay_\w+)/g)].map((m) => m[1]!);
  for (const [schema, permissions] of [["shared-store.sql", "shared-store-permissions.sql"], ["asr-store.sql", "asr-store-permissions.sql"]] as const) {
    it(`${permissions} enables RLS on and revokes every table of ${schema}`, () => {
      const perms = read(permissions);
      const created = tables(read(schema));
      expect(created.length).toBeGreaterThan(0);
      const revoked = [...perms.matchAll(/REVOKE ALL ON ([^;]+?) FROM PUBLIC, anon, authenticated;/g)]
        .flatMap((m) => m[1]!.split(",").map((t) => t.trim()));
      for (const table of created) {
        expect(perms, table).toContain(`ALTER TABLE ${table} ENABLE ROW LEVEL SECURITY;`);
        expect(revoked, table).toContain(table);
      }
      // Never FORCE: the owning role (shogun_relay) must keep bypassing RLS, or the relay stops.
      expect(perms).not.toMatch(/FORCE ROW LEVEL SECURITY/i);
      expect(perms).not.toMatch(/\bGRANT\b|\bDROP\b|CREATE POLICY/i);
    });
  }
});

it("keeps Workers invocation logs off: their request metadata would carry the licence bearer", () => {
  const config = readFileSync(new URL("../wrangler.jsonc", import.meta.url), "utf8").replace(/^\s*\/\/[^\n]*$/gm, "");
  expect(config).toMatch(/"observability":\s*\{[^}]*"logs":\s*\{[^}]*"invocation_logs":\s*false/);
});
