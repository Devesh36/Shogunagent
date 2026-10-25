/**
 * The durable settlement outbox (E33). Every settlement the router owes apps/api is written here
 * BEFORE it is sent, and stays until apps/api has acknowledged it, so a crash, a timeout or an
 * apps/api outage loses nothing:
 *
 *   reserve:  `open(draft)`   a $0 transport_error draft, written before apps/api holds money.
 *             `discard(id)`   the wallet refused (402, 401): nothing is held, the draft goes.
 *   settle:   `put(final)`    the run's settlement replaces the draft; then one inline delivery.
 *   cron:     `flush()`       promotes drafts older than the abandon window (the run died: its
 *                             delivery releases the hold at $0) and delivers every due row, with
 *                             exponential backoff, until apps/api answers 2xx.
 *
 * Delivery is at-least-once and apps/api is idempotent by `run_id` (the provider accrual) and by
 * `receipt_id` (the wallet settle), so a duplicate changes nothing.
 *
 * Rows hold a `RunSettlement` and delivery bookkeeping only (content-free, invariant 2;
 * apps/router/sql/settlement-outbox.sql). Logs carry fixed words and counts only.
 *
 * `InMemorySettlementOutbox` is for tests and development; `SqlSettlementOutbox` is production
 * (Postgres through the Worker's Hyperdrive binding, one connection per operation).
 */
import type { Logger } from "./logging.js";
import { SETTLEMENT_FORMAT, type RunSettlement, type SettledOutcome, type SettlementPoster } from "./settlement.js";

export type OutboxState = "open" | "pending" | "delivered" | "dead";

/** A row the retry job claimed: its settlement and how often delivery has failed so far. */
export interface OutboxClaim {
  settlement: RunSettlement;
  attempts: number;
}

export interface SettlementOutbox {
  /** Record a draft before the wallet is asked to hold money. Throws when the store is unavailable:
   * the run must then not reserve. A second open for the same run changes nothing. */
  open(draft: RunSettlement, at: number): Promise<void>;
  /** The reservation was refused: remove the draft (only while it is still open). */
  discard(runId: string): Promise<void>;
  /** Write the run's settlement (replacing its draft) as due now. Once: a row already pending,
   * delivered or dead is left alone. */
  put(settlement: RunSettlement, at: number): Promise<void>;
  /** Promote drafts created at or before `abandonBefore` to pending, then claim up to `limit` due
   * rows, pushing their next attempt to `leaseUntil` so a concurrent flush skips them. */
  claimDue(at: number, limit: number, abandonBefore: number, leaseUntil: number): Promise<OutboxClaim[]>;
  markDelivered(runId: string, at: number, status: number): Promise<void>;
  /** A failed delivery: one more attempt, retried at `nextAttemptAt`, or `dead` when permanent. */
  markFailed(runId: string, at: number, status: number, nextAttemptAt: number, permanent: boolean): Promise<void>;
  /** Rows per state. */
  stats(): Promise<Record<OutboxState, number>>;
  /** Retention: delivered rows delivered before `before`. Returns how many were removed. */
  prune(before: number): Promise<number>;
}

/** How long a draft may stay open before the run counts as abandoned. A run's provider call times
 * out after at most 60 s; ten minutes leaves room for a slow isolate without releasing a live hold. */
export const ABANDON_AFTER_MS = 10 * 60_000;
/** A claimed row is invisible to other flushes this long. */
export const CLAIM_LEASE_MS = 60_000;
/** Delivered rows are kept this long (for an operator), then pruned. Dead rows are never pruned. */
export const DELIVERED_KEEP_MS = 30 * 86_400_000;
export const MAX_SETTLEMENTS_PER_POST = 100;

/** 30 s, 1 min, 2 min, … capped at one hour. Never gives up on a retryable failure. */
export function backoffMs(attempts: number): number {
  const n = Math.max(1, attempts);
  return Math.min(60 * 60_000, 30_000 * 2 ** Math.min(n - 1, 20));
}

