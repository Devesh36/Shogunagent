/**
 * The relay's store (issue #29 FR-SVC-04): `UsageStore` and `RateLimiter` over Postgres, so every
 * Worker isolate — or every Node instance — draws from the SAME caps instead of doubling them.
 *
 * The production store: the Cloudflare Worker (`worker.ts`) runs ONLY this, through a Hyperdrive
 * binding whose query cache is disabled — the ownership and idempotency reads below are correct
 * because they are live (apps/api/DEPLOY.md). The Node entry selects it with `RELAY_DATABASE_URL`
 * (otherwise it runs the in-memory store, development only). Schema: `apps/api/sql/shared-store.sql`;
 * provisioning: apps/api/DEPLOY.md.
 *
 * The transition logic is `usage.ts`'s pure helpers (`claimInto`, `recoverFrom`,
 * `cancelSubmission`, `releaseInto`, `bindSubmission`), run on one row read under `SELECT … FOR
 * UPDATE` inside a transaction. Concurrency is handled by row locks and conditional statements:
 *
 * - **A missing identity is claimed by `INSERT … ON CONFLICT DO NOTHING`**, so two first claims
 *   for the same `client_request_id` cannot both see "absent" and both win.
 * - **The chunk and spend budgets are conditional upserts** (`… DO UPDATE SET n = n + $k WHERE
 *   n + $k <= $cap RETURNING`): the check and the increment are one statement, so N instances
 *   submitting at once cannot each read the same "used" and all pass. No row back = capped.
 * - **Chunk and spend are reserved in one transaction**, fenced by the claim's row lock
 *   (reserve-then-commit, §4.5).
 *
 * A request transaction touches only the rows of its own licence, batch and identity. Retention
 * (expired counters, old legacy owner rows, idle rate buckets) is `maintenance`, run by the
 * periodic sweep — never per request, where a `DELETE … WHERE date < …` on every submit would be a
 * full-table scan's worth of lock traffic for nothing.
 *
 * Failure policy: reservation/claim failures become `unavailable` (→ 503, fail-closed, no
 * provider call); best-effort releases swallow errors (an unreleased reservation only makes the
 * cap stricter). The rate limiter fails OPEN on a store error — pacing is not the spend control;
 * the ledger right behind it is (runbook §9-2).
 */
import { createHash, randomUUID } from "node:crypto";

import type { InFlightLimiter, InFlightRelease, RateLimiter } from "./ratelimit.js";
import type { Row, SqlClient, SqlExecutor } from "./sql.js";
import {
  BATCH_OWNER_KEEP_DAYS,
  KEY_COLUMNS,
  LedgerUnavailable,
  RATE_BUCKET_IDLE_EVICT_MS,
  RECORD_COLUMNS,
  USAGE_DAY_KEEP_DAYS,
  bindSubmission,
  cancelSubmission,
  claimInto,
  idempotencyMapKey,
  keepCutoff,
  keyValues,
  recordFromRow,
  recoverFrom,
  releaseInto,
  validSpend,
  type BatchOwners,
  type IdemMap,
  type IdemRecord,
  type IdempotencyClaim,
  type IdempotencyInput,
  type IdempotencyKey,
  type IdempotencyRecovery,
  type Reservation,
  type SpendReservation,
  type UnsettledBatch,
  type UsageStore,
} from "./usage.js";
import { resolveBatchOwner } from "./batch-owner.js";

const KEY_WHERE = "license_id=$1::text AND device_id=$2::text AND purpose=$3::text AND request_id=$4::text";

/** Every table the store needs; `checkSchema` refuses to serve until all exist. */
export const SHARED_STORE_TABLES = [
  "relay_usage_days", "relay_usage_idem", "relay_usage_owners",
  "relay_spend_days", "relay_spend_batches", "relay_rate_buckets",
] as const;

/** Thrown inside a transaction to roll it back and return a value (e.g. `spend_capped` after the
 * chunk budget was already incremented in the same transaction). */
class Rollback<T> {
  constructor(readonly value: T) {}
}

const num = (v: unknown): number => Number(v ?? 0);

export class SqlUsageStore implements UsageStore {
  constructor(private readonly db: SqlClient) {}

