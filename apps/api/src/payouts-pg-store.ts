/**
 * `PayoutStore` over Postgres (sql/provider-payouts.sql). Same discipline as agent-pg-store.ts:
 * the settlement row is the idempotency record (`INSERT … ON CONFLICT DO NOTHING RETURNING`), and
 * the accrual is added in the same transaction only when that insert returned a row, so a
 * redelivered settlement changes nothing. A payout decision is a conditional upsert that never
 * overwrites a `pending` or `paid` month, so two concurrent jobs send at most one transfer amount
 * per provider-month (and Stripe's Idempotency-Key makes the second send return the first).
 *
 * Failure policy: every method throws `PayoutStoreUnavailable` with a fixed message on a driver
 * error (driver errors can quote parameters and never pass their text on).
 */
import { monthBounds } from "./billing/ledger.js";
import {
  PayoutStoreUnavailable, SETTLEMENT_KEEP_MS, monthEndDay,
  type AccountStatus, type AccrualRow, type AdjustmentKind, type AdjustmentStatus, type HoldReason, type Payee, type PayoutInputs, type PayoutRow, type PayoutStatus, type PayoutStore,
  type ProviderAccount, type ProviderAdjustment, type ProviderAdjustmentReason, type SettlementRecord,
} from "./payouts.js";
import type { Row, SqlClient, SqlExecutor } from "./sql.js";

export const PAYOUT_STORE_TABLES = ["provider_accounts", "provider_settlements", "provider_accruals", "provider_adjustments", "provider_payouts"] as const;

const num = (v: unknown): number => Number(v ?? 0);
const optText = (v: unknown): string | undefined => (v === null || v === undefined ? undefined : String(v));
const optNum = (v: unknown): number | undefined => (v === null || v === undefined ? undefined : Number(v));

const ACCOUNT_COLUMNS = "provider_id,owner_principal_id,stripe_account_id,payout_account_ref,status,created_at,updated_at";
const ACCRUAL_COLUMNS = "provider_id,day,payee,payout_account_ref,runs,ok_runs,empty_runs,failed_runs,provider_micros,take_micros";
const ADJUSTMENT_COLUMNS = "entry_id,provider_id,kind,micros,status,reason,run_id,ts,resolved_at";
const PAYOUT_COLUMNS = "provider_id,month,status,reason,earned_micros,refunded_micros,disputes_lost_micros,previously_paid_micros,payable_micros,amount_micros,destination,transfer_id,idempotency_key,created_at,updated_at";

const status = (v: unknown): AccountStatus => (v === "onboarding" || v === "active" || v === "restricted" ? v : "unboarded");

function accountFromRow(r: Row): ProviderAccount {
  return {
    providerId: String(r.provider_id), ownerPrincipalId: String(r.owner_principal_id), status: status(r.status), createdAt: num(r.created_at), updatedAt: num(r.updated_at),
    ...(optText(r.stripe_account_id) !== undefined ? { stripeAccountId: String(r.stripe_account_id) } : {}),
    ...(optText(r.payout_account_ref) !== undefined ? { payoutAccountRef: String(r.payout_account_ref) } : {}),
  };
}

function accrualFromRow(r: Row): AccrualRow {
  const payee: Payee = r.payee === "first_party" || r.payee === "passthrough" ? r.payee : "connect";
  return {
    providerId: String(r.provider_id), day: String(r.day), payee, payoutAccountRef: optText(r.payout_account_ref) ?? null,
    runs: num(r.runs), okRuns: num(r.ok_runs), emptyRuns: num(r.empty_runs), failedRuns: num(r.failed_runs), providerMicros: num(r.provider_micros), takeMicros: num(r.take_micros),
  };
}

function adjustmentFromRow(r: Row): ProviderAdjustment {
  return {
    entryId: String(r.entry_id), providerId: String(r.provider_id), kind: (r.kind === "dispute" ? "dispute" : "refund") as AdjustmentKind,
    micros: num(r.micros), status: String(r.status) as AdjustmentStatus, reason: String(r.reason) as ProviderAdjustmentReason, ts: num(r.ts),
    ...(optText(r.run_id) !== undefined ? { runId: String(r.run_id) } : {}),
    ...(optNum(r.resolved_at) !== undefined ? { resolvedAt: num(r.resolved_at) } : {}),
  };
}

