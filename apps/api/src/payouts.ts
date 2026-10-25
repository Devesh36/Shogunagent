/**
 * Provider payouts (docs/implementation-plan.md E29; contract: docs/api/payouts.md).
 *
 * 1. **Accrual.** The router settles every run it reserved (apps/router/src/settlement.ts) and
 *    posts the content-free settlement to `POST /v1/internal/router/settlements`. Each settlement
 *    is recorded once by its `run_id` and added to the provider's daily accrual: runs by outcome,
 *    the provider's price (failures and empty results at $0) and our take, computed here from
 *    the one rate table (`RUN_MARKUP_BPS`, 5%, pricing.ts).
 * 2. **Payee.** A first-party settlement is our own revenue (ledgered, never paid out). A
 *    settlement tagged with a `marketplace` (or, in format /1, without a payout reference) is an
 *    imported marketplace catalog (Monid), settled on that marketplace's invoice to us. Everything
 *    else is paid through Stripe Connect. Since E32 the take is the same for both third-party
 *    payees: the caller paid provider price + one take through the wallet
 *    (router-wallet.ts), and the accrual records exactly that split.
 * 3. **Payout.** Monthly and cumulative: what a provider earned through the end of the month, less
 *    refunds and lost disputes, less everything already paid, in whole cents, once per
 *    provider-month (Stripe `Idempotency-Key: payout:<provider>:<YYYY-MM>`). Held while a
 *    dispute is open, while the Connect account cannot receive payouts, or when the reference in
 *    provider.json names a different account; carried to the next month below the minimum.
 *
 * This module holds the types, the pure rules, the in-memory store (tests, Node development) and
 * the monthly job. Production is `SqlPayoutStore` (payouts-pg-store.ts) on sql/provider-payouts.sql.
 */
import { monthBounds, monthClosed } from "./billing/ledger.js";
import { RUN_MARKUP_BPS } from "./pricing.js";

/** The router's current settlement format (E32): `/1` plus `receipt_id` and `marketplace`. */
export const SETTLEMENT_FORMAT = "shogunagent-run-settlement/2";
/** The E29 format, still accepted (accrual only; never settles a wallet). */
export const SETTLEMENT_FORMAT_V1 = "shogunagent-run-settlement/1";
/** The most settlements one POST may carry. */
export const MAX_SETTLEMENTS_PER_POST = 100;
/** Default minimum payout: $10. Below it the balance carries to the next month. */
export const DEFAULT_MIN_PAYOUT_MICROS = 10_000_000;
/** Settlement rows (the per-run idempotency records) are kept this long; accruals are kept. */
export const SETTLEMENT_KEEP_MS = 400 * 86_400_000;

export type SettledOutcome = "ok" | "empty" | "provider_error" | "transport_error";
export type Payee = "connect" | "first_party" | "passthrough";
export type AccountStatus = "unboarded" | "onboarding" | "active" | "restricted";
export type PayoutStatus = "pending" | "paid" | "held" | "below_minimum";
export type HoldReason = "dispute_open" | "account_not_active" | "payout_ref_mismatch";

/** What the router posts (apps/router/src/settlement.ts `RunSettlement`). */
export interface RunSettlementInput {
  format: typeof SETTLEMENT_FORMAT | typeof SETTLEMENT_FORMAT_V1;
  run_id: string;
  /** /2: the wallet reservation this run settles (`rcpt_run_<run_id>`), or null. Absent in /1. */
  receipt_id?: string | null;
  /** /2: the marketplace an imported tool came from (`"monid"`), or null. Absent in /1. */
  marketplace?: string | null;
  ts: number;
  tool_id: string;
  provider_id: string;
  first_party: boolean;
  payout_account_ref: string | null;
  outcome: SettledOutcome;
  quantity: number;
  provider_micro_usd: number;
}

