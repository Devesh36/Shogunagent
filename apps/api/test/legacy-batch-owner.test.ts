/** Batch ownership on GET /v1/batch/:id (§4.1): keyed work is device-bound for good; device-less
 * legacy owner rows are readable account-wide only inside their seven-day window. */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { SqlUsageStore } from "../src/pg-store.js";
import { InMemoryUsageStore, utcDate, type UsageStore } from "../src/usage.js";
import { makeRig, postBatch, sampleBody, signLicense } from "./helpers.js";
import { sqliteDouble } from "./sql-double.js";

const daysAgo = (days: number) => utcDate(new Date(Date.now() - days * 86_400_000));

const stores: Array<[string, () => UsageStore]> = [
  ["memory", () => new InMemoryUsageStore()],
  ["postgres double", () => new SqlUsageStore(sqliteDouble())],
];

describe("device-bound ownership", () => {
  it("blocks a second device on the same licence before any provider read", async () => {
    const rig = await makeRig();
    const result = await postBatch(rig, { ...sampleBody, client_request_id: "owned-night" });
    const { batch_id } = await result.json() as { batch_id: string };
    const otherDevice = await signLicense(rig.keys.privateKey, { device: "other-device" });
    expect((await rig.app.request(`/v1/batch/${batch_id}`, { headers: { authorization: `Bearer ${otherDevice}` } })).status).toBe(404);
    expect(rig.gateway.getCalls).toHaveLength(0);
    expect((await rig.app.request(`/v1/batch/${batch_id}`, { headers: { authorization: `Bearer ${rig.token}` } })).status).toBe(200);
    expect(rig.gateway.getCalls).toEqual(["rb_fake_1"]);
    expect(batch_id).not.toBe("rb_fake_1");
  });
});

describe("device-less legacy batch recovery", () => {
  beforeEach(() => {
    vi.useFakeTimers({ toFake: ["Date"] });
    vi.setSystemTime(new Date("2026-09-27T12:00:00Z"));
  });
  afterEach(() => { vi.useRealTimers(); });

  for (const [backend, open] of stores) {
    it(`${backend}: recovers existing work without another provider submission or reservation`, async () => {
      const usage = open();
      expect(await usage.tryReserve("lic_test_1", utcDate(), 3, 100)).toBe("ok");
      await usage.attachBatch("rb_legacy", "lic_test_1", daysAgo(7), 3, undefined, "msgbatch_legacy");
      const rig = await makeRig({ usage });
      const poll = (token: string) => rig.app.request("/v1/batch/rb_legacy", { headers: { authorization: `Bearer ${token}` } });
      expect((await poll(rig.token)).status).toBe(200);
      const sameAccountOtherDevice = await signLicense(rig.keys.privateKey, { device: "dev_other" });
      expect((await poll(sameAccountOtherDevice)).status).toBe(200);
      const otherAccount = await signLicense(rig.keys.privateKey, { sub: "lic_other" });
      expect((await poll(otherAccount)).status).toBe(404);
      expect(rig.gateway.getCalls).toEqual(["msgbatch_legacy", "msgbatch_legacy"]);
      expect(rig.gateway.createCalls).toHaveLength(0);
      expect(await usage.usedOn("lic_test_1", utcDate())).toBe(3);
    });

    it(`${backend}: rejects expired, invalid and future legacy owner dates`, async () => {
      for (const [i, date] of [daysAgo(8), "not-a-date", "2026-02-30", daysAgo(-1)].entries()) {
        const usage = open();
        await usage.attachBatch(`rb_bad_${i}`, "lic_test_1", date, 1);
        const rig = await makeRig({ usage });
        expect(await usage.batchOwner(`rb_bad_${i}`, "dev_test_device")).toBeUndefined();
        expect((await rig.app.request(`/v1/batch/rb_bad_${i}`, { headers: { authorization: `Bearer ${rig.token}` } })).status).toBe(404);
        expect(rig.gateway.getCalls).toHaveLength(0);
        expect(rig.gateway.createCalls).toHaveLength(0);
      }
    });

    it(`${backend}: keeps keyed work device-bound even if a legacy owner loses its device`, async () => {
      const usage = open();
      const key = { licenseId: "lic_test_1", deviceId: "dev_test_device", purpose: "mail_triage", clientRequestId: "keyed-1" };
      const claim = await usage.claimIdempotency({ ...key, payloadHash: "hash", date: daysAgo(90) });
      if (claim.kind !== "fresh") throw new Error("fresh claim expected");
      const batchId = await usage.bindIdempotency({ ...key, claimToken: claim.claimToken }, "msgbatch_keyed");
      await usage.attachBatch(batchId, key.licenseId, utcDate(), 1);
      expect(await usage.batchOwner(batchId, key.deviceId)).toBe(key.licenseId);
      expect(await usage.batchOwner(batchId, "dev_other")).toBeUndefined();
      const rig = await makeRig({ usage });
      const otherDevice = await signLicense(rig.keys.privateKey, { device: "dev_other" });
      expect((await rig.app.request(`/v1/batch/${batchId}`, { headers: { authorization: `Bearer ${otherDevice}` } })).status).toBe(404);
      expect(rig.gateway.getCalls).toHaveLength(0);
    });

    it(`${backend}: maintenance ages out legacy owner rows but never keyed ownership`, async () => {
      const usage = open();
      const key = { licenseId: "lic_test_1", deviceId: "dev_test_device", purpose: "mail_triage", clientRequestId: "keyed-old" };
      const claim = await usage.claimIdempotency({ ...key, payloadHash: "hash", date: daysAgo(90) });
      if (claim.kind !== "fresh") throw new Error("fresh claim expected");
      const keyed = await usage.bindIdempotency({ ...key, claimToken: claim.claimToken }, "msgbatch_keyed_old");
      await usage.attachBatch(keyed, key.licenseId, daysAgo(90), 1, key.deviceId, "msgbatch_keyed_old");
      await usage.attachBatch("rb_legacy_old", "lic_test_1", daysAgo(30), 1, undefined, "msgbatch_legacy_old");
      await usage.attachBatch("rb_legacy_fresh", "lic_test_1", daysAgo(2), 1, undefined, "msgbatch_legacy_fresh");
      await usage.maintenance(utcDate(), Date.now());
      expect(await usage.providerBatchId("rb_legacy_old")).toBeUndefined();
      expect(await usage.providerBatchId("rb_legacy_fresh")).toBe("msgbatch_legacy_fresh");
      expect(await usage.providerBatchId(keyed)).toBe("msgbatch_keyed_old");
      expect(await usage.batchOwner(keyed, key.deviceId)).toBe(key.licenseId);
    });
  }
});
