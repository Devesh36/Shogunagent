/** Audit 2026-10-10 M9: the relay's own settlement sweep books the provider's actual usage for
 * ended batches, once, whether or not the client ever reads its results. */
import { afterEach, describe, expect, it, vi } from "vitest";

import { DEFAULT_SETTLE_MAX_PER_TICK, DEFAULT_SETTLE_RECHECK_SECONDS, MAX_SETTLE_MAX_PER_TICK, relayConfigFromEnv, settleOnce, startPeriodicSweep, sweepOnce } from "../src/boot.js";
import { NO_RESPONSE, UpstreamError } from "../src/gateway.js";
import { MODEL_BY_CLASS } from "../src/models.js";
import { DEFAULT_BATCH_PRICES } from "../src/spend.js";
import { USAGE_DAY_KEEP_DAYS, utcDate } from "../src/usage.js";
import { makeRig, postBatch, sampleBody, type Rig } from "./helpers.js";
import releaseKey from "../../../packages/config/license-public-key.json";

const breaker = { capMicros: 100_000_000, prices: DEFAULT_BATCH_PRICES };
const today = () => utcDate(new Date());

function resultLine(customId: string, usage: Record<string, number>, model = MODEL_BY_CLASS.classify): string {
  return JSON.stringify({ custom_id: customId, result: { type: "succeeded", message: { model, content: [{ type: "text", text: "{}" }], usage } } });
}

function ended(rig: Rig): void {
  rig.gateway.status = { ...rig.gateway.status, processing_status: "ended", request_counts: { processing: 0, succeeded: 2, errored: 0, canceled: 0, expired: 0 } };
  rig.gateway.resultLines = [resultLine("1234", { input_tokens: 100, output_tokens: 10 }), resultLine("5678", { input_tokens: 100, output_tokens: 10 })];
}

async function submitted(overrides: Parameters<typeof makeRig>[0] = {}): Promise<{ rig: Rig; batchId: string; estimate: number }> {
  const rig = await makeRig({ spendBreaker: breaker, licenseSpendCapMicros: 10_000_000, ...overrides });
  const res = await postBatch(rig, sampleBody);
  expect(res.status).toBe(202);
  const { batch_id } = (await res.json()) as { batch_id: string };
  const estimate = await rig.usage.spentOn(utcDate(overrides.now?.() ?? new Date()));
  return { rig, batchId: batch_id, estimate };
}

/** Settlement deps with the re-poll holdoff off, so a test can poll the same batch twice in a row;
 * the holdoff has its own test below. -1 rather than 0: a batch is due again only when checked
 * strictly before the bound, and two sweeps can land in the same millisecond. */
const deps = (rig: Rig, log: string[] = []) => ({ usage: rig.usage, gateway: rig.gateway, prices: DEFAULT_BATCH_PRICES, log: (line: string) => log.push(line), recheckSeconds: -1 });

afterEach(() => vi.useRealTimers());

