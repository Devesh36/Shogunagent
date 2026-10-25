/** Content-free usage/job ledger: the pure transition logic (reservations, idempotency claims,
 * batch ownership, spend) shared by every store, plus `InMemoryUsageStore` — the store of the
 * tests and of a Node dev relay without `RELAY_DATABASE_URL`. Production is `SqlUsageStore`
 * (pg-store.ts) on Postgres; this module is runtime-neutral so the Worker bundles it. */
import { randomUUID } from "node:crypto";
import { resolveBatchOwner } from "./batch-owner.js";

/** The outcome of a reservation. `unavailable` is deliberately distinct from `capped`: a
 * ledger we cannot read is not "you spent your quota", and answering 429 for it would hide an
 * operational failure behind a normal-looking client error. */
export type Reservation = "ok" | "capped" | "spend_capped" | "license_spend_capped" | "unavailable" | "unknown";

/** The spend side of one reservation (issue #29 FR-SVC-08; audit 2026-10-10 H1). Reserved in the
 * SAME transaction as the chunk budget, so a submission either holds all of it or none of it.
 * Amounts are integer micro-USD (1 USD = 1,000,000). `batchId` is the opaque relay id the claim
 * minted, so results can later settle the estimate to the provider-reported usage exactly once.
 * `micros` is booked twice: against the global day (`capMicros`, the breaker) and against the
 * licence's own day (`licenseCapMicros`, the per-licence budget; absent = booked but unbounded). */
export interface SpendReservation {
  batchId: string;
  micros: number;
  capMicros: number;
  licenseCapMicros?: number;
}

/** A spend reservation the relay has not yet replaced with the provider's usage, with the
 * provider batch id to ask about. Only bound batches appear (an unbound claim has no provider id). */
export interface UnsettledBatch {
  batchId: string;
  providerId: string;
}

export interface UsageStore {
  /** Chunks already accepted for `licenseId` on `date` (UTC `YYYY-MM-DD`). */
  usedOn(licenseId: string, date: string): Promise<number>;
  /** Atomically reserve `chunks` against `cap` for `licenseId` on `date`. Reserves nothing
   * unless the whole amount fits — a partially accepted batch is not a thing the device can
   * act on. */
  tryReserve(licenseId: string, date: string, chunks: number, cap: number, key?: IdempotencyKey, spend?: SpendReservation): Promise<Reservation>;
  /** Replace a batch's reserved spend estimate with its actual cost, once (FR-SVC-08). A second
   * call for the same batch changes nothing, so re-reading results never double-counts. */
  settleSpend(batchId: string, actualMicros: number): Promise<void>;
  /** Global spend (micro-USD) booked against `date`: estimates of unsettled batches plus the
   * actual cost of settled ones. Read by the metrics endpoint. */
  spentOn(date: string): Promise<number>;
  /** Spend (micro-USD) booked against `licenseId` on `date` — the per-licence budget's counter. */
  licenseSpentOn(licenseId: string, date: string): Promise<number>;
  /** Up to `limit` unsettled, bound batches dated `sinceDate` or later, for the settlement sweep,
   * in a fixed order (date, then batch id) so two overlapping ticks walk the same list. A batch the
   * sweep found still processing at or after `recheckAfterMs` (`markChecked`) is left out: it is
   * not asked about again until that much time has passed. */
  unsettledBatches(limit: number, sinceDate: string, recheckAfterMs: number): Promise<UnsettledBatch[]>;
  /** Record that the sweep polled `batchId` at `nowMs` and found it still processing. */
  markChecked(batchId: string, nowMs: number): Promise<void>;
  /** Close a batch's reservation keeping its estimate as the final figure (the provider can no
   * longer report usage for it). Idempotent; a settled batch is left alone. */
  markSettled(batchId: string): Promise<void>;
  /** Retention: delete day counters, spend days and spend batches older than
   * `USAGE_DAY_KEEP_DAYS`, legacy owner rows older than `BATCH_OWNER_KEEP_DAYS` and idle rate
   * buckets, all measured from `date` (today, UTC). Request tombstones are never deleted. Called
   * from the periodic sweep only — never from a request, whose transaction must stay small. */
  maintenance(date: string, nowMs: number): Promise<void>;
  /** Only the handler that has not called the provider may cancel its own unbound reservation.
   * Token check, refund and reopening happen atomically; repeating cancellation refunds nothing.
   * `meterId` is the subject the chunks were reserved on (`tryReserve`'s first argument, the
   * payer — audit 2026-10-20 H1); absent = `input.licenseId`, the subject before metering split
   * from ownership. */
  cancelReservation(input: IdempotencyKey, chunks: number, meterId?: string): Promise<void>;
  /** Give back a reservation whose batch never happened (the upstream call failed). Best
   * effort: an unreleased reservation only makes the cap stricter for the rest of the day,
   * which is the safe direction. */
  release(licenseId: string, date: string, chunks: number): Promise<void>;
  /** Bind an accepted batch to the licence that created it — the GET route's ownership check
   * (§4.1: a licence may only ever read back its own batches). */
  attachBatch(batchId: string, licenseId: string, date: string, chunks: number, deviceId?: string, providerId?: string): Promise<void>;
  /** The licence that created `batchId`, or undefined for an unknown/other-device batch.
   * Keyed request ownership survives legacy owner-map pruning. */
  batchOwner(batchId: string, deviceId?: string): Promise<string | undefined>;
  providerBatchId(batchId: string): Promise<string | undefined>;
  /**
   * Claim a `client_request_id` for this owner/device/purpose (#343). Content-free: the
   * payload is represented only by its hash. Same hash with a bound batch is a replay;
   * a different hash is a conflict; a claimed-but-unbound slot is `unknown` (do not submit).
   */
  claimIdempotency(input: IdempotencyInput): Promise<IdempotencyClaim>;
  /** Recover without captured content; optionally retire an unarmed identity atomically, or (with
   * `today`) a create left unknown on an earlier UTC day ([`recoverFrom`]). */
  recoverIdempotency(input: IdempotencyKey, retireIfMissing?: boolean, today?: string): Promise<IdempotencyRecovery>;
  /** Attach the provider/relay batch id to a fresh claim after upstream acceptance. */
  bindIdempotency(input: IdempotencyKey, batchId: string): Promise<string>;
  /** Reopen a definitely unsubmitted/rejected claim while preserving its payload identity.
   * Also refunds that claim's unsettled spend reservation, if it holds one. */
  releaseIdempotency(input: IdempotencyKey): Promise<void>;
}