/** One settlement as the ledger records it. */
export interface SettlementRecord {
  runId: string;
  providerId: string;
  toolId: string;
  day: string;
  outcome: SettledOutcome;
  payee: Payee;
  payoutAccountRef: string | null;
  providerMicros: number;
  takeMicros: number;
  ts: number;
}

export interface AccrualRow {
  providerId: string;
  day: string;
  payee: Payee;
  payoutAccountRef: string | null;
  runs: number;
  okRuns: number;
  emptyRuns: number;
  failedRuns: number;
  providerMicros: number;
  takeMicros: number;
}

export interface ProviderAccount {
  providerId: string;
  ownerPrincipalId: string;
  stripeAccountId?: string;
  /** Bound to the Connect account id once it can receive payouts. */
  payoutAccountRef?: string;
  status: AccountStatus;
  createdAt: number;
  updatedAt: number;
}

export type AdjustmentKind = "refund" | "dispute";
export type AdjustmentStatus = "applied" | "open" | "won" | "lost";
export const PROVIDER_ADJUSTMENT_REASONS = ["refund", "chargeback", "dispute", "correction", "fraud"] as const;
export type ProviderAdjustmentReason = (typeof PROVIDER_ADJUSTMENT_REASONS)[number];

export interface ProviderAdjustment {
  entryId: string;
  providerId: string;
  kind: AdjustmentKind;
  micros: number;
  status: AdjustmentStatus;
  reason: ProviderAdjustmentReason;
  runId?: string;
  ts: number;
  resolvedAt?: number;
}

export interface PayoutRow {
  providerId: string;
  month: string;
  status: PayoutStatus;
  reason?: HoldReason;
  earnedMicros: number;
  refundedMicros: number;
  disputesLostMicros: number;
  previouslyPaidMicros: number;
  payableMicros: number;
  /** What is (or would be) transferred: `payable` rounded down to whole cents. 0 unless pending/paid. */
  amountMicros: number;
  destination?: string;
  transferId?: string;
  idempotencyKey: string;
  createdAt: number;
  updatedAt: number;
}

/** What a payout decision for one provider-month is computed from. */
export interface PayoutInputs {
  earnedMicros: number;
  refundedMicros: number;
  disputesLostMicros: number;
  openDisputes: number;
  /** Paid or in-flight amounts of every OTHER month. */
  previouslyPaidMicros: number;
  /** The reference provider.json declared on the provider's last accrual, if any. */
  declaredRef: string | null;
}

export class PayoutStoreUnavailable extends Error {}

export interface PayoutStore {
  // --- accounts ---
  /** Create the provider's account row (or move it to a new owner). */
  upsertAccount(providerId: string, ownerPrincipalId: string, at: number): Promise<ProviderAccount>;
  account(providerId: string): Promise<ProviderAccount | undefined>;
  accountsOwnedBy(principalId: string): Promise<ProviderAccount[]>;
  /** Record the Connect account created at onboarding. Never replaces a different one. */
  setStripeAccount(providerId: string, stripeAccountId: string, at: number): Promise<ProviderAccount | undefined>;
  /** Move the onboarding status; `active` binds `payout_account_ref` to the Connect account id. */
  setAccountStatus(providerId: string, status: AccountStatus, at: number): Promise<ProviderAccount | undefined>;

  // --- the settlement seam ---
  /** Record a settlement once per run id and add it to the day's accrual. */
  recordSettlement(s: SettlementRecord): Promise<"recorded" | "duplicate">;
  /** A provider's accruals with `fromDay <= day < toDay`, oldest first. */
  accruals(providerId: string, fromDay: string, toDay: string): Promise<AccrualRow[]>;

  // --- adjustments ---
  addAdjustment(a: ProviderAdjustment): Promise<{ outcome: "created" | "duplicate"; adjustment: ProviderAdjustment }>;
  /** Resolve an open dispute; a resolved one is returned unchanged. */
  resolveDispute(providerId: string, entryId: string, outcome: "won" | "lost", at: number): Promise<ProviderAdjustment | undefined>;
  adjustments(providerId: string): Promise<ProviderAdjustment[]>;

