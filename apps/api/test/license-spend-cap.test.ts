/** Audit 2026-10-10 H1: the per-licence daily spend budget, the mandatory global breaker, and the
 * USD settings that configure them. */
import { describe, expect, it } from "vitest";

import { spendCapRequirementError } from "../src/boot.js";
import { UpstreamError } from "../src/gateway.js";
import { MODEL_BY_CLASS } from "../src/models.js";
import { DEFAULT_BATCH_PRICES, usdCapMicrosFromEnv } from "../src/spend.js";
import { DEFAULT_LICENSE_DAILY_SPEND_CAP_USD, DEFAULT_TRIAL_DAILY_SPEND_CAP_USD } from "../src/boot.js";
import { utcDate } from "../src/usage.js";
import { makeRig, postBatch, sampleBody, signLicense, type Rig } from "./helpers.js";

const breaker = { capMicros: 100_000_000, prices: DEFAULT_BATCH_PRICES }; // $100: never the binding limit here
const today = () => utcDate(new Date());

/** Micro-USD one `sampleBody` submission reserves, measured rather than hard-coded. */
async function oneSubmission(): Promise<number> {
  const probe = await makeRig({ spendBreaker: breaker, licenseSpendCapMicros: 1_000_000_000 });
  expect((await postBatch(probe, sampleBody)).status).toBe(202);
  const one = await probe.usage.licenseSpentOn("lic_test_1", today());
  expect(one).toBeGreaterThan(0);
  return one;
}

function resultLine(customId: string, usage: Record<string, number>, model = MODEL_BY_CLASS.classify): string {
  return JSON.stringify({ custom_id: customId, result: { type: "succeeded", message: { model, content: [{ type: "text", text: "{}" }], usage } } });
}

async function readResults(rig: Rig, batchId: string, token = rig.token): Promise<void> {
  const r = await rig.app.request(`/v1/batch/${batchId}`, { headers: { authorization: `Bearer ${token}` } });
  expect(r.status).toBe(200);
  await r.text();
}

describe("per-licence daily spend budget (H1)", () => {
  it("answers 429 license_spend_cap with no Retry-After and no provider call once a licence's day is spent", async () => {
    const one = await oneSubmission();
    const rig = await makeRig({ spendBreaker: breaker, licenseSpendCapMicros: Math.floor(one * 1.5) });
    expect((await postBatch(rig, sampleBody)).status).toBe(202);
    const over = await postBatch(rig, sampleBody);
    expect(over.status).toBe(429);
    expect(over.headers.get("retry-after")).toBeNull();
    expect(await over.json()).toMatchObject({ reason: "license_spend_cap", error: "daily licence spend cap reached" });
    expect(rig.gateway.createCalls).toHaveLength(1);
    expect(rig.logLines.at(-1)).toContain("event=license_spend_cap_reached");
    // Nothing of the refused submission was held: not chunks, not global spend, not the licence's.
    expect(await rig.usage.usedOn("lic_test_1", today())).toBe(2);
    expect(await rig.usage.spentOn(today())).toBe(one);
    expect(await rig.usage.licenseSpentOn("lic_test_1", today())).toBe(one);
  });

  it("is per licence: another licence on the same relay still submits", async () => {
    const one = await oneSubmission();
    const rig = await makeRig({ spendBreaker: breaker, licenseSpendCapMicros: Math.floor(one * 1.5) });
    const other = await signLicense(rig.keys.privateKey, { sub: "lic_other" });
    expect((await postBatch(rig, sampleBody)).status).toBe(202);
    expect((await postBatch(rig, sampleBody)).status).toBe(429);
    expect((await postBatch(rig, sampleBody, other)).status).toBe(202);
    expect(await rig.usage.licenseSpentOn("lic_other", today())).toBe(one);
  });

  it("holds a trial device to the trial budget, never above the licence budget", async () => {
    const one = await oneSubmission();
    const trialOf = (rig: Rig) => signLicense(rig.keys.privateKey, { sub: "trial_dev", plan: "trial", status: "trialing" });
    const tight = await makeRig({ spendBreaker: breaker, trialDailyChunkCap: 50, licenseSpendCapMicros: one * 10, trialSpendCapMicros: Math.floor(one * 1.5) });
    const trial = await trialOf(tight);
    expect((await postBatch(tight, sampleBody, trial)).status).toBe(202);
    const over = await postBatch(tight, sampleBody, trial);
    expect(over.status).toBe(429);
    expect(((await over.json()) as { reason: string }).reason).toBe("license_spend_cap");
    expect((await postBatch(tight, sampleBody)).status).toBe(202); // the paid licence has its own budget
    // A trial budget configured above the licence budget is clamped to it.
    const clamped = await makeRig({ spendBreaker: breaker, trialDailyChunkCap: 50, licenseSpendCapMicros: Math.floor(one * 1.5), trialSpendCapMicros: one * 100 });
    const trial2 = await trialOf(clamped);
    expect((await postBatch(clamped, sampleBody, trial2)).status).toBe(202);
    expect((await postBatch(clamped, sampleBody, trial2)).status).toBe(429);
  });

  it("works without the global breaker (development relay): the licence budget alone refuses", async () => {
    const one = await oneSubmission();
    const rig = await makeRig({ licenseSpendCapMicros: Math.floor(one * 1.5) });
    expect((await postBatch(rig, sampleBody)).status).toBe(202);
    const over = await postBatch(rig, sampleBody);
    expect(over.status).toBe(429);
    expect(((await over.json()) as { reason: string }).reason).toBe("license_spend_cap");
    expect(rig.gateway.createCalls).toHaveLength(1);
  });

  it("gives the licence its estimate back when the provider definitely rejects the create", async () => {
    const rig = await makeRig({ spendBreaker: breaker, licenseSpendCapMicros: 1_000_000 });
    rig.gateway.createBatch = () => Promise.reject(new UpstreamError("create", 400));
    expect((await postBatch(rig, sampleBody)).status).toBe(502);
    expect(await rig.usage.licenseSpentOn("lic_test_1", today())).toBe(0);
    expect(await rig.usage.spentOn(today())).toBe(0);
  });

  it("settles the licence's counter to the provider-reported usage along with the global one", async () => {
    const rig = await makeRig({ spendBreaker: breaker, licenseSpendCapMicros: 1_000_000 });
    const res = await postBatch(rig, sampleBody);
    expect(res.status).toBe(202);
    const { batch_id } = (await res.json()) as { batch_id: string };
    expect(await rig.usage.licenseSpentOn("lic_test_1", today())).toBeGreaterThan(1_000);
    rig.gateway.status = { ...rig.gateway.status, processing_status: "ended", request_counts: { processing: 0, succeeded: 2, errored: 0, canceled: 0, expired: 0 } };
    rig.gateway.resultLines = [resultLine("1234", { input_tokens: 100, output_tokens: 10 }), resultLine("5678", { input_tokens: 100, output_tokens: 10 })];
    await readResults(rig, batch_id);
    // 200 input × 0.05 + 20 output × 0.25 = 15 micro-USD (Haiku 5.5), on both counters.
    expect(await rig.usage.licenseSpentOn("lic_test_1", today())).toBe(15);
    expect(await rig.usage.spentOn(today())).toBe(15);
    await readResults(rig, batch_id);
    expect(await rig.usage.licenseSpentOn("lic_test_1", today())).toBe(15);
  });

  it("counts the refusal in the metrics as its own outcome, with no ids", async () => {
    const one = await oneSubmission();
    const rig = await makeRig({ metricsToken: "ops", spendBreaker: breaker, licenseSpendCapMicros: Math.floor(one * 1.5) });
    expect((await postBatch(rig, sampleBody)).status).toBe(202);
    expect((await postBatch(rig, sampleBody)).status).toBe(429);
    const res = await rig.app.request("/metrics", { headers: { authorization: "Bearer ops" } });
    const text = await res.text();
    const snap = JSON.parse(text) as { batch: { plans: Record<string, { reservations: Record<string, number> }> } };
    expect(snap.batch.plans.standard?.reservations).toMatchObject({ ok: 1, license_spend_capped: 1 });
    expect(text).not.toContain("lic_test_1");
  });
});

