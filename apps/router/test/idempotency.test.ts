/**
 * E48: run idempotency across isolates. Both stores (in memory, and the real SQL of
 * sql/run-idempotency.sql on an in-memory SQLite double) behave the same; two routers ("isolates")
 * sharing the SQL store never run or charge one idempotency key twice, and the store holds no body.
 */
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { describe, expect, it } from "vitest";
import {
  CLAIM_STALE_MS, IDEMPOTENCY_TTL_MS, InMemoryIdempotencyStore, SqlIdempotencyStore, idempotencyKeyHash, requestFingerprint,
  type IdempotencyStore,
} from "../src/idempotency.js";
import { InMemoryMeteringClient } from "../src/metering.js";
import type { SqlConnector, SqlRow } from "../src/outbox.js";
import { run } from "../src/run.js";
import { AGENT_KEY, bearer, makeDeps } from "./helpers.js";

const { DatabaseSync } = createRequire(import.meta.url)("node:sqlite") as typeof import("node:sqlite");
const IDEM_SQL = readFileSync(new URL("../sql/run-idempotency.sql", import.meta.url), "utf8");
const IDEM_PERMISSIONS_SQL = readFileSync(new URL("../sql/run-idempotency-permissions.sql", import.meta.url), "utf8");

type Db = InstanceType<typeof DatabaseSync>;

function sqliteDb(): Db {
  const db = new DatabaseSync(":memory:");
  for (const stmt of IDEM_SQL.replace(/--[^\n]*/g, "").split(";").map((x) => x.trim()).filter(Boolean)) db.exec(stmt);
  return db;
}