/** A status apps/api will answer the same way forever: the settlement itself is malformed. */
export function permanentStatus(status: number): boolean {
  return status === 400 || status === 404 || status === 413 || status === 422;
}

// --- in memory ----------------------------------------------------------------------------------

interface MemRow {
  settlement: RunSettlement;
  state: OutboxState;
  attempts: number;
  nextAttemptAt: number;
  lastStatus: number | null;
  createdAt: number;
  deliveredAt: number | null;
}

export class InMemorySettlementOutbox implements SettlementOutbox {
  readonly rows = new Map<string, MemRow>();
  /** Tests: make the next `n` operations throw, as an unavailable database would. */
  failNext = 0;

  #guard(): void {
    if (this.failNext > 0) {
      this.failNext -= 1;
      throw new Error("outbox unavailable");
    }
  }

  async open(draft: RunSettlement, at: number): Promise<void> {
    this.#guard();
    if (this.rows.has(draft.run_id)) return;
    this.rows.set(draft.run_id, { settlement: { ...draft }, state: "open", attempts: 0, nextAttemptAt: at, lastStatus: null, createdAt: at, deliveredAt: null });
  }

  async discard(runId: string): Promise<void> {
    this.#guard();
    if (this.rows.get(runId)?.state === "open") this.rows.delete(runId);
  }

  async put(settlement: RunSettlement, at: number): Promise<void> {
    this.#guard();
    const row = this.rows.get(settlement.run_id);
    if (row && row.state !== "open") return;
    this.rows.set(settlement.run_id, { settlement: { ...settlement }, state: "pending", attempts: 0, nextAttemptAt: at, lastStatus: null, createdAt: row?.createdAt ?? at, deliveredAt: null });
  }

  async claimDue(at: number, limit: number, abandonBefore: number, leaseUntil: number): Promise<OutboxClaim[]> {
    this.#guard();
    for (const row of this.rows.values()) if (row.state === "open" && row.createdAt <= abandonBefore) row.state = "pending";
    const due = [...this.rows.values()]
      .filter((r) => r.state === "pending" && r.nextAttemptAt <= at)
      .sort((a, b) => a.nextAttemptAt - b.nextAttemptAt)
      .slice(0, limit);
    for (const r of due) r.nextAttemptAt = leaseUntil;
    return due.map((r) => ({ settlement: { ...r.settlement }, attempts: r.attempts }));
  }

  async markDelivered(runId: string, at: number, status: number): Promise<void> {
    this.#guard();
    const row = this.rows.get(runId);
    if (!row || row.state !== "pending") return;
    row.state = "delivered";
    row.deliveredAt = at;
    row.lastStatus = status;
  }

  async markFailed(runId: string, _at: number, status: number, nextAttemptAt: number, permanent: boolean): Promise<void> {
    this.#guard();
    const row = this.rows.get(runId);
    if (!row || row.state !== "pending") return;
    row.attempts += 1;
    row.lastStatus = status;
    row.nextAttemptAt = nextAttemptAt;
    if (permanent) row.state = "dead";
  }

  async stats(): Promise<Record<OutboxState, number>> {
    const out: Record<OutboxState, number> = { open: 0, pending: 0, delivered: 0, dead: 0 };
    for (const r of this.rows.values()) out[r.state] += 1;
    return out;
  }

  async prune(before: number): Promise<number> {
    let n = 0;
    for (const [id, r] of this.rows) {
      if (r.state === "delivered" && (r.deliveredAt ?? 0) < before) {
        this.rows.delete(id);
        n += 1;
      }
    }
    return n;
  }
}

// --- Postgres -----------------------------------------------------------------------------------

export type SqlRow = Record<string, unknown>;

/** The narrow SQL seam (the same shape as apps/api/src/sql.ts): `$n` placeholders and `::type`
 * casts, which the SQLite test double rewrites. */