describe("USD settings and the mandatory breaker", () => {
  it("parses plain decimal USD into micro-USD, with the default when unset, and refuses anything else by name", () => {
    expect(usdCapMicrosFromEnv(undefined, "LICENSE_DAILY_SPEND_CAP_USD", DEFAULT_LICENSE_DAILY_SPEND_CAP_USD)).toBe(500_000);
    expect(usdCapMicrosFromEnv(" ", "TRIAL_DAILY_SPEND_CAP_USD", DEFAULT_TRIAL_DAILY_SPEND_CAP_USD)).toBe(250_000);
    expect(usdCapMicrosFromEnv("0.25", "X", 1)).toBe(250_000);
    expect(usdCapMicrosFromEnv("0", "X", 1)).toBe(0);
    for (const bad of ["-1", "abc", "$3", "3 USD", "1e3", "NaN", "Infinity"]) {
      expect(() => usdCapMicrosFromEnv(bad, "LICENSE_DAILY_SPEND_CAP_USD", 3)).toThrow(/LICENSE_DAILY_SPEND_CAP_USD/);
      try { usdCapMicrosFromEnv(bad, "LICENSE_DAILY_SPEND_CAP_USD", 3); } catch (e) { expect((e as Error).message).not.toContain(bad); }
    }
  });

  it("refuses to boot without DAILY_SPEND_CAP_USD unless RELAY_ALLOW_NO_SPEND_CAP=1 says so explicitly", () => {
    expect(spendCapRequirementError({})).toMatch(/DAILY_SPEND_CAP_USD/);
    expect(spendCapRequirementError({ DAILY_SPEND_CAP_USD: "  " })).toMatch(/DAILY_SPEND_CAP_USD/);
    expect(spendCapRequirementError({ RELAY_ALLOW_NO_SPEND_CAP: "true" })).toMatch(/DAILY_SPEND_CAP_USD/);
    expect(spendCapRequirementError({ RELAY_ALLOW_NO_SPEND_CAP: "1" })).toBeUndefined();
    expect(spendCapRequirementError({ DAILY_SPEND_CAP_USD: "25" })).toBeUndefined();
    expect(spendCapRequirementError({ DAILY_SPEND_CAP_USD: "0" })).toBeUndefined(); // the emergency stop is a value
  });
});