/** Owner + device + purpose + client request id. Never includes chunk text. */
export interface IdempotencyKey {
  licenseId: string;
  deviceId: string;
  purpose: string;
  clientRequestId: string;
  claimToken?: string;
}

export interface IdempotencyInput extends IdempotencyKey {
  payloadHash: string;
  date: string;
}

export type IdempotencyClaim =
  | { kind: "fresh"; claimToken: string; batchId: string }
  | { kind: "replay"; batchId: string }
  | { kind: "conflict" }
  | { kind: "unavailable" }
  | { kind: "unknown"; batchId?: string };

export type IdempotencyRecovery =
  | { kind: "missing" }
  | { kind: "retired" }
  | { kind: "unknown"; batchId?: string }
  | { kind: "bound"; batchId: string };

export function recoverFrom(map: IdemMap, input: IdempotencyKey, retireIfMissing = false, today?: string): IdempotencyRecovery {
  const key = idempotencyMapKey(input);
  const record = map[key];
  if (record?.state === "retired") return { kind: "retired" };
  if (!record || record.state === "pending") {
    if (!retireIfMissing) return { kind: "missing" };
    // Fence even a claimant paused before reservation. Absence alone cannot release local work.
    map[key] = { hash: record?.hash ?? "", date: record?.date ?? "", state: "retired" };
    return { kind: "retired" };
  }
  // A create that died without an answer (the relay slept mid-request: upstream status 0) stays
  // unknown, and recovery would answer 503 forever. Once its reservation day (UTC) is over, an explicit
  // retirement fences it so the device can re-send the work under a new id. The provider is not
  // asked: a create it did accept is billed twice, at most once per such row (docs/batch-relay-design.md,
  // "Crash recovery for Dream Cycle submissions", 2026-10-15). The reservation is kept, and a late bind of a retired claim is refused.
  if (retireIfMissing && !record.batchId && record.state === "submitting" && today !== undefined && record.date !== "" && record.date < today) {
    record.state = "retired";
    delete record.claimToken;
    return { kind: "retired" };
  }
  return record.batchId ? { kind: "bound", batchId: record.relayId ?? record.batchId } : { kind: "unknown", ...(record.relayId ? { batchId: record.relayId } : {}) };
}