  /** Boot check: every table exists and is readable. Throws with table names only. */
  async checkSchema(): Promise<void> {
    for (const table of SHARED_STORE_TABLES) {
      try {
        await this.db.query(`SELECT 1 FROM ${table} LIMIT 1`);
      } catch {
        throw new LedgerUnavailable(`shared store table missing or unreadable: ${table}`);
      }
    }
  }

  private async tx<T>(fn: (tx: SqlExecutor) => Promise<T>): Promise<T> {
    try {
      return await this.db.transaction(fn);
    } catch (e) {
      if (e instanceof Rollback) return e.value as T;
      if (e instanceof LedgerUnavailable) throw e;
      // Driver errors can quote parameters (licence/device ids); never pass their text on.
      throw new LedgerUnavailable("shared usage store unavailable");
    }
  }

  private async getRecord(tx: SqlExecutor, input: IdempotencyKey, lock: boolean): Promise<IdemRecord | undefined> {
    const rows = await tx.query(`SELECT ${RECORD_COLUMNS} FROM relay_usage_idem WHERE ${KEY_WHERE}${lock ? " FOR UPDATE" : ""}`, keyValues(input));
    return recordFromRow(rows[0]);
  }

  private async putRecord(tx: SqlExecutor, input: IdempotencyKey, r: IdemRecord): Promise<void> {
    await tx.query(`INSERT INTO relay_usage_idem(${KEY_COLUMNS},${RECORD_COLUMNS})
      VALUES($1::text,$2::text,$3::text,$4::text,$5::text,$6::text,$7::text,$8::text,$9::text,$10::text)
      ON CONFLICT(${KEY_COLUMNS}) DO UPDATE SET hash=excluded.hash,date=excluded.date,provider_id=excluded.provider_id,
      relay_id=excluded.relay_id,claim_token=excluded.claim_token,state=excluded.state`,
    [...keyValues(input), r.hash, r.date, r.batchId ?? null, r.relayId ?? null, r.claimToken ?? null, r.state ?? null]);
  }

  /** Insert `r` only if the identity is absent. True when this call created it. */
  private async insertIfAbsent(tx: SqlExecutor, input: IdempotencyKey, r: IdemRecord): Promise<boolean> {
    const rows = await tx.query(`INSERT INTO relay_usage_idem(${KEY_COLUMNS},${RECORD_COLUMNS})
      VALUES($1::text,$2::text,$3::text,$4::text,$5::text,$6::text,$7::text,$8::text,$9::text,$10::text)
      ON CONFLICT(${KEY_COLUMNS}) DO NOTHING RETURNING license_id`,
    [...keyValues(input), r.hash, r.date, r.batchId ?? null, r.relayId ?? null, r.claimToken ?? null, r.state ?? null]);
    return rows.length > 0;
  }

  private async putOwner(tx: SqlExecutor, batchId: string, owner: BatchOwners[string]): Promise<void> {
    await tx.query(`INSERT INTO relay_usage_owners(batch_id,license_id,date,device_id,provider_id)
      VALUES($1::text,$2::text,$3::text,$4::text,$5::text)
      ON CONFLICT(batch_id) DO UPDATE SET license_id=excluded.license_id,date=excluded.date,
      device_id=excluded.device_id,provider_id=excluded.provider_id`,
    [batchId, owner.lic, owner.date, owner.device ?? null, owner.providerId ?? null]);
  }

  private async refundSpend(tx: SqlExecutor, batchId: string | undefined): Promise<void> {
    if (!batchId) return;
    const rows = await tx.query(`DELETE FROM relay_spend_batches WHERE batch_id=$1::text AND settled=0
      RETURNING date,estimate_micros,license_id`, [batchId]);
    const batch = rows[0];
    if (!batch) return;
    const estimate = num(batch.estimate_micros), date = String(batch.date);
    await tx.query(`UPDATE relay_spend_days SET micros=CASE WHEN micros<$1::bigint THEN 0 ELSE micros-$1::bigint END
      WHERE date=$2::text`, [estimate, date]);
    await this.adjustLicenceSpend(tx, batch.license_id, date, -estimate);
  }

  /** Move the licence day row's spend by `delta` (never below zero) when the batch knows its
   * licence; rows from before the column carry NULL and only move the global counter. */
  private async adjustLicenceSpend(tx: SqlExecutor, licenseId: unknown, date: string, delta: number): Promise<void> {
    if (typeof licenseId !== "string") return;
    await tx.query(`UPDATE relay_usage_days SET spend_micros=CASE WHEN spend_micros+$1::bigint<0 THEN 0 ELSE spend_micros+$1::bigint END
      WHERE date=$2::text AND license_id=$3::text`, [delta, date, licenseId]);
  }

