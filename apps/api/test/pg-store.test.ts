/** Issue #29 FR-SVC-04: the shared transactional store. Runs the store's real SQL (and the real
 * schema file) against an in-memory SQLite double in CI; set RELAY_TEST_DATABASE_URL to also
 * run the concurrency checks against a real Postgres (never in CI, never required). */
import { randomUUID } from "node:crypto";
import { afterAll, describe, expect, it } from "vitest";

import { RATE_BUCKET_SWEEP_EVERY_MS, SHARED_STORE_TABLES, SqlRateLimiter, SqlUsageStore, resetRateBucketSweepForTests } from "../src/pg-store.js";
import { DEFAULT_BATCH_PRICES } from "../src/spend.js";
import { postgresClient, type SqlClient } from "../src/sql.js";
import { BATCH_OWNER_KEEP_DAYS, RATE_BUCKET_IDLE_EVICT_MS, USAGE_DAY_KEEP_DAYS, keepCutoff, type IdempotencyInput } from "../src/usage.js";
import { makeRig, postBatch, sampleBody } from "./helpers.js";
import { sqliteDouble, toSqlite } from "./sql-double.js";

const DATE = "2026-09-29";
/** "Any `checked_at`": the recheck holdoff off, for tests that are not about it. */
const ANY = Number.MAX_SAFE_INTEGER;
const url = process.env.RELAY_TEST_DATABASE_URL;

// Every behavioural test runs against the SQLite double; with RELAY_TEST_DATABASE_URL set (a
// throwaway database with sql/shared-store.sql applied) the same tests also run on Postgres.
const pgClients: SqlClient[] = [];
afterAll(async () => { await Promise.all(pgClients.map((c) => c.close())); });
async function freshPostgres(): Promise<SqlClient> {
  const client = postgresClient(url!);
  pgClients.push(client);
  await client.query(`TRUNCATE ${SHARED_STORE_TABLES.join(",")}`);
  return client;
}
const backends: Array<[string, () => Promise<SqlClient>]> = [
  ["sqlite double", () => Promise.resolve(sqliteDouble())],
  ...(url ? [["postgres", freshPostgres] as [string, () => Promise<SqlClient>]] : []),
];

function input(overrides: Partial<IdempotencyInput> = {}): IdempotencyInput {
  return { licenseId: "lic_a", deviceId: "dev_a", purpose: "consolidation", clientRequestId: randomUUID(), payloadHash: "h1", date: DATE, ...overrides };
}

describe("SQL dialect double", () => {
  it("rewrites only placeholders, casts and row locks", () => {
    expect(toSqlite("SELECT a FROM t WHERE x=$1::text AND y<=$12::bigint FOR UPDATE"))
      .toBe("SELECT a FROM t WHERE x=?1 AND y<=?12");
  });
});

