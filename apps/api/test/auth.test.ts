/** §4.1/§4.5: license verification — valid, expired, wrong key, missing, plan-gated. */
import { describe, expect, it } from "vitest";

import { DEFAULT_BATCH_PRICES } from "../src/spend.js";
import { makeKeys, makeRig, postBatch, sampleBody, signLicense } from "./helpers.js";

/** A relay whose trial lane is open: both of the trial's ceilings are configured (issue #763). */
const trialLane = { spendBreaker: { capMicros: 5_000_000, prices: DEFAULT_BATCH_PRICES }, trialDailyChunkCap: 50 };

describe("POST /v1/batch auth", () => {
  it("accepts a valid license and returns 202 with the batch id", async () => {
    const rig = await makeRig();
    const res = await postBatch(rig, sampleBody);
    expect(res.status).toBe(202);
    const body = (await res.json()) as { batch_id: string; accepted: number };
    expect(body.batch_id).toMatch(/^rb_[a-f0-9-]{36}$/);
    expect(body.accepted).toBe(2);
  });

  it("rejects an expired token with 401 and never reaches the gateway", async () => {
    const rig = await makeRig();
    const expired = await signLicense(rig.keys.privateKey, { expiresIn: "-1h" });
    const res = await postBatch(rig, sampleBody, expired);
    expect(res.status).toBe(401);
    expect(rig.gateway.createCalls).toHaveLength(0);
  });

  it("rejects a token signed with a different key with 401", async () => {
    const rig = await makeRig();
    const stranger = await makeKeys();
    const forged = await signLicense(stranger.privateKey);
    const res = await postBatch(rig, sampleBody, forged);
    expect(res.status).toBe(401);
    expect(rig.gateway.createCalls).toHaveLength(0);
  });

  it("rejects a missing Authorization header with 401", async () => {
    const rig = await makeRig();
    const res = await rig.app.request("/v1/batch", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(sampleBody),
    });
    expect(res.status).toBe(401);
  });

  it("rejects a plan outside the Batch lane with 402 (§4.5)", async () => {
    const rig = await makeRig();
    const freeloader = await signLicense(rig.keys.privateKey, { plan: "expired-trial" });
    const res = await postBatch(rig, sampleBody, freeloader);
    expect(res.status).toBe(402);
    expect(rig.gateway.createCalls).toHaveLength(0);
  });

  it("guards GET /v1/batch/:id with the same verification", async () => {
    const rig = await makeRig();
    const res = await rig.app.request("/v1/batch/rb_1");
    expect(res.status).toBe(401);
    expect(rig.gateway.getCalls).toHaveLength(0);
  });

  it("accepts the 7-day trial's token (issue #763)", async () => {
    const rig = await makeRig(trialLane);
    const trial = await signLicense(rig.keys.privateKey, { plan: "trial", status: "trialing" });
    const res = await postBatch(rig, sampleBody, trial);
    expect(res.status).toBe(202);
    expect(rig.gateway.createCalls).toHaveLength(1);
  });

  it("refuses every trial token with 402 unless both of its ceilings are configured", async () => {
    // A trial token is minted for any device id, so it is never served on the per-licence cap
    // alone: no spend breaker, or no trial cap, closes the lane. A paid licence is unaffected.
    for (const deps of [{}, { spendBreaker: trialLane.spendBreaker }, { trialDailyChunkCap: 50 }]) {
      const rig = await makeRig(deps);
      const trial = await signLicense(rig.keys.privateKey, { plan: "trial", status: "trialing" });
      expect((await postBatch(rig, sampleBody, trial)).status).toBe(402);
      for (const [method, path] of [["GET", "/v1/batch/rb_1"], ["POST", "/v1/batch/recover"]] as const) {
        const res = await rig.app.request(path, { method, headers: { authorization: `Bearer ${trial}` } });
        expect(res.status).toBe(402);
      }
      expect(rig.gateway.createCalls).toHaveLength(0);
      expect(rig.usage.records).toHaveLength(0);
      expect((await postBatch(rig, sampleBody)).status).toBe(202);
    }
  });

  it("rejects a trial token with any status but trialing with 402", async () => {
    const rig = await makeRig(trialLane);
    for (const status of ["active", "past_due"]) {
      const trial = await signLicense(rig.keys.privateKey, { plan: "trial", status });
      const res = await postBatch(rig, sampleBody, trial);
      expect(res.status).toBe(402);
    }
    expect(rig.gateway.createCalls).toHaveLength(0);
  });

  it("answers an ENDED trial with 402 plan_required on every relay route, a lapsed token with 401", async () => {
    // #940: a trial whose signed `period_end` has passed is a plan question, not a stale token.
    // The Mac maps 402 to "the plan does not cover this lane" (QuotaExhausted) and stops; a 401
    // would first send it to re-verify a licence it does not have. A trial token that only ran
    // out mid-trial (period_end ahead) must stay 401: that is how the Mac refreshes it (#777).
    const rig = await makeRig({ ...trialLane, reviewer: async () => { throw new Error("never called"); } });
    const now = Math.floor(Date.now() / 1000);
    const ended = await signLicense(rig.keys.privateKey, { plan: "trial", status: "trialing", iat: now - 7200, exp: now - 60, periodEnd: now - 60 });
    const lapsed = await signLicense(rig.keys.privateKey, { plan: "trial", status: "trialing", iat: now - 7200, exp: now - 60, periodEnd: now + 86_400 });
    const legacy = await signLicense(rig.keys.privateKey, { plan: "trial", status: "trialing", iat: now - 7200, exp: now - 60 });
    const routes = [
      ["POST", "/v1/batch", JSON.stringify({ client_request_id: "r1", ...sampleBody })],
      ["GET", "/v1/batch/rb_1", undefined],
      ["POST", "/v1/batch/recover", JSON.stringify({ purpose: "consolidation", client_request_id: "r1" })],
      ["POST", "/v1/review", JSON.stringify({ purpose: "consolidation", system: "s", user: "u" })],
    ] as const;
    for (const [method, path, body] of routes) {
      const send = (token: string) => rig.app.request(path, {
        method, headers: { authorization: `Bearer ${token}`, "content-type": "application/json" }, ...(body === undefined ? {} : { body }),
      });
      const res = await send(ended);
      expect(res.status, `${method} ${path}`).toBe(402);
      expect(await res.json()).toEqual({ error: "plan_required" });
      expect((await send(lapsed)).status, `${method} ${path} lapsed`).toBe(401);
      // No `period_end` claim: the relay cannot tell the trial ended, so it is a stale token.
      expect((await send(legacy)).status, `${method} ${path} legacy`).toBe(401);
    }
    // An expired paid token stays 401 whatever its period_end says.
    const paid = await signLicense(rig.keys.privateKey, { iat: now - 7200, exp: now - 60, periodEnd: now - 60 });
    expect((await postBatch(rig, sampleBody, paid)).status).toBe(401);
    expect(rig.gateway.createCalls).toHaveLength(0);
    expect(rig.usage.records).toHaveLength(0);
  });

  it("rejects a cancelled subscription's token with 402 even before exp", async () => {
    const rig = await makeRig();
    const cancelled = await signLicense(rig.keys.privateKey, { status: "canceled" });
    const res = await postBatch(rig, sampleBody, cancelled);
    expect(res.status).toBe(402);
    expect(rig.gateway.createCalls).toHaveLength(0);
  });
});

