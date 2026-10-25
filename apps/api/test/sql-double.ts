/** A Postgres test double for `SqlClient`: an in-memory SQLite database that runs the store's
 * real SQL (and the real `sql/shared-store.sql`) after a mechanical rewrite of the two dialect
 * differences the store uses — `$n` placeholders (→ `?n`) and `::type` casts / `FOR UPDATE`
 * (→ dropped). Transactions are serialized, which is the SQLite analogue of the row locks the
 * store takes in Postgres; concurrency *between* Postgres sessions is verified against a real
 * server when `RELAY_TEST_DATABASE_URL` is set (pg-store.test.ts), never in CI. */
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";

import type { Row, SqlClient, SqlExecutor } from "../src/sql.js";

const { DatabaseSync } = createRequire(import.meta.url)("node:sqlite") as typeof import("node:sqlite");

export function toSqlite(text: string): string {
  return text
    .replace(/::(?:text|bigint|float8|smallint|integer|bytea)\b/g, "")
    .replace(/\s+FOR UPDATE\b/g, "")
    .replace(/\$(\d+)/g, "?$1");
}

export interface SqliteDouble extends SqlClient {
  /** Statements seen, post-rewrite — lets a test assert on what the store actually ran. */
  statements: string[];
  /** Make the next `n` statements throw, to exercise the store's failure policy. */
  failNext(n: number): void;
}

/** Apply `sql/shared-store.sql` to the double, statement by statement. SQLite has no
 * `ALTER TABLE … ADD COLUMN IF NOT EXISTS`; the clause is dropped and a "duplicate column" error
 * (the column is already in the CREATE for a fresh store) is the no-op Postgres would make of it. */
export function applySchema(db: InstanceType<typeof DatabaseSync>, sql: string): void {
  // Comments first (one holds a `;`), then statements.
  const statements = sql.replace(/--[^\n]*/g, "").split(";").map((s) => s.trim()).filter((s) => s.length > 0);
  for (const statement of statements) {
    const isAddColumn = /^ALTER TABLE \S+ ADD COLUMN IF NOT EXISTS/i.test(statement);
    try {
      db.exec(isAddColumn ? statement.replace(/ADD COLUMN IF NOT EXISTS/i, "ADD COLUMN") : statement);
    } catch (e) {
      if (!(isAddColumn && /duplicate column/i.test(String(e)))) throw e;
    }
  }
}

export const SHARED_STORE_SQL = readFileSync(new URL("../sql/shared-store.sql", import.meta.url), "utf8");

export function sqliteDouble(extraSchema = ""): SqliteDouble {
  const db = new DatabaseSync(":memory:");
  applySchema(db, SHARED_STORE_SQL);
  if (extraSchema) applySchema(db, extraSchema);
  const statements: string[] = [];
  let failures = 0;
  let chain: Promise<unknown> = Promise.resolve();

  const exec: SqlExecutor = {
    query(text: string, params: unknown[] = []): Promise<Row[]> {
      const sql = toSqlite(text);
      statements.push(sql);
      if (failures > 0) {
        failures -= 1;
        return Promise.reject(new Error("injected failure"));
      }
      const args = params.map((p) => (typeof p === "boolean" ? Number(p) : p)) as Array<string | number | null>;
      const stmt = db.prepare(sql);
      const rows = /^\s*(SELECT|INSERT[\s\S]*RETURNING|UPDATE[\s\S]*RETURNING|DELETE[\s\S]*RETURNING)/i.test(sql)
        ? stmt.all(...args) : (stmt.run(...args), []);
      return Promise.resolve(rows as Row[]);
    },
  };

  return {
    statements,
    failNext(n: number) { failures = n; },
    query: exec.query,
    transaction<T>(fn: (tx: SqlExecutor) => Promise<T>): Promise<T> {
      const next = chain.then(async () => {
        db.exec("BEGIN IMMEDIATE");
        try {
          const value = await fn(exec);
          db.exec("COMMIT");
          return value;
        } catch (e) {
          db.exec("ROLLBACK");
          throw e;
        }
      });
      chain = next.catch(() => undefined);
      return next;
    },
    close() { db.close(); return Promise.resolve(); },
  };
}