describe.each(backends)("SqlUsageStore on %s", (_name, open) => {
  it("claims, fences, reserves and binds exactly like the in-memory store", async () => {
    const store = new SqlUsageStore(await open());
    await store.checkSchema();
    const key = input();
    const claim = await store.claimIdempotency(key);
    expect(claim.kind).toBe("fresh");
    if (claim.kind !== "fresh") return;
    // A second claim of a pending identity re-arms it with a new token; the old token is fenced.
    const again = await store.claimIdempotency(key);
    expect(again.kind).toBe("fresh");
    if (again.kind !== "fresh") return;
    expect(again.batchId).toBe(claim.batchId);
    expect(await store.tryReserve("lic_a", DATE, 2, 10, { ...key, claimToken: claim.claimToken })).toBe("unknown");
    expect(await store.tryReserve("lic_a", DATE, 2, 10, { ...key, claimToken: again.claimToken })).toBe("ok");
    expect(await store.usedOn("lic_a", DATE)).toBe(2);
    // Armed but unbound: a retry must not submit again.
    expect((await store.claimIdempotency(key)).kind).toBe("unknown");
    const relayId = await store.bindIdempotency({ ...key, claimToken: again.claimToken }, "msgbatch_1");
    expect(relayId).toBe(claim.batchId);
    expect(await store.claimIdempotency(key)).toEqual({ kind: "replay", batchId: relayId });
    expect((await store.claimIdempotency({ ...key, payloadHash: "other" })).kind).toBe("conflict");
    expect(await store.batchOwner(relayId, "dev_a")).toBe("lic_a");
    expect(await store.batchOwner(relayId, "dev_other")).toBeUndefined();
    expect(await store.providerBatchId(relayId)).toBe("msgbatch_1");
    expect(await store.recoverIdempotency(key)).toEqual({ kind: "bound", batchId: relayId });
  });

  it("never crosses the cap, and a refused reservation leaves the counter untouched", async () => {
    const store = new SqlUsageStore(await open());
    expect(await store.tryReserve("lic_a", DATE, 8, 10)).toBe("ok");
    expect(await store.tryReserve("lic_a", DATE, 3, 10)).toBe("capped");
    expect(await store.usedOn("lic_a", DATE)).toBe(8);
    expect(await store.tryReserve("lic_a", DATE, 2, 10)).toBe("ok");
    // A first row larger than the cap on its own is refused too.
    expect(await store.tryReserve("lic_b", DATE, 11, 10)).toBe("capped");
    expect(await store.usedOn("lic_b", DATE)).toBe(0);
  });

  it("holds exactly the cap under 50 concurrent submissions through the app", async () => {
    const usage = new SqlUsageStore(await open());
    const rig = await makeRig({ usage, dailyChunkCap: 10 });
    const one = { purpose: "consolidation", model_class: "classify", items: [{ custom_id: "1", chunk: "c" }] };
    const results = await Promise.all(Array.from({ length: 50 }, (_, i) => postBatch(rig, { ...one, client_request_id: `c-${i}` })));
    expect(results.filter((r) => r.status === 202)).toHaveLength(10);
    expect(results.filter((r) => r.status === 429)).toHaveLength(40);
    expect(rig.gateway.createCalls).toHaveLength(10);
    expect(await usage.usedOn("lic_test_1", new Date().toISOString().slice(0, 10))).toBe(10);
  });

  it("reserves spend with chunks atomically, settles once, refunds unsubmitted claims", async () => {
    const store = new SqlUsageStore(await open());
    const key = input();
    const claim = await store.claimIdempotency(key);
    if (claim.kind !== "fresh") throw new Error("expected fresh");
    const armed = { ...key, claimToken: claim.claimToken };
    // Over the spend cap: nothing is held — not the spend, and not the chunks either.
    expect(await store.tryReserve("lic_a", DATE, 3, 10, armed, { batchId: claim.batchId, micros: 600, capMicros: 500 })).toBe("spend_capped");
    expect(await store.usedOn("lic_a", DATE)).toBe(0);
    expect(await store.spentOn(DATE)).toBe(0);

    expect(await store.tryReserve("lic_a", DATE, 3, 10, armed, { batchId: claim.batchId, micros: 400, capMicros: 500 })).toBe("ok");
    expect(await store.spentOn(DATE)).toBe(400);
    // Definite provider rejection → the claim reopens and its spend estimate comes back.
    await store.releaseIdempotency(armed);
    expect(await store.spentOn(DATE)).toBe(0);

    const again = await store.claimIdempotency(key);
    if (again.kind !== "fresh") throw new Error("expected fresh");
    const rearmed = { ...key, claimToken: again.claimToken };
    expect(await store.tryReserve("lic_a", DATE, 3, 10, rearmed, { batchId: again.batchId, micros: 400, capMicros: 500 })).toBe("ok");
    await store.bindIdempotency(rearmed, "msgbatch_2");
    await store.settleSpend(again.batchId, 90);
    expect(await store.spentOn(DATE)).toBe(90);
    await store.settleSpend(again.batchId, 5_000); // a re-read settles nothing twice
    expect(await store.spentOn(DATE)).toBe(90);
  });

  it("cancelReservation refunds chunks and spend only for the armed token", async () => {
    const store = new SqlUsageStore(await open());
    const key = input();
    const claim = await store.claimIdempotency(key);
    if (claim.kind !== "fresh") throw new Error("expected fresh");
    const armed = { ...key, claimToken: claim.claimToken };
    expect(await store.tryReserve("lic_a", DATE, 4, 10, armed, { batchId: claim.batchId, micros: 100, capMicros: 1000 })).toBe("ok");
    await store.cancelReservation({ ...key, claimToken: "stale" }, 4);
    expect(await store.usedOn("lic_a", DATE)).toBe(4);
    await store.cancelReservation(armed, 4);
    expect(await store.usedOn("lic_a", DATE)).toBe(0);
    expect(await store.spentOn(DATE)).toBe(0);
    await store.cancelReservation(armed, 4); // repeating refunds nothing
    expect(await store.usedOn("lic_a", DATE)).toBe(0);
  });

  it("cancelReservation refunds the payer's row when budgets are metered apart from ownership (H1)", async () => {
    const store = new SqlUsageStore(await open());
    const key = input();
    const claim = await store.claimIdempotency(key);
    if (claim.kind !== "fresh") throw new Error("expected fresh");
    const armed = { ...key, claimToken: claim.claimToken };
    expect(await store.tryReserve("account:acc_1", DATE, 4, 10, armed, { batchId: claim.batchId, micros: 100, capMicros: 1000, licenseCapMicros: 1000 })).toBe("ok");
    expect(await store.licenseSpentOn("account:acc_1", DATE)).toBe(100);
    await store.cancelReservation(armed, 4, "account:acc_1");
    expect(await store.usedOn("account:acc_1", DATE)).toBe(0);
    expect(await store.licenseSpentOn("account:acc_1", DATE)).toBe(0);
    expect(await store.spentOn(DATE)).toBe(0);
  });

  it("retires a missing identity atomically for recovery", async () => {
    const store = new SqlUsageStore(await open());
    const key = input();
    expect(await store.recoverIdempotency(key)).toEqual({ kind: "missing" });
    expect(await store.recoverIdempotency(key, true)).toEqual({ kind: "retired" });
    expect((await store.claimIdempotency(key)).kind).toBe("conflict");
  });

});

