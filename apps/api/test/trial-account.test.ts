/** Audit 2026-10-20 H1 (relay part): a trial issued through an account is metered per ACCOUNT, not
 * per device id, so asking for more device ids does not buy more trial budget. Ownership of a
 * batch stays licence + device. */
import { describe, expect, it } from "vitest";

import { bucketKey } from "../src/app.js";
import { meterSubject, verifyLicense } from "../src/auth.js";
import { UpstreamError } from "../src/gateway.js";
import { DEFAULT_BATCH_PRICES } from "../src/spend.js";
import { utcDate } from "../src/usage.js";
import { makeRig, postBatch, sampleBody, signLicense, type Rig } from "./helpers.js";

const NOW = new Date("2026-10-20T03:00:00Z");
const DATE = utcDate(NOW);
const iat = Math.floor(NOW.getTime() / 1000);

const trial = (r: Rig, device: string, acc?: string) => signLicense(r.keys.privateKey, {
  sub: `trial_${device}`, plan: "trial", status: "trialing", device, iat, ...(acc ? { acc } : {}),
});

async function trialRig(extra: Parameters<typeof makeRig>[0] = {}) {
  return makeRig({
    now: () => NOW,
    dailyChunkCap: 100,
    trialDailyChunkCap: 3,
    spendBreaker: { capMicros: 1_000_000_000, prices: DEFAULT_BATCH_PRICES },
    ...extra,
  });
}

describe("trial metering per account (H1)", () => {
  it("meterSubject: account for an account's trial, licence otherwise", async () => {
    const r = await trialRig();
    const verify = async (t: string) => verifyLicense(`Bearer ${t}`, r.keys.publicKey, iat);
    expect(meterSubject(await verify(await trial(r, "a", "acc_1")))).toBe("account:acc_1");
    expect(meterSubject(await verify(await trial(r, "a")))).toBe("trial_a");
    // A paid licence keeps its licence key, account or not.
    const paid = await signLicense(r.keys.privateKey, { sub: "lic_paid", acc: "acc_1", iat });
    expect(meterSubject(await verify(paid))).toBe("lic_paid");
    // The device stays in the rate key; the payer prefix follows the meter.
    expect(bucketKey(await verify(await trial(r, "a", "acc_1")), "batch_submit")).toBe("account:acc_1\u0000a\u0000batch_submit");
    // A malformed `acc` is a forged-looking token, not a free new budget.
    await expect(verify(await signLicense(r.keys.privateKey, { acc: "bad acc!", iat }))).rejects.toThrow();
  });

  it("two device ids under the same account share one trial chunk budget", async () => {
    const r = await trialRig();
    const a = await trial(r, "dev_a", "acc_1");
    const b = await trial(r, "dev_b", "acc_1");
    expect((await postBatch(r, sampleBody, a)).status).toBe(202); // 2 of 3
    const second = await postBatch(r, sampleBody, b); // 4 > 3 across the account
    expect(second.status).toBe(429);
    expect(await second.json()).toMatchObject({ reason: "daily_cap" });
    expect(await r.usage.usedOn("account:acc_1", DATE)).toBe(2);
    expect(await r.usage.usedOn("trial_dev_a", DATE)).toBe(0);
    // Another account, and a device-only trial, have budgets of their own.
    expect((await postBatch(r, sampleBody, await trial(r, "dev_c", "acc_2"))).status).toBe(202);
    expect((await postBatch(r, sampleBody, await trial(r, "dev_d"))).status).toBe(202);
  });

  it("two device ids under the same account share one trial spend budget", async () => {
    // A reviewer whose bill is unknown keeps its whole reservation booked against the payer.
    const r = await trialRig({
      reviewer: async () => { throw new UpstreamError("review", 0); },
      licenseSpendCapMicros: 1_000_000,
      trialSpendCapMicros: 20_000,
    });
    const post = async (token: string) => r.app.request("/v1/review", {
      method: "POST", headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      body: JSON.stringify({ purpose: "consolidation", system: "s", user: "u" }),
    });
    expect((await post(await trial(r, "dev_a", "acc_1"))).status).toBe(502);
    const spent = await r.usage.licenseSpentOn("account:acc_1", DATE);
    expect(spent).toBeGreaterThan(10_000);
    const other = await post(await trial(r, "dev_b", "acc_1"));
    expect(other.status).toBe(429);
    expect(await other.json()).toMatchObject({ reason: "license_spend_cap" });
    // A device-only trial is still its own budget.
    expect((await post(await trial(r, "dev_c"))).status).toBe(502);
  });

  it("ownership does not loosen: another device of the same account cannot read the batch", async () => {
    const r = await trialRig();
    const a = await trial(r, "dev_a", "acc_1");
    const b = await trial(r, "dev_b", "acc_1");
    const res = await postBatch(r, { ...sampleBody, items: sampleBody.items.slice(0, 1) }, a);
    expect(res.status).toBe(202);
    const { batch_id } = (await res.json()) as { batch_id: string };
    const get = (t: string) => r.app.request(`/v1/batch/${batch_id}`, { headers: { authorization: `Bearer ${t}` } });
    expect((await get(a)).status).toBe(200);
    expect((await get(b)).status).toBe(404);
  });

  it("the operator denylist (L3) refuses a licence or an account before its tokens expire", async () => {
    const r = await trialRig({ denySubjects: ["account:acc_bad", "lic_revoked"] });
    expect((await postBatch(r, sampleBody, await trial(r, "dev_a", "acc_bad"))).status).toBe(401);
    expect((await postBatch(r, sampleBody, await signLicense(r.keys.privateKey, { sub: "lic_revoked", iat }))).status).toBe(401);
    expect((await r.app.request("/v1/batch/rb_x", { headers: { authorization: `Bearer ${await signLicense(r.keys.privateKey, { sub: "lic_revoked", iat })}` } })).status).toBe(401);
    expect((await postBatch(r, sampleBody, await trial(r, "dev_b", "acc_ok"))).status).toBe(202);
    expect(await r.usage.usedOn("account:acc_bad", DATE)).toBe(0);
  });

  it("a definitely rejected submission refunds the payer's day row, not the owner licence's", async () => {
    const r = await trialRig();
    const a = await trial(r, "dev_a", "acc_1");
    r.gateway.createBatch = async () => { throw new UpstreamError("create", 400); }; // definite rejection
    expect((await postBatch(r, sampleBody, a)).status).toBe(502);
    expect(await r.usage.usedOn("account:acc_1", DATE)).toBe(0);
  });
});
