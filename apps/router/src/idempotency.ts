/**
 * Idempotency for `run` (E8-T5, E48; CLAUDE.md "every write path takes an idempotency key"). A
 * repeated key from the same actor never runs and never charges twice, on any isolate:
 *
 *   get      a completed record answers the retry with a replay marker (below); a record still
 *            running answers 409 `idempotency_in_progress` with `Retry-After`; the same key with a
 *            different tool or input is 409 `idempotency_conflict`.
 *   claim    an atomic insert, taken right before money is reserved. Two isolates racing on one key
 *            both try it; exactly one wins and the other answers as above. A claim whose isolate
 *            died is taken over once it is older than `CLAIM_STALE_MS` (the outbox's abandon window,
 *            so the dead run's hold has been released at $0 by then).
 *   complete the run produced a result: the record keeps the run id, the outcome and the usage the
 *            caller was charged. Written BEFORE the settlement, so a crash between the two can only
 *            under-charge (the draft releases the hold at $0), never run again and charge twice.
 *   release  the run failed or was refused (nothing charged): the claim is dropped so the same key
 *            can be retried.
 *
 * What is stored (invariant 2: bodies never persist outside the principal's store): the actor (an
 * opaque hash of the caller's key), a SHA-256 of the idempotency key, the tool id, a salted
 * fingerprint of the input (a one-way hash keyed by actor, key and tool, so equal inputs of
 * different callers do not correlate), the run id, the state, the outcome and the usage numbers.
 * Never the input, never the result. A replay therefore cannot return the result body:
 *
 *   { "resultType": "replay", "replayed": true, "run_id": "…", "original": { "outcome", "usage",
 *     "completed_at" }, "usage": { …0 }, "next_action": null }
 *
 * The result was returned once, to the call that ran it. An agent that lost it runs again with a
 * new idempotency key (and pays again); see apps/router/README.md "Idempotency".
 *
 * `SqlIdempotencyStore` is production (Postgres through the Worker's Hyperdrive binding, the same
 * connector as the settlement outbox; apps/router/sql/run-idempotency.sql). `InMemoryIdempotencyStore`
 * is for development and tests and behaves the same within one isolate.
 */
import { sha256Hex } from "./hash.js";
import type { SqlConnector, SqlRow } from "./outbox.js";
import { ABANDON_AFTER_MS } from "./outbox.js";
import type { Usage } from "./usage.js";

/** How long a completed record answers replays. */
export const IDEMPOTENCY_TTL_MS = 24 * 60 * 60 * 1000;
/** A claim older than this belongs to a run whose isolate died; another isolate may take it over.
 * Equal to the outbox's abandon window: by then the dead run's hold was released at $0. */
export const CLAIM_STALE_MS = ABANDON_AFTER_MS;

export type IdempotencyState = "running" | "done";
export type CompletedOutcome = "ok" | "empty";

export interface IdempotencyRecord {
  tool_id: string;
  fingerprint: string;
  run_id: string;
  state: IdempotencyState;
  claimed_at: number;
  /** Set when `state` is `done`. */
  outcome?: CompletedOutcome;
  /** What the caller was charged for the original run. Set when `state` is `done`. */
  usage?: Usage;
  completed_at?: number;
}

export interface IdempotencyClaim {
  tool_id: string;
  fingerprint: string;
  run_id: string;
}

export interface IdempotencyStore {
  /** The live record for `(actor, keyHash)`, or undefined (none, or expired). */
  get(actor: string, keyHash: string, at: number): Promise<IdempotencyRecord | undefined>;
  /** Atomically claim the key for `claim.run_id`. True when this caller holds it now: no record,
   * an expired one, or a stale `running` one with the same fingerprint. */
  claim(actor: string, keyHash: string, claim: IdempotencyClaim, at: number): Promise<boolean>;
  /** Mark the claim held by `runId` done with its outcome and charged usage. */
  complete(actor: string, keyHash: string, runId: string, outcome: CompletedOutcome, usage: Usage, at: number): Promise<void>;
  /** Drop the claim held by `runId` while it is still running (a failed or refused run). */
  release(actor: string, keyHash: string, runId: string): Promise<void>;
  /** Retention: remove expired records. Returns how many. */
  prune(at: number): Promise<number>;
}

/** The stored form of the agent's idempotency key: its SHA-256, never the key itself. */
export function idempotencyKeyHash(key: string): string {
  return sha256Hex(`idem\u0000${key}`);
}

