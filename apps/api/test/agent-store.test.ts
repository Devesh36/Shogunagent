/** E9-T1 / E9-T3: the wallet ledger — reserve-then-settle with no double settlement, caps checked
 * in the same transaction, idempotent credits — on the in-memory store and on the SQL store over
 * the SQLite double running the real sql/agent-keys.sql. */
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

import { AGENT_STORE_TABLES, SqlAgentStore } from "../src/agent-pg-store.js";
import { InMemoryAgentStore, capSubjectKey, capSubjectPrincipal, type AgentStore, type ReserveInput } from "../src/agent-store.js";
import { DEFAULT_CAPS, type CapSet } from "../src/caps.js";
import { sqliteDouble } from "./sql-double.js";

export const AGENT_SQL = readFileSync(new URL("../sql/agent-keys.sql", import.meta.url), "utf8") + "\n" + readFileSync(new URL("../sql/agent-receipts-settled-quantity.sql", import.meta.url), "utf8");

const backends: Array<[string, () => AgentStore]> = [
  ["in-memory", () => new InMemoryAgentStore()],
  ["sql (sqlite double)", () => new SqlAgentStore(sqliteDouble(AGENT_SQL))],
];

const DAY = "2026-10-25";
const MONTH = "2026-10";
const TS = Date.parse("2026-10-25T12:00:00Z");

function reserve(overrides: Partial<ReserveInput> = {}, caps: CapSet = DEFAULT_CAPS): ReserveInput {
  return {
    receiptId: overrides.receiptId ?? `rcpt_${Math.random().toString(36).slice(2)}`,
    principalId: "prn_a", keyId: "ak_a", method: "tools/call", name: "memory.ingest", unit: "ingest",
    quantity: 10, micros: 5_000, ts: TS, day: DAY, month: MONTH, caps, ...overrides,
  };
}