  async usedOn(licenseId: string, date: string): Promise<number> {
    const rows = await this.tx(tx => tx.query("SELECT chunks FROM relay_usage_days WHERE date=$1::text AND license_id=$2::text", [date, licenseId]));
    return num(rows[0]?.chunks);
  }

  async spentOn(date: string): Promise<number> {
    const rows = await this.tx(tx => tx.query("SELECT micros FROM relay_spend_days WHERE date=$1::text", [date]));
    return num(rows[0]?.micros);
  }

  async licenseSpentOn(licenseId: string, date: string): Promise<number> {
    const rows = await this.tx(tx => tx.query("SELECT spend_micros FROM relay_usage_days WHERE date=$1::text AND license_id=$2::text", [date, licenseId]));
    return num(rows[0]?.spend_micros);
  }

  async unsettledBatches(limit: number, sinceDate: string, recheckAfterMs: number): Promise<UnsettledBatch[]> {
    if (!Number.isSafeInteger(limit) || limit <= 0) return [];
    // Deterministic order so overlapping ticks walk the same list; a batch polled since
    // `recheckAfterMs` is skipped until that much time has passed (`checked_at`, set by `markChecked`).
    const rows = await this.tx(tx => tx.query(`SELECT b.batch_id AS batch_id,o.provider_id AS provider_id FROM relay_spend_batches b
      JOIN relay_usage_owners o ON o.batch_id=b.batch_id
      WHERE b.settled=0 AND b.date>=$1::text AND o.provider_id IS NOT NULL
        AND (b.checked_at IS NULL OR b.checked_at<$3::bigint)
      ORDER BY b.date,b.batch_id LIMIT $2::bigint`, [sinceDate, limit, recheckAfterMs]));
    return rows.map(r => ({ batchId: String(r.batch_id), providerId: String(r.provider_id) }));
  }

  markChecked(batchId: string, nowMs: number): Promise<void> {
    return this.tx(async (tx) => {
      await tx.query("UPDATE relay_spend_batches SET checked_at=$2::bigint WHERE batch_id=$1::text AND settled=0", [batchId, nowMs]);
    }).catch(() => undefined);
  }

  markSettled(batchId: string): Promise<void> {
    return this.tx(async (tx) => {
      await tx.query("UPDATE relay_spend_batches SET settled=1 WHERE batch_id=$1::text AND settled=0", [batchId]);
    });
  }