  // --- payouts ---
  /** Providers paid through Connect with any accrual before `toDay`. */
  connectProviders(toDay: string): Promise<string[]>;
  payoutInputs(providerId: string, month: string): Promise<PayoutInputs>;
  payout(providerId: string, month: string): Promise<PayoutRow | undefined>;
  /** Store a decision unless the month is already `pending` or `paid`; returns what is stored. */
  saveDecision(row: PayoutRow): Promise<PayoutRow>;
  /** Mark a pending payout paid with its transfer. */
  markPaid(providerId: string, month: string, transferId: string, at: number): Promise<PayoutRow | undefined>;

  /** Retention: settlement rows older than `SETTLEMENT_KEEP_MS`. */
  maintenance(nowMs: number): Promise<void>;
}

// --- pure rules ---------------------------------------------------------------------------------

const ID_RE = /^[a-z0-9][a-z0-9_.-]{0,127}$/;
const RUN_ID_RE = /^[A-Za-z0-9_.:-]{1,128}$/;
const REF_RE = /^[A-Za-z0-9_:.-]{3,128}$/;
const OUTCOMES: readonly SettledOutcome[] = ["ok", "empty", "provider_error", "transport_error"];
const V1_KEYS: ReadonlySet<string> = new Set(["format", "run_id", "ts", "tool_id", "provider_id", "first_party", "payout_account_ref", "outcome", "quantity", "provider_micro_usd"]);
const V2_KEYS: ReadonlySet<string> = new Set([...V1_KEYS, "receipt_id", "marketplace"]);

/** Our take on a provider price (`RUN_MARKUP_BPS`, rounded up), the same figure `quoteCost("run")` adds. */
export function takeOf(providerMicros: number): number {
  return Math.ceil((providerMicros * RUN_MARKUP_BPS) / 10_000);
}

export function payeeOf(s: Pick<RunSettlementInput, "first_party" | "payout_account_ref" | "marketplace">): Payee {
  if (s.first_party || (s.payout_account_ref ?? "").startsWith("first-party:")) return "first_party";
  if ((s.marketplace ?? null) !== null || s.payout_account_ref === null) return "passthrough";
  return "connect";
}

export function utcDay(ms: number): string {
  return new Date(ms).toISOString().slice(0, 10);
}

/** Validate one posted settlement; the error is a fixed phrase naming the field. */
export function parseSettlement(raw: unknown): { ok: true; value: RunSettlementInput } | { ok: false; error: string } {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) return { ok: false, error: "a settlement must be an object" };
  const r = raw as Record<string, unknown>;
  if (r.format !== SETTLEMENT_FORMAT && r.format !== SETTLEMENT_FORMAT_V1) return { ok: false, error: `format must be ${SETTLEMENT_FORMAT}` };
  const v2 = r.format === SETTLEMENT_FORMAT;
  const allowed = v2 ? V2_KEYS : V1_KEYS;
  for (const k of Object.keys(r)) if (!allowed.has(k)) return { ok: false, error: `unknown field ${k.slice(0, 32)}` };
  if (typeof r.run_id !== "string" || !RUN_ID_RE.test(r.run_id)) return { ok: false, error: "run_id is invalid" };
  if (v2) {
    // A settlement can settle only its own run's reservation: the id is derived from the run id.
    if (r.receipt_id !== null && r.receipt_id !== routerReceiptId(r.run_id)) return { ok: false, error: "receipt_id must be null or rcpt_run_<run_id>" };
    if (r.marketplace !== null && (typeof r.marketplace !== "string" || !ID_RE.test(r.marketplace))) return { ok: false, error: "marketplace must be null or a provider id" };
    if (r.marketplace !== null && r.first_party === true) return { ok: false, error: "a first-party run has no marketplace" };
  }
  if (typeof r.ts !== "number" || !Number.isSafeInteger(r.ts) || r.ts <= 0) return { ok: false, error: "ts must be ms since the epoch" };
  if (typeof r.provider_id !== "string" || !ID_RE.test(r.provider_id)) return { ok: false, error: "provider_id is invalid" };
  if (typeof r.tool_id !== "string" || !ID_RE.test(r.tool_id) || !r.tool_id.startsWith(`${r.provider_id}.`)) return { ok: false, error: "tool_id must be <provider_id>.<endpoint>" };
  if (typeof r.first_party !== "boolean") return { ok: false, error: "first_party must be a boolean" };
  if (r.payout_account_ref !== null && (typeof r.payout_account_ref !== "string" || !REF_RE.test(r.payout_account_ref))) return { ok: false, error: "payout_account_ref must be null or an opaque id" };
  if (typeof r.outcome !== "string" || !(OUTCOMES as readonly string[]).includes(r.outcome)) return { ok: false, error: "outcome is invalid" };
  if (typeof r.quantity !== "number" || !Number.isSafeInteger(r.quantity) || r.quantity < 0) return { ok: false, error: "quantity must be a non-negative integer" };
  if (typeof r.provider_micro_usd !== "number" || !Number.isSafeInteger(r.provider_micro_usd) || r.provider_micro_usd < 0) return { ok: false, error: "provider_micro_usd must be a non-negative integer" };
  if (r.outcome !== "ok" && r.provider_micro_usd !== 0) return { ok: false, error: "only an ok run carries a price; failures and empty results settle at 0" };
  return { ok: true, value: r as unknown as RunSettlementInput };
}