describe.each(backends)("AgentStore on %s", (_name, open) => {
  it("creates principals and keys, finds keys by prefix, revokes once", async () => {
    const store = open();
    if (store instanceof SqlAgentStore) await store.checkSchema();
    await store.createPrincipal({ principalId: "prn_a", tenantId: "prn_a", createdAt: TS, ref: "dev_1" });
    expect(await store.principal("prn_a")).toEqual({ principalId: "prn_a", tenantId: "prn_a", createdAt: TS, ref: "dev_1" });
    await store.insertKey({ keyId: "ak_a", prefix: "abcdefghij", hash: "pbkdf2-sha256$1$x$y", mode: "live", principalId: "prn_a", tenantId: "prn_a", scopes: ["read", "run"], createdAt: TS, expiresAt: TS + 10, ref: "dev_1" });
    await store.insertKey({ keyId: "ak_b", prefix: "abcdefghij", hash: "h2", mode: "test", principalId: "prn_a", tenantId: "prn_a", scopes: ["admin"], createdAt: TS });
    const found = await store.keysByPrefix("abcdefghij");
    expect(found.map((k) => k.keyId).sort()).toEqual(["ak_a", "ak_b"]);
    expect(found.find((k) => k.keyId === "ak_a")).toEqual(expect.objectContaining({ scopes: ["read", "run"], expiresAt: TS + 10, ref: "dev_1", mode: "live" }));
    expect(await store.keysByPrefix("zzzzzzzzzz")).toEqual([]);
    expect(await store.revokeKey("ak_a", TS + 5)).toBe(true);
    expect(await store.revokeKey("ak_a", TS + 9)).toBe(true);
    expect((await store.keyById("ak_a"))?.revokedAt).toBe(TS + 5);
    expect(await store.revokeKey("ak_none", TS)).toBe(false);
    expect(await store.setKeyCaps("ak_b", { dayMicros: 1, monthMicros: 2 })).toBe(true);
    expect(await store.keyById("ak_b")).toEqual(expect.objectContaining({ capDayMicros: 1, capMonthMicros: 2 }));
    expect(await store.setKeyCaps("ak_b", {})).toBe(true);
    const reset = await store.keyById("ak_b");
    expect(reset?.capDayMicros).toBeUndefined();
  });

  it("inserts a key once per id: the second insert of the same id keeps the first (E39)", async () => {
    const store = open();
    if (store instanceof SqlAgentStore) await store.checkSchema();
    await store.createPrincipal({ principalId: "prn_o", tenantId: "prn_o", createdAt: TS });
    const k = { keyId: "ak_once", prefix: "onceonceon", hash: "h-first", mode: "live" as const, principalId: "prn_o", tenantId: "prn_o", scopes: ["read" as const], createdAt: TS };
    expect(await store.insertKeyOnce(k)).toBe(true);
    expect(await store.insertKeyOnce({ ...k, hash: "h-second" })).toBe(false);
    expect((await store.keyById("ak_once"))?.hash).toBe("h-first");
  });

  it("credits once per topup id and creates the wallet on first credit", async () => {
    const store = open();
    expect(await store.wallet("prn_a")).toEqual({ principalId: "prn_a", balanceMicros: 0, reservedMicros: 0 });
    expect(await store.credit("prn_a", "evt_1", 5_000_000, "stripe_checkout", TS)).toBe("credited");
    expect(await store.credit("prn_a", "evt_1", 5_000_000, "stripe_checkout", TS)).toBe("duplicate");
    expect(await store.credit("prn_a", "evt_2", 1, "stripe_checkout", TS)).toBe("credited");
    expect((await store.wallet("prn_a")).balanceMicros).toBe(5_000_001);
    expect(await store.credit("prn_a", "evt_3", -5, "x", TS)).toBe("duplicate");
    expect((await store.wallet("prn_a")).balanceMicros).toBe(5_000_001);
  });

  it("reserves against balance minus reserved, settles once, and never double-settles", async () => {
    const store = open();
    await store.credit("prn_a", "grant", 10_000, "signup_grant", TS);
    expect(await store.reserve(reserve({ receiptId: "r1", micros: 6_000 }))).toEqual({ kind: "ok" });
    expect(await store.wallet("prn_a")).toEqual({ principalId: "prn_a", balanceMicros: 10_000, reservedMicros: 6_000 });
    // Only 4,000 is spendable while r1 is held.
    expect(await store.reserve(reserve({ receiptId: "r2", micros: 5_000 }))).toEqual({ kind: "insufficient_funds", availableMicros: 4_000 });
    expect(await store.reserve(reserve({ receiptId: "r3", micros: 4_000 }))).toEqual({ kind: "ok" });
    // Settle r1 at less than its estimate; the difference is spendable again.
    await store.settle("r1", 2_500);
    expect(await store.wallet("prn_a")).toEqual({ principalId: "prn_a", balanceMicros: 7_500, reservedMicros: 4_000 });
    // A second settlement of r1 (a retried path) changes nothing — no double charge, no double refund.
    await store.settle("r1", 2_500);
    await store.settle("r1", 0);
    expect(await store.wallet("prn_a")).toEqual({ principalId: "prn_a", balanceMicros: 7_500, reservedMicros: 4_000 });
    expect((await store.receipt("r1"))).toEqual(expect.objectContaining({ settled: true, costMicros: 2_500, estimateMicros: 6_000 }));
    // Settling at 0 refunds the hold entirely (a failed call).
    await store.settle("r3", 0);
    expect(await store.wallet("prn_a")).toEqual({ principalId: "prn_a", balanceMicros: 7_500, reservedMicros: 0 });
    // Unknown receipts and malformed amounts are no-ops.
    await store.settle("nope", 1);
    await store.settle("r3", -1);
    expect(await store.wallet("prn_a")).toEqual({ principalId: "prn_a", balanceMicros: 7_500, reservedMicros: 0 });
  });

  it("records a free call as a settled receipt holding nothing", async () => {
    const store = open();
    expect(await store.reserve(reserve({ receiptId: "free", micros: 0, unit: "read", name: "memory.search", quantity: 1 }))).toEqual({ kind: "ok" });
    expect(await store.wallet("prn_a")).toEqual({ principalId: "prn_a", balanceMicros: 0, reservedMicros: 0 });
    expect(await store.receipt("free")).toEqual(expect.objectContaining({ settled: true, costMicros: 0, estimateMicros: 0, unit: "read", quantity: 1, settledQuantity: 1 }));
    expect(await store.periodSpend(capSubjectKey("ak_a"), DAY)).toBe(0);
  });

  it("records the settled quantity next to the reserved one, capped at the reservation (E48)", async () => {
    const store = open();
    await store.credit("prn_a", "grant", 100_000, "signup_grant", TS);
    for (const id of ["q_partial", "q_over", "q_default", "q_zero"]) expect(await store.reserve(reserve({ receiptId: id, quantity: 10, micros: 5_000 }))).toEqual({ kind: "ok" });
    expect((await store.receipt("q_partial"))?.settledQuantity).toBeUndefined();
    await store.settle("q_partial", 2_000, 2_000, 4);
    await store.settle("q_over", 5_000, 5_000, 25);
    await store.settle("q_default", 5_000);
    await store.settle("q_zero", 0);
    // A second settle changes nothing, quantity included.
    await store.settle("q_partial", 5_000, 5_000, 10);
    const got = await Promise.all(["q_partial", "q_over", "q_default", "q_zero"].map((id) => store.receipt(id)));
    expect(got.map((r) => [r?.quantity, r?.settledQuantity, r?.costMicros])).toEqual([[10, 4, 2_000], [10, 10, 5_000], [10, 10, 5_000], [10, 0, 0]]);
  });

  it("checks the four cap counters in the same transaction and books the estimate on all of them", async () => {
    const store = open();
    await store.credit("prn_a", "grant", 1_000_000, "signup_grant", TS);
    const caps: CapSet = { key: { day: 10_000, month: 15_000 }, principal: { day: 50_000, month: 100_000 } };
    expect(await store.reserve(reserve({ receiptId: "a", micros: 6_000 }, caps))).toEqual({ kind: "ok" });
    // 6,000 + 5,000 > key day 10,000: refused on the key's day, nothing booked, nothing held.
    expect(await store.reserve(reserve({ receiptId: "b", micros: 5_000 }, caps))).toEqual({ kind: "cap_exceeded", scope: "key", period: "day", limitMicros: 10_000, spentMicros: 6_000 });
    expect((await store.wallet("prn_a")).reservedMicros).toBe(6_000);
    expect(await store.periodSpend(capSubjectKey("ak_a"), DAY)).toBe(6_000);
    expect(await store.periodSpend(capSubjectPrincipal("prn_a"), MONTH)).toBe(6_000);
    expect(await store.receipt("b")).toBeUndefined();
    // Another key of the same principal has its own day, and the month cap on the first key still holds.
    expect(await store.reserve(reserve({ receiptId: "c", keyId: "ak_c", micros: 9_000 }, caps))).toEqual({ kind: "ok" });
    expect(await store.reserve(reserve({ receiptId: "d", keyId: "ak_a", micros: 4_000, day: "2026-10-26" }, caps))).toEqual({ kind: "ok" });
    expect(await store.reserve(reserve({ receiptId: "e", keyId: "ak_a", micros: 5_001, day: "2026-10-27" }, caps)))
      .toEqual({ kind: "cap_exceeded", scope: "key", period: "month", limitMicros: 15_000, spentMicros: 10_000 });
    // Settlement moves the counters by the actual - estimate delta, never below zero.
    await store.settle("a", 1_000);
    expect(await store.periodSpend(capSubjectKey("ak_a"), DAY)).toBe(1_000);
    expect(await store.periodSpend(capSubjectPrincipal("prn_a"), DAY)).toBe(10_000);
    expect(await store.reserve(reserve({ receiptId: "f", micros: 9_000 }, caps))).toEqual({ kind: "ok" });
  });

  it("the principal cap bounds every key together", async () => {
    const store = open();
    await store.credit("prn_a", "grant", 1_000_000, "signup_grant", TS);
    const caps: CapSet = { key: { day: 100_000, month: 100_000 }, principal: { day: 10_000, month: 100_000 } };
    expect(await store.reserve(reserve({ receiptId: "a", keyId: "ak_1", micros: 7_000 }, caps))).toEqual({ kind: "ok" });
    expect(await store.reserve(reserve({ receiptId: "b", keyId: "ak_2", micros: 4_000 }, caps)))
      .toEqual({ kind: "cap_exceeded", scope: "principal", period: "day", limitMicros: 10_000, spentMicros: 7_000 });
  });

  it("insufficient funds is checked before the caps", async () => {
    const store = open();
    expect(await store.reserve(reserve({ receiptId: "a", micros: 1 }))).toEqual({ kind: "insufficient_funds", availableMicros: 0 });
  });

  it("lists receipts since an instant, oldest first, and disputes a receipt once", async () => {
    const store = open();
    await store.credit("prn_a", "grant", 100_000, "signup_grant", TS);
    await store.reserve(reserve({ receiptId: "r1", micros: 100, ts: TS }));
    await store.reserve(reserve({ receiptId: "r2", micros: 0, unit: "read", ts: TS + 1 }));
    await store.reserve(reserve({ receiptId: "r3", micros: 100, ts: TS + 2, principalId: "prn_other" }));
    await store.settle("r1", 100);
    const rows = await store.receipts("prn_a", TS + 1, 10);
    expect(rows.map((r) => r.receiptId)).toEqual(["r2"]);
    expect((await store.receipts("prn_a", 0, 10)).map((r) => r.receiptId)).toEqual(["r1", "r2"]);
    expect((await store.receipts("prn_a", 0, 1)).map((r) => r.receiptId)).toEqual(["r1"]);
    const first = await store.dispute("r1", "prn_a", "wrong quantity", "dsp_1", TS + 5);
    expect(first).toEqual({ dispute: { disputeId: "dsp_1", receiptId: "r1", principalId: "prn_a", reason: "wrong quantity", status: "open", ts: TS + 5 }, created: true });
    const again = await store.dispute("r1", "prn_a", "again", "dsp_2", TS + 6);
    expect(again?.created).toBe(false);
    expect(again?.dispute.disputeId).toBe("dsp_1");
    // Someone else's receipt is "not found".
    expect(await store.dispute("r3", "prn_a", "x", "dsp_3", TS)).toBeUndefined();
  });

  it("spends a PoW challenge once until it expires", async () => {
    const store = open();
    expect(await store.spendChallenge("h1", TS + 1_000, TS)).toBe(true);
    expect(await store.spendChallenge("h1", TS + 1_000, TS + 10)).toBe(false);
    expect(await store.spendChallenge("h1", TS + 5_000, TS + 1_000)).toBe(true);
    await store.maintenance(TS + 6_000);
    expect(await store.spendChallenge("h1", TS + 9_000, TS + 6_000)).toBe(true);
  });
});

