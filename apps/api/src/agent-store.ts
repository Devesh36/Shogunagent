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
import {
  chargeRefundEntryId, commitActive, monthBounds, overagePart, overageReportId,
  type Adjustment, type CheckoutStatus, type CommitRecord, type CommitTerms, type CreditEntry, type DisputeLine, type DisputeStatus, type Funding,
  type LicenceLink, type OverageReport, type StatementInputs, type StoredStatement, type UnitUsage,
} from "./billing/ledger.js";
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
  /** The settled figure (0 until settled, and for free calls). Discounted when a commit applied. */
  costMicros: number;
  /** The settled figure at list price (equals `costMicros` without a commit). */
  listCostMicros: number;
  /** Wallet-held, or drawn down from a commit (E19). */
  funding: Funding;
  /** The commit whose discount applied, if any. */
  commitId?: string;
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
  /** The quoted cost to hold (discounted when `commit` applies). 0 = a free call: recorded
   * settled, nothing held. */
  micros: number;
  /** The quote at list price, when a commit discount applies (default: `micros`). */
  listMicros?: number;
  /** The active commit's terms (E19): its discount is already in `micros`; a `commit`-funded
   * call holds nothing on the wallet and is drawn down from the commit at settlement. */
  commit?: CommitTerms;
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
  status: DisputeStatus;
  ts: number;
  /** Set when an operator resolves the dispute (E19). */
  creditMicros?: number;
  resolvedAt?: number;
}

/** A Checkout session the gateway created (E19: `checkout.session.expired` closes it). */
export interface CheckoutSessionRecord {
  sessionId: string;
  principalId: string;
  micros: number;
  status: CheckoutStatus;
  createdAt: number;
  updatedAt: number;
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
  /** E26: revoke every live key of a principal at `at` (SCIM deactivation and deletion). Returns
   * how many were live; a key already revoked keeps its first revocation time. */
  revokePrincipalKeys(principalId: string, at: number): Promise<number>;
  /** E26: record the principal (and its keys) under an enterprise tenant. The membership itself
   * (at most one tenant per principal) is the enterprise store's; this keeps the `tenant_id` the
   * commit lookup and `/v1/agents/me` read in step with it. False when the principal is unknown. */
  assignTenant(principalId: string, tenantId: string): Promise<boolean>;

  // --- wallet ---
  wallet(principalId: string): Promise<Wallet>;
  /** Credit `micros` once per `topupId` (the idempotency key: a Stripe event id, or the signup
   * grant's fixed id). Creates the wallet when absent. `duplicate` credits nothing. */
  credit(principalId: string, topupId: string, micros: number, source: string, at: number): Promise<"credited" | "duplicate">;
  reserve(input: ReserveInput): Promise<ReserveOutcome>;
  /** Replace the held estimate with `actualMicros`, once. A settled or unknown receipt is left
   * alone. `listMicros` is the same figure at list price (default `actualMicros`); a commit-funded
   * receipt draws `actualMicros` down its commit instead of the wallet. */
  settle(receiptId: string, actualMicros: number, listMicros?: number): Promise<void>;
  /** Booked micro-USD (held estimates plus settled cost) on a cap counter. */
  periodSpend(subject: string, period: string): Promise<number>;

  // --- receipts and disputes ---
  receipt(receiptId: string): Promise<Receipt | undefined>;
  /** The principal's receipts with `ts >= sinceMs`, oldest first, at most `limit`. */
  receipts(principalId: string, sinceMs: number, limit: number): Promise<Receipt[]>;
  /** Open a dispute on the principal's own receipt. `undefined` when the receipt is not theirs;
   * the existing dispute (`created: false`) when one is already open. */
  dispute(receiptId: string, principalId: string, reason: string, disputeId: string, at: number): Promise<{ dispute: Dispute; created: boolean } | undefined>;

