/** #343: required client_request_id makes retries idempotent at the relay boundary.
 * Every create identifies its original logical request before provider access. */
import { describe, expect, it, vi } from "vitest";

import { UpstreamError, type BatchStatus } from "../src/gateway.js";
import { SqlUsageStore } from "../src/pg-store.js";
import { parseRelayBatchRequest, relayPayloadHash } from "../src/types.js";
import {
  InMemoryUsageStore,
  utcDate,
  type IdempotencyClaim,
  type UsageStore,
} from "../src/usage.js";
import { makeRig, postBatch, sampleBody, signLicense } from "./helpers.js";
import { sqliteDouble } from "./sql-double.js";

/** The two stores: in-memory (development) and the SQL store over the Postgres double. A second
 * rig on the same store instance is a relay that restarted, or a second isolate, against the same
 * ledger. */
const stores: Array<[string, () => UsageStore]> = [
  ["memory", () => new InMemoryUsageStore()],
  ["postgres double", () => new SqlUsageStore(sqliteDouble())],
];

const withId = (id: string, body: unknown = sampleBody) => ({
  ...(body as Record<string, unknown>),
  client_request_id: id,
});

describe("required client_request_id (#343)", () => {
  it("requires an identity before reserving or calling the provider", async () => {
    const rig = await makeRig();
    const res = await rig.app.request("/v1/batch", { method: "POST", headers: { authorization: `Bearer ${rig.token}`, "content-type": "application/json" }, body: JSON.stringify(sampleBody) });
    expect(res.status).toBe(400);
    expect(rig.gateway.createCalls).toHaveLength(0);
    expect(await rig.usage.usedOn("lic_test_1", utcDate())).toBe(0);
  });

  it("replays the same id and payload without a second provider call or a second reservation", async () => {
    const rig = await makeRig({ dailyChunkCap: 2 });
    const first = await postBatch(rig, withId("dream-cycle-1"));
    expect(first.status).toBe(202);
    const firstBody = (await first.json()) as { batch_id: string; accepted: number };

    const second = await postBatch(rig, withId("dream-cycle-1"));
    expect(second.status).toBe(202);
    expect(await second.json()).toEqual(firstBody);
    expect(rig.gateway.createCalls).toHaveLength(1);
    expect(rig.usage.records).toHaveLength(1);
    // Cap is 2 items; a second reservation would 429. Replay must not spend again.
    expect((await postBatch(rig, sampleBody)).status).toBe(429);
  });

  it("returns 409 before the provider when the same id carries a different payload", async () => {
    const rig = await makeRig();
    expect((await postBatch(rig, withId("dream-cycle-1"))).status).toBe(202);
    const conflict = await postBatch(
      rig,
      withId("dream-cycle-1", {
        ...sampleBody,
        items: [{ custom_id: "9999", chunk: "DIFFERENT-CHUNK" }],
      }),
    );
    expect(conflict.status).toBe(409);
    expect(await conflict.json()).toEqual({ error: "client_request_id conflict" });
    expect(rig.gateway.createCalls).toHaveLength(1);
  });

  it("scopes the id per licence, device, and purpose", async () => {
    const rig = await makeRig();
    const other = await signLicense(rig.keys.privateKey, { sub: "lic_other" });
    const otherDevice = await signLicense(rig.keys.privateKey, { device: "dev_other" });
    expect((await postBatch(rig, withId("shared-id"))).status).toBe(202);
    expect((await postBatch(rig, withId("shared-id"), other)).status).toBe(202);
    expect((await postBatch(rig, withId("shared-id"), otherDevice)).status).toBe(202);
    expect((await postBatch(rig, withId("shared-id", {
      ...sampleBody,
      purpose: "morning_brief",
      model_class: "brief",
    }))).status).toBe(202);
    expect(rig.gateway.createCalls).toHaveLength(4);
  });

  it("never calls the provider twice for concurrent duplicates of the same id", async () => {
    const rig = await makeRig();
    const results = await Promise.all([
      postBatch(rig, withId("dream-cycle-1")),
      postBatch(rig, withId("dream-cycle-1")),
    ]);
    const statuses = results.map((r) => r.status).sort();
    expect(rig.gateway.createCalls).toHaveLength(1);
    expect(statuses).toEqual([202, 503]);
  });

  it.each([
    ["transport failure", new Error("upstream down")],
    ["upstream timeout", new UpstreamError("create", 408)],
    ["upstream 5xx", new UpstreamError("create", 503)],
    ["malformed successful response", new UpstreamError("create (malformed response)", 200)],
  ])("keeps the claim after %s, so retry cannot duplicate a paid batch", async (_kind, error) => {
    const rig = await makeRig();
    rig.gateway.createBatch = (requests) => {
      rig.gateway.createCalls.push(requests);
      return Promise.reject(error);
    };
    expect((await postBatch(rig, withId("dream-cycle-1"))).status).toBe(error instanceof UpstreamError ? 502 : 500);
    expect(rig.gateway.createCalls).toHaveLength(1);
    expect(await rig.usage.usedOn("lic_test_1", utcDate())).toBe(2);

    const retry = await postBatch(rig, withId("dream-cycle-1"));
    expect(retry.status).toBe(503);
    expect(retry.headers.get("retry-after")).toBe("1");
    expect(rig.gateway.createCalls).toHaveLength(1);
  });

  it("releases the claim after a definite upstream rejection, so a retry may submit", async () => {
    const rig = await makeRig();
    const realCreate = rig.gateway.createBatch.bind(rig.gateway);
    rig.gateway.createBatch = (requests) => {
      rig.gateway.createCalls.push(requests);
      return Promise.reject(new UpstreamError("create", 400));
    };
    expect((await postBatch(rig, withId("dream-cycle-1"))).status).toBe(502);
    expect(await rig.usage.usedOn("lic_test_1", utcDate())).toBe(0);

    rig.gateway.createBatch = realCreate;
    expect((await postBatch(rig, withId("dream-cycle-1"))).status).toBe(202);
    expect(rig.gateway.createCalls).toHaveLength(2);
  });

  it("keeps the payload hash out of the log line", async () => {
    const rig = await makeRig();
    await postBatch(rig, withId("dream-cycle-1"));
    const logged = rig.logLines.join("\n");
    expect(logged).not.toContain("TOP-SECRET-CHUNK-ALPHA");
    expect(logged).not.toContain("dream-cycle-1");
    expect(logged).not.toMatch(/[a-f0-9]{64}/);
  });
});