/** The Postgres SQL on SQLite after the same mechanical rewrite as test/outbox.test.ts. */
function sqliteConnector(db: Db, failures?: { next: number }): SqlConnector {
  const exec = {
    async query(text: string, params: unknown[] = []): Promise<SqlRow[]> {
      if (failures && failures.next > 0) {
        failures.next -= 1;
        throw new Error("database unavailable");
      }
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

const backends: Array<[string, () => IdempotencyStore]> = [
  ["in-memory", () => new InMemoryIdempotencyStore()],
  ["sql (sqlite double)", () => new SqlIdempotencyStore(sqliteConnector(sqliteDb()))],
];

const K = idempotencyKeyHash("key-1");
const claimOf = (run_id: string, fingerprint = "fp-a") => ({ tool_id: "acme.lookup", fingerprint, run_id });
const USAGE = { unit: "call", quantity: 1, cost_micro_usd: 1365 };

for (const [name, make] of backends) {
  describe(`idempotency store (${name})`, () => {
    it("one claim wins; complete keeps the outcome reference; get answers it until it expires", async () => {
      const s = make();
      expect(await s.get("actor", K, 1_000)).toBeUndefined();
      expect(await s.claim("actor", K, claimOf("r1"), 1_000)).toBe(true);
      expect(await s.claim("actor", K, claimOf("r2"), 1_001)).toBe(false);
      expect(await s.get("actor", K, 1_002)).toMatchObject({ state: "running", run_id: "r1", tool_id: "acme.lookup", fingerprint: "fp-a", claimed_at: 1_000 });
      await s.complete("actor", K, "r1", "ok", USAGE, 2_000);
      const done = await s.get("actor", K, 2_001);
      expect(done).toMatchObject({ state: "done", run_id: "r1", outcome: "ok", usage: USAGE, completed_at: 2_000 });
      expect(await s.claim("actor", K, claimOf("r3"), 2_002)).toBe(false);
      // Other actors and other keys are separate.
      expect(await s.claim("other", K, claimOf("r4"), 2_003)).toBe(true);
      expect(await s.claim("actor", idempotencyKeyHash("key-2"), claimOf("r5"), 2_004)).toBe(true);
      // Expiry: the record answers for a day after completion, then the key is free again.
      expect(await s.get("actor", K, 2_000 + IDEMPOTENCY_TTL_MS)).toBeUndefined();
      expect(await s.claim("actor", K, claimOf("r6", "fp-other"), 2_000 + IDEMPOTENCY_TTL_MS)).toBe(true);
    });

    it("release frees a running claim for a retry, but never a completed one or someone else's", async () => {
      const s = make();
      await s.claim("actor", K, claimOf("r1"), 1_000);
      await s.release("actor", K, "someone-else");
      expect(await s.claim("actor", K, claimOf("r2"), 1_001)).toBe(false);
      await s.release("actor", K, "r1");
      expect(await s.get("actor", K, 1_002)).toBeUndefined();
      expect(await s.claim("actor", K, claimOf("r2"), 1_002)).toBe(true);
      await s.complete("actor", K, "r2", "empty", { unit: "call", quantity: 0, cost_micro_usd: 0 }, 1_003);
      await s.release("actor", K, "r2");
      expect(await s.get("actor", K, 1_004)).toMatchObject({ state: "done", outcome: "empty" });
    });

    it("a claim whose isolate died is taken over after the stale window, by the same request only", async () => {
      const s = make();
      await s.claim("actor", K, claimOf("r1"), 1_000);
      expect(await s.claim("actor", K, claimOf("r2"), 1_000 + CLAIM_STALE_MS - 1)).toBe(false);
      expect(await s.claim("actor", K, claimOf("r2", "fp-different"), 1_000 + CLAIM_STALE_MS)).toBe(false);
      expect(await s.claim("actor", K, claimOf("r2"), 1_000 + CLAIM_STALE_MS)).toBe(true);
      // The dead run can no longer complete or release the key.
      await s.complete("actor", K, "r1", "ok", USAGE, 1_000 + CLAIM_STALE_MS + 1);
      await s.release("actor", K, "r1");
      expect(await s.get("actor", K, 1_000 + CLAIM_STALE_MS + 2)).toMatchObject({ state: "running", run_id: "r2" });
    });

    it("prune removes expired records only", async () => {
      const s = make();
      await s.claim("actor", K, claimOf("r1"), 1_000);
      await s.complete("actor", K, "r1", "ok", USAGE, 1_000);
      await s.claim("actor", idempotencyKeyHash("fresh"), claimOf("r2"), 5_000);
      expect(await s.prune(1_000 + IDEMPOTENCY_TTL_MS)).toBe(1);
      expect(await s.get("actor", idempotencyKeyHash("fresh"), 1_000 + IDEMPOTENCY_TTL_MS)).toBeDefined();
    });
  });
}

describe("the SQL store across isolates", () => {
  it("two stores on one database: exactly one of two racing claims wins", async () => {
    const db = sqliteDb();
    const a = new SqlIdempotencyStore(sqliteConnector(db));
    const b = new SqlIdempotencyStore(sqliteConnector(db));
    const [x, y] = await Promise.all([a.claim("actor", K, claimOf("ra"), 1_000), b.claim("actor", K, claimOf("rb"), 1_000)]);
    expect([x, y].filter(Boolean)).toHaveLength(1);
  });

  it("stores no body: the table has no column an input, a result or a key could go in", () => {
    const db = sqliteDb();
    const cols = (db.prepare("PRAGMA table_info(router_run_idempotency)").all() as { name: string }[]).map((c) => c.name).sort();
    expect(cols).toEqual(["actor", "claimed_at", "completed_at", "expires_at", "fingerprint", "key_hash", "outcome", "run_id", "state", "tool_id", "usage_cost_micro_usd", "usage_quantity", "usage_unit"]);
  });

  it("the permissions file enables RLS and revokes from every other role, without FORCE, GRANT or a policy", () => {
    const perms = IDEM_PERMISSIONS_SQL.replace(/--[^\n]*/g, "");
    expect(perms).toContain("ALTER TABLE router_run_idempotency ENABLE ROW LEVEL SECURITY;");
    expect(perms).toContain("REVOKE ALL ON router_run_idempotency FROM PUBLIC, anon, authenticated;");
    expect(perms).not.toMatch(/FORCE ROW LEVEL SECURITY|\bGRANT\b|\bDROP\b|CREATE POLICY/i);
  });
});

const SEARCH = "monid.tinyfish.web-search";
const okResponse = { status: 200, body: { status: "succeeded", output: { results: [{ url: "https://e.x/1" }] } } };

/** Two routers ("isolates") sharing one SQL idempotency store and one wallet. */
function twoIsolates(responder: () => Promise<typeof okResponse> | typeof okResponse) {
  const db = sqliteDb();
  const metering = new InMemoryMeteringClient();
  let calls = 0;
  const make = (prefix: string) => {
    const deps = makeDeps({ metering, responder: () => { calls += 1; return responder(); } });
    deps.idempotency = new SqlIdempotencyStore(sqliteConnector(db));
    let n = 0;
    deps.randomId = () => `${prefix}${++n}`;
    return deps;
  };
  return { a: make("a"), b: make("b"), metering, db, calls: () => calls };
}

describe("run idempotency across isolates (E48)", () => {
  it("a retry on another isolate after completion replays: no second provider call, no second charge, no body", async () => {
    const { a, b, metering, db, calls } = twoIsolates(() => okResponse);
    const first = await run(a, { input: { tool_id: SEARCH, input: { query: "SENTINEL-INPUT" }, idempotency_key: "retry-me" }, bearer: bearer() });
    expect(first.status).toBe(200);
    const again = await run(b, { input: { tool_id: SEARCH, input: { query: "SENTINEL-INPUT" }, idempotency_key: "retry-me" }, bearer: bearer() });
    expect(again.status).toBe(200);
    expect(calls()).toBe(1);
    expect(metering.balanceOf(AGENT_KEY)).toBe(1_000_000 - 1365);
    if (first.body.resultType !== "result" || again.body.resultType !== "replay") throw new Error("expected a result, then a replay");
    expect(again.body.run_id).toBe(first.body.run_id);
    expect(again.body.original).toMatchObject({ outcome: "ok", usage: { unit: "call", quantity: 1, cost_micro_usd: 1365 } });
    expect(again.body.usage.cost_micro_usd).toBe(0);
    // Nothing of the input, the result or the key itself is in the table.
    const dump = JSON.stringify(db.prepare("SELECT * FROM router_run_idempotency").all());
    expect(dump).not.toContain("SENTINEL-INPUT");
    expect(dump).not.toContain("e.x/1");
    expect(dump).not.toContain("retry-me");
    expect(dump).not.toContain(AGENT_KEY);
  });

  it("a concurrent retry on another isolate answers 409 in progress with Retry-After and does not run", async () => {
    let release!: () => void;
    const gate = new Promise<void>((r) => (release = r));
    const { a, b, metering, calls } = twoIsolates(async () => { await gate; return okResponse; });
    const pending = run(a, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "race" }, bearer: bearer() });
    await new Promise((r) => setTimeout(r, 10));
    const second = await run(b, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "race" }, bearer: bearer() });
    expect(second.status).toBe(409);
    if (second.body.resultType !== "error") throw new Error("expected an error");
    expect(second.body.error.code).toBe("idempotency_in_progress");
    expect(second.body.error.next_action).toBe("retry_after");
    expect(second.body.retry_after).toBeGreaterThanOrEqual(1);
    release();
    expect((await pending).status).toBe(200);
    expect(calls()).toBe(1);
    expect(metering.balanceOf(AGENT_KEY)).toBe(1_000_000 - 1365);
  });

  it("a failed run releases its key (nothing was charged) so the same key can be retried", async () => {
    let fail = true;
    const { a, b, metering, calls } = twoIsolates(() => (fail ? { status: 500, body: {} } : okResponse) as typeof okResponse);
    const first = await run(a, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "flaky" }, bearer: bearer() });
    expect(first.status).toBe(502);
    fail = false;
    const second = await run(b, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "flaky" }, bearer: bearer() });
    expect(second.status).toBe(200);
    expect(second.body.resultType).toBe("result");
    expect(calls()).toBe(2);
    expect(metering.balanceOf(AGENT_KEY)).toBe(1_000_000 - 1365);
  });

  it("the same key with a different input is a conflict, across isolates", async () => {
    const { a, b } = twoIsolates(() => okResponse);
    await run(a, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "k" }, bearer: bearer() });
    const c = await run(b, { input: { tool_id: SEARCH, input: { query: "y" }, idempotency_key: "k" }, bearer: bearer() });
    expect(c.status).toBe(409);
    if (c.body.resultType === "error") expect(c.body.error.code).toBe("idempotency_conflict");
  });

  it("fails closed when the store is down for a metered run; a free L1 read still answers", async () => {
    const failures = { next: 0 };
    const deps = makeDeps({ responder: () => okResponse });
    deps.idempotency = new SqlIdempotencyStore(sqliteConnector(sqliteDb(), failures));
    failures.next = 1;
    const paid = await run(deps, { input: { tool_id: SEARCH, input: { query: "x" }, idempotency_key: "down" }, bearer: bearer() });
    expect(paid.status).toBe(503);
    if (paid.body.resultType === "error") {
      expect(paid.body.error.next_action).toBe("retry_after");
      expect(paid.body.retry_after).toBe(5);
    }
    expect(deps.calls).toHaveLength(0);
    expect(deps.metering.balanceOf(AGENT_KEY)).toBe(1_000_000);

    failures.next = 2;
    const free = await run(deps, { input: { tool_id: "shogunagent.state.commitments.list", input: {}, idempotency_key: "down2" }, bearer: bearer() });
    expect(free.status).toBe(200);
    expect(deps.logs.some((e) => e.outcome === "idempotency_bypassed")).toBe(true);
  });

  it("the fingerprint is salted per caller and key, so equal inputs do not correlate", () => {
    const a = requestFingerprint("actor-a", idempotencyKeyHash("k"), "t", "h");
    expect(requestFingerprint("actor-b", idempotencyKeyHash("k"), "t", "h")).not.toBe(a);
    expect(requestFingerprint("actor-a", idempotencyKeyHash("k2"), "t", "h")).not.toBe(a);
    expect(idempotencyKeyHash("k")).toMatch(/^[0-9a-f]{64}$/);
  });
});