describe("SqlUsageStore failure policy and hygiene (SQLite double)", () => {
  it("fails closed on store errors: reservation unavailable, claim unavailable, no driver text", async () => {
    const db = sqliteDouble();
    const store = new SqlUsageStore(db);
    db.failNext(1);
    expect(await store.tryReserve("lic_a", DATE, 1, 10)).toBe("unavailable");
    db.failNext(1);
    expect(await store.claimIdempotency(input())).toEqual({ kind: "unavailable" });
    db.failNext(1);
    await expect(store.usedOn("lic_a", DATE)).rejects.toThrow("shared usage store unavailable");
    expect(await store.usedOn("lic_a", DATE)).toBe(0);
  });

  it("refuses to start against a database without the schema", async () => {
    const db = sqliteDouble();
    await db.query("DROP TABLE relay_spend_batches");
    await expect(new SqlUsageStore(db).checkSchema()).rejects.toThrow("relay_spend_batches");
  });

  it("parameterizes everything: no chunk text or ids are spliced into SQL", async () => {
    const db = sqliteDouble();
    const rig = await makeRig({ usage: new SqlUsageStore(db) });
    expect((await postBatch(rig, sampleBody)).status).toBe(202);
    const all = db.statements.join("\n");
    expect(all).not.toContain("TOP-SECRET");
    expect(all).not.toContain("lic_test_1");
    expect(all).not.toContain("dev_test_device");
  });

  it("runs no retention DELETE inside a submit, a poll or a results read; the sweep's maintenance runs them all", async () => {
    const db = sqliteDouble();
    const store = new SqlUsageStore(db);
    const rig = await makeRig({ usage: store, rateLimit: new SqlRateLimiter(db, 12, 12), spendBreaker: { capMicros: 1_000_000, prices: DEFAULT_BATCH_PRICES } });
    const res = await postBatch(rig, sampleBody);
    expect(res.status).toBe(202);
    const { batch_id } = (await res.json()) as { batch_id: string };
    expect((await rig.app.request(`/v1/batch/${batch_id}`, { headers: { authorization: `Bearer ${rig.token}` } })).status).toBe(200);
    rig.gateway.status = { ...rig.gateway.status, processing_status: "ended", request_counts: { processing: 0, succeeded: 2, errored: 0, canceled: 0, expired: 0 } };
    await (await rig.app.request(`/v1/batch/${batch_id}`, { headers: { authorization: `Bearer ${rig.token}` } })).text();
    const deletes = () => db.statements.filter((q) => /^\s*DELETE/i.test(q));
    expect(deletes()).toEqual([]);

    await store.maintenance("2026-10-12", Date.now());
    const tables = deletes().map((q) => /DELETE FROM (\w+)/i.exec(q)?.[1]).sort();
    expect(tables).toEqual(["relay_rate_buckets", "relay_spend_batches", "relay_spend_days", "relay_usage_days", "relay_usage_owners"]);
    // Never the tombstones.
    expect(deletes().some((q) => /^\s*DELETE FROM relay_usage_idem\b/i.test(q))).toBe(false);
  });
});