export function settlementRecord(s: RunSettlementInput): SettlementRecord {
  const payee = payeeOf(s);
  return {
    runId: s.run_id, providerId: s.provider_id, toolId: s.tool_id, day: utcDay(s.ts), outcome: s.outcome, payee,
    payoutAccountRef: s.payout_account_ref,
    providerMicros: s.provider_micro_usd,
    takeMicros: takeFor(s, payee),
    ts: s.ts,
  };
}

/** The take a settlement records. First-party revenue is all ours (no take). A third-party run
 * (Connect-paid or a marketplace import) carries exactly one take on the provider price, the one
 * the wallet charged the caller (E32). A format /1 passthrough came from the E29 importer, whose
 * price already had the markup baked in: no second take is recorded for it. */
export function takeFor(s: Pick<RunSettlementInput, "format" | "provider_micro_usd">, payee: Payee): number {
  if (payee === "first_party") return 0;
  if (payee === "passthrough" && s.format === SETTLEMENT_FORMAT_V1) return 0;
  return takeOf(s.provider_micro_usd);
}

/** The receipt id apps/api gives a router run's reservation (router-wallet.ts). */
export function routerReceiptId(runId: string): string {
  return `rcpt_run_${runId}`;
}

export function payoutIdempotencyKey(providerId: string, month: string): string {
  return `payout:${providerId}:${month}`;
}

/** The first day of the month after `month` (`YYYY-MM-DD`), the exclusive bound of its accruals. */
export function monthEndDay(month: string): string | undefined {
  const b = monthBounds(month);
  return b ? utcDay(b.end) : undefined;
}