/** A one-way fingerprint of the request, salted by who sent it under which key, for conflict
 * detection only. */
export function requestFingerprint(actor: string, keyHash: string, toolId: string, inputHash: string): string {
  return sha256Hex(`fp\u0000${actor}\u0000${keyHash}\u0000${toolId}\u0000${inputHash}`);
}

// --- in memory ----------------------------------------------------------------------------------

interface MemRecord extends IdempotencyRecord {
  expires_at: number;
}

export class InMemoryIdempotencyStore implements IdempotencyStore {
  readonly #entries = new Map<string, MemRecord>();
  readonly #ttlMs: number;
  readonly #staleMs: number;
  readonly #max: number;
  /** Tests: make the next `n` operations throw, as an unavailable database would. */
  failNext = 0;

  constructor(opts: { ttlMs?: number; staleMs?: number; max?: number } = {}) {
    this.#ttlMs = opts.ttlMs ?? IDEMPOTENCY_TTL_MS;
    this.#staleMs = opts.staleMs ?? CLAIM_STALE_MS;
    this.#max = opts.max ?? 10_000;
  }

  #guard(): void {
    if (this.failNext > 0) {
      this.failNext -= 1;
      throw new Error("idempotency store unavailable");
    }
  }

  async get(actor: string, keyHash: string, at: number): Promise<IdempotencyRecord | undefined> {
    this.#guard();
    const e = this.#entries.get(`${actor}\u0000${keyHash}`);
    if (!e || e.expires_at <= at) return undefined;
    const { expires_at: _expires, ...record } = e;
    return { ...record, ...(record.usage ? { usage: { ...record.usage } } : {}) };
  }

  async claim(actor: string, keyHash: string, claim: IdempotencyClaim, at: number): Promise<boolean> {
    this.#guard();
    const k = `${actor}\u0000${keyHash}`;
    const e = this.#entries.get(k);
    const free = !e || e.expires_at <= at || (e.state === "running" && e.claimed_at <= at - this.#staleMs && e.fingerprint === claim.fingerprint);
    if (!free) return false;
    if (!e && this.#entries.size >= this.#max) {
      const oldest = this.#entries.keys().next().value;
      if (oldest !== undefined) this.#entries.delete(oldest);
    }
    this.#entries.delete(k);
    this.#entries.set(k, { ...claim, state: "running", claimed_at: at, expires_at: at + this.#ttlMs });
    return true;
  }

  async complete(actor: string, keyHash: string, runId: string, outcome: CompletedOutcome, usage: Usage, at: number): Promise<void> {
    this.#guard();
    const e = this.#entries.get(`${actor}\u0000${keyHash}`);
    if (!e || e.run_id !== runId) return;
    e.state = "done";
    e.outcome = outcome;
    e.usage = { ...usage };
    e.completed_at = at;
    e.expires_at = at + this.#ttlMs;
  }

  async release(actor: string, keyHash: string, runId: string): Promise<void> {
    this.#guard();
    const k = `${actor}\u0000${keyHash}`;
    const e = this.#entries.get(k);
    if (e && e.run_id === runId && e.state === "running") this.#entries.delete(k);
  }

  async prune(at: number): Promise<number> {
    let n = 0;
    for (const [k, e] of this.#entries) {
      if (e.expires_at <= at) {
        this.#entries.delete(k);
        n += 1;
      }
    }
    return n;
  }
}

// --- Postgres -----------------------------------------------------------------------------------

export const IDEMPOTENCY_TABLE = "router_run_idempotency";

const COLUMNS = "tool_id,fingerprint,run_id,state,claimed_at,outcome,usage_unit,usage_quantity,usage_cost_micro_usd,completed_at";

function str(v: unknown): string {
  return typeof v === "string" ? v : String(v ?? "");
}
function num(v: unknown): number {
  const n = typeof v === "number" ? v : Number(v);
  return Number.isFinite(n) ? n : 0;
}

function recordFromRow(r: SqlRow): IdempotencyRecord {
  const state: IdempotencyState = str(r["state"]) === "done" ? "done" : "running";
  const record: IdempotencyRecord = { tool_id: str(r["tool_id"]), fingerprint: str(r["fingerprint"]), run_id: str(r["run_id"]), state, claimed_at: num(r["claimed_at"]) };
  if (state === "done") {
    record.outcome = str(r["outcome"]) === "empty" ? "empty" : "ok";
    record.usage = { unit: str(r["usage_unit"]) || "call", quantity: num(r["usage_quantity"]), cost_micro_usd: num(r["usage_cost_micro_usd"]) };
    record.completed_at = num(r["completed_at"]);
  }
  return record;
}