describe.each(backends)("SqlRateLimiter on %s", (_name, open) => {
  it("behaves like the in-memory token bucket", async () => {
    const limiter = new SqlRateLimiter(await open(), 2, 60); // 1/sec sustained
    expect(await limiter.take("a", 0)).toBe(true);
    expect(await limiter.take("a", 0)).toBe(true);
    expect(await limiter.take("a", 0)).toBe(false);
    expect(await limiter.retryAfterSecs("a", 0)).toBe(1);
    expect(await limiter.take("a", 999)).toBe(false);
    expect(await limiter.take("a", 1000)).toBe(true);
    expect(await limiter.take("b", 0)).toBe(true); // separate key, separate budget
  });

  it("stores only hashed keys (bucket keys carry licence + device ids and NUL separators)", async () => {
    const db = await open();
    await new SqlRateLimiter(db, 1, 1).take("lic_x\u0000dev_y\u0000batch_submit", 0);
    const rows = await db.query("SELECT bucket_key FROM relay_rate_buckets");
    expect(rows).toHaveLength(1);
    expect(String(rows[0]!.bucket_key)).toMatch(/^[0-9a-f]{64}$/);
  });

  it("fails open when the store is down, and says so", async () => {
    const db = sqliteDouble();
    let errors = 0;
    const limiter = new SqlRateLimiter(db, 1, 1, () => { errors += 1; });
    db.failNext(10);
    expect(await limiter.take("a", 0)).toBe(true);
    expect(errors).toBe(1);
  });

  it("evicts idle buckets at most once per holdoff per isolate, never on the isolate's first call, whatever the number of limiter instances", async () => {
    resetRateBucketSweepForTests();
    const db = sqliteDouble();
    const deletes = () => db.statements.filter((q) => /^\s*DELETE/i.test(q)).length;
    const t0 = 1_700_000_000_000;
    // The Worker builds a limiter per request: three instances stand for three requests.
    await new SqlRateLimiter(db, 5, 5).take("a", t0);
    await new SqlRateLimiter(db, 5, 5).take("b", t0 + 1_000);
    await new SqlRateLimiter(db, 5, 5).take("c", t0 + RATE_BUCKET_SWEEP_EVERY_MS);
    expect(deletes()).toBe(0);
    // Past the holdoff: one eviction, of the buckets idle longer than the evict window.
    const t1 = t0 + RATE_BUCKET_SWEEP_EVERY_MS + 1;
    await new SqlRateLimiter(db, 5, 5).take("d", t1);
    expect(deletes()).toBe(1);
    const left = (await db.query("SELECT bucket_key,at_ms FROM relay_rate_buckets")).map((r) => Number(r.at_ms));
    expect(left.every((at) => at >= t1 - RATE_BUCKET_IDLE_EVICT_MS)).toBe(true);
    expect(left).toHaveLength(2); // "c" and "d"; "a" and "b" had been idle 15 minutes
    // And not again until the next holdoff.
    await new SqlRateLimiter(db, 5, 5).take("e", t1 + 1_000);
    expect(deletes()).toBe(1);
    resetRateBucketSweepForTests();
  });

  it("drives the relay's 429 through the app", async () => {
    const rig = await makeRig({ rateLimit: new SqlRateLimiter(await open(), 1, 1), dailyChunkCap: 1000 });
    expect((await postBatch(rig, sampleBody)).status).toBe(202);
    const over = await postBatch(rig, sampleBody);
    expect(over.status).toBe(429);
    expect(over.headers.get("retry-after")).toBe("60");
  });
});

describe.skipIf(!url)("against a real Postgres (RELAY_TEST_DATABASE_URL)", () => {
  it("two instances sharing the store hold one cap between them", async () => {
    const a = postgresClient(url!);
    const b = postgresClient(url!);
    try {
      const license = `lic_${randomUUID()}`;
      const stores = [new SqlUsageStore(a), new SqlUsageStore(b)];
      await stores[0]!.checkSchema();
      const results = await Promise.all(Array.from({ length: 40 }, (_, i) => stores[i % 2]!.tryReserve(license, DATE, 1, 10)));
      expect(results.filter((r) => r === "ok")).toHaveLength(10);
      expect(await stores[1]!.usedOn(license, DATE)).toBe(10);
    } finally {
      await a.close();
      await b.close();
    }
  });
});