export interface SqlExecutor {
  query(text: string, params?: unknown[]): Promise<SqlRow[]>;
}

/** Runs `fn` on a connection opened for it and closed after it. Workers do not let one request's
 * socket serve another, so the production connector opens one per operation (Hyperdrive pools). */
export type SqlConnector = <T>(fn: (sql: SqlExecutor) => Promise<T>) => Promise<T>;

export const OUTBOX_TABLE = "router_settlement_outbox";

const COLUMNS = "run_id,receipt_id,ts,tool_id,provider_id,first_party,payout_account_ref,marketplace,outcome,quantity,provider_micro_usd,attempts";

const OUTCOMES: readonly SettledOutcome[] = ["ok", "empty", "provider_error", "transport_error"];

function str(v: unknown): string {
  return typeof v === "string" ? v : String(v ?? "");
}
function strOrNull(v: unknown): string | null {
  return v === null || v === undefined ? null : String(v);
}
function num(v: unknown): number {
  const n = typeof v === "number" ? v : Number(v);
  return Number.isFinite(n) ? n : 0;
}

function claimFromRow(r: SqlRow): OutboxClaim {
  const outcome = str(r["outcome"]) as SettledOutcome;
  return {
    attempts: num(r["attempts"]),
    settlement: {
      format: SETTLEMENT_FORMAT,
      run_id: str(r["run_id"]),
      receipt_id: strOrNull(r["receipt_id"]),
      ts: num(r["ts"]),
      tool_id: str(r["tool_id"]),
      provider_id: str(r["provider_id"]),
      first_party: num(r["first_party"]) === 1 || r["first_party"] === true,
      payout_account_ref: strOrNull(r["payout_account_ref"]),
      marketplace: strOrNull(r["marketplace"]),
      outcome: OUTCOMES.includes(outcome) ? outcome : "transport_error",
      quantity: num(r["quantity"]),
      provider_micro_usd: num(r["provider_micro_usd"]),
    },
  };
}

function values(s: RunSettlement): unknown[] {
  return [s.run_id, s.receipt_id, s.ts, s.tool_id, s.provider_id, s.first_party ? 1 : 0, s.payout_account_ref, s.marketplace, s.outcome, s.quantity, s.provider_micro_usd];
}

const INSERT_HEAD = `INSERT INTO ${OUTBOX_TABLE}(run_id,receipt_id,ts,tool_id,provider_id,first_party,payout_account_ref,marketplace,outcome,quantity,provider_micro_usd,state,attempts,next_attempt_at,created_at,updated_at)
  VALUES($1::text,$2::text,$3::bigint,$4::text,$5::text,$6::smallint,$7::text,$8::text,$9::text,$10::bigint,$11::bigint,$12::text,0,$13::bigint,$13::bigint,$13::bigint)`;

export class SqlSettlementOutbox implements SettlementOutbox {
  readonly #connect: SqlConnector;

  constructor(connect: SqlConnector) {
    this.#connect = connect;
  }

  /** Fails when the table is missing (settlement-outbox.sql not applied). */
  async checkSchema(): Promise<void> {
    await this.#connect((sql) => sql.query(`SELECT 1 FROM ${OUTBOX_TABLE} LIMIT 1`));
  }

  async open(draft: RunSettlement, at: number): Promise<void> {
    await this.#connect((sql) => sql.query(`${INSERT_HEAD} ON CONFLICT(run_id) DO NOTHING`, [...values(draft), "open", at]));
  }

  async discard(runId: string): Promise<void> {
    await this.#connect((sql) => sql.query(`DELETE FROM ${OUTBOX_TABLE} WHERE run_id=$1::text AND state='open'`, [runId]));
  }

