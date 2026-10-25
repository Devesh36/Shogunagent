/**
 * The narrow SQL seam the shared store is written against (issue #29 FR-SVC-04).
 *
 * Two methods: run a statement, or run a function inside one transaction. The production client
 * is Postgres (`postgresClient`, via the small `postgres` driver the website already uses); the
 * unit tests hand the same store an in-memory SQLite double that runs the same SQL, so the
 * store's statements are exercised without a database server in CI.
 *
 * SQL written against this seam sticks to the dialect both sides accept: `$n` placeholders with
 * `::type` casts (the SQLite double rewrites both), `INSERT … ON CONFLICT … DO UPDATE … WHERE …
 * RETURNING`, `CASE` instead of `GREATEST`/`LEAST`, and `SELECT … FOR UPDATE` row locks (the
 * double serializes whole transactions instead).
 *
 * Two transports, one client:
 * - **Direct** (the Node entry, `RELAY_DATABASE_URL`): a TLS connection to Postgres. TLS is on
 *   unless the URL itself says otherwise (`sslmode=` / `ssl=`), so a copied URL without the
 *   parameter cannot silently downgrade to cleartext (audit 2026-10-10 F8).
 * - **Hyperdrive** (the Cloudflare Worker, `worker.ts`): the URL is the `HYPERDRIVE` binding's
 *   `connectionString`. The Worker↔Hyperdrive leg stays inside Cloudflare's network; the
 *   Hyperdrive→origin leg is TLS by Hyperdrive's own configuration (apps/api/DEPLOY.md). Clients
 *   are per request — the Workers runtime does not allow a socket opened by one request to be
 *   used by another — so `max` is 1 and the driver's type-introspection round trip is skipped.
 *   Startup parameters are not sent: a connection pooler may not forward them, so the per-statement
 *   ceiling for that path is set on the relay's database role instead (DEPLOY.md).
 */
import postgres from "postgres";

export type Row = Record<string, unknown>;

export interface SqlExecutor {
  query(text: string, params?: unknown[]): Promise<Row[]>;
}

export interface SqlClient extends SqlExecutor {
  /** Run `fn` in one transaction: commit when it resolves, roll back when it throws. */
  transaction<T>(fn: (tx: SqlExecutor) => Promise<T>): Promise<T>;
  close(): Promise<void>;
}

/** Driver-level knobs. Defaults suit one small relay process against a pooled Postgres. */
export interface PostgresOptions {
  /** Connections per relay process (per request on Workers). */
  max?: number;
  /** Per-statement ceiling, ms, direct transport only. A stalled ledger is a 503, never a hung
   * request. */
  statementTimeoutMs?: number;
  /** The URL is a Cloudflare Hyperdrive binding's `connectionString` (see the module comment). */
  hyperdrive?: boolean;
}

/** `ssl: "require"` unless the URL carries its own `sslmode`/`ssl` (e.g. `verify-full`, or
 * `disable` for a throwaway local database in `pg-store.test.ts`). The driver would otherwise
 * read a URL without the parameter as "no TLS". Never logs or returns any part of the URL. */
export function sslOptionFor(url: string): { ssl?: "require" } {
  try {
    const params = new URL(url).searchParams;
    if (params.has("sslmode") || params.has("ssl")) return {};
  } catch {
    // Not a URL the driver will accept either; let it raise its own error on connect.
  }
  return { ssl: "require" };
}

export function postgresClient(url: string, opts: PostgresOptions = {}): SqlClient {
  const hyperdrive = opts.hyperdrive ?? false;
  const sql = postgres(url, {
    max: opts.max ?? (hyperdrive ? 1 : 10),
    idle_timeout: 30,
    connect_timeout: 5,
    // Never print a notice or a query: parameters are licence/device ids.
    onnotice: () => undefined,
    // Supabase's transaction pooler (port 6543) does not support prepared statements.
    prepare: false,
    ...(hyperdrive
      ? { fetch_types: false }
      : { connection: { statement_timeout: opts.statementTimeoutMs ?? 5000 }, ...sslOptionFor(url) }),
  });
  const run = (exec: postgres.Sql | postgres.TransactionSql) =>
    async (text: string, params: unknown[] = []): Promise<Row[]> =>
      [...(await exec.unsafe(text, params as postgres.ParameterOrJSON<never>[]))] as Row[];
  return {
    query: run(sql),
    transaction: async <T>(fn: (tx: SqlExecutor) => Promise<T>): Promise<T> =>
      (await sql.begin((tx) => fn({ query: run(tx) }))) as T,
    close: () => sql.end({ timeout: 5 }),
  };
}