  tryReserve(licenseId: string, date: string, chunks: number, cap: number, key?: IdempotencyKey, spend?: SpendReservation): Promise<Reservation> {
    return this.tx<Reservation>(async (tx) => {
      const record = key ? await this.getRecord(tx, key, true) : undefined;
      if (key && (!record || record.state !== "pending" || record.claimToken !== key.claimToken)) return "unknown";
      if (!Number.isSafeInteger(chunks) || chunks <= 0 || !Number.isSafeInteger(cap) || cap < 0) return "capped";
      if (spend && !validSpend(spend)) return "spend_capped";
      // Check-and-increment in one statement, for the chunk cap AND the licence's spend budget
      // (H1). The SELECT … WHERE also refuses a first row that would already exceed a cap on its
      // own. Without a per-licence ceiling the spend is still booked, against no limit.
      const licenceMicros = spend?.micros ?? 0;
      const licenceCap = spend?.licenseCapMicros ?? Number.MAX_SAFE_INTEGER;
      const counted = await tx.query(`INSERT INTO relay_usage_days(date,license_id,chunks,spend_micros)
        SELECT $1::text,$2::text,$3::bigint,$5::bigint WHERE $3::bigint <= $4::bigint AND $5::bigint <= $6::bigint
        ON CONFLICT(date,license_id) DO UPDATE SET chunks=relay_usage_days.chunks+excluded.chunks,
          spend_micros=relay_usage_days.spend_micros+excluded.spend_micros
        WHERE relay_usage_days.chunks+excluded.chunks <= $4::bigint
          AND relay_usage_days.spend_micros+excluded.spend_micros <= $6::bigint
        RETURNING chunks`, [date, licenseId, chunks, cap, licenceMicros, licenceCap]);
      if (counted.length === 0) {
        // Nothing was changed; this read only names which ceiling refused (the chunk cap wins a tie).
        const row = (await tx.query("SELECT chunks,spend_micros FROM relay_usage_days WHERE date=$1::text AND license_id=$2::text", [date, licenseId]))[0];
        return num(row?.chunks) + chunks > cap ? "capped" : "license_spend_capped";
      }
      if (spend) {
        const booked = await tx.query(`INSERT INTO relay_spend_days(date,micros)
          SELECT $1::text,$2::bigint WHERE $2::bigint <= $3::bigint
          ON CONFLICT(date) DO UPDATE SET micros=relay_spend_days.micros+excluded.micros
          WHERE relay_spend_days.micros+excluded.micros <= $3::bigint
          RETURNING micros`, [date, spend.micros, spend.capMicros]);
        // Undo the chunk increment above with the transaction, not with a compensating write.
        if (booked.length === 0) throw new Rollback<Reservation>("spend_capped");
        await tx.query(`INSERT INTO relay_spend_batches(batch_id,date,estimate_micros,settled,license_id,checked_at)
          VALUES($1::text,$2::text,$3::bigint,0,$4::text,NULL)
          ON CONFLICT(batch_id) DO UPDATE SET date=excluded.date,estimate_micros=excluded.estimate_micros,settled=0,
          license_id=excluded.license_id,checked_at=NULL`,
        [spend.batchId, date, spend.micros, licenseId]);
      }
      if (key && record) {
        record.state = "submitting";
        record.date = date;
        await this.putRecord(tx, key, record);
      }
      return "ok";
    }).catch(() => "unavailable" as const);
  }

  settleSpend(batchId: string, actualMicros: number): Promise<void> {
    return this.tx(async (tx) => {
      if (!Number.isSafeInteger(actualMicros) || actualMicros < 0) return;
      // The flag flip is the once-only guard: a second settle finds no unsettled row.
      const rows = await tx.query(`UPDATE relay_spend_batches SET settled=1 WHERE batch_id=$1::text AND settled=0
        RETURNING date,estimate_micros,license_id`, [batchId]);
      const batch = rows[0];
      if (!batch) return;
      const estimate = num(batch.estimate_micros), date = String(batch.date);
      await tx.query(`UPDATE relay_spend_days
        SET micros=CASE WHEN micros-$1::bigint+$2::bigint<0 THEN 0 ELSE micros-$1::bigint+$2::bigint END
        WHERE date=$3::text`, [estimate, actualMicros, date]);
      await this.adjustLicenceSpend(tx, batch.license_id, date, actualMicros - estimate);
    });
  }

  cancelReservation(input: IdempotencyKey, chunks: number, meterId: string = input.licenseId): Promise<void> {
    return this.tx(async (tx) => {
      const current = await this.getRecord(tx, input, true);
      const map: IdemMap = current ? { [idempotencyMapKey(input)]: current } : {};
      const record = cancelSubmission(map, input, chunks);
      if (!record) return;
      // The day row the chunks were reserved on: the payer (H1), not necessarily the owner licence.
      await tx.query(`UPDATE relay_usage_days SET chunks=CASE WHEN chunks<$1::bigint THEN 0 ELSE chunks-$1::bigint END
        WHERE date=$2::text AND license_id=$3::text`, [chunks, record.date, meterId]);
      await this.putRecord(tx, input, record);
      await this.refundSpend(tx, record.relayId);
    });
  }

  release(licenseId: string, date: string, chunks: number): Promise<void> {
    return this.tx(async (tx) => {
      await tx.query(`UPDATE relay_usage_days SET chunks=CASE WHEN chunks<$1::bigint THEN 0 ELSE chunks-$1::bigint END
        WHERE date=$2::text AND license_id=$3::text`, [chunks, date, licenseId]);
    }).catch(() => undefined);
  }

  attachBatch(batchId: string, licenseId: string, date: string, _chunks: number, deviceId?: string, providerId = batchId): Promise<void> {
    return this.tx(async (tx) => {
      await this.putOwner(tx, batchId, { lic: licenseId, date, device: deviceId, providerId });
    });
  }