  async put(settlement: RunSettlement, at: number): Promise<void> {
    await this.#connect((sql) => sql.query(`${INSERT_HEAD}
      ON CONFLICT(run_id) DO UPDATE SET receipt_id=excluded.receipt_id, outcome=excluded.outcome, quantity=excluded.quantity,
        provider_micro_usd=excluded.provider_micro_usd, payout_account_ref=excluded.payout_account_ref, marketplace=excluded.marketplace,
        state='pending', next_attempt_at=excluded.next_attempt_at, updated_at=excluded.updated_at
      WHERE ${OUTBOX_TABLE}.state='open'`, [...values(settlement), "pending", at]));
  }

  async claimDue(at: number, limit: number, abandonBefore: number, leaseUntil: number): Promise<OutboxClaim[]> {
    return this.#connect(async (sql) => {
      await sql.query(`UPDATE ${OUTBOX_TABLE} SET state='pending', updated_at=$1::bigint WHERE state='open' AND created_at<=$2::bigint`, [at, abandonBefore]);
      const rows = await sql.query(`UPDATE ${OUTBOX_TABLE} SET next_attempt_at=$2::bigint, updated_at=$1::bigint
        WHERE run_id IN (SELECT run_id FROM ${OUTBOX_TABLE} WHERE state='pending' AND next_attempt_at<=$1::bigint ORDER BY next_attempt_at LIMIT $3::integer)
        RETURNING ${COLUMNS}`, [at, leaseUntil, limit]);
      return rows.map(claimFromRow);
    });
  }

  async markDelivered(runId: string, at: number, status: number): Promise<void> {
    await this.#connect((sql) => sql.query(`UPDATE ${OUTBOX_TABLE} SET state='delivered', delivered_at=$2::bigint, updated_at=$2::bigint, last_status=$3::integer
      WHERE run_id=$1::text AND state='pending'`, [runId, at, status]));
  }

  async markFailed(runId: string, at: number, status: number, nextAttemptAt: number, permanent: boolean): Promise<void> {
    await this.#connect((sql) => sql.query(`UPDATE ${OUTBOX_TABLE} SET attempts=attempts+1, last_status=$3::integer, next_attempt_at=$4::bigint, updated_at=$2::bigint,
      state=CASE WHEN $5::integer=1 THEN 'dead' ELSE state END
      WHERE run_id=$1::text AND state='pending'`, [runId, at, status, nextAttemptAt, permanent ? 1 : 0]));
  }

  async stats(): Promise<Record<OutboxState, number>> {
    const rows = await this.#connect((sql) => sql.query(`SELECT state, COUNT(*) AS n FROM ${OUTBOX_TABLE} GROUP BY state`));
    const out: Record<OutboxState, number> = { open: 0, pending: 0, delivered: 0, dead: 0 };
    for (const r of rows) {
      const s = str(r["state"]);
      if (s === "open" || s === "pending" || s === "delivered" || s === "dead") out[s] = num(r["n"]);
    }
    return out;
  }

  async prune(before: number): Promise<number> {
    const rows = await this.#connect((sql) => sql.query(`DELETE FROM ${OUTBOX_TABLE} WHERE state='delivered' AND delivered_at<$1::bigint RETURNING run_id`, [before]));
    return rows.length;
  }
}

// --- delivery -----------------------------------------------------------------------------------

export interface FlushResult {
  delivered: number;
  retried: number;
  dead: number;
}

export interface DeliveryOptions {
  now?: () => number;
  abandonAfterMs?: number;
  leaseMs?: number;
  /** Rows per POST (≤ 100, the receiver's limit). */
  batch?: number;
  /** Batches per flush, so one cron invocation stays bounded. */
  maxBatches?: number;
}

/**
 * Sends outbox rows to apps/api and records the answer. Never throws: a failure leaves the row
 * pending for the next flush. Logs `kind=settlement` with a fixed outcome word and a count.
 */
export class SettlementDelivery {
  readonly #outbox: SettlementOutbox;
  readonly #post: SettlementPoster;
  readonly #logger: Logger;
  readonly #now: () => number;
  readonly #abandonAfter: number;
  readonly #lease: number;
  readonly #batch: number;
  readonly #maxBatches: number;

  constructor(outbox: SettlementOutbox, post: SettlementPoster, logger: Logger, opts: DeliveryOptions = {}) {
    this.#outbox = outbox;
    this.#post = post;
    this.#logger = logger;
    this.#now = opts.now ?? Date.now;
    this.#abandonAfter = opts.abandonAfterMs ?? ABANDON_AFTER_MS;
    this.#lease = opts.leaseMs ?? CLAIM_LEASE_MS;
    this.#batch = Math.min(MAX_SETTLEMENTS_PER_POST, Math.max(1, opts.batch ?? MAX_SETTLEMENTS_PER_POST));
    this.#maxBatches = Math.max(1, opts.maxBatches ?? 10);
  }

  /** Deliver one settlement just written by `put` (the inline attempt after a run). A failure is
   * left to the retry job: the row stays pending and due now. */
  async deliverNow(settlement: RunSettlement): Promise<boolean> {
    let status = 0;
    try {
      status = (await this.#post([settlement])).status;
    } catch {
      status = 0;
    }
    if (status < 200 || status >= 300) {
      this.#logger.log({ kind: "settlement", outcome: "deferred", count: 1 });
      return false;
    }
    await this.#outbox.markDelivered(settlement.run_id, this.#now(), status).catch(() => this.#logger.log({ kind: "settlement", outcome: "outbox_unavailable", count: 1 }));
    return true;
  }

  /** The retry job: promote abandoned drafts, deliver every due row, prune old delivered rows. */
  async flush(): Promise<FlushResult> {
    const total: FlushResult = { delivered: 0, retried: 0, dead: 0 };
    try {
      for (let i = 0; i < this.#maxBatches; i += 1) {
        const at = this.#now();
        const claims = await this.#outbox.claimDue(at, this.#batch, at - this.#abandonAfter, at + this.#lease);
        if (claims.length === 0) break;
        const r = await this.#deliver(claims);
        total.delivered += r.delivered;
        total.retried += r.retried;
        total.dead += r.dead;
        if (claims.length < this.#batch) break;
      }
      await this.#outbox.prune(this.#now() - DELIVERED_KEEP_MS);
    } catch {
      this.#logger.log({ kind: "settlement", outcome: "outbox_unavailable", count: 1 });
    }
    if (total.delivered) this.#logger.log({ kind: "settlement", outcome: "delivered", count: total.delivered });
    if (total.retried) this.#logger.log({ kind: "settlement", outcome: "retry_scheduled", count: total.retried });
    if (total.dead) this.#logger.log({ kind: "settlement", outcome: "dead", count: total.dead });
    return total;
  }

  async #deliver(claims: OutboxClaim[]): Promise<FlushResult> {
    const out: FlushResult = { delivered: 0, retried: 0, dead: 0 };
    let status = 0;
    try {
      status = (await this.#post(claims.map((c) => c.settlement))).status;
    } catch {
      status = 0;
    }
    const at = this.#now();
    if (status >= 200 && status < 300) {
      for (const c of claims) await this.#outbox.markDelivered(c.settlement.run_id, at, status);
      out.delivered = claims.length;
      return out;
    }
    // One malformed settlement rejects its whole batch: isolate it by sending the rest one by one.
    if (permanentStatus(status) && claims.length > 1) {
      for (const c of claims) {
        const r = await this.#deliver([c]);
        out.delivered += r.delivered;
        out.retried += r.retried;
        out.dead += r.dead;
      }
      return out;
    }
    const permanent = permanentStatus(status);
    for (const c of claims) {
      await this.#outbox.markFailed(c.settlement.run_id, at, status, at + backoffMs(c.attempts + 1), permanent);
    }
    if (permanent) out.dead = claims.length;
    else out.retried = claims.length;
    return out;
  }
}