  // --- E19: commits ---
  /** Create a commit, idempotently by id. `overlap` when the same subject already has a commit
   * whose period intersects this one (nothing is written). */
  createCommit(c: CommitRecord): Promise<"created" | "duplicate" | "overlap">;
  commit(commitId: string): Promise<CommitRecord | undefined>;
  /** The commit in force for a principal at `atMs`: its own first, else its tenant's. */
  activeCommit(principalId: string, tenantId: string, atMs: number): Promise<CommitRecord | undefined>;
  /** Attach the Stripe metered subscription item overage is reported on. */
  setCommitSubscriptionItem(commitId: string, customerId: string, itemId: string): Promise<boolean>;
  /** Turn each commit's unreported overage into whole-cent report rows (deterministic ids: the
   * Stripe idempotency keys) and return the reports not yet marked sent, oldest first. */
  claimOverageReports(nowMs: number, limit: number): Promise<OverageReport[]>;
  markOverageReportSent(reportId: string, at: number): Promise<void>;

  // --- E19: ledger adjustments, Stripe payments and sessions ---
  /** Apply a signed ledger entry once per `entryId`. A debit stops at a zero balance; the entry
   * records what was applied. */
  adjust(a: Omit<Adjustment, "appliedMicros">): Promise<{ outcome: "applied" | "duplicate"; adjustment: Adjustment }>;
  /** Raise a charge's refunded high-water mark to `cumulativeMicros` and debit the wallet by the
   * increase, once (the Stripe webhook and the admin route converge on the same figure). */
  recordChargeRefund(principalId: string, chargeId: string, cumulativeMicros: number, at: number): Promise<{ debitedMicros: number }>;
  recordPayment(paymentIntentId: string, principalId: string, sessionId: string, micros: number, at: number): Promise<void>;
  paymentPrincipal(paymentIntentId: string): Promise<string | undefined>;
  recordCheckoutSession(sessionId: string, principalId: string, micros: number, at: number): Promise<void>;
  /** Move a session to `completed` or `expired`. A completed session never expires; an unknown or
   * already-final session answers false. */
  setCheckoutStatus(sessionId: string, status: "completed" | "expired", at: number): Promise<boolean>;
  checkoutSession(sessionId: string): Promise<CheckoutSessionRecord | undefined>;

  // --- E19: disputes and statements ---
  disputeById(disputeId: string): Promise<Dispute | undefined>;
  /** Resolve an open dispute. One already resolved or rejected is returned unchanged. */
  resolveDispute(disputeId: string, status: "resolved" | "rejected", creditMicros: number, at: number): Promise<Dispute | undefined>;
  /** What a statement for `month` is built from. */
  statementInputs(principalId: string, month: string, nowMs: number): Promise<StatementInputs>;
  storedStatement(principalId: string, month: string): Promise<StoredStatement | undefined>;
  /** Store a closed statement if none is stored yet; return whichever is stored. */
  saveStatement(s: StoredStatement): Promise<StoredStatement>;

  // --- E14-T3: ShogunAI licence links ---
  linkLicence(link: LicenceLink): Promise<void>;
  /** Links on `plan` whose licence was verified at or after `sinceMs`. */
  licenceLinks(plan: string, sinceMs: number): Promise<LicenceLink[]>;

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
  private readonly topups = new Map<string, CreditEntry>();
  private readonly receiptRows = new Map<string, Receipt>();
  private readonly periods = new Map<string, number>();
  private readonly disputes = new Map<string, Dispute>();
  private readonly spentChallenges = new Map<string, number>();
  private readonly commits = new Map<string, CommitRecord>();
  private readonly overageReports = new Map<string, OverageReport>();
  private readonly adjustments = new Map<string, Adjustment>();
  private readonly chargeRefunds = new Map<string, number>();
  private readonly payments = new Map<string, string>();
  private readonly sessions = new Map<string, CheckoutSessionRecord>();
  private readonly statements = new Map<string, StoredStatement>();
  private readonly links = new Map<string, LicenceLink>();
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

  revokePrincipalKeys(principalId: string, at: number): Promise<number> {
    return this.enqueue(() => {
      let live = 0;
      for (const k of this.keys.values()) {
        if (k.principalId !== principalId || k.revokedAt !== undefined) continue;
        k.revokedAt = at;
        live += 1;
      }
      return live;
    });
  }