/** Decide one provider-month. Pure: the job stores it and, when `pending`, sends the transfer. */
export function decidePayout(providerId: string, month: string, inputs: PayoutInputs, account: ProviderAccount | undefined, minPayoutMicros: number, nowMs: number): PayoutRow {
  const payable = Math.max(0, inputs.earnedMicros - inputs.refundedMicros - inputs.disputesLostMicros - inputs.previouslyPaidMicros);
  const base: PayoutRow = {
    providerId, month, status: "held",
    earnedMicros: inputs.earnedMicros, refundedMicros: inputs.refundedMicros, disputesLostMicros: inputs.disputesLostMicros,
    previouslyPaidMicros: inputs.previouslyPaidMicros, payableMicros: payable, amountMicros: 0,
    idempotencyKey: payoutIdempotencyKey(providerId, month), createdAt: nowMs, updatedAt: nowMs,
  };
  if (!account || account.status !== "active" || !account.stripeAccountId) return { ...base, reason: "account_not_active" };
  // provider.json names the account money should go to. Once it is an `acct_` id it must be the
  // account this provider onboarded; anything else is a listing mismatch a human resolves.
  if (inputs.declaredRef?.startsWith("acct_") && inputs.declaredRef !== account.stripeAccountId) return { ...base, reason: "payout_ref_mismatch" };
  if (inputs.openDisputes > 0) return { ...base, reason: "dispute_open" };
  const amount = Math.floor(payable / 10_000) * 10_000;
  if (amount < Math.max(10_000, minPayoutMicros)) return { ...base, status: "below_minimum" };
  return { ...base, status: "pending", amountMicros: amount, destination: account.stripeAccountId };
}

// --- in-memory store ----------------------------------------------------------------------------

export class InMemoryPayoutStore implements PayoutStore {
  private readonly accounts = new Map<string, ProviderAccount>();
  private readonly settlements = new Map<string, SettlementRecord>();
  private readonly accrualRows = new Map<string, AccrualRow>();
  private readonly adjustmentRows = new Map<string, ProviderAdjustment>();
  private readonly payouts = new Map<string, PayoutRow>();
  private queue: Promise<unknown> = Promise.resolve();

  private enqueue<T>(f: () => T): Promise<T> {
    const next = this.queue.then(f);
    this.queue = next.catch(() => undefined);
    return next;
  }

  upsertAccount(providerId: string, ownerPrincipalId: string, at: number): Promise<ProviderAccount> {
    return this.enqueue(() => {
      const existing = this.accounts.get(providerId);
      const row: ProviderAccount = existing ? { ...existing, ownerPrincipalId, updatedAt: at } : { providerId, ownerPrincipalId, status: "unboarded", createdAt: at, updatedAt: at };
      this.accounts.set(providerId, row);
      return { ...row };
    });
  }

  account(providerId: string): Promise<ProviderAccount | undefined> {
    const a = this.accounts.get(providerId);
    return Promise.resolve(a ? { ...a } : undefined);
  }

  accountsOwnedBy(principalId: string): Promise<ProviderAccount[]> {
    return Promise.resolve([...this.accounts.values()].filter((a) => a.ownerPrincipalId === principalId).sort((a, b) => a.providerId.localeCompare(b.providerId)).map((a) => ({ ...a })));
  }

  setStripeAccount(providerId: string, stripeAccountId: string, at: number): Promise<ProviderAccount | undefined> {
    return this.enqueue(() => {
      const a = this.accounts.get(providerId);
      if (!a) return undefined;
      if (a.stripeAccountId === undefined) {
        a.stripeAccountId = stripeAccountId;
        if (a.status === "unboarded") a.status = "onboarding";
        a.updatedAt = at;
      }
      return { ...a };
    });
  }

  setAccountStatus(providerId: string, status: AccountStatus, at: number): Promise<ProviderAccount | undefined> {
    return this.enqueue(() => {
      const a = this.accounts.get(providerId);
      if (!a) return undefined;
      a.status = status;
      if (status === "active" && a.stripeAccountId) a.payoutAccountRef = a.stripeAccountId;
      a.updatedAt = at;
      return { ...a };
    });
  }

  recordSettlement(s: SettlementRecord): Promise<"recorded" | "duplicate"> {
    return this.enqueue(() => {
      if (this.settlements.has(s.runId)) return "duplicate";
      this.settlements.set(s.runId, { ...s });
      const k = `${s.providerId}\u0000${s.day}`;
      const row = this.accrualRows.get(k) ?? { providerId: s.providerId, day: s.day, payee: s.payee, payoutAccountRef: s.payoutAccountRef, runs: 0, okRuns: 0, emptyRuns: 0, failedRuns: 0, providerMicros: 0, takeMicros: 0 };
      row.runs += 1;
      if (s.outcome === "ok") row.okRuns += 1;
      else if (s.outcome === "empty") row.emptyRuns += 1;
      else row.failedRuns += 1;
      row.providerMicros += s.providerMicros;
      row.takeMicros += s.takeMicros;
      row.payee = s.payee;
      row.payoutAccountRef = s.payoutAccountRef;
      this.accrualRows.set(k, row);
      return "recorded";
    });
  }