function payoutFromRow(r: Row): PayoutRow {
  return {
    providerId: String(r.provider_id), month: String(r.month), status: String(r.status) as PayoutStatus,
    ...(optText(r.reason) !== undefined ? { reason: String(r.reason) as HoldReason } : {}),
    earnedMicros: num(r.earned_micros), refundedMicros: num(r.refunded_micros), disputesLostMicros: num(r.disputes_lost_micros),
    previouslyPaidMicros: num(r.previously_paid_micros), payableMicros: num(r.payable_micros), amountMicros: num(r.amount_micros),
    ...(optText(r.destination) !== undefined ? { destination: String(r.destination) } : {}),
    ...(optText(r.transfer_id) !== undefined ? { transferId: String(r.transfer_id) } : {}),
    idempotencyKey: String(r.idempotency_key), createdAt: num(r.created_at), updatedAt: num(r.updated_at),
  };
}

export class SqlPayoutStore implements PayoutStore {
  constructor(private readonly db: SqlClient) {}

  private async read(text: string, params: unknown[] = []): Promise<Row[]> {
    try {
      return await this.db.query(text, params);
    } catch {
      throw new PayoutStoreUnavailable("payout store unavailable");
    }
  }

  private async tx<T>(fn: (tx: SqlExecutor) => Promise<T>): Promise<T> {
    try {
      return await this.db.transaction(fn);
    } catch {
      throw new PayoutStoreUnavailable("payout store unavailable");
    }
  }

  /** Boot / sweep: every table exists. */
  async checkSchema(): Promise<void> {
    for (const t of PAYOUT_STORE_TABLES) await this.read(`SELECT 1 FROM ${t} LIMIT 1`);
  }

  upsertAccount(providerId: string, ownerPrincipalId: string, at: number): Promise<ProviderAccount> {
    return this.tx(async (tx) => {
      await tx.query(`INSERT INTO provider_accounts(provider_id,owner_principal_id,status,created_at,updated_at) VALUES($1::text,$2::text,'unboarded',$3::bigint,$3::bigint)
        ON CONFLICT(provider_id) DO UPDATE SET owner_principal_id=excluded.owner_principal_id,updated_at=excluded.updated_at`, [providerId, ownerPrincipalId, at]);
      const r = (await tx.query(`SELECT ${ACCOUNT_COLUMNS} FROM provider_accounts WHERE provider_id=$1::text`, [providerId]))[0];
      if (!r) throw new Error("account vanished");
      return accountFromRow(r);
    });
  }

  async account(providerId: string): Promise<ProviderAccount | undefined> {
    const r = (await this.read(`SELECT ${ACCOUNT_COLUMNS} FROM provider_accounts WHERE provider_id=$1::text`, [providerId]))[0];
    return r ? accountFromRow(r) : undefined;
  }

  async accountsOwnedBy(principalId: string): Promise<ProviderAccount[]> {
    return (await this.read(`SELECT ${ACCOUNT_COLUMNS} FROM provider_accounts WHERE owner_principal_id=$1::text ORDER BY provider_id`, [principalId])).map(accountFromRow);
  }

  setStripeAccount(providerId: string, stripeAccountId: string, at: number): Promise<ProviderAccount | undefined> {
    return this.tx(async (tx) => {
      await tx.query(`UPDATE provider_accounts SET stripe_account_id=$2::text,
        status=CASE WHEN status='unboarded' THEN 'onboarding' ELSE status END,updated_at=$3::bigint
        WHERE provider_id=$1::text AND stripe_account_id IS NULL`, [providerId, stripeAccountId, at]);
      const r = (await tx.query(`SELECT ${ACCOUNT_COLUMNS} FROM provider_accounts WHERE provider_id=$1::text`, [providerId]))[0];
      return r ? accountFromRow(r) : undefined;
    });
  }

  setAccountStatus(providerId: string, next: AccountStatus, at: number): Promise<ProviderAccount | undefined> {
    return this.tx(async (tx) => {
      await tx.query(`UPDATE provider_accounts SET status=$2::text,updated_at=$3::bigint,
        payout_account_ref=CASE WHEN $2::text='active' AND stripe_account_id IS NOT NULL THEN stripe_account_id ELSE payout_account_ref END
        WHERE provider_id=$1::text`, [providerId, next, at]);
      const r = (await tx.query(`SELECT ${ACCOUNT_COLUMNS} FROM provider_accounts WHERE provider_id=$1::text`, [providerId]))[0];
      return r ? accountFromRow(r) : undefined;
    });
  }