export function idempotencyMapKey(input: IdempotencyKey): string {
  return JSON.stringify([input.licenseId, input.deviceId, input.purpose, input.clientRequestId]);
}

// reservedChunks is the in-memory record's own metering; the SQL store meters on the day row.
export type IdemRecord = { reservedChunks?: number; hash: string; date: string; batchId?: string; relayId?: string; claimToken?: string; state?: "pending" | "submitting" | "retired" };

export type IdemMap = Record<string, IdemRecord>;

export function claimInto(map: IdemMap, input: IdempotencyInput): IdempotencyClaim {
  const key = idempotencyMapKey(input);
  const existing = map[key];
  if (!existing) {
    const claimToken = randomUUID();
    const batchId = `rb_${randomUUID()}`;
    map[key] = { hash: input.payloadHash, date: input.date, relayId: batchId, state: "pending", claimToken };
    return { kind: "fresh", claimToken, batchId };
  }
  if (existing.state === "retired" || existing.hash !== input.payloadHash) return { kind: "conflict" };
  if (existing.batchId) return { kind: "replay", batchId: existing.relayId ?? existing.batchId };
  if (existing.state === "pending") {
    existing.claimToken = randomUUID();
    existing.relayId ??= `rb_${randomUUID()}`;
    return { kind: "fresh", claimToken: existing.claimToken, batchId: existing.relayId };
  }
  return { kind: "unknown", ...(existing.relayId ? { batchId: existing.relayId } : {}) };
}

function bindInto(map: IdemMap, input: IdempotencyKey, batchId: string): void {
  const key = idempotencyMapKey(input);
  const existing = map[key];
  if (existing) existing.batchId = batchId;
}

export function releaseInto(map: IdemMap, input: IdempotencyKey): void {
  const key = idempotencyMapKey(input);
  const existing = map[key];
  if (existing && existing.state !== "retired" && !existing.batchId && (input.claimToken === undefined || existing.claimToken === input.claimToken)) {
    existing.state = "pending";
    delete existing.claimToken;
  }
}

export function cancelSubmission(map: IdemMap, input: IdempotencyKey, chunks: number): IdemRecord | undefined {
  const record = map[idempotencyMapKey(input)];
  if (!Number.isSafeInteger(chunks) || chunks <= 0 || !input.claimToken ||
      record?.state !== "submitting" || record.claimToken !== input.claimToken || record.batchId) return undefined;
  releaseInto(map, input);
  return record;
}

/** In-memory spend breaker state (the test store). */
interface SpendMemory {
  days: Record<string, number>;
  /** date → licence → booked micro-USD (the per-licence budget). */
  licenceDays: Record<string, Record<string, number>>;
  batches: Record<string, { date: string; estimate: number; settled: boolean; licenseId: string; checkedAt?: number }>;
}

/** Adjust the two spend counters a batch is booked on (global day and licence day) by `delta`,
 * never below zero. */
function adjustSpend(spent: SpendMemory, date: string, licenseId: string, delta: number): void {
  spent.days[date] = Math.max(0, (spent.days[date] ?? 0) + delta);
  const day = (spent.licenceDays[date] ??= {});
  day[licenseId] = Math.max(0, (day[licenseId] ?? 0) + delta);
}

// Content-free request tombstones never expire: deleting one would authorize a duplicate.
function reserveSubmission(days: Ledger, idem: IdemMap, licenseId: string, date: string, chunks: number, cap: number, key?: IdempotencyKey, spend?: SpendReservation, spent?: SpendMemory): Reservation {
  const record = key ? idem[idempotencyMapKey(key)] : undefined;
  if (key && (!record || record.state !== "pending" || record.claimToken !== key.claimToken)) return "unknown";
  if (!Number.isSafeInteger(chunks) || chunks <= 0 || !Number.isSafeInteger(cap) || cap < 0) return "capped";
  if ((days[date]?.[licenseId] ?? 0) + chunks > cap) return "capped";
  if (spend && spent) {
    if (!validSpend(spend) || (spent.days[date] ?? 0) + spend.micros > spend.capMicros) return "spend_capped";
    if (spend.licenseCapMicros !== undefined && (spent.licenceDays[date]?.[licenseId] ?? 0) + spend.micros > spend.licenseCapMicros) {
      return "license_spend_capped";
    }
  }
  reserveInto(days, licenseId, date, chunks, cap);
  if (spend && spent) {
    adjustSpend(spent, date, licenseId, spend.micros);
    spent.batches[spend.batchId] = { date, estimate: spend.micros, settled: false, licenseId };
  }
  if (record) { record.state = "submitting"; record.date = date; record.reservedChunks = chunks; }
  return "ok";
}