describe("client_request_id parsing", () => {
  it("rejects an empty or oversized id before the gateway", () => {
    expect(parseRelayBatchRequest({ ...sampleBody, client_request_id: "" })).toBe(
      "client_request_id must be a non-empty string",
    );
    expect(
      parseRelayBatchRequest({ ...sampleBody, client_request_id: "x".repeat(200) }),
    ).toBe("client_request_id is too long");
  });

  it("hashes purpose, class, and items, and nothing else", () => {
    const a = parseRelayBatchRequest(withId("parse"));
    const b = parseRelayBatchRequest({ ...withId("parse"), model: "claude-opus-4-6" });
    if (typeof a === "string" || typeof b === "string") throw new Error("parse failed");
    expect(relayPayloadHash(a)).toBe(relayPayloadHash(b));
    const c = parseRelayBatchRequest({
      ...withId("parse"),
      items: [{ custom_id: "1234", chunk: "CHANGED" }, sampleBody.items[1]],
    });
    if (typeof c === "string") throw new Error("parse failed");
    expect(relayPayloadHash(a)).not.toBe(relayPayloadHash(c));
  });
});

class AlwaysUnknownStore extends InMemoryUsageStore {
  override claimIdempotency(): Promise<IdempotencyClaim> {
    return Promise.resolve({ kind: "unknown" });
  }
}

class BindFailsStore extends InMemoryUsageStore {
  override bindIdempotency(): Promise<string> {
    return Promise.reject(new Error("disk full"));
  }
}

