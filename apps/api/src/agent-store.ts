/**
 * The agent-side store (docs/implementation-plan.md E3 / E9): principals, keys, the prepaid
 * wallet, cap counters, receipts, disputes and spent PoW challenges. This module holds the
 * interface and the in-memory implementation (tests, and the Node development entry);
 * production is `SqlAgentStore` (agent-pg-store.ts) on the same schema (sql/agent-keys.sql).
 *
 * **Reserve-then-settle**, the same shape as the licence ledger (usage.ts / pg-store.ts):
 * - `reserve` holds a call's quoted cost against the wallet (`balance - reserved >= cost`) and
 *   against the four cap counters (key × day/month, principal × day/month), all in one
 *   transaction, or holds nothing and says which ceiling refused.
 * - `settle` replaces the held estimate with the actual cost exactly once (the receipt's
 *   `settled` flag flips 0 → 1; a second call finds no row and changes nothing), so a retried
 *   settlement, or two paths settling the same call, never charges twice.
 * - A free call (`cost 0`) is a receipt written already settled: counted, never charged.
 *
 * Content-free: every row is ids, names, counters and timestamps.
 */
import type { AgentKeyRecord, AgentScope } from "./agent-keys.js";
import type { CapPeriod, CapScope, CapSet } from "./caps.js";
import type { MeterUnit } from "./pricing.js";

export interface PrincipalRecord {
  principalId: string;
  tenantId: string;
  createdAt: number;
  ref?: string;
  capDayMicros?: number;
  capMonthMicros?: number;
}

export interface Wallet {
  principalId: string;
  balanceMicros: number;
  reservedMicros: number;
}

export interface Receipt {
  receiptId: string;
  principalId: string;
  keyId: string;
  method: string;
  name?: string;
  unit: MeterUnit;
  quantity: number;
  /** Held at reservation. */
  estimateMicros: number;
  /** The settled figure (0 until settled, and for free calls). */
  costMicros: number;
  settled: boolean;
  day: string;
  month: string;
  /** ms since the epoch. */
  ts: number;
}

export interface ReserveInput {
  receiptId: string;
  principalId: string;
  keyId: string;
  method: string;
  name?: string;
  unit: MeterUnit;
  quantity: number;
  /** The quoted cost to hold. 0 = a free call: recorded settled, nothing held. */
  micros: number;
  ts: number;
  day: string;
  month: string;
  caps: CapSet;
}

export type ReserveOutcome =
  | { kind: "ok" }
  | { kind: "insufficient_funds"; availableMicros: number }
  | { kind: "cap_exceeded"; scope: CapScope; period: CapPeriod; limitMicros: number; spentMicros: number }
  | { kind: "unavailable" };

export interface Dispute {
  disputeId: string;
  receiptId: string;
  principalId: string;
  reason: string;
  status: "open";
  ts: number;
}

export interface AgentStore {
  // --- principals and keys ---
  createPrincipal(p: PrincipalRecord): Promise<void>;
  principal(principalId: string): Promise<PrincipalRecord | undefined>;
  insertKey(k: AgentKeyRecord): Promise<void>;
  /** The candidate rows for a lookup prefix (the middleware verifies the hash of each). */
  keysByPrefix(prefix: string): Promise<AgentKeyRecord[]>;
  keyById(keyId: string): Promise<AgentKeyRecord | undefined>;
  /** Revoke at `at`. False when the key does not exist; a second revocation changes nothing. */
  revokeKey(keyId: string, at: number): Promise<boolean>;
  /** Set the key's own caps (`undefined` = back to the defaults). False when the key does not exist. */
  setKeyCaps(keyId: string, caps: { dayMicros?: number; monthMicros?: number }): Promise<boolean>;