function refundSpend(spent: SpendMemory, batchId: string | undefined): void {
  const batch = batchId ? spent.batches[batchId] : undefined;
  if (!batch || batch.settled) return;
  adjustSpend(spent, batch.date, batch.licenseId, -batch.estimate);
  delete spent.batches[batchId!];
}

export function bindSubmission(idem: IdemMap, owners: BatchOwners, input: IdempotencyKey, providerId: string): string {
  const record = idem[idempotencyMapKey(input)];
  if (!record) throw new LedgerUnavailable("missing submission claim");
  if (record.batchId && record.batchId !== providerId) throw new LedgerUnavailable("submission already bound");
  if (record.state === "retired") throw new LedgerUnavailable("submission retired");
  bindInto(idem, input, providerId);
  const relayId = record.relayId ?? providerId;
  owners[relayId] = { lic: input.licenseId, device: input.deviceId, date: record.date, providerId };
  return relayId;
}

/** Today's metering date (UTC). The cap day boundary is UTC on purpose: it needs no per-device
 * timezone and the nightly Dream window never straddles two same-day submissions. */
export function utcDate(now: Date = new Date()): string {
  return now.toISOString().slice(0, 10);
}

/** Day counters keyed by UTC date and licence (the in-memory store). */
export type Ledger = Record<string, Record<string, number>>;

/** batchId → {licence, metering date}. The date is only there so old entries can be pruned. */
export type BatchOwners = Record<string, { lic: string; date: string; device?: string | undefined; providerId?: string }>;

/** How long a batch→licence mapping outlives its metering date. Anthropic batches expire after
 * 24h; a week covers every legitimate late read with margin. */
export const BATCH_OWNER_KEEP_DAYS = 7;

/** How long a day's counters stay in the ledger. The cap itself never needs yesterday — it is a
 * daily limit, and `usedOn` is only ever called with today's UTC date — so this window exists
 * purely so a human can reconcile a billing period against what the relay actually metered.
 * 45 days covers a monthly cycle plus the lag before an invoice is checked. Without it the tables
 * grow without bound; the sweep's `maintenance` deletes only expired rows, by index. */
export const USAGE_DAY_KEEP_DAYS = 45;

/** Rate buckets idle this long are deleted by `maintenance` (and by the limiter's own per-isolate
 * fallback sweep, pg-store.ts): a bucket is only state while its licence is active. */
export const RATE_BUCKET_IDLE_EVICT_MS = 10 * 60_000;

/** The oldest UTC date still kept when writing against `date` (counters and spend days). */
export function keepCutoff(date: string, keepDays: number): string {
  return new Date(new Date(`${date}T00:00:00Z`).getTime() - keepDays * 86_400_000).toISOString().slice(0, 10);
}

/** A spend reservation is well-formed only as non-negative safe integers. */
export function validSpend(spend: SpendReservation): boolean {
  return Number.isSafeInteger(spend.micros) && spend.micros >= 0 &&
    Number.isSafeInteger(spend.capMicros) && spend.capMicros >= 0 && spend.batchId.length > 0 &&
    (spend.licenseCapMicros === undefined || (Number.isSafeInteger(spend.licenseCapMicros) && spend.licenseCapMicros >= 0));
}

/** The per-licence counter the sweep and the per-licence ceiling read: `spend_micros` on the
 * (date, licence) day row. Reservations dated before `sinceDate` are left to age out — the
 * provider keeps results about a day, so a batch older than that has nothing left to settle. */
export const SETTLE_WINDOW_DAYS = 3;