  recordSettlement(s: SettlementRecord): Promise<"recorded" | "duplicate"> {
    return this.tx(async (tx) => {
      const inserted = await tx.query(`INSERT INTO provider_settlements(run_id,provider_id,tool_id,day,outcome,provider_micros,take_micros,ts)
        VALUES($1::text,$2::text,$3::text,$4::text,$5::text,$6::bigint,$7::bigint,$8::bigint) ON CONFLICT(run_id) DO NOTHING RETURNING run_id`,
      [s.runId, s.providerId, s.toolId, s.day, s.outcome, s.providerMicros, s.takeMicros, s.ts]);
      if (inserted.length === 0) return "duplicate";
      const ok = s.outcome === "ok" ? 1 : 0;
      const empty = s.outcome === "empty" ? 1 : 0;
      const failed = ok + empty === 0 ? 1 : 0;
      await tx.query(`INSERT INTO provider_accruals(${ACCRUAL_COLUMNS})
        VALUES($1::text,$2::text,$3::text,$4::text,1,$5::bigint,$6::bigint,$7::bigint,$8::bigint,$9::bigint)
        ON CONFLICT(provider_id,day) DO UPDATE SET runs=provider_accruals.runs+1,ok_runs=provider_accruals.ok_runs+excluded.ok_runs,
          empty_runs=provider_accruals.empty_runs+excluded.empty_runs,failed_runs=provider_accruals.failed_runs+excluded.failed_runs,
          provider_micros=provider_accruals.provider_micros+excluded.provider_micros,take_micros=provider_accruals.take_micros+excluded.take_micros,
          payee=excluded.payee,payout_account_ref=excluded.payout_account_ref`,
      [s.providerId, s.day, s.payee, s.payoutAccountRef, ok, empty, failed, s.providerMicros, s.takeMicros]);
      return "recorded";
    });
  }

  async accruals(providerId: string, fromDay: string, toDay: string): Promise<AccrualRow[]> {
    return (await this.read(`SELECT ${ACCRUAL_COLUMNS} FROM provider_accruals WHERE provider_id=$1::text AND day>=$2::text AND day<$3::text ORDER BY day`, [providerId, fromDay, toDay])).map(accrualFromRow);
  }

  addAdjustment(a: ProviderAdjustment): Promise<{ outcome: "created" | "duplicate"; adjustment: ProviderAdjustment }> {
    return this.tx(async (tx) => {
      const inserted = await tx.query(`INSERT INTO provider_adjustments(${ADJUSTMENT_COLUMNS})
        VALUES($1::text,$2::text,$3::text,$4::bigint,$5::text,$6::text,$7::text,$8::bigint,NULL) ON CONFLICT(entry_id) DO NOTHING RETURNING entry_id`,
      [a.entryId, a.providerId, a.kind, a.micros, a.status, a.reason, a.runId ?? null, a.ts]);
      const r = (await tx.query(`SELECT ${ADJUSTMENT_COLUMNS} FROM provider_adjustments WHERE entry_id=$1::text`, [a.entryId]))[0];
      if (!r) throw new Error("adjustment vanished");
      return { outcome: inserted.length > 0 ? "created" as const : "duplicate" as const, adjustment: adjustmentFromRow(r) };
    });
  }

  resolveDispute(providerId: string, entryId: string, outcome: "won" | "lost", at: number): Promise<ProviderAdjustment | undefined> {
    return this.tx(async (tx) => {
      await tx.query(`UPDATE provider_adjustments SET status=$3::text,resolved_at=$4::bigint
        WHERE entry_id=$1::text AND provider_id=$2::text AND kind='dispute' AND status='open'`, [entryId, providerId, outcome, at]);
      const r = (await tx.query(`SELECT ${ADJUSTMENT_COLUMNS} FROM provider_adjustments WHERE entry_id=$1::text AND provider_id=$2::text AND kind='dispute'`, [entryId, providerId]))[0];
      return r ? adjustmentFromRow(r) : undefined;
    });
  }

  async adjustments(providerId: string): Promise<ProviderAdjustment[]> {
    return (await this.read(`SELECT ${ADJUSTMENT_COLUMNS} FROM provider_adjustments WHERE provider_id=$1::text ORDER BY ts,entry_id`, [providerId])).map(adjustmentFromRow);
  }

  async connectProviders(toDay: string): Promise<string[]> {
    return (await this.read("SELECT DISTINCT provider_id FROM provider_accruals WHERE payee='connect' AND day<$1::text ORDER BY provider_id", [toDay])).map((r) => String(r.provider_id));
  }