  /** Retention, in one short transaction per table (each DELETE walks an index on `date` or
   * `at_ms`). Request tombstones (`relay_usage_idem`) and keyed ownership are never deleted:
   * removing either would authorize a duplicate paid submission or orphan a readable batch. */
  maintenance(date: string, nowMs: number): Promise<void> {
    const dayCutoff = keepCutoff(date, USAGE_DAY_KEEP_DAYS);
    return this.tx(async (tx) => {
      await tx.query("DELETE FROM relay_usage_days WHERE date<$1::text", [dayCutoff]);
      await tx.query("DELETE FROM relay_spend_days WHERE date<$1::text", [dayCutoff]);
      await tx.query("DELETE FROM relay_spend_batches WHERE date<$1::text", [dayCutoff]);
      await tx.query(`DELETE FROM relay_usage_owners WHERE date<$1::text AND NOT EXISTS
        (SELECT 1 FROM relay_usage_idem WHERE COALESCE(relay_id,provider_id)=relay_usage_owners.batch_id)`,
      [keepCutoff(date, BATCH_OWNER_KEEP_DAYS)]);
      await tx.query("DELETE FROM relay_rate_buckets WHERE at_ms<$1::bigint", [nowMs - RATE_BUCKET_IDLE_EVICT_MS]);
    });
  }

  async batchOwner(batchId: string, deviceId?: string): Promise<string | undefined> {
    return this.tx(async (tx) => {
      const idem = (await tx.query("SELECT license_id,device_id FROM relay_usage_idem WHERE COALESCE(relay_id,provider_id)=$1::text", [batchId]))[0];
      const owner = (await tx.query("SELECT license_id,date,device_id FROM relay_usage_owners WHERE batch_id=$1::text", [batchId]))[0];
      return resolveBatchOwner(
        owner ? { lic: String(owner.license_id), date: String(owner.date), ...(owner.device_id !== null ? { device: String(owner.device_id) } : {}) } : undefined,
        idem ? { licenseId: String(idem.license_id), deviceId: String(idem.device_id) } : undefined,
        deviceId, new Date());
    });
  }

  async providerBatchId(batchId: string): Promise<string | undefined> {
    return this.tx(async (tx) => {
      const record = (await tx.query("SELECT provider_id FROM relay_usage_idem WHERE COALESCE(relay_id,provider_id)=$1::text", [batchId]))[0];
      if (record?.provider_id) return String(record.provider_id);
      const owner = (await tx.query("SELECT provider_id FROM relay_usage_owners WHERE batch_id=$1::text", [batchId]))[0];
      return owner ? String(owner.provider_id ?? batchId) : undefined;
    });
  }

  claimIdempotency(input: IdempotencyInput): Promise<IdempotencyClaim> {
    return this.tx<IdempotencyClaim>(async (tx) => {
      const claimToken = randomUUID();
      const batchId = `rb_${randomUUID()}`;
      const fresh: IdemRecord = { hash: input.payloadHash, date: input.date, relayId: batchId, state: "pending", claimToken };
      if (await this.insertIfAbsent(tx, input, fresh)) return { kind: "fresh", claimToken, batchId };
      const existing = await this.getRecord(tx, input, true);
      const map: IdemMap = existing ? { [idempotencyMapKey(input)]: existing } : {};
      const result = claimInto(map, input);
      if (result.kind === "fresh") await this.putRecord(tx, input, map[idempotencyMapKey(input)]!);
      return result;
    }).catch(() => ({ kind: "unavailable" } as const));
  }

  recoverIdempotency(input: IdempotencyKey, retireIfMissing = false, today?: string): Promise<IdempotencyRecovery> {
    return this.tx<IdempotencyRecovery>(async (tx) => {
      if (retireIfMissing && await this.insertIfAbsent(tx, input, { hash: "", date: "", state: "retired" })) {
        return { kind: "retired" };
      }
      const existing = await this.getRecord(tx, input, retireIfMissing);
      const map: IdemMap = existing ? { [idempotencyMapKey(input)]: existing } : {};
      const result = recoverFrom(map, input, retireIfMissing, today);
      if (retireIfMissing && result.kind === "retired") await this.putRecord(tx, input, map[idempotencyMapKey(input)]!);
      return result;
    });
  }