/**
 * The cross-isolate store. Every operation is one statement on one connection (the outbox's
 * `SqlConnector`); `claim` is a single `INSERT … ON CONFLICT DO UPDATE … WHERE … RETURNING`, so
 * the database decides which of two racing isolates holds the key.
 */
export class SqlIdempotencyStore implements IdempotencyStore {
  readonly #connect: SqlConnector;
  readonly #ttlMs: number;
  readonly #staleMs: number;

  constructor(connect: SqlConnector, opts: { ttlMs?: number; staleMs?: number } = {}) {
    this.#connect = connect;
    this.#ttlMs = opts.ttlMs ?? IDEMPOTENCY_TTL_MS;
    this.#staleMs = opts.staleMs ?? CLAIM_STALE_MS;
  }

  /** Fails when the table is missing (run-idempotency.sql not applied). */
  async checkSchema(): Promise<void> {
    await this.#connect((sql) => sql.query(`SELECT 1 FROM ${IDEMPOTENCY_TABLE} LIMIT 1`));
  }

  async get(actor: string, keyHash: string, at: number): Promise<IdempotencyRecord | undefined> {
    const rows = await this.#connect((sql) => sql.query(`SELECT ${COLUMNS} FROM ${IDEMPOTENCY_TABLE} WHERE actor=$1::text AND key_hash=$2::text AND expires_at>$3::bigint`, [actor, keyHash, at]));
    const row = rows[0];
    return row ? recordFromRow(row) : undefined;
  }

  async claim(actor: string, keyHash: string, claim: IdempotencyClaim, at: number): Promise<boolean> {
    const rows = await this.#connect((sql) =>
      sql.query(
        `INSERT INTO ${IDEMPOTENCY_TABLE}(actor,key_hash,tool_id,fingerprint,run_id,state,claimed_at,expires_at)
         VALUES($1::text,$2::text,$3::text,$4::text,$5::text,'running',$6::bigint,$7::bigint)
         ON CONFLICT(actor,key_hash) DO UPDATE SET tool_id=excluded.tool_id, fingerprint=excluded.fingerprint, run_id=excluded.run_id,
           state='running', claimed_at=excluded.claimed_at, expires_at=excluded.expires_at,
           outcome=NULL, usage_unit=NULL, usage_quantity=NULL, usage_cost_micro_usd=NULL, completed_at=NULL
         WHERE ${IDEMPOTENCY_TABLE}.expires_at<=$6::bigint
            OR (${IDEMPOTENCY_TABLE}.state='running' AND ${IDEMPOTENCY_TABLE}.claimed_at<=$8::bigint AND ${IDEMPOTENCY_TABLE}.fingerprint=excluded.fingerprint)
         RETURNING run_id`,
        [actor, keyHash, claim.tool_id, claim.fingerprint, claim.run_id, at, at + this.#ttlMs, at - this.#staleMs],
      ),
    );
    return rows.length === 1 && str(rows[0]?.["run_id"]) === claim.run_id;
  }

  async complete(actor: string, keyHash: string, runId: string, outcome: CompletedOutcome, usage: Usage, at: number): Promise<void> {
    await this.#connect((sql) =>
      sql.query(
        `UPDATE ${IDEMPOTENCY_TABLE} SET state='done', outcome=$4::text, usage_unit=$5::text, usage_quantity=$6, usage_cost_micro_usd=$7::bigint,
           completed_at=$8::bigint, expires_at=$9::bigint
         WHERE actor=$1::text AND key_hash=$2::text AND run_id=$3::text`,
        [actor, keyHash, runId, outcome, usage.unit, usage.quantity, Math.round(usage.cost_micro_usd), at, at + this.#ttlMs],
      ),
    );
  }

  async release(actor: string, keyHash: string, runId: string): Promise<void> {
    await this.#connect((sql) => sql.query(`DELETE FROM ${IDEMPOTENCY_TABLE} WHERE actor=$1::text AND key_hash=$2::text AND run_id=$3::text AND state='running'`, [actor, keyHash, runId]));
  }

  async prune(at: number): Promise<number> {
    const rows = await this.#connect((sql) => sql.query(`DELETE FROM ${IDEMPOTENCY_TABLE} WHERE expires_at<=$1::bigint RETURNING run_id`, [at]));
    return rows.length;
  }
}