describe("GET /v1/batch/:id ownership (§4.1)", () => {
  it("404s another licence's batch id and never touches the gateway", async () => {
    const rig = await makeRig();
    // lic_test_1 creates the batch…
    expect((await postBatch(rig, sampleBody)).status).toBe(202);
    // …and a different (validly licensed!) user tries to read it back.
    const other = await signLicense(rig.keys.privateKey, { sub: "lic_other" });
    const res = await rig.app.request("/v1/batch/rb_fake_1", {
      headers: { authorization: `Bearer ${other}` },
    });
    expect(res.status).toBe(404);
    expect(rig.gateway.getCalls).toHaveLength(0);
    expect(rig.gateway.resultsCalls).toHaveLength(0);
  });

  it("404s a batch id this relay never issued (no oracle for probing)", async () => {
    const rig = await makeRig();
    const res = await rig.app.request("/v1/batch/msgbatch_guessed", {
      headers: { authorization: `Bearer ${rig.token}` },
    });
    expect(res.status).toBe(404);
    expect(rig.gateway.getCalls).toHaveLength(0);
  });

  it("still serves the creator its own batch", async () => {
    const rig = await makeRig();
    const submitted = await postBatch(rig, sampleBody);
    expect(submitted.status).toBe(202);
    const { batch_id } = await submitted.json() as { batch_id: string };
    const res = await rig.app.request(`/v1/batch/${batch_id}`, {
      headers: { authorization: `Bearer ${rig.token}` },
    });
    expect(res.status).toBe(200);
    expect(rig.gateway.getCalls).toEqual(["rb_fake_1"]);
  });
});

describe("device claim (#334)", () => {
  it("rejects a token with no device claim with 401 and never reaches the gateway", async () => {
    const rig = await makeRig();
    const undevised = await signLicense(rig.keys.privateKey, { device: null });
    const res = await postBatch(rig, sampleBody, undevised);
    expect(res.status).toBe(401);
    expect(rig.gateway.createCalls).toHaveLength(0);
  });

  it("rejects an empty device claim", async () => {
    const rig = await makeRig();
    const blank = await signLicense(rig.keys.privateKey, { device: "" });
    expect((await postBatch(rig, sampleBody, blank)).status).toBe(401);
    expect(rig.gateway.createCalls).toHaveLength(0);
  });
});