  assignTenant(principalId: string, tenantId: string): Promise<boolean> {
    return this.enqueue(() => {
      const p = this.principals.get(principalId);
      if (!p) return false;
      p.tenantId = tenantId;
      for (const k of this.keys.values()) if (k.principalId === principalId) k.tenantId = tenantId;
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

  credit(principalId: string, topupId: string, micros: number, source: string, at: number): Promise<"credited" | "duplicate"> {
    return this.enqueue(() => {
      if (!Number.isSafeInteger(micros) || micros < 0) return "duplicate";
      if (this.topups.has(topupId)) return "duplicate";
      this.topups.set(topupId, { topupId, principalId, micros, source, ts: at });
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
        unit: input.unit, quantity: input.quantity, estimateMicros: input.micros, costMicros: 0, listCostMicros: 0,
        funding: input.commit?.funding ?? "wallet", ...(input.commit ? { commitId: input.commit.commitId } : {}),
        settled: false, day: input.day, month: input.month, ts: input.ts,
      };
      if (input.micros === 0) {
        this.receiptRows.set(input.receiptId, { ...receipt, settled: true });
        return { kind: "ok" };
      }
      const w = this.walletRow(input.principalId);
      const fromWallet = receipt.funding === "wallet";
      const available = w.balanceMicros - w.reservedMicros;
      if (fromWallet && available < input.micros) return { kind: "insufficient_funds", availableMicros: Math.max(0, available) };
      for (const counter of capCounters(input)) {
        const spent = this.periods.get(`${counter.subject}\u0000${counter.periodKey}`) ?? 0;
        if (spent + input.micros > counter.limit) {
          return { kind: "cap_exceeded", scope: counter.scope, period: counter.period, limitMicros: counter.limit, spentMicros: spent };
        }
      }
      if (fromWallet) w.reservedMicros += input.micros;
      for (const counter of capCounters(input)) {
        const k = `${counter.subject}\u0000${counter.periodKey}`;
        this.periods.set(k, (this.periods.get(k) ?? 0) + input.micros);
      }
      this.receiptRows.set(input.receiptId, receipt);
      return { kind: "ok" };
    });
  }

  settle(receiptId: string, actualMicros: number, listMicros: number = actualMicros): Promise<void> {
    return this.enqueue(() => {
      const r = this.receiptRows.get(receiptId);
      if (!r || r.settled || !Number.isSafeInteger(actualMicros) || actualMicros < 0) return;
      r.settled = true;
      r.costMicros = actualMicros;
      r.listCostMicros = Number.isSafeInteger(listMicros) && listMicros >= actualMicros ? listMicros : actualMicros;
      const commit = r.commitId !== undefined ? this.commits.get(r.commitId) : undefined;
      if (r.funding === "commit" && commit) {
        commit.overageMicros += overagePart(commit.amountMicros, commit.drawnMicros, actualMicros);
        commit.drawnMicros += actualMicros;
      } else {
        const w = this.walletRow(r.principalId);
        w.reservedMicros = Math.max(0, w.reservedMicros - r.estimateMicros);
        w.balanceMicros = Math.max(0, w.balanceMicros - actualMicros);
      }
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

  // --- E19 ---

  createCommit(c: CommitRecord): Promise<"created" | "duplicate" | "overlap"> {
    return this.enqueue(() => {
      if (this.commits.has(c.commitId)) return "duplicate";
      const clash = [...this.commits.values()].some((o) => o.subjectKind === c.subjectKind && o.subjectId === c.subjectId && o.startsAt < c.endsAt && c.startsAt < o.endsAt);
      if (clash) return "overlap";
      this.commits.set(c.commitId, { ...c });
      return "created";
    });
  }

  commit(commitId: string): Promise<CommitRecord | undefined> {
    const c = this.commits.get(commitId);
    return Promise.resolve(c ? { ...c } : undefined);
  }

  activeCommit(principalId: string, tenantId: string, atMs: number): Promise<CommitRecord | undefined> {
    const all = [...this.commits.values()].filter((c) => commitActive(c, atMs));
    const own = all.find((c) => c.subjectKind === "principal" && c.subjectId === principalId)
      ?? all.find((c) => c.subjectKind === "tenant" && c.subjectId === tenantId);
    return Promise.resolve(own ? { ...own } : undefined);
  }

  setCommitSubscriptionItem(commitId: string, customerId: string, itemId: string): Promise<boolean> {
    return this.enqueue(() => {
      const c = this.commits.get(commitId);
      if (!c) return false;
      c.stripeCustomerId = customerId;
      c.stripeSubscriptionItemId = itemId;
      return true;
    });
  }

  claimOverageReports(nowMs: number, limit: number): Promise<OverageReport[]> {
    return this.enqueue(() => {
      for (const c of this.commits.values()) {
        if (c.stripeSubscriptionItemId === undefined) continue;
        const cents = Math.floor((c.overageMicros - c.overageReportedMicros) / 10_000);
        if (cents <= 0) continue;
        const reportId = overageReportId(c.commitId, c.overageReportedMicros);
        this.overageReports.set(reportId, { reportId, commitId: c.commitId, subscriptionItemId: c.stripeSubscriptionItemId, cents, ts: nowMs });
        c.overageReportedMicros += cents * 10_000;
      }
      return [...this.overageReports.values()].filter((r) => r.sentAt === undefined)
        .sort((a, b) => a.ts - b.ts || a.reportId.localeCompare(b.reportId)).slice(0, Math.max(0, limit)).map((r) => ({ ...r }));
    });
  }

  markOverageReportSent(reportId: string, at: number): Promise<void> {
    return this.enqueue(() => {
      const r = this.overageReports.get(reportId);
      if (r && r.sentAt === undefined) r.sentAt = at;
    });
  }

  adjust(a: Omit<Adjustment, "appliedMicros">): Promise<{ outcome: "applied" | "duplicate"; adjustment: Adjustment }> {
    return this.enqueue(() => this.adjustNow(a));
  }

  private adjustNow(a: Omit<Adjustment, "appliedMicros">): { outcome: "applied" | "duplicate"; adjustment: Adjustment } {
    const existing = this.adjustments.get(a.entryId);
    if (existing) return { outcome: "duplicate", adjustment: { ...existing } };
    const w = this.walletRow(a.principalId);
    const applied = a.micros >= 0 ? a.micros : -Math.min(w.balanceMicros, -a.micros);
    w.balanceMicros += applied;
    const row: Adjustment = { ...a, appliedMicros: applied };
    this.adjustments.set(a.entryId, row);
    return { outcome: "applied", adjustment: { ...row } };
  }

  recordChargeRefund(principalId: string, chargeId: string, cumulativeMicros: number, at: number): Promise<{ debitedMicros: number }> {
    return this.enqueue(() => {
      const before = this.chargeRefunds.get(chargeId) ?? 0;
      if (!Number.isSafeInteger(cumulativeMicros) || cumulativeMicros <= before) return { debitedMicros: 0 };
      this.chargeRefunds.set(chargeId, cumulativeMicros);
      const out = this.adjustNow({ entryId: chargeRefundEntryId(chargeId, cumulativeMicros), principalId, micros: -(cumulativeMicros - before), kind: "refund", reason: "refund", ref: chargeId, ts: at });
      return { debitedMicros: -out.adjustment.appliedMicros };
    });
  }

  recordPayment(paymentIntentId: string, principalId: string, _sessionId: string, _micros: number, _at: number): Promise<void> {
    return this.enqueue(() => { if (!this.payments.has(paymentIntentId)) this.payments.set(paymentIntentId, principalId); });
  }

  paymentPrincipal(paymentIntentId: string): Promise<string | undefined> {
    return Promise.resolve(this.payments.get(paymentIntentId));
  }

  recordCheckoutSession(sessionId: string, principalId: string, micros: number, at: number): Promise<void> {
    return this.enqueue(() => {
      if (!this.sessions.has(sessionId)) this.sessions.set(sessionId, { sessionId, principalId, micros, status: "open", createdAt: at, updatedAt: at });
    });
  }

  setCheckoutStatus(sessionId: string, status: "completed" | "expired", at: number): Promise<boolean> {
    return this.enqueue(() => {
      const s = this.sessions.get(sessionId);
      if (!s || s.status === "completed" || s.status === status) return false;
      s.status = status;
      s.updatedAt = at;
      return true;
    });
  }

  checkoutSession(sessionId: string): Promise<CheckoutSessionRecord | undefined> {
    const s = this.sessions.get(sessionId);
    return Promise.resolve(s ? { ...s } : undefined);
  }

  disputeById(disputeId: string): Promise<Dispute | undefined> {
    const d = this.disputes.get(disputeId);
    return Promise.resolve(d ? { ...d } : undefined);
  }

  resolveDispute(disputeId: string, status: "resolved" | "rejected", creditMicros: number, at: number): Promise<Dispute | undefined> {
    return this.enqueue(() => {
      const d = this.disputes.get(disputeId);
      if (!d) return undefined;
      if (d.status === "open") {
        d.status = status;
        d.creditMicros = status === "resolved" ? Math.max(0, creditMicros) : 0;
        d.resolvedAt = at;
      }
      return { ...d };
    });
  }

  statementInputs(principalId: string, month: string, nowMs: number): Promise<StatementInputs> {
    return this.enqueue(() => {
      const bounds = monthBounds(month) ?? { start: 0, end: 0 };
      const usage = new Map<string, UnitUsage>();
      let unsettled = 0;
      const receiptIds = new Set<string>();
      for (const r of this.receiptRows.values()) {
        if (r.principalId !== principalId || r.month !== month) continue;
        receiptIds.add(r.receiptId);
        if (!r.settled) { unsettled += 1; continue; }
        const u = usage.get(r.unit) ?? { unit: r.unit, calls: 0, quantity: 0, listMicros: 0, chargedMicros: 0, commitFundedMicros: 0 };
        u.calls += 1;
        u.quantity += r.quantity;
        u.listMicros += r.listCostMicros;
        u.chargedMicros += r.costMicros;
        if (r.funding === "commit") u.commitFundedMicros += r.costMicros;
        usage.set(r.unit, u);
      }
      const inMonth = (ts: number): boolean => ts >= bounds.start && ts < bounds.end;
      const disputes: DisputeLine[] = [...this.disputes.values()].filter((d) => d.principalId === principalId && receiptIds.has(d.receiptId))
        .map((d) => ({ disputeId: d.disputeId, receiptId: d.receiptId, status: d.status, creditMicros: d.creditMicros ?? 0, openedAt: d.ts, ...(d.resolvedAt !== undefined ? { resolvedAt: d.resolvedAt } : {}) }));
      const tenantId = this.principals.get(principalId)?.tenantId ?? principalId;
      const at = Math.min(nowMs, bounds.end - 1);
      const all = [...this.commits.values()].filter((c) => commitActive(c, at));
      const commit = all.find((c) => c.subjectKind === "principal" && c.subjectId === principalId) ?? all.find((c) => c.subjectKind === "tenant" && c.subjectId === tenantId);
      return {
        usage: [...usage.values()],
        unsettled,
        credits: [...this.topups.values()].filter((t) => t.principalId === principalId && inMonth(t.ts)).map((t) => ({ ...t })),
        adjustments: [...this.adjustments.values()].filter((a) => a.principalId === principalId && inMonth(a.ts)).map((a) => ({ ...a })),
        disputes,
        ...(commit ? { commit: { ...commit } } : {}),
      };
    });
  }

  storedStatement(principalId: string, month: string): Promise<StoredStatement | undefined> {
    const s = this.statements.get(`${principalId}\u0000${month}`);
    return Promise.resolve(s ? { ...s } : undefined);
  }

  saveStatement(s: StoredStatement): Promise<StoredStatement> {
    return this.enqueue(() => {
      const k = `${s.principalId}\u0000${s.month}`;
      const existing = this.statements.get(k);
      if (existing) return { ...existing };
      this.statements.set(k, { ...s });
      return { ...s };
    });
  }

  linkLicence(link: LicenceLink): Promise<void> {
    return this.enqueue(() => { this.links.set(link.licenceSubject, { ...link }); });
  }

  licenceLinks(plan: string, sinceMs: number): Promise<LicenceLink[]> {
    return Promise.resolve([...this.links.values()].filter((l) => l.plan === plan && l.verifiedAt >= sinceMs).map((l) => ({ ...l })));
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