  // --- wallet ---
  wallet(principalId: string): Promise<Wallet>;
  /** Credit `micros` once per `topupId` (the idempotency key: a Stripe event id, or the signup
   * grant's fixed id). Creates the wallet when absent. `duplicate` credits nothing. */
  credit(principalId: string, topupId: string, micros: number, source: string, at: number): Promise<"credited" | "duplicate">;
  reserve(input: ReserveInput): Promise<ReserveOutcome>;
  /** Replace the held estimate with `actualMicros`, once. A settled or unknown receipt is left alone. */
  settle(receiptId: string, actualMicros: number): Promise<void>;
  /** Booked micro-USD (held estimates plus settled cost) on a cap counter. */
  periodSpend(subject: string, period: string): Promise<number>;

  // --- receipts and disputes ---
  receipt(receiptId: string): Promise<Receipt | undefined>;
  /** The principal's receipts with `ts >= sinceMs`, oldest first, at most `limit`. */
  receipts(principalId: string, sinceMs: number, limit: number): Promise<Receipt[]>;
  /** Open a dispute on the principal's own receipt. `undefined` when the receipt is not theirs;
   * the existing dispute (`created: false`) when one is already open. */
  dispute(receiptId: string, principalId: string, reason: string, disputeId: string, at: number): Promise<{ dispute: Dispute; created: boolean } | undefined>;

  // --- proof of work ---
  /** Mark a challenge spent until `expiresAt`. False when it was already spent. */
  spendChallenge(challengeHash: string, expiresAt: number, nowMs: number): Promise<boolean>;
  /** Retention: expired PoW rows. Called from the periodic sweep only. */
  maintenance(nowMs: number): Promise<void>;
}

/** Thrown by a store when it exists but cannot be read or written. The app maps it to 503. */
export class AgentStoreUnavailable extends Error {}

export const capSubjectKey = (keyId: string): string => `key:${keyId}`;
export const capSubjectPrincipal = (principalId: string): string => `principal:${principalId}`;

/** The four (subject, period, limit) counters a reservation books, in a fixed order so two stores
 * refuse on the same ceiling. */
export function capCounters(input: Pick<ReserveInput, "keyId" | "principalId" | "day" | "month" | "caps">): Array<{ scope: CapScope; period: CapPeriod; subject: string; periodKey: string; limit: number }> {
  return [
    { scope: "key", period: "day", subject: capSubjectKey(input.keyId), periodKey: input.day, limit: input.caps.key.day },
    { scope: "key", period: "month", subject: capSubjectKey(input.keyId), periodKey: input.month, limit: input.caps.key.month },
    { scope: "principal", period: "day", subject: capSubjectPrincipal(input.principalId), periodKey: input.day, limit: input.caps.principal.day },
    { scope: "principal", period: "month", subject: capSubjectPrincipal(input.principalId), periodKey: input.month, limit: input.caps.principal.month },
  ];
}

/** A reservation is well-formed only as non-negative safe integers. */
export function validReserve(input: ReserveInput): boolean {
  const caps = [input.caps.key.day, input.caps.key.month, input.caps.principal.day, input.caps.principal.month];
  return Number.isSafeInteger(input.micros) && input.micros >= 0 &&
    Number.isSafeInteger(input.quantity) && input.quantity >= 0 &&
    caps.every((c) => Number.isSafeInteger(c) && c >= 0) && input.receiptId.length > 0;
}

export const SCOPES_SEPARATOR = ",";

export function scopesToText(scopes: readonly AgentScope[]): string {
  return scopes.join(SCOPES_SEPARATOR);
}

export function scopesFromText(text: string): AgentScope[] {
  return text.split(SCOPES_SEPARATOR).filter((s): s is AgentScope => s === "read" || s === "write" || s === "run" || s === "admin");
}

/** Receipts older than this are deleted by maintenance in the SQL store (the in-memory store
 * lives for a process). 400 days: a year of statements plus a dispute window. */
export const RECEIPT_KEEP_MS = 400 * 86_400_000;