function reserveInto(days: Ledger, licenseId: string, date: string, chunks: number, cap: number): boolean {
  if (!Number.isSafeInteger(chunks) || chunks <= 0 || !Number.isSafeInteger(cap) || cap < 0) return false;
  const day = (days[date] ??= {});
  const used = day[licenseId] ?? 0;
  if (used + chunks > cap) return false;
  day[licenseId] = used + chunks;
  return true;
}

export class InMemoryUsageStore implements UsageStore {
  private days: Ledger = {};
  private owners: BatchOwners = {};
  private idem: IdemMap = {};
  private spent: SpendMemory = { days: {}, licenceDays: {}, batches: {} };
  private queue: Promise<unknown> = Promise.resolve();
  /** (date, license, chunks, batchId) tuples, for tests to assert on what was persisted. */
  readonly records: Array<{ date: string; licenseId: string; chunks: number; batchId: string }> = [];

  private enqueue<T>(f: () => T | Promise<T>): Promise<T> {
    const next = this.queue.then(f);
    this.queue = next.catch(() => undefined);
    return next;
  }

  usedOn(licenseId: string, date: string): Promise<number> {
    return Promise.resolve(this.days[date]?.[licenseId] ?? 0);
  }

  tryReserve(licenseId: string, date: string, chunks: number, cap: number, key?: IdempotencyKey, spend?: SpendReservation): Promise<Reservation> {
    return this.enqueue(() => reserveSubmission(this.days, this.idem, licenseId, date, chunks, cap, key, spend, this.spent));
  }

  settleSpend(batchId: string, actualMicros: number): Promise<void> {
    return this.enqueue(() => {
      const batch = this.spent.batches[batchId];
      if (!batch || batch.settled || !Number.isSafeInteger(actualMicros) || actualMicros < 0) return;
      batch.settled = true;
      adjustSpend(this.spent, batch.date, batch.licenseId, actualMicros - batch.estimate);
    });
  }

  spentOn(date: string): Promise<number> {
    return Promise.resolve(this.spent.days[date] ?? 0);
  }

  licenseSpentOn(licenseId: string, date: string): Promise<number> {
    return Promise.resolve(this.spent.licenceDays[date]?.[licenseId] ?? 0);
  }

  unsettledBatches(limit: number, sinceDate: string, recheckAfterMs: number): Promise<UnsettledBatch[]> {
    const out: UnsettledBatch[] = [];
    const ordered = Object.entries(this.spent.batches).sort(([idA, a], [idB, b]) => a.date.localeCompare(b.date) || idA.localeCompare(idB));
    for (const [batchId, batch] of ordered) {
      if (out.length >= limit) break;
      const providerId = this.owners[batchId]?.providerId;
      const due = batch.checkedAt === undefined || batch.checkedAt < recheckAfterMs;
      if (!batch.settled && due && batch.date >= sinceDate && providerId) out.push({ batchId, providerId });
    }
    return Promise.resolve(out);
  }

  markChecked(batchId: string, nowMs: number): Promise<void> {
    return this.enqueue(() => {
      const batch = this.spent.batches[batchId];
      if (batch && !batch.settled) batch.checkedAt = nowMs;
    });
  }

  markSettled(batchId: string): Promise<void> {
    return this.enqueue(() => {
      const batch = this.spent.batches[batchId];
      if (batch) batch.settled = true;
    });
  }

  cancelReservation(input: IdempotencyKey, chunks: number, meterId: string = input.licenseId): Promise<void> {
    return this.enqueue(() => {
      const record = cancelSubmission(this.idem, input, chunks);
      const day = record ? this.days[record.date] : undefined;
      if (day) day[meterId] = Math.max(0, (day[meterId] ?? 0) - chunks);
      if (record) refundSpend(this.spent, record.relayId);
    });
  }

  release(licenseId: string, date: string, chunks: number): Promise<void> {
    return this.enqueue(() => {
      const day = this.days[date];
      if (day) day[licenseId] = Math.max(0, (day[licenseId] ?? 0) - chunks);
    });
  }

