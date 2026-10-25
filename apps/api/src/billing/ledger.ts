/**
 * Usage billing to production grade (docs/implementation-plan.md E19, plus E14-T3): the types and
 * the pure rules the two agent stores (`agent-store.ts` in memory, `agent-pg-store.ts` on
 * Postgres) share, so both compute the same discount, the same drawdown and the same statement.
 *
 * - **Annual commits.** A `commit` belongs to one principal or one tenant: an amount of spend
 *   committed for a period at `discount_bps` (1000–2500, i.e. 10–25% off, README §7 "Enterprise").
 *   While a commit is active, every metered call is priced at the discounted rate; the receipt
 *   keeps the list price next to it. A call is *commit-funded* (no wallet hold, drawn down from the
 *   commit) while the commit has headroom, or past it when overage billing is configured (Stripe
 *   usage records, `billing/stripe.ts`); otherwise it falls back to the wallet, still discounted.
 * - **Adjustments.** Signed ledger entries an operator or a Stripe event writes: a goodwill or
 *   dispute credit (+), a refund (−). Idempotent on the entry id; a debit never takes the balance
 *   below zero (the shortfall is recorded, not charged).
 * - **Statements.** One per principal per UTC month, aggregated per unit; immutable once the month
 *   closes (a stored snapshot whose SHA-256 is part of the body).
 *
 * Content-free like every agent table: ids, units, counters, amounts and timestamps. A dispute's
 * free-text reason is never copied into a statement.
 */
import { createHash } from "node:crypto";

import { canonicalJson } from "../wellknown/agent-card.js";
import type { MeterUnit } from "../pricing.js";

/** How a call was paid for. */
export type Funding = "wallet" | "commit";

/** The bounds of a commit discount, in basis points (README §7: "10–25% off with annual commit"). */
export const MIN_COMMIT_DISCOUNT_BPS = 1_000;
export const MAX_COMMIT_DISCOUNT_BPS = 2_500;

/** The longest commit period the admin route accepts (a year and a leap day of slack). */
export const MAX_COMMIT_PERIOD_MS = 367 * 86_400_000;

export interface CommitRecord {
  commitId: string;
  subjectKind: "principal" | "tenant";
  subjectId: string;
  /** The committed spend, µ$. */
  amountMicros: number;
  discountBps: number;
  /** ms since the epoch; active on [startsAt, endsAt). */
  startsAt: number;
  endsAt: number;
  /** Discounted spend drawn from the commit so far (settled, commit-funded). */
  drawnMicros: number;
  /** The part of `drawnMicros` above `amountMicros`. */
  overageMicros: number;
  /** How much of `overageMicros` has been turned into Stripe usage records. */
  overageReportedMicros: number;
  createdAt: number;
  /** The Stripe customer and the metered subscription item overage is reported on. */
  stripeCustomerId?: string;
  stripeSubscriptionItemId?: string;
}

/** What a commit contributes to one reservation. */
export interface CommitTerms {
  commitId: string;
  discountBps: number;
  funding: Funding;
}

/** The price a commit charges for `listMicros`: the list price less the discount, rounded so the
 * customer is never charged more than `list × (1 − bps)` rounded up to the micro-dollar. */
export function discounted(listMicros: number, discountBps: number): number {
  if (!Number.isSafeInteger(listMicros) || listMicros <= 0) return 0;
  const bps = Math.min(10_000, Math.max(0, Math.trunc(discountBps)));
  return listMicros - Math.floor((listMicros * bps) / 10_000);
}

/** Whether a commit is in force at `atMs`. */
export function commitActive(c: Pick<CommitRecord, "startsAt" | "endsAt">, atMs: number): boolean {
  return c.startsAt <= atMs && atMs < c.endsAt;
}

/** The headroom left on a commit (µ$, never negative). */
export function commitRemaining(c: Pick<CommitRecord, "amountMicros" | "drawnMicros">): number {
  return Math.max(0, c.amountMicros - c.drawnMicros);
}