describe.each(stores)("idempotency across relay instances on the %s store (#343)", (_name, open) => {
  it("replays from a second instance on the same ledger without a second create", async () => {
    const usage = open();
    const firstRig = await makeRig({ usage });
    const first = await postBatch(firstRig, withId("dream-cycle-1"));
    expect(first.status).toBe(202);
    const firstBody = await first.json();

    const secondRig = await makeRig({ usage });
    const second = await postBatch(secondRig, withId("dream-cycle-1"));
    expect(second.status).toBe(202);
    expect(await second.json()).toEqual(firstBody);
    expect(firstRig.gateway.createCalls).toHaveLength(1);
    expect(secondRig.gateway.createCalls).toHaveLength(0);
  });

  it("returns 503 unknown from a second instance while the first is between claim and bind", async () => {
    const usage = open();
    const firstRig = await makeRig({ usage });
    let finish!: (value: BatchStatus) => void;
    const hung = new Promise<BatchStatus>((resolve) => {
      finish = resolve;
    });
    firstRig.gateway.createBatch = (requests) => {
      firstRig.gateway.createCalls.push(requests);
      return hung;
    };
    const first = postBatch(firstRig, withId("dream-cycle-1"));
    await vi.waitFor(() => expect(firstRig.gateway.createCalls).toHaveLength(1));

    const secondRig = await makeRig({ usage });
    const second = await postBatch(secondRig, withId("dream-cycle-1"));
    expect(second.status).toBe(503);
    expect(await second.json()).toMatchObject({ error: "submission unknown" });
    expect(secondRig.gateway.createCalls).toHaveLength(0);

    finish({
      id: "rb_hung",
      processing_status: "in_progress",
      request_counts: { processing: 2, succeeded: 0, errored: 0, canceled: 0, expired: 0 },
    });
    expect((await first).status).toBe(202);
  });

  it("retains bound and unknown tombstones after the old seven-day expiry", async () => {
    const stale = "2026-09-01";
    const date = "2026-09-09";
    const usage = open();
    for (const [clientRequestId, bound] of [["stale-bound", true], ["stale-unbound", false]] as const) {
      const input = {
        licenseId: "lic_test_1",
        deviceId: "dev_test_device",
        purpose: "consolidation",
        clientRequestId,
        payloadHash: "hash",
        date: stale,
      };
      const claim = await usage.claimIdempotency(input);
      expect(claim.kind).toBe("fresh");
      if (claim.kind !== "fresh") throw new Error("not fresh");
      expect(await usage.tryReserve(input.licenseId, stale, 1, 100, { ...input, claimToken: claim.claimToken })).toBe("ok");
      if (bound) await usage.bindIdempotency({ ...input, claimToken: claim.claimToken }, "rb_old");
      // Retention maintenance never touches a tombstone, however old.
      await usage.maintenance(date, Date.now());
      expect(await usage.claimIdempotency({ ...input, date })).toMatchObject({ kind: bound ? "replay" : "unknown" });
    }
  });
});

describe("claimed-but-unbound 503 (#343)", () => {
  it("returns a deterministic submission-unknown 503 without calling the provider", async () => {
    const rig = await makeRig({ usage: new AlwaysUnknownStore() });
    const res = await postBatch(rig, withId("dream-cycle-1"));
    expect(res.status).toBe(503);
    expect(await res.json()).toMatchObject({ error: "submission unknown" });
    expect(res.headers.get("retry-after")).toBe("1");
    expect(rig.gateway.createCalls).toHaveLength(0);
  });

  it("fails closed when binding fails after provider acceptance", async () => {
    const rig = await makeRig({ usage: new BindFailsStore() });
    const res = await postBatch(rig, withId("dream-cycle-1"));
    expect(res.status).toBe(500);
    expect((await postBatch(rig, withId("dream-cycle-1"))).status).toBe(503);
    expect(rig.gateway.createCalls).toHaveLength(1);
  });
});