  maintenance(date: string): Promise<void> {
    return this.enqueue(() => {
      const dayCutoff = keepCutoff(date, USAGE_DAY_KEEP_DAYS);
      const ownerCutoff = keepCutoff(date, BATCH_OWNER_KEEP_DAYS);
      for (const day of Object.keys(this.days)) if (day < dayCutoff) delete this.days[day];
      for (const day of Object.keys(this.spent.days)) if (day < dayCutoff) delete this.spent.days[day];
      for (const day of Object.keys(this.spent.licenceDays)) if (day < dayCutoff) delete this.spent.licenceDays[day];
      for (const [id, batch] of Object.entries(this.spent.batches)) if (batch.date < dayCutoff) delete this.spent.batches[id];
      // Keyed ownership (a batch bound to a request identity) is permanent; only legacy owner rows age out.
      const keyed = new Set(Object.values(this.idem).map((r) => r.relayId ?? r.batchId));
      for (const [id, owner] of Object.entries(this.owners)) if (owner.date < ownerCutoff && !keyed.has(id)) delete this.owners[id];
    });
  }

  attachBatch(batchId: string, licenseId: string, date: string, chunks: number, deviceId?: string, providerId = batchId): Promise<void> {
    return this.enqueue(() => {
      this.owners[batchId] = { lic: licenseId, date, device: deviceId, providerId };
      this.records.push({ date, licenseId, chunks, batchId });
    });
  }

  batchOwner(batchId: string, deviceId?: string): Promise<string | undefined> {
    const owner = this.owners[batchId];
    const entry = Object.entries(this.idem).find(([, r]) => (r.relayId ?? r.batchId) === batchId);
    const tuple = entry ? JSON.parse(entry[0]) as [string, string, string, string] : undefined;
    return Promise.resolve(resolveBatchOwner(owner, tuple ? { licenseId: tuple[0], deviceId: tuple[1] } : undefined, deviceId));
  }

  providerBatchId(batchId: string): Promise<string | undefined> {
    return Promise.resolve(this.owners[batchId]?.providerId ?? (this.owners[batchId] ? batchId : undefined));
  }

  claimIdempotency(input: IdempotencyInput): Promise<IdempotencyClaim> {
    return this.enqueue(() => claimInto(this.idem, input));
  }

  recoverIdempotency(input: IdempotencyKey, retireIfMissing = false, today?: string): Promise<IdempotencyRecovery> {
    return this.enqueue(() => recoverFrom(this.idem, input, retireIfMissing, today));
  }

  bindIdempotency(input: IdempotencyKey, batchId: string): Promise<string> {
    return this.enqueue(() => {
      const id = bindSubmission(this.idem, this.owners, input, batchId);
      const record = this.idem[idempotencyMapKey(input)];
      this.records.push({ date: record?.date ?? "", licenseId: input.licenseId, chunks: record?.reservedChunks ?? 0, batchId: id });
      return id;
    });
  }

  releaseIdempotency(input: IdempotencyKey): Promise<void> {
    return this.enqueue(() => {
      const record = this.idem[idempotencyMapKey(input)];
      const wasSubmitting = record?.state === "submitting";
      releaseInto(this.idem, input);
      if (wasSubmitting && record?.state === "pending") refundSpend(this.spent, record.relayId);
    });
  }
}

/** Thrown by a store when the ledger exists but cannot be read or written. Never swallowed into
 * "start clean": a ledger that resets every licence's counter to zero removes the ONLY spend
 * control on the operator's Anthropic key. The app maps it to 503 (fail-closed). */
export class LedgerUnavailable extends Error {}

export const KEY_COLUMNS = "license_id,device_id,purpose,request_id";
export const RECORD_COLUMNS = "hash,date,provider_id,relay_id,claim_token,state";

export function keyValues(input: IdempotencyKey): string[] {
  return [input.licenseId, input.deviceId, input.purpose, input.clientRequestId];
}

export function recordFromRow(row: Record<string, unknown> | undefined): IdemRecord | undefined {
  if (!row) return undefined;
  return {
    hash: String(row.hash), date: String(row.date),
    ...(row.provider_id !== null ? { batchId: String(row.provider_id) } : {}),
    ...(row.relay_id !== null ? { relayId: String(row.relay_id) } : {}),
    ...(row.claim_token !== null ? { claimToken: String(row.claim_token) } : {}),
    ...(row.state !== null ? { state: row.state as NonNullable<IdemRecord["state"]> } : {}),
  };
}
