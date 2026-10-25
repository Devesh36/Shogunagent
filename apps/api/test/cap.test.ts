/** §2.2/§4.5: the daily chunk cap — the reason the relay exists. Enforced server-side, per
 * license, per UTC day; a capped submission is refused with 429 and never reaches Anthropic. */
import { describe, expect, it } from "vitest";

import { DEFAULT_BATCH_PRICES } from "../src/spend.js";
import { makeRig, postBatch, signLicense } from "./helpers.js";

function bodyWithChunks(n: number): unknown {
  return {
    purpose: "consolidation",
    model_class: "classify",
    items: Array.from({ length: n }, (_, i) => ({ custom_id: String(i), chunk: `c${i}` })),
  };
}

describe("trial daily chunk cap (issue #763)", () => {
  const spendBreaker = { capMicros: 5_000_000, prices: DEFAULT_BATCH_PRICES };

  it("holds a trial Mac to its own, lower cap while a paid licence keeps the full one", async () => {
    const rig = await makeRig({ dailyChunkCap: 10, trialDailyChunkCap: 3, spendBreaker });
    const trial = await signLicense(rig.keys.privateKey, { sub: "trial_dev_a", plan: "trial", status: "trialing" });

    expect((await postBatch(rig, bodyWithChunks(3), trial)).status).toBe(202);
    const over = await postBatch(rig, bodyWithChunks(1), trial);
    expect(over.status).toBe(429);
    expect(((await over.json()) as { reason: string }).reason).toBe("daily_cap");
    // The paid licence on the same relay still has all ten.
    expect((await postBatch(rig, bodyWithChunks(10))).status).toBe(202);
    expect(rig.gateway.createCalls).toHaveLength(2);
  });

  it("never lets a trial cap exceed the paid cap, however it is configured", async () => {
    const rig = await makeRig({ dailyChunkCap: 2, trialDailyChunkCap: 1000, spendBreaker });
    const trial = await signLicense(rig.keys.privateKey, { sub: "trial_dev_b", plan: "trial", status: "trialing" });
    expect((await postBatch(rig, bodyWithChunks(2), trial)).status).toBe(202);
    expect((await postBatch(rig, bodyWithChunks(1), trial)).status).toBe(429);
  });

  it("a trial cap of 0 keeps the lane open but sends nothing", async () => {
    const rig = await makeRig({ dailyChunkCap: 10, trialDailyChunkCap: 0, spendBreaker });
    const trial = await signLicense(rig.keys.privateKey, { sub: "trial_dev_c", plan: "trial", status: "trialing" });
    expect((await postBatch(rig, bodyWithChunks(1), trial)).status).toBe(429);
    expect(rig.gateway.createCalls).toHaveLength(0);
  });
});

describe("daily chunk cap", () => {
  it("accepts submissions up to the cap and refuses the one that would cross it", async () => {
    const rig = await makeRig({ dailyChunkCap: 10 });

    expect((await postBatch(rig, bodyWithChunks(8))).status).toBe(202);
    // 8 used + 3 more would cross 10 → 429, and the gateway is not called again.
    const over = await postBatch(rig, bodyWithChunks(3));
    expect(over.status).toBe(429);
    expect(rig.gateway.createCalls).toHaveLength(1);
    // Exactly at the cap is still allowed.
    expect((await postBatch(rig, bodyWithChunks(2))).status).toBe(202);
  });

  it("records usage only for accepted submissions", async () => {
    const rig = await makeRig({ dailyChunkCap: 5 });
    await postBatch(rig, bodyWithChunks(4));
    await postBatch(rig, bodyWithChunks(4)); // refused
    expect(rig.usage.records).toHaveLength(1);
    expect(rig.usage.records[0]?.chunks).toBe(4);
  });

  it("meters per license, not globally", async () => {
    const rig = await makeRig({ dailyChunkCap: 5 });
    const { signLicense } = await import("./helpers.js");
    const other = await signLicense(rig.keys.privateKey, { sub: "lic_other" });
    expect((await postBatch(rig, bodyWithChunks(5))).status).toBe(202);
    // The first license is at its cap; a different license still has headroom.
    expect((await postBatch(rig, bodyWithChunks(5))).status).toBe(429);
    expect((await postBatch(rig, bodyWithChunks(5), other)).status).toBe(202);
  });

  // #1066 B: derivation moved from one nightly submission to one per hour. The cap is a volume
  // (chunks per licence per UTC day), not a request count, so 24 hourly submissions spend the same
  // budget one nightly submission of the same text did, and a normal day's text still fits.
  it("meters volume, not requests: 24 hourly submissions share the day's chunk budget", async () => {
    const rig = await makeRig({ dailyChunkCap: 240 });
    for (let hour = 0; hour < 24; hour += 1) {
      expect((await postBatch(rig, bodyWithChunks(10))).status).toBe(202);
    }
    expect(rig.gateway.createCalls).toHaveLength(24);
    expect(rig.usage.records.reduce((sum, r) => sum + r.chunks, 0)).toBe(240);
    // The budget is exactly spent: the next chunk is refused as a cap, with no retry hint.
    const over = await postBatch(rig, bodyWithChunks(1));
    expect(over.status).toBe(429);
    expect(over.headers.get("retry-after")).toBeNull();
    expect(((await over.json()) as { reason: string }).reason).toBe("daily_cap");
  });

  it("does not make an hourly cadence trip the transient rate limit at the shipping defaults", async () => {
    // index.ts wires RATE_BURST / RATE_PER_MIN = 12: a device submitting once an hour (plus the
    // 1-2-4-8-15 minute polls of each batch) stays far below it.
    const { TokenBucketLimiter } = await import("../src/ratelimit.js");
    const limiter = new TokenBucketLimiter(12, 12);
    let nowMs = 0;
    for (let hour = 0; hour < 48; hour += 1) {
      for (const minutes of [0, 1, 3, 7, 15, 30]) {
        expect(limiter.take("lic_hourly", nowMs + minutes * 60_000)).toBe(true);
      }
      nowMs += 3_600_000;
    }
  });

  it("resets on the next UTC day", async () => {
    let today = new Date("2026-08-09T23:00:00Z");
    const rig = await makeRig({ dailyChunkCap: 5, now: () => today });
    expect((await postBatch(rig, bodyWithChunks(5))).status).toBe(202);
    expect((await postBatch(rig, bodyWithChunks(1))).status).toBe(429);
    today = new Date("2026-08-10T01:00:00Z");
    expect((await postBatch(rig, bodyWithChunks(5))).status).toBe(202);
  });
});