describe("content-free batch recovery", () => {
  it.each(stores)("recovers a bound request on the %s store without resending deleted inputs, and enforces owner/device/purpose", async (_name, open) => {
    const usage = open();
    const rig = await makeRig({ usage });
    const created = await postBatch(rig, withId("recover-night"));
    const original = await created.json() as { batch_id: string };
    const restarted = await makeRig({ usage });
    const recover = (token = restarted.token, purpose = "consolidation", client_request_id = "recover-night") => restarted.app.request("/v1/batch/recover", {
      method: "POST", headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      body: JSON.stringify({ purpose, client_request_id }),
    });
    expect(await (await recover()).json()).toEqual({ found: true, batch_id: original.batch_id });
    expect(restarted.gateway.createCalls).toHaveLength(0);
    expect(await (await recover(await signLicense(restarted.keys.privateKey, { sub: "other" }))).json()).toEqual({ found: false });
    expect(await (await recover(await signLicense(restarted.keys.privateKey, { device: "other" }))).json()).toEqual({ found: false });
    expect(await (await recover(restarted.token, "other")).json()).toEqual({ found: false });
    expect((await recover(restarted.token, "consolidation", "")).status).toBe(400);
    expect(restarted.logLines.join("\n")).not.toContain("recover-night");
  });

  it("does not treat an unbound submission as permission to resubmit", async () => {
    const rig = await makeRig();
    const key = { licenseId: "lic_test_1", deviceId: "dev_test_device", purpose: "consolidation", clientRequestId: "unknown-night", payloadHash: "hash", date: utcDate() };
    const claim = await rig.usage.claimIdempotency(key);
    if (claim.kind !== "fresh") throw new Error("not fresh");
    await rig.usage.tryReserve(key.licenseId, key.date, 1, 100, { ...key, claimToken: claim.claimToken });
    const res = await rig.app.request("/v1/batch/recover", {
      method: "POST", headers: { authorization: `Bearer ${rig.token}`, "content-type": "application/json" },
      body: JSON.stringify({ purpose: "consolidation", client_request_id: "unknown-night" }),
    });
    expect(res.status).toBe(503);
    expect(rig.gateway.createCalls).toHaveLength(0);
    expect(rig.logLines.join("\n")).toContain("event=submission_unknown");
  });
});


it.each(stores)("%s store: retirement fences a paused claimant and never cancels armed or bound jobs", async (_name, open) => {
  const store = open();
  const recovery = store;
  const key = { licenseId: "l", deviceId: "d", purpose: "mail_triage", clientRequestId: "old", payloadHash: "hash", date: utcDate() };
  const paused = await store.claimIdempotency(key);
  if (paused.kind !== "fresh") throw new Error("missing claim");
  expect(await store.recoverIdempotency(key)).toEqual({ kind: "missing" });
  expect(await recovery.recoverIdempotency(key, true)).toEqual({ kind: "retired" });
  expect(await store.tryReserve("l", key.date, 1, 100, { ...key, claimToken: paused.claimToken })).toBe("unknown");
  await store.releaseIdempotency(key);
  expect((await store.claimIdempotency(key)).kind).toBe("conflict");
  const absent = { ...key, clientRequestId: "absent" };
  expect(await store.recoverIdempotency(absent, true)).toEqual({ kind: "retired" });
  expect((await store.claimIdempotency(absent)).kind).toBe("conflict");
  const replacement = { ...key, clientRequestId: "changed" };
  const claim = await store.claimIdempotency(replacement);
  if (claim.kind !== "fresh") throw new Error("missing replacement");
  expect(await store.tryReserve("l", key.date, 1, 100, { ...replacement, claimToken: claim.claimToken })).toBe("ok");
  expect((await store.recoverIdempotency(replacement, true)).kind).toBe("unknown");
  await store.bindIdempotency(replacement, "accepted");
  expect((await store.recoverIdempotency(replacement, true)).kind).toBe("bound");
  expect(await store.usedOn("l", key.date)).toBe(1);
});

