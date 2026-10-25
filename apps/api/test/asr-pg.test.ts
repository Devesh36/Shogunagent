/** Optional integration check against a fresh local Unix-socket Postgres, never production. */
import { readFileSync } from "node:fs";
import postgres from "postgres";
import { expect, it } from "vitest";
import { SqlAsrStore } from "../src/asr-store.js";
import type { SqlClient, SqlExecutor } from "../src/sql.js";
const socket = process.env.SHOGUN_ASR_PG_SOCKET;
it.skipIf(!socket)("serializes company speech reservations and refunds in real Postgres", async () => {
  if (!socket?.startsWith("/tmp/shogun-asr-pg.")) throw new Error("requires disposable local socket");
  const sql = postgres({ host: socket, port: 55471, database: "postgres", max: 8, onnotice: () => undefined });
  const query = (executor: typeof sql | postgres.TransactionSql): SqlExecutor["query"] => async (text, params = []) => [...await executor.unsafe(text, params as postgres.ParameterOrJSON<never>[])];
  const db: SqlClient = { query: query(sql), transaction: async <T>(fn: (tx: SqlExecutor) => Promise<T>): Promise<T> => await sql.begin(tx => fn({ query: query(tx) })) as T, close: () => sql.end() };
  try {
    await db.query("CREATE TABLE relay_spend_days(date text PRIMARY KEY,micros bigint NOT NULL)");
    const ddl = readFileSync(new URL("../sql/asr-store.sql", import.meta.url), "utf8");
    for (const statement of ddl.replace(/--[^\n]*/g, "").split(";").filter(s => s.trim())) await db.query(statement);
    let now = Date.UTC(2026, 9, 17);
    const store = new SqlAsrStore(db, () => now);
    const attempts = await Promise.allSettled(Array.from({ length: 8 }, () => store.reserve("test-license", "test-device", 2)));
    const accepted = attempts.filter(r => r.status === "fulfilled");
    expect(accepted).toHaveLength(1);
    const ticket = (accepted[0] as PromiseFulfilledResult<{ id: string }>).value;
    now += 60_000;
    await store.refresh("test-license", "test-device", ticket.id);
    await store.finish("test-license", "test-device", ticket.id);
    await store.finish("test-license", "test-device", ticket.id);
    expect(Number((await db.query("SELECT seconds FROM relay_asr_months"))[0]?.seconds)).toBe(120);
    expect(Number((await db.query("SELECT micros FROM relay_spend_days"))[0]?.micros)).toBeLessThan(34_000);
    const usage = { day: { seconds: 3600, unsupported: false }, month: { seconds: 71900, unsupported: false } };
    await Promise.all(Array.from({ length: 8 }, () => store.reconcile("test-license", usage)));
    const observed = Number((await db.query("SELECT micros FROM relay_spend_days"))[0]?.micros);
    expect(observed).toBeLessThan(1_034_000);
    expect(observed).toBeGreaterThanOrEqual(1_000_000);
    const nearCap = await Promise.allSettled([store.reserve("test-license", "new-a"), store.reserve("test-license", "new-b")]);
    expect(nearCap.filter(r => r.status === "fulfilled")).toHaveLength(1);
    expect(Number((await db.query("SELECT seconds FROM relay_asr_months"))[0]?.seconds)).toBe(72000);

  } finally { await db.close(); }
});