  accruals(providerId: string, fromDay: string, toDay: string): Promise<AccrualRow[]> {
    return Promise.resolve([...this.accrualRows.values()].filter((r) => r.providerId === providerId && r.day >= fromDay && r.day < toDay)
      .sort((a, b) => a.day.localeCompare(b.day)).map((r) => ({ ...r })));
  }

  addAdjustment(a: ProviderAdjustment): Promise<{ outcome: "created" | "duplicate"; adjustment: ProviderAdjustment }> {
    return this.enqueue(() => {
      const existing = this.adjustmentRows.get(a.entryId);
      if (existing) return { outcome: "duplicate" as const, adjustment: { ...existing } };
      this.adjustmentRows.set(a.entryId, { ...a });
      return { outcome: "created" as const, adjustment: { ...a } };
    });
  }

  resolveDispute(providerId: string, entryId: string, outcome: "won" | "lost", at: number): Promise<ProviderAdjustment | undefined> {
    return this.enqueue(() => {
      const a = this.adjustmentRows.get(entryId);
      if (!a || a.providerId !== providerId || a.kind !== "dispute") return undefined;
      if (a.status === "open") {
        a.status = outcome;
        a.resolvedAt = at;
      }
      return { ...a };
    });
  }

  adjustments(providerId: string): Promise<ProviderAdjustment[]> {
    return Promise.resolve([...this.adjustmentRows.values()].filter((a) => a.providerId === providerId).sort((a, b) => a.ts - b.ts || a.entryId.localeCompare(b.entryId)).map((a) => ({ ...a })));
  }

  connectProviders(toDay: string): Promise<string[]> {
    const ids = new Set<string>();
    for (const r of this.accrualRows.values()) if (r.payee === "connect" && r.day < toDay) ids.add(r.providerId);
    return Promise.resolve([...ids].sort());
  }

  payoutInputs(providerId: string, month: string): Promise<PayoutInputs> {
    return this.enqueue(() => {
      const end = monthEndDay(month) ?? "0000-00-00";
      const endMs = monthBounds(month)?.end ?? 0;
      const rows = [...this.accrualRows.values()].filter((r) => r.providerId === providerId && r.payee === "connect" && r.day < end).sort((a, b) => a.day.localeCompare(b.day));
      const adjustments = [...this.adjustmentRows.values()].filter((a) => a.providerId === providerId);
      return {
        earnedMicros: rows.reduce((n, r) => n + r.providerMicros, 0),
        refundedMicros: adjustments.filter((a) => a.kind === "refund" && a.ts < endMs).reduce((n, a) => n + a.micros, 0),
        disputesLostMicros: adjustments.filter((a) => a.kind === "dispute" && a.status === "lost").reduce((n, a) => n + a.micros, 0),
        openDisputes: adjustments.filter((a) => a.kind === "dispute" && a.status === "open").length,
        previouslyPaidMicros: [...this.payouts.values()].filter((p) => p.providerId === providerId && p.month !== month && (p.status === "paid" || p.status === "pending")).reduce((n, p) => n + p.amountMicros, 0),
        declaredRef: rows[rows.length - 1]?.payoutAccountRef ?? null,
      };
    });
  }

  payout(providerId: string, month: string): Promise<PayoutRow | undefined> {
    const p = this.payouts.get(`${providerId}\u0000${month}`);
    return Promise.resolve(p ? { ...p } : undefined);
  }