/** How a call reserves against an active commit: commit-funded while there is headroom, or past
 * it when overage is billed through Stripe; otherwise from the wallet at the discounted price. */
export function commitTerms(c: CommitRecord): CommitTerms {
  const funding: Funding = commitRemaining(c) > 0 || c.stripeSubscriptionItemId !== undefined ? "commit" : "wallet";
  return { commitId: c.commitId, discountBps: c.discountBps, funding };
}

/** Drawing `micros` down a commit that has drawn `drawnBefore` of `amount`: the overage part. */
export function overagePart(amount: number, drawnBefore: number, micros: number): number {
  const after = drawnBefore + micros;
  return Math.max(0, Math.min(micros, after - Math.max(amount, drawnBefore)));
}

/** A signed ledger entry (credit +, refund −). */
export interface Adjustment {
  entryId: string;
  principalId: string;
  /** Requested amount, signed µ$. */
  micros: number;
  /** What the wallet actually moved by (a debit stops at zero). */
  appliedMicros: number;
  kind: AdjustmentKind;
  /** A fixed reason code, never free text (`ADJUSTMENT_REASONS`). */
  reason: string;
  /** A Stripe charge / refund id, a dispute id, or a receipt id the entry refers to. */
  ref?: string;
  ts: number;
}

export type AdjustmentKind = "credit" | "refund";
export const ADJUSTMENT_REASONS = ["goodwill", "dispute", "service_credit", "refund", "chargeback", "correction"] as const;

/** One credit row (`agent_topups`): a Stripe top-up, the signup allowance, the Pro bundle grant. */
export interface CreditEntry {
  topupId: string;
  principalId: string;
  micros: number;
  source: string;
  ts: number;
}

export type DisputeStatus = "open" | "resolved" | "rejected";

/** A dispute as a statement shows it: no reason text. */
export interface DisputeLine {
  disputeId: string;
  receiptId: string;
  status: DisputeStatus;
  creditMicros: number;
  openedAt: number;
  resolvedAt?: number;
}

/** Usage aggregated per unit for one principal and month. */
export interface UnitUsage {
  unit: MeterUnit;
  calls: number;
  quantity: number;
  listMicros: number;
  chargedMicros: number;
  commitFundedMicros: number;
}

/** Everything a statement is built from; the stores produce it, `buildStatement` renders it. */
export interface StatementInputs {
  usage: UnitUsage[];
  unsettled: number;
  credits: CreditEntry[];
  adjustments: Adjustment[];
  disputes: DisputeLine[];
  commit?: CommitRecord;
}

/** A stored, closed statement: the JSON body and its hash. */
export interface StoredStatement {
  principalId: string;
  month: string;
  body: string;
  hash: string;
  closedAt: number;
}

/** A month closes this long after it ends, so calls in flight at midnight settle into it. */
export const STATEMENT_CLOSE_GRACE_MS = 3_600_000;

const MONTH_RE = /^(\d{4})-(0[1-9]|1[0-2])$/;

/** `[start, end)` of a `YYYY-MM` month in ms, UTC. */
export function monthBounds(month: string): { start: number; end: number } | undefined {
  const m = MONTH_RE.exec(month);
  if (!m) return undefined;
  const y = Number(m[1]);
  const mo = Number(m[2]) - 1;
  return { start: Date.UTC(y, mo, 1), end: Date.UTC(y, mo + 1, 1) };
}

export function monthOf(ms: number): string {
  return new Date(ms).toISOString().slice(0, 7);
}

/** Whether `month`'s statement is closed (immutable) at `nowMs`. */
export function monthClosed(month: string, nowMs: number): boolean {
  const b = monthBounds(month);
  return b !== undefined && nowMs >= b.end + STATEMENT_CLOSE_GRACE_MS;
}

export const STATEMENT_SCHEMA = "shogunagent/statement/v1";

const iso = (ms: number): string => new Date(ms).toISOString();