export class InMemoryAgentStore implements AgentStore {
  private readonly principals = new Map<string, PrincipalRecord>();
  private readonly keys = new Map<string, AgentKeyRecord>();
  private readonly wallets = new Map<string, Wallet>();
  private readonly topups = new Set<string>();
  private readonly receiptRows = new Map<string, Receipt>();
  private readonly periods = new Map<string, number>();
  private readonly disputes = new Map<string, Dispute>();
  private readonly spentChallenges = new Map<string, number>();
  private queue: Promise<unknown> = Promise.resolve();

  private enqueue<T>(f: () => T): Promise<T> {
    const next = this.queue.then(f);
    this.queue = next.catch(() => undefined);
    return next;
  }

  createPrincipal(p: PrincipalRecord): Promise<void> {
    return this.enqueue(() => { this.principals.set(p.principalId, { ...p }); });
  }

  principal(principalId: string): Promise<PrincipalRecord | undefined> {
    const p = this.principals.get(principalId);
    return Promise.resolve(p ? { ...p } : undefined);
  }

  insertKey(k: AgentKeyRecord): Promise<void> {
    return this.enqueue(() => { this.keys.set(k.keyId, { ...k, scopes: [...k.scopes] }); });
  }

  keysByPrefix(prefix: string): Promise<AgentKeyRecord[]> {
    return Promise.resolve([...this.keys.values()].filter((k) => k.prefix === prefix).map((k) => ({ ...k, scopes: [...k.scopes] })));
  }

  keyById(keyId: string): Promise<AgentKeyRecord | undefined> {
    const k = this.keys.get(keyId);
    return Promise.resolve(k ? { ...k, scopes: [...k.scopes] } : undefined);
  }

  revokeKey(keyId: string, at: number): Promise<boolean> {
    return this.enqueue(() => {
      const k = this.keys.get(keyId);
      if (!k) return false;
      if (k.revokedAt === undefined) k.revokedAt = at;
      return true;
    });
  }

  setKeyCaps(keyId: string, caps: { dayMicros?: number; monthMicros?: number }): Promise<boolean> {
    return this.enqueue(() => {
      const k = this.keys.get(keyId);
      if (!k) return false;
      if (caps.dayMicros === undefined) delete k.capDayMicros; else k.capDayMicros = caps.dayMicros;
      if (caps.monthMicros === undefined) delete k.capMonthMicros; else k.capMonthMicros = caps.monthMicros;
      return true;
    });
  }

  private walletRow(principalId: string): Wallet {
    let w = this.wallets.get(principalId);
    if (!w) {
      w = { principalId, balanceMicros: 0, reservedMicros: 0 };
      this.wallets.set(principalId, w);
    }
    return w;
  }

  wallet(principalId: string): Promise<Wallet> {
    return Promise.resolve({ ...this.walletRow(principalId) });
  }

  credit(principalId: string, topupId: string, micros: number, _source: string, _at: number): Promise<"credited" | "duplicate"> {
    return this.enqueue(() => {
      if (!Number.isSafeInteger(micros) || micros < 0) return "duplicate";
      if (this.topups.has(topupId)) return "duplicate";
      this.topups.add(topupId);
      this.walletRow(principalId).balanceMicros += micros;
      return "credited";
    });
  }

