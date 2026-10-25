/**
 * E33: the durable settlement outbox. Both stores (in memory, and the real SQL of
 * sql/settlement-outbox.sql on an in-memory SQLite double) behave the same: a draft is recorded
 * before money is held, the final settlement replaces it, delivery retries with backoff until
 * apps/api answers 2xx, abandoned drafts release their hold at $0, and nothing is lost when the
 * process dies between any two steps.
 */
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { describe, expect, it } from "vitest";
import type { LogEvent, Logger } from "../src/logging.js";
import {
  ABANDON_AFTER_MS, InMemorySettlementOutbox, SettlementDelivery, SqlSettlementOutbox, backoffMs, permanentStatus,
  type SettlementOutbox, type SqlConnector, type SqlRow,
} from "../src/outbox.js";
import { SETTLEMENT_FORMAT, type RunSettlement } from "../src/settlement.js";

const { DatabaseSync } = createRequire(import.meta.url)("node:sqlite") as typeof import("node:sqlite");
const OUTBOX_SQL = readFileSync(new URL("../sql/settlement-outbox.sql", import.meta.url), "utf8");

/** The Postgres SQL, run on SQLite after the same mechanical rewrite apps/api's double does. */
function sqliteConnector(): SqlConnector {
  const db = new DatabaseSync(":memory:");
  for (const stmt of OUTBOX_SQL.replace(/--[^\n]*/g, "").split(";").map((x) => x.trim()).filter(Boolean)) db.exec(stmt);
  const exec = {
    async query(text: string, params: unknown[] = []): Promise<SqlRow[]> {
      const sql = text.replace(/::(?:text|bigint|smallint|integer)\b/g, "").replace(/\$(\d+)/g, "?$1");
      const stmt = db.prepare(sql);
      const bound = params.map((p) => (typeof p === "boolean" ? (p ? 1 : 0) : p)) as (string | number | null)[];
      if (/^\s*(SELECT|WITH)|RETURNING/i.test(sql)) return stmt.all(...bound) as SqlRow[];
      stmt.run(...bound);
      return [];
    },
  };
  return (fn) => fn(exec);
}

const backends: Array<[string, () => SettlementOutbox]> = [
  ["in-memory", () => new InMemorySettlementOutbox()],
  ["sql (sqlite double)", () => new SqlSettlementOutbox(sqliteConnector())],
];

function settlement(over: Partial<RunSettlement> = {}): RunSettlement {
  return {
    format: SETTLEMENT_FORMAT, run_id: "r1", receipt_id: "rcpt_run_r1", ts: 1_000, tool_id: "acme.lookup", provider_id: "acme", first_party: false,
    payout_account_ref: "acct_1Acme", marketplace: null, outcome: "ok", quantity: 1, provider_micro_usd: 1000, ...over,
  };
}

function poster(answers: number[]) {
  const sent: RunSettlement[][] = [];
  const post = async (batch: RunSettlement[]) => {
    sent.push(batch);
    const status = answers.length > 1 ? (answers.shift() as number) : (answers[0] ?? 200);
    if (status === 0) throw new Error("network");
    return { status };
  };
  return { post, sent };
}

function logger(): Logger & { events: LogEvent[] } {
  const events: LogEvent[] = [];
  return { events, log: (e) => events.push(e) };
}

describe("backoff and permanence", () => {
  it("doubles from 30 s up to one hour and never gives up on a retryable status", () => {
    expect([1, 2, 3, 4].map(backoffMs)).toEqual([30_000, 60_000, 120_000, 240_000]);
    expect(backoffMs(50)).toBe(3_600_000);
    expect([400, 404, 413, 422].every(permanentStatus)).toBe(true);
    expect([0, 401, 403, 408, 429, 500, 503].some(permanentStatus)).toBe(false);
  });
});