/**
 * Render a statement body from its inputs. Free units are listed at $0 like any other; every
 * amount is integer µ$. The `hash` is SHA-256 over the canonical JSON (RFC 8785, the agent card's
 * canonicaliser) of every other field, so a client can check that the copy it holds is the stored
 * one.
 */
export function buildStatement(principalId: string, month: string, inputs: StatementInputs, status: "open" | "closed", nowMs: number): Record<string, unknown> {
  const bounds = monthBounds(month);
  if (!bounds) throw new Error("invalid month");
  const usage = [...inputs.usage].sort((a, b) => a.unit.localeCompare(b.unit)).map((u) => ({
    unit: u.unit, calls: u.calls, quantity: u.quantity,
    list_micro_usd: u.listMicros, charged_micro_usd: u.chargedMicros, discount_micro_usd: Math.max(0, u.listMicros - u.chargedMicros),
    commit_funded_micro_usd: u.commitFundedMicros, wallet_funded_micro_usd: Math.max(0, u.chargedMicros - u.commitFundedMicros),
    free: u.listMicros === 0,
  }));
  const credits = [...inputs.credits].sort((a, b) => a.ts - b.ts || a.topupId.localeCompare(b.topupId))
    .map((c) => ({ id: c.topupId, source: c.source, amount_micro_usd: c.micros, ts: iso(c.ts) }));
  const adjustments = [...inputs.adjustments].sort((a, b) => a.ts - b.ts || a.entryId.localeCompare(b.entryId))
    .map((a) => ({ id: a.entryId, kind: a.kind, reason: a.reason, amount_micro_usd: a.micros, applied_micro_usd: a.appliedMicros, ref: a.ref ?? null, ts: iso(a.ts) }));
  const disputes = [...inputs.disputes].sort((a, b) => a.openedAt - b.openedAt || a.disputeId.localeCompare(b.disputeId))
    .map((d) => ({ dispute_id: d.disputeId, receipt_id: d.receiptId, status: d.status, credit_micro_usd: d.creditMicros, opened_at: iso(d.openedAt), resolved_at: d.resolvedAt === undefined ? null : iso(d.resolvedAt) }));
  const sum = (xs: number[]): number => xs.reduce((a, b) => a + b, 0);
  const listTotal = sum(usage.map((u) => u.list_micro_usd));
  const chargedTotal = sum(usage.map((u) => u.charged_micro_usd));
  const body: Record<string, unknown> = {
    schema: STATEMENT_SCHEMA,
    statement_id: `stm_${month}_${principalId}`,
    principal_id: principalId,
    month,
    status,
    period: { start: iso(bounds.start), end: iso(bounds.end) },
    currency: "USD",
    amount_unit: "micro_usd",
    usage,
    totals: {
      calls: sum(usage.map((u) => u.calls)),
      free_calls: sum(usage.filter((u) => u.free).map((u) => u.calls)),
      list_micro_usd: listTotal,
      charged_micro_usd: chargedTotal,
      discount_micro_usd: Math.max(0, listTotal - chargedTotal),
      commit_funded_micro_usd: sum(usage.map((u) => u.commit_funded_micro_usd)),
      wallet_funded_micro_usd: sum(usage.map((u) => u.wallet_funded_micro_usd)),
      credits_micro_usd: sum(credits.map((c) => c.amount_micro_usd)),
      adjustments_micro_usd: sum(adjustments.map((a) => a.applied_micro_usd)),
    },
    credits,
    adjustments,
    disputes,
    commit: inputs.commit ? {
      commit_id: inputs.commit.commitId, subject: inputs.commit.subjectKind, discount_bps: inputs.commit.discountBps,
      amount_micro_usd: inputs.commit.amountMicros, drawn_micro_usd: inputs.commit.drawnMicros,
      remaining_micro_usd: commitRemaining(inputs.commit), overage_micro_usd: inputs.commit.overageMicros,
      starts_at: iso(inputs.commit.startsAt), ends_at: iso(inputs.commit.endsAt),
    } : null,
    unsettled_receipts: inputs.unsettled,
    generated_at: iso(nowMs),
  };
  return { ...body, hash: statementHash(body) };
}