  payoutInputs(providerId: string, month: string): Promise<PayoutInputs> {
    return this.tx(async (tx) => {
      const end = monthEndDay(month) ?? "0000-00-00";
      const endMs = monthBounds(month)?.end ?? 0;
      const earned = (await tx.query("SELECT SUM(provider_micros) AS m FROM provider_accruals WHERE provider_id=$1::text AND payee='connect' AND day<$2::text", [providerId, end]))[0];
      const last = (await tx.query("SELECT payout_account_ref FROM provider_accruals WHERE provider_id=$1::text AND payee='connect' AND day<$2::text ORDER BY day DESC LIMIT 1", [providerId, end]))[0];
      const refunded = (await tx.query("SELECT SUM(micros) AS m FROM provider_adjustments WHERE provider_id=$1::text AND kind='refund' AND ts<$2::bigint", [providerId, endMs]))[0];
      const lost = (await tx.query("SELECT SUM(micros) AS m FROM provider_adjustments WHERE provider_id=$1::text AND kind='dispute' AND status='lost'", [providerId]))[0];
      const open = (await tx.query("SELECT COUNT(*) AS n FROM provider_adjustments WHERE provider_id=$1::text AND kind='dispute' AND status='open'", [providerId]))[0];
      const paid = (await tx.query("SELECT SUM(amount_micros) AS m FROM provider_payouts WHERE provider_id=$1::text AND month<>$2::text AND status IN ('paid','pending')", [providerId, month]))[0];
      return {
        earnedMicros: num(earned?.m), refundedMicros: num(refunded?.m), disputesLostMicros: num(lost?.m), openDisputes: num(open?.n),
        previouslyPaidMicros: num(paid?.m), declaredRef: optText(last?.payout_account_ref) ?? null,
      };
    });
  }

  async payout(providerId: string, month: string): Promise<PayoutRow | undefined> {
    const r = (await this.read(`SELECT ${PAYOUT_COLUMNS} FROM provider_payouts WHERE provider_id=$1::text AND month=$2::text`, [providerId, month]))[0];
    return r ? payoutFromRow(r) : undefined;
  }

  saveDecision(row: PayoutRow): Promise<PayoutRow> {
    return this.tx(async (tx) => {
      await tx.query(`INSERT INTO provider_payouts(${PAYOUT_COLUMNS})
        VALUES($1::text,$2::text,$3::text,$4::text,$5::bigint,$6::bigint,$7::bigint,$8::bigint,$9::bigint,$10::bigint,$11::text,NULL,$12::text,$13::bigint,$13::bigint)
        ON CONFLICT(provider_id,month) DO UPDATE SET status=excluded.status,reason=excluded.reason,earned_micros=excluded.earned_micros,
          refunded_micros=excluded.refunded_micros,disputes_lost_micros=excluded.disputes_lost_micros,previously_paid_micros=excluded.previously_paid_micros,
          payable_micros=excluded.payable_micros,amount_micros=excluded.amount_micros,destination=excluded.destination,updated_at=excluded.updated_at
        WHERE provider_payouts.status NOT IN ('pending','paid')`,
      [row.providerId, row.month, row.status, row.reason ?? null, row.earnedMicros, row.refundedMicros, row.disputesLostMicros, row.previouslyPaidMicros,
        row.payableMicros, row.amountMicros, row.destination ?? null, row.idempotencyKey, row.updatedAt]);
      const r = (await tx.query(`SELECT ${PAYOUT_COLUMNS} FROM provider_payouts WHERE provider_id=$1::text AND month=$2::text`, [row.providerId, row.month]))[0];
      if (!r) throw new Error("payout vanished");
      return payoutFromRow(r);
    });
  }

  markPaid(providerId: string, month: string, transferId: string, at: number): Promise<PayoutRow | undefined> {
    return this.tx(async (tx) => {
      await tx.query("UPDATE provider_payouts SET status='paid',transfer_id=$3::text,updated_at=$4::bigint WHERE provider_id=$1::text AND month=$2::text AND status='pending'", [providerId, month, transferId, at]);
      const r = (await tx.query(`SELECT ${PAYOUT_COLUMNS} FROM provider_payouts WHERE provider_id=$1::text AND month=$2::text`, [providerId, month]))[0];
      return r ? payoutFromRow(r) : undefined;
    });
  }

  async maintenance(nowMs: number): Promise<void> {
    await this.read("DELETE FROM provider_settlements WHERE ts<$1::bigint", [nowMs - SETTLEMENT_KEEP_MS]);
  }
}