describe.each(backends)("SqlUsageStore per-licence spend budget on %s (audit H1/M9)", (_name, open) => {
  it("refuses over the licence budget with its own reason, holding nothing, and books the licence's spend otherwise", async () => {
    const store = new SqlUsageStore(await open());
    const key = input();
    const claim = await store.claimIdempotency(key);
    if (claim.kind !== "fresh") throw new Error("expected fresh");
    const armed = { ...key, claimToken: claim.claimToken };
    expect(await store.tryReserve("lic_a", DATE, 3, 10, armed, { batchId: claim.batchId, micros: 600, capMicros: 5_000, licenseCapMicros: 500 })).toBe("license_spend_capped");
    expect(await store.usedOn("lic_a", DATE)).toBe(0);
    expect(await store.spentOn(DATE)).toBe(0);
    expect(await store.licenseSpentOn("lic_a", DATE)).toBe(0);
    expect(await store.tryReserve("lic_a", DATE, 3, 10, armed, { batchId: claim.batchId, micros: 400, capMicros: 5_000, licenseCapMicros: 500 })).toBe("ok");
    expect(await store.licenseSpentOn("lic_a", DATE)).toBe(400);
    expect(await store.spentOn(DATE)).toBe(400);
    // The chunk cap still names itself when both would refuse.
    expect(await store.tryReserve("lic_b", DATE, 11, 10, undefined, { batchId: "rb_x", micros: 999, capMicros: 5_000, licenseCapMicros: 1 })).toBe("capped");
    // Refund on a definite rejection, then settle to actual on both counters.
    await store.releaseIdempotency(armed);
    expect(await store.licenseSpentOn("lic_a", DATE)).toBe(0);
    const again = await store.claimIdempotency(key);
    if (again.kind !== "fresh") throw new Error("expected fresh");
    const rearmed = { ...key, claimToken: again.claimToken };
    expect(await store.tryReserve("lic_a", DATE, 3, 10, rearmed, { batchId: again.batchId, micros: 400, capMicros: 5_000, licenseCapMicros: 500 })).toBe("ok");
    expect(await store.unsettledBatches(10, DATE, ANY)).toEqual([]); // unbound: no provider id
    await store.bindIdempotency(rearmed, "msgbatch_lic");
    expect(await store.unsettledBatches(10, DATE, ANY)).toEqual([{ batchId: again.batchId, providerId: "msgbatch_lic" }]);
    expect(await store.unsettledBatches(10, "2026-09-30", ANY)).toEqual([]); // outside the window
    await store.settleSpend(again.batchId, 90);
    expect(await store.licenseSpentOn("lic_a", DATE)).toBe(90);
    expect(await store.spentOn(DATE)).toBe(90);
    expect(await store.unsettledBatches(10, DATE, ANY)).toEqual([]);
  });

  it("lists unsettled batches in (date, batch_id) order and skips ones checked within the holdoff", async () => {
    const store = new SqlUsageStore(await open());
    const bind = async (date: string, providerId: string): Promise<string> => {
      const key = input({ date });
      const claim = await store.claimIdempotency(key);
      if (claim.kind !== "fresh") throw new Error("expected fresh");
      const armed = { ...key, claimToken: claim.claimToken };
      expect(await store.tryReserve("lic_a", date, 1, 100, armed, { batchId: claim.batchId, micros: 10, capMicros: 5_000 })).toBe("ok");
      return store.bindIdempotency(armed, providerId);
    };
    const later = await bind("2026-09-30", "msgbatch_later");
    const earlyIds = [await bind(DATE, "msgbatch_e1"), await bind(DATE, "msgbatch_e2"), await bind(DATE, "msgbatch_e3")];
    const listed = await store.unsettledBatches(10, DATE, ANY);
    expect(listed.map((b) => b.batchId)).toEqual([...[...earlyIds].sort(), later]);
    expect(await store.unsettledBatches(2, DATE, ANY)).toEqual(listed.slice(0, 2));
    // Checked just now: skipped while the holdoff lasts, back once it has passed.
    await store.markChecked(listed[0]!.batchId, 1_000_000);
    expect((await store.unsettledBatches(10, DATE, 1_000_000)).map((b) => b.batchId)).toEqual(listed.slice(1).map((b) => b.batchId));
    expect((await store.unsettledBatches(10, DATE, 1_000_001)).map((b) => b.batchId)).toEqual(listed.map((b) => b.batchId));
    // Settling clears it from the list regardless; marking a settled batch changes nothing.
    await store.settleSpend(listed[0]!.batchId, 1);
    await store.markChecked(listed[0]!.batchId, 5);
    expect((await store.unsettledBatches(10, DATE, ANY)).map((b) => b.batchId)).toEqual(listed.slice(1).map((b) => b.batchId));
  });

  it("deletes expired rows only in maintenance, and only the right ones", async () => {
    const store = new SqlUsageStore(await open());
    const today = "2026-10-12";
    const stale = keepCutoff(today, USAGE_DAY_KEEP_DAYS + 1);
    const legacyStale = keepCutoff(today, BATCH_OWNER_KEEP_DAYS + 1);
    expect(await store.tryReserve("lic_old", stale, 2, 10, undefined, { batchId: "rb_stale", micros: 5, capMicros: 100 })).toBe("ok");
    expect(await store.tryReserve("lic_now", today, 2, 10, undefined, { batchId: "rb_now", micros: 5, capMicros: 100 })).toBe("ok");
    await store.attachBatch("rb_legacy_old", "lic_x", legacyStale, 1, undefined, "msgbatch_old");
    await store.attachBatch("rb_legacy_new", "lic_x", today, 1, undefined, "msgbatch_new");
    // A keyed batch from long ago keeps its owner row: its identity is a permanent tombstone.
    const key = input({ date: legacyStale });
    const claim = await store.claimIdempotency(key);
    if (claim.kind !== "fresh") throw new Error("expected fresh");
    const keyed = await store.bindIdempotency({ ...key, claimToken: claim.claimToken }, "msgbatch_keyed");
    await store.attachBatch(keyed, key.licenseId, legacyStale, 1, key.deviceId, "msgbatch_keyed");

    await store.maintenance(today, Date.now());
    expect(await store.usedOn("lic_old", stale)).toBe(0);
    expect(await store.usedOn("lic_now", today)).toBe(2);
    expect(await store.spentOn(stale)).toBe(0);
    expect(await store.spentOn(today)).toBe(5);
    expect(await store.providerBatchId("rb_legacy_old")).toBeUndefined();
    expect(await store.providerBatchId("rb_legacy_new")).toBe("msgbatch_new");
    expect(await store.providerBatchId(keyed)).toBe("msgbatch_keyed");
    expect(await store.claimIdempotency(key)).toEqual({ kind: "replay", batchId: keyed });
  });

  it("markSettled closes a reservation keeping its estimate, once", async () => {
    const store = new SqlUsageStore(await open());
    const key = input();
    const claim = await store.claimIdempotency(key);
    if (claim.kind !== "fresh") throw new Error("expected fresh");
    const armed = { ...key, claimToken: claim.claimToken };
    expect(await store.tryReserve("lic_a", DATE, 1, 10, armed, { batchId: claim.batchId, micros: 300, capMicros: 5_000 })).toBe("ok");
    await store.bindIdempotency(armed, "msgbatch_gone");
    await store.markSettled(claim.batchId);
    expect(await store.unsettledBatches(10, DATE, ANY)).toEqual([]);
    await store.settleSpend(claim.batchId, 1); // already closed: nothing moves
    expect(await store.spentOn(DATE)).toBe(300);
    expect(await store.licenseSpentOn("lic_a", DATE)).toBe(300);
  });

  it("drives the relay's license_spend_cap 429 through the app", async () => {
    const usage = new SqlUsageStore(await open());
    const rig = await makeRig({ usage, licenseSpendCapMicros: 1 });
    const res = await postBatch(rig, sampleBody);
    expect(res.status).toBe(429);
    expect(await res.json()).toMatchObject({ reason: "license_spend_cap" });
    expect(rig.gateway.createCalls).toHaveLength(0);
  });
});

describe("shared-store.sql is re-applicable (additive ALTER … IF NOT EXISTS)", () => {
  it("applies twice to the same database without error", async () => {
    const { createRequire } = await import("node:module");
    const { DatabaseSync } = createRequire(import.meta.url)("node:sqlite") as typeof import("node:sqlite");
    const { applySchema, SHARED_STORE_SQL } = await import("./sql-double.js");
    const db = new DatabaseSync(":memory:");
    applySchema(db, SHARED_STORE_SQL);
    applySchema(db, SHARED_STORE_SQL);
    expect(db.prepare("PRAGMA table_info(relay_usage_days)").all().map((c) => c.name)).toContain("spend_micros");
    expect(db.prepare("PRAGMA table_info(relay_spend_batches)").all().map((c) => c.name)).toContain("license_id");
    expect(db.prepare("PRAGMA table_info(relay_spend_batches)").all().map((c) => c.name)).toContain("checked_at");
    db.close();
  });
});