  saveDecision(row: PayoutRow): Promise<PayoutRow> {
    return this.enqueue(() => {
      const k = `${row.providerId}\u0000${row.month}`;
      const existing = this.payouts.get(k);
      if (existing && (existing.status === "pending" || existing.status === "paid")) return { ...existing };
      const stored = { ...row, createdAt: existing?.createdAt ?? row.createdAt };
      this.payouts.set(k, stored);
      return { ...stored };
    });
  }

  markPaid(providerId: string, month: string, transferId: string, at: number): Promise<PayoutRow | undefined> {
    return this.enqueue(() => {
      const p = this.payouts.get(`${providerId}\u0000${month}`);
      if (!p) return undefined;
      if (p.status === "pending") {
        p.status = "paid";
        p.transferId = transferId;
        p.updatedAt = at;
      }
      return { ...p };
    });
  }

  maintenance(nowMs: number): Promise<void> {
    return this.enqueue(() => {
      for (const [id, s] of this.settlements) if (s.ts < nowMs - SETTLEMENT_KEEP_MS) this.settlements.delete(id);
    });
  }
}

// --- the monthly job ----------------------------------------------------------------------------

/** What sends money: Stripe Connect transfers (billing/connect.ts) or a fake in tests. */
export type TransferFn = (t: { amountCents: number; destination: string; providerId: string; month: string; idempotencyKey: string }) => Promise<{ transferId: string }>;

export interface PayoutRunCounts {
  month: string;
  providers: number;
  paid: number;
  held: number;
  below_minimum: number;
  already_paid: number;
  failed: number;
}

/**
 * Pay every Connect provider for a closed month. Idempotent per provider-month: a paid month is
 * left alone, a pending one is resent with the same amount and Idempotency-Key (Stripe returns
 * the same transfer), a held or carried one is decided again. Never logs amounts or ids; returns
 * counts only.
 */
export async function runProviderPayouts(store: PayoutStore, transfer: TransferFn | undefined, month: string, nowMs: number, minPayoutMicros: number = DEFAULT_MIN_PAYOUT_MICROS): Promise<PayoutRunCounts | { error: "month_open" | "invalid_month" }> {
  const end = monthEndDay(month);
  if (!end) return { error: "invalid_month" };
  if (!monthClosed(month, nowMs)) return { error: "month_open" };
  const counts: PayoutRunCounts = { month, providers: 0, paid: 0, held: 0, below_minimum: 0, already_paid: 0, failed: 0 };
  for (const providerId of await store.connectProviders(end)) {
    counts.providers += 1;
    const existing = await store.payout(providerId, month);
    if (existing?.status === "paid") { counts.already_paid += 1; continue; }
    let row = existing?.status === "pending" ? existing : undefined;
    if (!row) {
      const decision = decidePayout(providerId, month, await store.payoutInputs(providerId, month), await store.account(providerId), minPayoutMicros, nowMs);
      row = await store.saveDecision(decision);
    }
    if (row.status === "held") { counts.held += 1; continue; }
    if (row.status === "below_minimum") { counts.below_minimum += 1; continue; }
    if (row.status === "paid") { counts.already_paid += 1; continue; }
    if (!transfer || !row.destination) { counts.failed += 1; continue; }
    try {
      const sent = await transfer({ amountCents: Math.round(row.amountMicros / 10_000), destination: row.destination, providerId, month, idempotencyKey: row.idempotencyKey });
      await store.markPaid(providerId, month, sent.transferId, nowMs);
      counts.paid += 1;
    } catch {
      // Left pending: the next run resends with the same amount and key.
      counts.failed += 1;
    }
  }
  return counts;
}

/** The previous UTC month of `nowMs` (`YYYY-MM`): what the sweep pays. */
export function previousMonth(nowMs: number): string {
  const d = new Date(nowMs);
  return new Date(Date.UTC(d.getUTCFullYear(), d.getUTCMonth() - 1, 1)).toISOString().slice(0, 7);
}