describe("settleOnce", () => {
  it("leaves an in-progress batch pending, then books actual usage on both counters once it ends", async () => {
    vi.useFakeTimers({ toFake: ["Date"] });
    const { rig } = await submitted();
    expect(await settleOnce(deps(rig))).toEqual({ checked: 1, settled: 0, abandoned: 0, pending: 1, failed: 0 });
    expect(rig.gateway.getCalls).toEqual(["rb_fake_1"]); // the provider id from the ledger, not the relay id
    expect(rig.gateway.resultsCalls).toHaveLength(0);
    ended(rig);
    // The ledger uses a strict cutoff; advance deterministically past the prior poll.
    vi.setSystemTime(Date.now() + 1);
    expect(await settleOnce(deps(rig))).toEqual({ checked: 1, settled: 1, abandoned: 0, pending: 0, failed: 0 });
    // 200 input × 0.05 + 20 output × 0.25 = 15 micro-USD (Haiku 5.5)
    expect(await rig.usage.spentOn(today())).toBe(15);
    expect(await rig.usage.licenseSpentOn("lic_test_1", today())).toBe(15);
    // Settled: the next sweep has nothing to ask about.
    expect(await settleOnce(deps(rig))).toEqual({ checked: 0, settled: 0, abandoned: 0, pending: 0, failed: 0 });
    expect(rig.gateway.getCalls).toHaveLength(2);
  });

  it("settles once whichever path runs first: sweep then client read, and client read then sweep", async () => {
    const a = await submitted();
    ended(a.rig);
    await settleOnce(deps(a.rig));
    const r = await a.rig.app.request(`/v1/batch/${a.batchId}`, { headers: { authorization: `Bearer ${a.rig.token}` } });
    expect(r.status).toBe(200);
    await r.text();
    expect(await a.rig.usage.spentOn(today())).toBe(15);

    const b = await submitted();
    ended(b.rig);
    const r2 = await b.rig.app.request(`/v1/batch/${b.batchId}`, { headers: { authorization: `Bearer ${b.rig.token}` } });
    await r2.text();
    expect(await settleOnce(deps(b.rig))).toMatchObject({ checked: 0 });
    expect(await b.rig.usage.spentOn(today())).toBe(15);
  });

  it("closes a batch the provider definitely no longer has, keeping the estimate, and stops asking", async () => {
    const { rig, estimate } = await submitted();
    rig.gateway.getBatch = () => Promise.reject(new UpstreamError("poll", 404));
    expect(await settleOnce(deps(rig))).toEqual({ checked: 1, settled: 0, abandoned: 1, pending: 0, failed: 0 });
    expect(await rig.usage.spentOn(today())).toBe(estimate);
    expect(await settleOnce(deps(rig))).toMatchObject({ checked: 0 });
  });

  it("keeps a batch for the next tick after a transport, 5xx or non-terminal 4xx failure, and never throws", async () => {
    const { rig, estimate } = await submitted();
    // 429/401/403 say nothing about the batch itself: a rate limit or a key problem on the poll
    // must not freeze the over-estimate as the final figure.
    for (const status of [NO_RESPONSE, 500, 408, 429, 401, 403]) {
      rig.gateway.getBatch = () => Promise.reject(new UpstreamError("poll", status));
      expect(await settleOnce(deps(rig))).toEqual({ checked: 1, settled: 0, abandoned: 0, pending: 0, failed: 1 });
    }
    rig.gateway.getBatch = () => Promise.reject(new Error("unexpected"));
    expect(await settleOnce(deps(rig))).toMatchObject({ failed: 1 });
    expect(await rig.usage.spentOn(today())).toBe(estimate);
    rig.usage.unsettledBatches = () => Promise.reject(new Error("ledger down"));
    expect(await settleOnce(deps(rig))).toEqual({ checked: 0, settled: 0, abandoned: 0, pending: 0, failed: 1 });
  });

  it("bounds a tick and skips batches older than the settlement window", async () => {
    const { rig } = await submitted();
    for (let i = 0; i < 3; i++) {
      rig.gateway.nextBatchId = `rb_fake_${i + 2}`;
      expect((await postBatch(rig, { ...sampleBody, client_request_id: `night-${i}` })).status).toBe(202);
    }
    expect(await settleOnce({ ...deps(rig), maxPerTick: 2 })).toMatchObject({ checked: 2 });
    // A batch reserved ten days ago is outside the window: the provider no longer has it.
    const old = await submitted({ now: () => new Date(Date.now() - 10 * 86_400_000) });
    expect(await settleOnce({ ...deps(old.rig), now: () => new Date() })).toMatchObject({ checked: 0 });
  });

  it("does not re-poll a batch found still processing until the recheck holdoff has passed", async () => {
    const t0 = Date.now();
    let now = new Date(t0);
    const { rig } = await submitted({ now: () => now });
    const holdoff = { ...deps(rig), recheckSeconds: DEFAULT_SETTLE_RECHECK_SECONDS, now: () => now };
    expect(await settleOnce(holdoff)).toMatchObject({ checked: 1, pending: 1 });
    // Within the holdoff the batch is left alone, even though it has ended by now.
    ended(rig);
    now = new Date(t0 + (DEFAULT_SETTLE_RECHECK_SECONDS - 1) * 1000);
    expect(await settleOnce(holdoff)).toEqual({ checked: 0, settled: 0, abandoned: 0, pending: 0, failed: 0 });
    expect(rig.gateway.getCalls).toHaveLength(1);
    // Past it, the batch is polled again and settled.
    now = new Date(t0 + (DEFAULT_SETTLE_RECHECK_SECONDS + 1) * 1000);
    expect(await settleOnce(holdoff)).toMatchObject({ checked: 1, settled: 1 });
    expect(await rig.usage.spentOn(today())).toBe(15);
  });

  it("walks unsettled batches in (date, batch id) order so overlapping ticks agree", async () => {
    const { rig } = await submitted();
    for (const id of ["rb_fake_c", "rb_fake_a", "rb_fake_b"]) {
      rig.gateway.nextBatchId = id;
      expect((await postBatch(rig, { ...sampleBody, client_request_id: id })).status).toBe(202);
    }
    const listed = await rig.usage.unsettledBatches(10, "2000-01-01", Number.MAX_SAFE_INTEGER);
    const ids = listed.map((b) => b.batchId);
    expect(ids).toEqual([...ids].sort());
    expect(listed).toHaveLength(4);
    expect(await rig.usage.unsettledBatches(2, "2000-01-01", Number.MAX_SAFE_INTEGER)).toEqual(listed.slice(0, 2));
  });

  it("reads the per-tick budget and the holdoff from the environment, bounded", () => {
    const env = { ANTHROPIC_API_KEY: "sk-ant-test-not-a-real-key", LICENSE_PUBKEY_B64: (releaseKey as { ed25519: string }).ed25519, DAILY_SPEND_CAP_USD: "10", BATCH_PROVIDER: "anthropic", OPENAI_API_KEY: "sk-test-not-a-real-key" };
    const config = relayConfigFromEnv(env);
    expect(config.settleMaxPerTick).toBe(DEFAULT_SETTLE_MAX_PER_TICK);
    expect(config.settleRecheckSec).toBe(DEFAULT_SETTLE_RECHECK_SECONDS);
    expect(DEFAULT_SETTLE_MAX_PER_TICK).toBe(500);
    expect(MAX_SETTLE_MAX_PER_TICK).toBe(2000);
    expect(relayConfigFromEnv({ ...env, SETTLE_MAX_PER_TICK: "2000", SETTLE_RECHECK_SECONDS: "0" })).toMatchObject({ settleMaxPerTick: 2000, settleRecheckSec: 0 });
    for (const bad of ["0", "2001", "fifty"]) expect(() => relayConfigFromEnv({ ...env, SETTLE_MAX_PER_TICK: bad })).toThrow("invalid SETTLE_MAX_PER_TICK");
    expect(() => relayConfigFromEnv({ ...env, SETTLE_RECHECK_SECONDS: "-1" })).toThrow("invalid SETTLE_RECHECK_SECONDS");
  });
});