for (const [name, make] of backends) {
  describe(`outbox (${name})`, () => {
    it("open → put replaces the draft; the inline delivery marks it delivered; redelivery is not attempted", async () => {
      const outbox = make();
      let clock = 10_000;
      const { post, sent } = poster([200]);
      const delivery = new SettlementDelivery(outbox, post, logger(), { now: () => clock });
      await outbox.open(settlement({ outcome: "transport_error", quantity: 0, provider_micro_usd: 0 }), clock);
      await outbox.put(settlement(), clock);
      expect(await outbox.stats()).toMatchObject({ open: 0, pending: 1 });
      expect(await delivery.deliverNow(settlement())).toBe(true);
      expect(await outbox.stats()).toMatchObject({ pending: 0, delivered: 1 });
      clock += 60_000;
      expect(await delivery.flush()).toEqual({ delivered: 0, retried: 0, dead: 0 });
      expect(sent).toHaveLength(1);
      expect(sent[0]?.[0]).toMatchObject({ outcome: "ok", provider_micro_usd: 1000 });
    });

    it("a failed inline delivery stays pending and the retry job delivers it, with backoff between failures", async () => {
      const outbox = make();
      let clock = 10_000;
      const { post, sent } = poster([503, 0, 503, 200]);
      const delivery = new SettlementDelivery(outbox, post, logger(), { now: () => clock });
      await outbox.put(settlement(), clock);
      expect(await delivery.deliverNow(settlement())).toBe(false);
      expect(await delivery.flush()).toEqual({ delivered: 0, retried: 1, dead: 0 }); // network error
      expect(await delivery.flush()).toEqual({ delivered: 0, retried: 0, dead: 0 }); // not due yet
      clock += backoffMs(1);
      expect(await delivery.flush()).toEqual({ delivered: 0, retried: 1, dead: 0 }); // 503
      clock += backoffMs(2) - 1;
      expect(await delivery.flush()).toEqual({ delivered: 0, retried: 0, dead: 0 });
      clock += 1;
      expect(await delivery.flush()).toEqual({ delivered: 1, retried: 0, dead: 0 });
      expect(sent).toHaveLength(4);
      expect(await outbox.stats()).toMatchObject({ pending: 0, delivered: 1 });
    });

    it("a draft whose run died is promoted after the abandon window and delivered as a $0 transport_error", async () => {
      const outbox = make();
      let clock = 10_000;
      const { post, sent } = poster([200]);
      const delivery = new SettlementDelivery(outbox, post, logger(), { now: () => clock });
      await outbox.open(settlement({ outcome: "transport_error", quantity: 0, provider_micro_usd: 0 }), clock);
      clock += ABANDON_AFTER_MS - 1;
      expect(await delivery.flush()).toEqual({ delivered: 0, retried: 0, dead: 0 });
      expect(await outbox.stats()).toMatchObject({ open: 1 });
      clock += 1;
      expect(await delivery.flush()).toEqual({ delivered: 1, retried: 0, dead: 0 });
      expect(sent[0]?.[0]).toMatchObject({ run_id: "r1", receipt_id: "rcpt_run_r1", outcome: "transport_error", provider_micro_usd: 0 });
    });

    it("a refused reservation discards its draft; a settled run is never overwritten by a later open or put", async () => {
      const outbox = make();
      await outbox.open(settlement({ run_id: "gone", receipt_id: "rcpt_run_gone", outcome: "transport_error", provider_micro_usd: 0, quantity: 0 }), 1);
      await outbox.discard("gone");
      await outbox.put(settlement(), 1);
      await outbox.open(settlement({ outcome: "transport_error", provider_micro_usd: 0, quantity: 0 }), 2);
      await outbox.put(settlement({ provider_micro_usd: 9999 }), 2);
      const claims = await outbox.claimDue(5, 100, 0, 65);
      expect(claims.map((c) => [c.settlement.run_id, c.settlement.outcome, c.settlement.provider_micro_usd])).toEqual([["r1", "ok", 1000]]);
      // Claimed rows are leased: a concurrent flush does not see them.
      expect(await outbox.claimDue(6, 100, 0, 66)).toEqual([]);
    });

    it("a malformed settlement in a batch is isolated: the others are delivered, it goes dead, nothing is dropped", async () => {
      const outbox = make();
      for (const id of ["a", "b", "c"]) await outbox.put(settlement({ run_id: id, receipt_id: `rcpt_run_${id}` }), 1);
      const post = async (batch: RunSettlement[]) => ({ status: batch.some((s) => s.run_id === "b") ? 400 : 200 });
      const log = logger();
      const delivery = new SettlementDelivery(outbox, post, log, { now: () => 2 });
      expect(await delivery.flush()).toEqual({ delivered: 2, retried: 0, dead: 1 });
      expect(await outbox.stats()).toMatchObject({ delivered: 2, dead: 1, pending: 0 });
      expect(log.events.map((e) => e.outcome)).toEqual(["delivered", "dead"]);
    });

    it("bounds a flush to its batches and prunes delivered rows after 30 days, never dead ones", async () => {
      const outbox = make();
      for (let i = 0; i < 7; i++) await outbox.put(settlement({ run_id: `r${i}`, receipt_id: null }), 1);
      const delivery = new SettlementDelivery(outbox, async () => ({ status: 200 }), logger(), { now: () => 2, batch: 2, maxBatches: 2 });
      expect((await delivery.flush()).delivered).toBe(4);
      expect((await delivery.flush()).delivered).toBe(3);
      expect(await outbox.prune(3)).toBe(7);
      expect(await outbox.stats()).toMatchObject({ delivered: 0 });
    });
  });
}

describe("outbox rows are content-free", () => {
  it("the schema has exactly the settlement's fields plus delivery bookkeeping (no body column)", () => {
    const cols = [...OUTBOX_SQL.matchAll(/^\s{2}([a-z_]+)\s+(TEXT|BIGINT|SMALLINT|INTEGER)/gm)].map((m) => m[1]);
    expect(cols.sort()).toEqual([
      "attempts", "created_at", "delivered_at", "first_party", "last_status", "marketplace", "next_attempt_at", "outcome", "payout_account_ref",
      "provider_id", "provider_micro_usd", "quantity", "receipt_id", "run_id", "state", "tool_id", "ts", "updated_at",
    ]);
  });
});