describe("SqlAgentStore failure policy", () => {
  it("answers unavailable (never ok) when the store cannot be written, and nothing is held", async () => {
    const db = sqliteDouble(AGENT_SQL);
    const store = new SqlAgentStore(db);
    await store.credit("prn_a", "g", 10_000, "x", TS);
    db.failNext(1);
    expect(await store.reserve(reserve({ receiptId: "a", micros: 100 }))).toEqual({ kind: "unavailable" });
    expect(await store.wallet("prn_a")).toEqual({ principalId: "prn_a", balanceMicros: 10_000, reservedMicros: 0 });
    db.failNext(1);
    await expect(store.wallet("prn_a")).rejects.toThrow("agent store unavailable");
  });

  it("checkSchema names a missing table and nothing else", async () => {
    const store = new SqlAgentStore(sqliteDouble());
    await expect(store.checkSchema()).rejects.toThrow(`agent store table missing or unreadable: ${AGENT_STORE_TABLES[0]}`);
  });

  it("checkSchema names the settled-quantity file when its column is missing (E48)", async () => {
    const store = new SqlAgentStore(sqliteDouble(readFileSync(new URL("../sql/agent-keys.sql", import.meta.url), "utf8")));
    await expect(store.checkSchema()).rejects.toThrow("agent store columns missing: agent_receipts (apply sql/agent-receipts-settled-quantity.sql)");
  });
});