it.each(stores)("%s store: a create left unknown on an earlier UTC day becomes retirable, never bindable", async (_name, open) => {
  const store = open();
  const key = { licenseId: "l", deviceId: "d", purpose: "consolidation", clientRequestId: "slept-night", payloadHash: "hash", date: "2026-10-14" };
  const claim = await store.claimIdempotency(key);
  if (claim.kind !== "fresh") throw new Error("missing claim");
  expect(await store.tryReserve("l", key.date, 8, 100, { ...key, claimToken: claim.claimToken })).toBe("ok");
  // The create died without an answer: the claim stays unknown that day, even when asked to retire.
  expect((await store.recoverIdempotency(key)).kind).toBe("unknown");
  expect((await store.recoverIdempotency(key, true, "2026-10-14")).kind).toBe("unknown");
  expect((await store.recoverIdempotency(key, false, "2026-10-15")).kind).toBe("unknown");
  // The next UTC day an explicit retirement fences it.
  expect(await store.recoverIdempotency(key, true, "2026-10-15")).toEqual({ kind: "retired" });
  expect(await store.recoverIdempotency(key)).toEqual({ kind: "retired" });
  await expect(store.bindIdempotency(key, "late-accept")).rejects.toThrow();
  expect((await store.claimIdempotency(key)).kind).toBe("conflict");
  expect(await store.usedOn("l", key.date)).toBe(8);
});

it("content-free retirement is explicit, device-scoped, and blocks future provider calls", async () => {
  const rig = await makeRig();
  const retire = (token: string, flag: unknown = true) => rig.app.request("/v1/batch/recover", {
    method: "POST", headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    body: JSON.stringify({ purpose: "consolidation", client_request_id: "retired-id", retire_if_missing: flag }),
  });
  expect((await retire(rig.token, "true")).status).toBe(400);
  expect(await (await retire(rig.token)).json()).toEqual({ found: false, retired: true });
  expect((await postBatch(rig, withId("retired-id"))).status).toBe(409);
  expect(rig.gateway.createCalls).toHaveLength(0);
  const otherDevice = await signLicense(rig.keys.privateKey, { device: "other-device" });
  expect((await postBatch(rig, withId("retired-id"), otherDevice)).status).toBe(202);
});


it.each(stores)("%s store: cancellation refunds once and rejects missing, stale or bound tokens", async (_name, open) => {
  const store = open();
  const key = { licenseId: "l", deviceId: "d", purpose: "mail_triage", clientRequestId: "cancel", payloadHash: "hash", date: utcDate() };
  {
    expect(await store.tryReserve("l", key.date, 3, 100)).toBe("ok");
    const first = await store.claimIdempotency(key);
    if (first.kind !== "fresh") throw new Error("missing first claim");
    const original = { ...key, claimToken: first.claimToken };
    expect(await store.tryReserve("l", key.date, 2, 100, original)).toBe("ok");
    await store.cancelReservation(key, 2);
    await store.cancelReservation({ ...original, claimToken: "wrong-token" }, 2);
    expect(await store.usedOn("l", key.date)).toBe(5);
    expect((await store.recoverIdempotency(key)).kind).toBe("unknown");
    await store.cancelReservation(original, 2);
    await store.cancelReservation(original, 2);
    expect(await store.usedOn("l", key.date)).toBe(3);
    expect(await store.recoverIdempotency(key)).toEqual({ kind: "missing" });
    const next = await store.claimIdempotency(key);
    if (next.kind !== "fresh") throw new Error("missing next claim");
    const replacement = { ...key, claimToken: next.claimToken };
    expect(next.claimToken).not.toBe(first.claimToken);
    expect(await store.tryReserve("l", key.date, 2, 100, replacement)).toBe("ok");
    await store.cancelReservation(original, 2);
    expect(await store.usedOn("l", key.date)).toBe(5);
    await store.bindIdempotency(replacement, "provider-accepted");
    await store.cancelReservation(replacement, 2);
    expect(await store.usedOn("l", key.date)).toBe(5);
    expect((await store.recoverIdempotency(key)).kind).toBe("bound");
  }
});