  reserve(input: ReserveInput): Promise<ReserveOutcome> {
    return this.enqueue((): ReserveOutcome => {
      if (!validReserve(input)) return { kind: "unavailable" };
      const receipt: Receipt = {
        receiptId: input.receiptId, principalId: input.principalId, keyId: input.keyId, method: input.method,
        ...(input.name !== undefined ? { name: input.name } : {}),
        unit: input.unit, quantity: input.quantity, estimateMicros: input.micros, costMicros: 0, settled: false,
        day: input.day, month: input.month, ts: input.ts,
      };
      if (input.micros === 0) {
        this.receiptRows.set(input.receiptId, { ...receipt, settled: true });
        return { kind: "ok" };
      }
      const w = this.walletRow(input.principalId);
      const available = w.balanceMicros - w.reservedMicros;
      if (available < input.micros) return { kind: "insufficient_funds", availableMicros: Math.max(0, available) };
      for (const counter of capCounters(input)) {
        const spent = this.periods.get(`${counter.subject}\u0000${counter.periodKey}`) ?? 0;
        if (spent + input.micros > counter.limit) {
          return { kind: "cap_exceeded", scope: counter.scope, period: counter.period, limitMicros: counter.limit, spentMicros: spent };
        }
      }
      w.reservedMicros += input.micros;
      for (const counter of capCounters(input)) {
        const k = `${counter.subject}\u0000${counter.periodKey}`;
        this.periods.set(k, (this.periods.get(k) ?? 0) + input.micros);
      }
      this.receiptRows.set(input.receiptId, receipt);
      return { kind: "ok" };
    });
  }

  settle(receiptId: string, actualMicros: number): Promise<void> {
    return this.enqueue(() => {
      const r = this.receiptRows.get(receiptId);
      if (!r || r.settled || !Number.isSafeInteger(actualMicros) || actualMicros < 0) return;
      r.settled = true;
      r.costMicros = actualMicros;
      const w = this.walletRow(r.principalId);
      w.reservedMicros = Math.max(0, w.reservedMicros - r.estimateMicros);
      w.balanceMicros = Math.max(0, w.balanceMicros - actualMicros);
      const delta = actualMicros - r.estimateMicros;
      for (const subject of [capSubjectKey(r.keyId), capSubjectPrincipal(r.principalId)]) {
        for (const period of [r.day, r.month]) {
          const k = `${subject}\u0000${period}`;
          this.periods.set(k, Math.max(0, (this.periods.get(k) ?? 0) + delta));
        }
      }
    });
  }

  periodSpend(subject: string, period: string): Promise<number> {
    return Promise.resolve(this.periods.get(`${subject}\u0000${period}`) ?? 0);
  }

  receipt(receiptId: string): Promise<Receipt | undefined> {
    const r = this.receiptRows.get(receiptId);
    return Promise.resolve(r ? { ...r } : undefined);
  }

  receipts(principalId: string, sinceMs: number, limit: number): Promise<Receipt[]> {
    const rows = [...this.receiptRows.values()]
      .filter((r) => r.principalId === principalId && r.ts >= sinceMs)
      .sort((a, b) => a.ts - b.ts || a.receiptId.localeCompare(b.receiptId))
      .slice(0, Math.max(0, limit))
      .map((r) => ({ ...r }));
    return Promise.resolve(rows);
  }

  dispute(receiptId: string, principalId: string, reason: string, disputeId: string, at: number): Promise<{ dispute: Dispute; created: boolean } | undefined> {
    return this.enqueue(() => {
      const r = this.receiptRows.get(receiptId);
      if (!r || r.principalId !== principalId) return undefined;
      const existing = [...this.disputes.values()].find((d) => d.receiptId === receiptId);
      if (existing) return { dispute: { ...existing }, created: false };
      const d: Dispute = { disputeId, receiptId, principalId, reason, status: "open", ts: at };
      this.disputes.set(disputeId, d);
      return { dispute: { ...d }, created: true };
    });
  }

  spendChallenge(challengeHash: string, expiresAt: number, nowMs: number): Promise<boolean> {
    return this.enqueue(() => {
      const until = this.spentChallenges.get(challengeHash);
      if (until !== undefined && until > nowMs) return false;
      this.spentChallenges.set(challengeHash, expiresAt);
      return true;
    });
  }

  maintenance(nowMs: number): Promise<void> {
    return this.enqueue(() => {
      for (const [hash, until] of this.spentChallenges) if (until <= nowMs) this.spentChallenges.delete(hash);
    });
  }
}