describe("sweepOnce", () => {
  it("runs the store's retention maintenance before settling, and a maintenance failure is one log line", async () => {
    const { rig } = await submitted();
    // A day counter and a spend day older than the retention window, visible before the sweep.
    const stale = utcDate(new Date(Date.now() - (USAGE_DAY_KEEP_DAYS + 5) * 86_400_000));
    expect(await rig.usage.tryReserve("lic_old", stale, 3, 10)).toBe("ok");
    expect(await rig.usage.usedOn("lic_old", stale)).toBe(3);
    ended(rig);
    const log: string[] = [];
    expect(await sweepOnce(deps(rig, log))).toMatchObject({ checked: 1, settled: 1 });
    expect(await rig.usage.usedOn("lic_old", stale)).toBe(0);
    expect(await rig.usage.usedOn("lic_test_1", today())).toBe(2); // today's counter is kept
    expect(log).toEqual([]);

    rig.usage.maintenance = () => Promise.reject(new Error("locked"));
    expect(await sweepOnce(deps(rig, log))).toMatchObject({ checked: 0 });
    expect(log).toEqual(["store_maintenance_failure"]);
  });
});

describe("startPeriodicSweep", () => {
  it("runs on the interval, logs counts only, and is disabled at 0", async () => {
    vi.useFakeTimers();
    const { rig, batchId } = await submitted();
    ended(rig);
    const log: string[] = [];
    const stop = startPeriodicSweep(deps(rig, log), 60);
    await vi.advanceTimersByTimeAsync(60_000);
    stop();
    expect(log).toHaveLength(1);
    expect(log[0]).toMatch(/^settlement_sweep \{"checked":1,"settled":1,/);
    for (const needle of [batchId, "rb_fake_1", "lic_test_1", "dev_test_device", "TOP-SECRET"]) expect(log[0]).not.toContain(needle);
    expect(await rig.usage.spentOn(today())).toBe(15);

    const off = startPeriodicSweep(deps(rig, log), 0);
    await vi.advanceTimersByTimeAsync(10 * 60_000);
    off();
    expect(log).toHaveLength(1);
  });
});