/** `sha256:<hex>` over the canonical JSON of a statement without its `hash`. */
export function statementHash(body: Record<string, unknown>): string {
  const { hash: _ignored, ...rest } = body;
  return `sha256:${createHash("sha256").update(canonicalJson(rest)).digest("hex")}`;
}

function csvCell(v: unknown): string {
  const s = v === null || v === undefined ? "" : String(v);
  return /[",\r\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
}

/** The CSV form of a statement body: one row per usage unit, credit, adjustment and dispute, then
 * the totals and the hash of the JSON it was rendered from. */
export function statementCsv(body: Record<string, unknown>): string {
  const rows: unknown[][] = [["section", "key", "calls", "quantity", "list_micro_usd", "amount_micro_usd", "discount_micro_usd", "status", "ref", "ts"]];
  for (const u of body.usage as Array<Record<string, unknown>>) {
    rows.push(["usage", u.unit, u.calls, u.quantity, u.list_micro_usd, u.charged_micro_usd, u.discount_micro_usd, u.free ? "free" : "billed", "", ""]);
  }
  for (const c of body.credits as Array<Record<string, unknown>>) rows.push(["credit", c.source, "", "", "", c.amount_micro_usd, "", "", c.id, c.ts]);
  for (const a of body.adjustments as Array<Record<string, unknown>>) rows.push(["adjustment", `${String(a.kind)}:${String(a.reason)}`, "", "", "", a.applied_micro_usd, "", "", a.id, a.ts]);
  for (const d of body.disputes as Array<Record<string, unknown>>) rows.push(["dispute", d.dispute_id, "", "", "", d.credit_micro_usd, "", d.status, d.receipt_id, d.opened_at]);
  const t = body.totals as Record<string, unknown>;
  rows.push(["total", "usage", t.calls, "", t.list_micro_usd, t.charged_micro_usd, t.discount_micro_usd, body.status, "", body.generated_at]);
  rows.push(["total", "credits", "", "", "", t.credits_micro_usd, "", "", "", ""]);
  rows.push(["total", "adjustments", "", "", "", t.adjustments_micro_usd, "", "", "", ""]);
  rows.push(["hash", body.hash, "", "", "", "", "", "", body.statement_id, ""]);
  return rows.map((r) => r.map(csvCell).join(",")).join("\r\n") + "\r\n";
}

/** The ShogunAI Pro bundle (E14-T3): $20 of credit a month for a linked Pro licence subject. */
export const PRO_BUNDLE_MICROS = 20_000_000;
/** A link counts for a month's grant only if the Pro licence was verified this recently. */
export const PRO_LINK_FRESH_MS = 35 * 86_400_000;

/** The grant's idempotency key: one per licence subject per UTC month, whichever principal the
 * subject is linked to. */
export function proGrantId(licenceSubject: string, month: string): string {
  return `grant:shogunai-pro:${licenceSubject}:${month}`;
}

export interface LicenceLink {
  licenceSubject: string;
  principalId: string;
  plan: string;
  verifiedAt: number;
}

/** A unsent (or sent) commit-overage usage report. `reportId` is the Stripe idempotency key. */
export interface OverageReport {
  reportId: string;
  commitId: string;
  subscriptionItemId: string;
  cents: number;
  ts: number;
  sentAt?: number;
}

export type CheckoutStatus = "open" | "completed" | "expired";

/** A report's id: the commit and the reported high-water mark it starts from, so a retried claim
 * produces the same id and Stripe (Idempotency-Key) records it once. */
export function overageReportId(commitId: string, reportedBeforeMicros: number): string {
  return `ovr_${commitId}_${reportedBeforeMicros}`;
}

/** The ledger entry for raising a charge's refunded total to `cumulativeMicros`. */
export function chargeRefundEntryId(chargeId: string, cumulativeMicros: number): string {
  return `refund:${chargeId}:${cumulativeMicros}`;
}