  bindIdempotency(input: IdempotencyKey, batchId: string): Promise<string> {
    return this.tx(async (tx) => {
      const existing = await this.getRecord(tx, input, true);
      const map: IdemMap = existing ? { [idempotencyMapKey(input)]: existing } : {};
      const owners: BatchOwners = {};
      const relayId = bindSubmission(map, owners, input, batchId);
      await this.putRecord(tx, input, map[idempotencyMapKey(input)]!);
      await this.putOwner(tx, relayId, owners[relayId]!);
      return relayId;
    });
  }

  releaseIdempotency(input: IdempotencyKey): Promise<void> {
    return this.tx(async (tx) => {
      const existing = await this.getRecord(tx, input, true);
      if (!existing) return;
      const map: IdemMap = { [idempotencyMapKey(input)]: existing };
      const wasSubmitting = existing.state === "submitting";
      releaseInto(map, input);
      const record = map[idempotencyMapKey(input)]!;
      await this.putRecord(tx, input, record);
      if (wasSubmitting && record.state === "pending") await this.refundSpend(tx, record.relayId);
    }).catch(() => undefined);
  }
}

/** How often the limiter's own fallback eviction of idle buckets may run, per isolate. The sweep
 * proper is `SqlUsageStore.maintenance` from the cron; this exists so a deployment whose cron is
 * off or missed still does not grow `relay_rate_buckets` without bound. */
export const RATE_BUCKET_SWEEP_EVERY_MS = 15 * 60_000;

/** When this isolate last evicted idle buckets, or `undefined` before its first `take`. Module
 * scope on purpose: the Worker builds a store — and so a limiter — per request, and an instance
 * field would make "once a while" into "once per request". The
 * first `take` in an isolate only arms it; a fresh isolate must not start with a DELETE. */
let lastBucketSweepMs: number | undefined;

/** Tests only: forget the isolate's sweep timestamp. */
export function resetRateBucketSweepForTests(): void {
  lastBucketSweepMs = undefined;
}

/**
 * `TokenBucketLimiter` semantics over the shared store: one bucket per key for every instance.
 * Keys are hashed before storage — a bucket key is licence + device + route, and Postgres text
 * cannot hold its NUL separators anyway.
 */
export class SqlRateLimiter implements RateLimiter {
  constructor(
    private readonly db: SqlClient,
    private readonly burst: number,
    private readonly perMinute: number,
    private readonly onError: () => void = () => undefined,
  ) {}

  private static id(key: string): string {
    return createHash("sha256").update(key).digest("hex");
  }

  private refill(row: Row | undefined, nowMs: number): number {
    if (!row) return this.burst;
    const elapsed = Math.max(0, nowMs - num(row.at_ms));
    return Math.min(this.burst, num(row.tokens) + (elapsed / 60_000) * this.perMinute);
  }

  async take(key: string, nowMs: number): Promise<boolean> {
    const id = SqlRateLimiter.id(key);
    try {
      const allowed = await this.db.transaction(async (tx) => {
        await tx.query(`INSERT INTO relay_rate_buckets(bucket_key,tokens,at_ms) VALUES($1::text,$2::float8,$3::bigint)
          ON CONFLICT(bucket_key) DO NOTHING`, [id, this.burst, nowMs]);
        const row = (await tx.query("SELECT tokens,at_ms FROM relay_rate_buckets WHERE bucket_key=$1::text FOR UPDATE", [id]))[0];
        const tokens = this.refill(row, nowMs);
        const ok = tokens >= 1;
        await tx.query("UPDATE relay_rate_buckets SET tokens=$2::float8,at_ms=$3::bigint WHERE bucket_key=$1::text",
          [id, ok ? tokens - 1 : tokens, nowMs]);
        return ok;
      });
      if (lastBucketSweepMs === undefined) {
        lastBucketSweepMs = nowMs;
      } else if (nowMs - lastBucketSweepMs > RATE_BUCKET_SWEEP_EVERY_MS) {
        lastBucketSweepMs = nowMs;
        await this.db.query("DELETE FROM relay_rate_buckets WHERE at_ms<$1::bigint", [nowMs - RATE_BUCKET_IDLE_EVICT_MS]).catch(() => undefined);
      }
      return allowed;
    } catch {
      // Fail open (runbook §9-2): the chunk cap and the spend breaker behind this are fail-closed.
      this.onError();
      return true;
    }
  }

  async retryAfterSecs(key: string, nowMs: number): Promise<number> {
    if (this.perMinute <= 0) return 1;
    try {
      const row = (await this.db.query("SELECT tokens,at_ms FROM relay_rate_buckets WHERE bucket_key=$1::text", [SqlRateLimiter.id(key)]))[0];
      const tokens = this.refill(row, nowMs);
      if (tokens >= 1) return 1;
      return Math.max(1, Math.ceil(((1 - tokens) / this.perMinute) * 60));
    } catch {
      return 1;
    }
  }
}

/** The stored key of a bucket that is not a token bucket: a fixed prefix keeps a window or an
 * in-flight slot from ever sharing a row with a `SqlRateLimiter` bucket. */
function prefixedId(prefix: string, key: string): string {
  return createHash("sha256").update(`${prefix}\u0000${key}`).digest("hex");
}

/**
 * `WindowLimiter` (ratelimit.ts) over the shared store: at most `limit` takes per key per window,
 * for every instance. One row per key and window in `relay_rate_buckets`: `tokens` counts the
 * takes, `at_ms` holds the window's END, so the idle-bucket eviction (ten minutes past `at_ms`)
 * cannot drop a row while its window is still open. The check and the increment are one
 * conditional upsert. Fails OPEN on a store error, like `SqlRateLimiter`: the spend ledger behind
 * it is the fail-closed control.
 */
export class SqlWindowLimiter implements RateLimiter {
  constructor(
    private readonly db: SqlClient,
    private readonly limit: number,
    private readonly windowMs: number,
    private readonly onError: () => void = () => undefined,
  ) {}

  async take(key: string, nowMs: number): Promise<boolean> {
    const window = Math.floor(nowMs / this.windowMs);
    const end = (window + 1) * this.windowMs;
    try {
      const rows = await this.db.query(`INSERT INTO relay_rate_buckets(bucket_key,tokens,at_ms) VALUES($1::text,1,$2::bigint)
        ON CONFLICT(bucket_key) DO UPDATE SET tokens=relay_rate_buckets.tokens+1
        WHERE relay_rate_buckets.tokens < $3::float8
        RETURNING tokens`, [prefixedId(`window:${window}`, key), end, this.limit]);
      return rows.length > 0;
    } catch {
      this.onError();
      return true;
    }
  }

  retryAfterSecs(_key: string, nowMs: number): number {
    const end = (Math.floor(nowMs / this.windowMs) + 1) * this.windowMs;
    return Math.max(1, Math.ceil((end - nowMs) / 1000));
  }
}

/**
 * A per-key concurrency limit over the shared store: `max` slot rows per key in
 * `relay_rate_buckets` (`tokens` 1 = held, `at_ms` = when it was taken). A slot is taken by one
 * conditional upsert, so two instances cannot both take the same one, and a held slot older than
 * `leaseMs` counts as free again: a request that died without releasing (an isolate evicted
 * mid-call) frees its slot by itself. `leaseMs` must exceed the upstream timeout. Release clears
 * only the slot this lease took (same stamp). Fails OPEN on a store error.
 */
export class SqlInFlightLimiter implements InFlightLimiter {
  constructor(
    private readonly db: SqlClient,
    private readonly max: number,
    private readonly leaseMs: number,
    private readonly onError: () => void = () => undefined,
  ) {}

  async acquire(key: string, nowMs: number): Promise<InFlightRelease | undefined> {
    try {
      for (let slot = 0; slot < this.max; slot += 1) {
        const id = prefixedId(`inflight:${slot}`, key);
        const taken = await this.db.query(`INSERT INTO relay_rate_buckets(bucket_key,tokens,at_ms) VALUES($1::text,1,$2::bigint)
          ON CONFLICT(bucket_key) DO UPDATE SET tokens=1,at_ms=excluded.at_ms
          WHERE relay_rate_buckets.tokens < 1 OR relay_rate_buckets.at_ms < $3::bigint
          RETURNING at_ms`, [id, nowMs, nowMs - this.leaseMs]);
        if (taken.length > 0) {
          let released = false;
          return async () => {
            if (released) return;
            released = true;
            await this.db.query("UPDATE relay_rate_buckets SET tokens=0 WHERE bucket_key=$1::text AND at_ms=$2::bigint", [id, nowMs])
              .catch(() => undefined);
          };
        }
      }
      return undefined;
    } catch {
      this.onError();
      return () => Promise.resolve();
    }
  }
}
