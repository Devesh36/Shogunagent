/**
 * `AgentStore` over Postgres (sql/agent-keys.sql), for every Worker isolate and every Node
 * instance to reserve against the same wallet. Same concurrency discipline as `pg-store.ts`:
 * the wallet row is locked (`SELECT … FOR UPDATE`) for the whole reservation, the cap counters
 * are conditional upserts (check and increment in one statement), and settlement is the one
 * `UPDATE … WHERE settled=0 RETURNING` that flips the flag — a second settle finds no row.
 *
 * Failure policy: a store error inside `reserve` is `unavailable` (→ 503, fail-closed, nothing
 * forwarded); every other method throws `AgentStoreUnavailable` with a fixed message (driver
 * errors can quote parameters and never pass their text on).
 */
import type { AgentKeyRecord } from "./agent-keys.js";
import {
  AgentStoreUnavailable, RECEIPT_KEEP_MS, capCounters, capSubjectKey, capSubjectPrincipal, scopesFromText, scopesToText, validReserve,
  type AgentStore, type CheckoutSessionRecord, type Dispute, type PrincipalRecord, type Receipt, type ReserveInput, type ReserveOutcome, type Wallet,
} from "./agent-store.js";
import {
  chargeRefundEntryId, monthBounds, overagePart, overageReportId,
  type Adjustment, type CommitRecord, type DisputeStatus, type LicenceLink, type OverageReport, type StatementInputs, type StoredStatement,
} from "./billing/ledger.js";
import type { MeterUnit } from "./pricing.js";
import type { Row, SqlClient, SqlExecutor } from "./sql.js";

export const AGENT_STORE_TABLES = [
  "agent_principals", "agent_keys", "agent_wallets", "agent_topups", "agent_receipts", "agent_spend_periods", "agent_disputes", "agent_pow_spent",
  // E19 (re-apply sql/agent-keys.sql on a database provisioned before it).
  "agent_commits", "agent_commit_overage_reports", "agent_adjustments", "agent_charge_refunds", "agent_stripe_payments",
  "agent_checkout_sessions", "agent_statements", "agent_licence_links",
] as const;

/** Columns E19 added to existing tables; `checkSchema` reads them so a stale schema fails boot. */
const E19_COLUMNS: ReadonlyArray<[string, string]> = [
  ["agent_receipts", "list_cost,funding,commit_id"],
  ["agent_disputes", "credit_micros,resolved_at"],
];

/** Columns added by a later, separate provisioning file: [table, columns, the file to apply]. */
const LATER_COLUMNS: ReadonlyArray<[string, string, string]> = [
  // E48: the quantity a receipt settled at.
  ["agent_receipts", "settled_quantity", "sql/agent-receipts-settled-quantity.sql"],
];

const COMMIT_COLUMNS = "commit_id,subject_kind,subject_id,amount_micros,discount_bps,starts_at,ends_at,drawn_micros,overage_micros,overage_reported_micros,created_at,stripe_customer_id,stripe_subscription_item";

function commitFromRow(r: Row): CommitRecord {
  return {
    commitId: String(r.commit_id), subjectKind: r.subject_kind === "tenant" ? "tenant" : "principal", subjectId: String(r.subject_id),
    amountMicros: num(r.amount_micros), discountBps: num(r.discount_bps), startsAt: num(r.starts_at), endsAt: num(r.ends_at),
    drawnMicros: num(r.drawn_micros), overageMicros: num(r.overage_micros), overageReportedMicros: num(r.overage_reported_micros), createdAt: num(r.created_at),
    ...(optText(r.stripe_customer_id) !== undefined ? { stripeCustomerId: String(r.stripe_customer_id) } : {}),
    ...(optText(r.stripe_subscription_item) !== undefined ? { stripeSubscriptionItemId: String(r.stripe_subscription_item) } : {}),
  };
}

function adjustmentFromRow(r: Row): Adjustment {
  return {
    entryId: String(r.entry_id), principalId: String(r.principal_id), micros: num(r.micros), appliedMicros: num(r.applied_micros),
    kind: r.kind === "refund" ? "refund" : "credit", reason: String(r.reason), ...(optText(r.ref) !== undefined ? { ref: String(r.ref) } : {}), ts: num(r.ts),
  };
}

const disputeStatus = (v: unknown): DisputeStatus => (v === "resolved" || v === "rejected" ? v : "open");

class Rollback<T> {
  constructor(readonly value: T) {}
}

const num = (v: unknown): number => Number(v ?? 0);
const opt = (v: unknown): number | undefined => (v === null || v === undefined ? undefined : Number(v));
const optText = (v: unknown): string | undefined => (v === null || v === undefined ? undefined : String(v));

const KEY_COLUMNS = "key_id,prefix,hash,mode,principal_id,tenant_id,scopes,created_at,expires_at,revoked_at,ref,cap_day_micros,cap_month_micros";

function keyFromRow(r: Row): AgentKeyRecord {
  return {
    keyId: String(r.key_id), prefix: String(r.prefix), hash: String(r.hash),
    mode: r.mode === "test" ? "test" : "live",
    principalId: String(r.principal_id), tenantId: String(r.tenant_id),
    scopes: scopesFromText(String(r.scopes)), createdAt: num(r.created_at),
    ...(opt(r.expires_at) !== undefined ? { expiresAt: num(r.expires_at) } : {}),
    ...(opt(r.revoked_at) !== undefined ? { revokedAt: num(r.revoked_at) } : {}),
    ...(optText(r.ref) !== undefined ? { ref: String(r.ref) } : {}),
    ...(opt(r.cap_day_micros) !== undefined ? { capDayMicros: num(r.cap_day_micros) } : {}),
    ...(opt(r.cap_month_micros) !== undefined ? { capMonthMicros: num(r.cap_month_micros) } : {}),
  };
}

const RECEIPT_COLUMNS = "receipt_id,principal_id,key_id,method,name,unit,quantity,estimate,cost,settled,day,month,ts,list_cost,funding,commit_id,settled_quantity";

function receiptFromRow(r: Row): Receipt {
  return {
    receiptId: String(r.receipt_id), principalId: String(r.principal_id), keyId: String(r.key_id), method: String(r.method),
    ...(optText(r.name) !== undefined ? { name: String(r.name) } : {}),
    unit: String(r.unit) as MeterUnit, quantity: num(r.quantity), estimateMicros: num(r.estimate), costMicros: num(r.cost),
    listCostMicros: opt(r.list_cost) ?? num(r.cost), funding: r.funding === "commit" ? "commit" : r.funding === "x402" ? "x402" : "wallet",
    ...(optText(r.commit_id) !== undefined ? { commitId: String(r.commit_id) } : {}),
    ...(opt(r.settled_quantity) !== undefined ? { settledQuantity: num(r.settled_quantity) } : {}),
    settled: num(r.settled) === 1, day: String(r.day), month: String(r.month), ts: num(r.ts),
  };
}

const DISPUTE_COLUMNS = "dispute_id,receipt_id,principal_id,reason,status,ts,credit_micros,resolved_at";

function disputeFromRow(r: Row): Dispute {
  return {
    disputeId: String(r.dispute_id), receiptId: String(r.receipt_id), principalId: String(r.principal_id), reason: String(r.reason), status: disputeStatus(r.status), ts: num(r.ts),
    ...(opt(r.credit_micros) !== undefined ? { creditMicros: num(r.credit_micros) } : {}),
    ...(opt(r.resolved_at) !== undefined ? { resolvedAt: num(r.resolved_at) } : {}),
  };
}

export class SqlAgentStore implements AgentStore {
  constructor(private readonly db: SqlClient) {}

  /** Boot check: every table exists and is readable. Throws with table names only. */
  async checkSchema(): Promise<void> {
    for (const table of AGENT_STORE_TABLES) {
      try {
        await this.db.query(`SELECT 1 FROM ${table} LIMIT 1`);
      } catch {
        throw new AgentStoreUnavailable(`agent store table missing or unreadable: ${table}`);
      }
    }
    for (const [table, columns] of E19_COLUMNS) {
      try {
        await this.db.query(`SELECT ${columns} FROM ${table} LIMIT 1`);
      } catch {
        throw new AgentStoreUnavailable(`agent store columns missing: ${table} (re-apply sql/agent-keys.sql)`);
      }
    }
    for (const [table, columns, file] of LATER_COLUMNS) {
      try {
        await this.db.query(`SELECT ${columns} FROM ${table} LIMIT 1`);
      } catch {
        throw new AgentStoreUnavailable(`agent store columns missing: ${table} (apply ${file})`);
      }
    }
  }

  private async tx<T>(fn: (tx: SqlExecutor) => Promise<T>): Promise<T> {
    try {
      return await this.db.transaction(fn);
    } catch (e) {
      if (e instanceof Rollback) return e.value as T;
      throw new AgentStoreUnavailable("agent store unavailable");
    }
  }

  private async read(text: string, params: unknown[]): Promise<Row[]> {
    try {
      return await this.db.query(text, params);
    } catch {
      throw new AgentStoreUnavailable("agent store unavailable");
    }
  }

  createPrincipal(p: PrincipalRecord): Promise<void> {
    return this.tx(async (tx) => {
      await tx.query(`INSERT INTO agent_principals(principal_id,tenant_id,created_at,ref,cap_day_micros,cap_month_micros)
        VALUES($1::text,$2::text,$3::bigint,$4::text,$5::bigint,$6::bigint) ON CONFLICT(principal_id) DO NOTHING`,
      [p.principalId, p.tenantId, p.createdAt, p.ref ?? null, p.capDayMicros ?? null, p.capMonthMicros ?? null]);
    });
  }

  async principal(principalId: string): Promise<PrincipalRecord | undefined> {
    const r = (await this.read("SELECT principal_id,tenant_id,created_at,ref,cap_day_micros,cap_month_micros FROM agent_principals WHERE principal_id=$1::text", [principalId]))[0];
    if (!r) return undefined;
    return {
      principalId: String(r.principal_id), tenantId: String(r.tenant_id), createdAt: num(r.created_at),
      ...(optText(r.ref) !== undefined ? { ref: String(r.ref) } : {}),
      ...(opt(r.cap_day_micros) !== undefined ? { capDayMicros: num(r.cap_day_micros) } : {}),
      ...(opt(r.cap_month_micros) !== undefined ? { capMonthMicros: num(r.cap_month_micros) } : {}),
    };
  }

  insertKeyOnce(k: AgentKeyRecord): Promise<boolean> {
    return this.tx(async (tx) => {
      const inserted = await tx.query(`INSERT INTO agent_keys(${KEY_COLUMNS})
        VALUES($1::text,$2::text,$3::text,$4::text,$5::text,$6::text,$7::text,$8::bigint,$9::bigint,$10::bigint,$11::text,$12::bigint,$13::bigint)
        ON CONFLICT(key_id) DO NOTHING RETURNING key_id`,
      [k.keyId, k.prefix, k.hash, k.mode, k.principalId, k.tenantId, scopesToText(k.scopes), k.createdAt,
        k.expiresAt ?? null, k.revokedAt ?? null, k.ref ?? null, k.capDayMicros ?? null, k.capMonthMicros ?? null]);
      return inserted.length > 0;
    });
  }

  insertKey(k: AgentKeyRecord): Promise<void> {
    return this.tx(async (tx) => {
      await tx.query(`INSERT INTO agent_keys(${KEY_COLUMNS})
        VALUES($1::text,$2::text,$3::text,$4::text,$5::text,$6::text,$7::text,$8::bigint,$9::bigint,$10::bigint,$11::text,$12::bigint,$13::bigint)`,
      [k.keyId, k.prefix, k.hash, k.mode, k.principalId, k.tenantId, scopesToText(k.scopes), k.createdAt,
        k.expiresAt ?? null, k.revokedAt ?? null, k.ref ?? null, k.capDayMicros ?? null, k.capMonthMicros ?? null]);
    });
  }

  async keysByPrefix(prefix: string): Promise<AgentKeyRecord[]> {
    return (await this.read(`SELECT ${KEY_COLUMNS} FROM agent_keys WHERE prefix=$1::text`, [prefix])).map(keyFromRow);
  }

  async keyById(keyId: string): Promise<AgentKeyRecord | undefined> {
    const r = (await this.read(`SELECT ${KEY_COLUMNS} FROM agent_keys WHERE key_id=$1::text`, [keyId]))[0];
    return r ? keyFromRow(r) : undefined;
  }

  revokeKey(keyId: string, at: number): Promise<boolean> {
    return this.tx(async (tx) => {
      const rows = await tx.query(`UPDATE agent_keys SET revoked_at=CASE WHEN revoked_at IS NULL THEN $2::bigint ELSE revoked_at END
        WHERE key_id=$1::text RETURNING key_id`, [keyId, at]);
      return rows.length > 0;
    });
  }

  setKeyCaps(keyId: string, caps: { dayMicros?: number; monthMicros?: number }): Promise<boolean> {
    return this.tx(async (tx) => {
      const rows = await tx.query("UPDATE agent_keys SET cap_day_micros=$2::bigint,cap_month_micros=$3::bigint WHERE key_id=$1::text RETURNING key_id",
        [keyId, caps.dayMicros ?? null, caps.monthMicros ?? null]);
      return rows.length > 0;
    });
  }

  revokePrincipalKeys(principalId: string, at: number): Promise<number> {
    return this.tx(async (tx) => {
      const rows = await tx.query("UPDATE agent_keys SET revoked_at=$2::bigint WHERE principal_id=$1::text AND revoked_at IS NULL RETURNING key_id", [principalId, at]);
      return rows.length;
    });
  }

  assignTenant(principalId: string, tenantId: string): Promise<boolean> {
    return this.tx(async (tx) => {
      const rows = await tx.query("UPDATE agent_principals SET tenant_id=$2::text WHERE principal_id=$1::text RETURNING principal_id", [principalId, tenantId]);
      if (rows.length === 0) return false;
      await tx.query("UPDATE agent_keys SET tenant_id=$2::text WHERE principal_id=$1::text", [principalId, tenantId]);
      return true;
    });
  }

  async wallet(principalId: string): Promise<Wallet> {
    const r = (await this.read("SELECT balance,reserved FROM agent_wallets WHERE principal_id=$1::text", [principalId]))[0];
    return { principalId, balanceMicros: num(r?.balance), reservedMicros: num(r?.reserved) };
  }

  credit(principalId: string, topupId: string, micros: number, source: string, at: number): Promise<"credited" | "duplicate"> {
    return this.tx(async (tx) => {
      if (!Number.isSafeInteger(micros) || micros < 0) return "duplicate";
      // The topup row is the idempotency record: a second insert of the same id changes nothing.
      const inserted = await tx.query(`INSERT INTO agent_topups(topup_id,principal_id,micros,source,ts) VALUES($1::text,$2::text,$3::bigint,$4::text,$5::bigint)
        ON CONFLICT(topup_id) DO NOTHING RETURNING topup_id`, [topupId, principalId, micros, source, at]);
      if (inserted.length === 0) return "duplicate";
      await tx.query(`INSERT INTO agent_wallets(principal_id,balance,reserved,updated_at) VALUES($1::text,$2::bigint,0,$3::bigint)
        ON CONFLICT(principal_id) DO UPDATE SET balance=agent_wallets.balance+excluded.balance,updated_at=excluded.updated_at`, [principalId, micros, at]);
      return "credited";
    });
  }

  reserve(input: ReserveInput): Promise<ReserveOutcome> {
    return this.tx<ReserveOutcome>(async (tx) => {
      if (!validReserve(input)) return { kind: "unavailable" };
      const funding = input.funding ?? input.commit?.funding ?? "wallet";
      // A free call is written settled at its reserved quantity; any other starts unsettled (NULL).
      const insertReceipt = (settled: 0 | 1): Promise<Row[]> => tx.query(`INSERT INTO agent_receipts(${RECEIPT_COLUMNS})
        VALUES($1::text,$2::text,$3::text,$4::text,$5::text,$6::text,$7::bigint,$8::bigint,0,$9::smallint,$10::text,$11::text,$12::bigint,0,$13::text,$14::text,$15::bigint)`,
      [input.receiptId, input.principalId, input.keyId, input.method, input.name ?? null, input.unit, input.quantity, input.micros, settled, input.day, input.month, input.ts,
        funding, input.commit?.commitId ?? null, settled === 1 ? input.quantity : null]);
      if (input.micros === 0) {
        await insertReceipt(1);
        return { kind: "ok" };
      }
      if (funding === "commit" || funding === "x402") {
        // Commit-funded (E19) or paid per call over x402 (E30): nothing is held on the wallet;
        // the caps still bound the call.
        for (const counter of capCounters(input)) {
          const booked = await tx.query(`INSERT INTO agent_spend_periods(subject,period,micros)
            SELECT $1::text,$2::text,$3::bigint WHERE $3::bigint <= $4::bigint
            ON CONFLICT(subject,period) DO UPDATE SET micros=agent_spend_periods.micros+excluded.micros
            WHERE agent_spend_periods.micros+excluded.micros <= $4::bigint
            RETURNING micros`, [counter.subject, counter.periodKey, input.micros, counter.limit]);
          if (booked.length === 0) {
            const spent = num((await tx.query("SELECT micros FROM agent_spend_periods WHERE subject=$1::text AND period=$2::text", [counter.subject, counter.periodKey]))[0]?.micros);
            throw new Rollback<ReserveOutcome>({ kind: "cap_exceeded", scope: counter.scope, period: counter.period, limitMicros: counter.limit, spentMicros: spent });
          }
        }
        await insertReceipt(0);
        return { kind: "ok" };
      }
      // Lock the wallet for the whole reservation; create it empty if the principal has none.
      await tx.query("INSERT INTO agent_wallets(principal_id,balance,reserved,updated_at) VALUES($1::text,0,0,$2::bigint) ON CONFLICT(principal_id) DO NOTHING", [input.principalId, input.ts]);
      const held = await tx.query(`UPDATE agent_wallets SET reserved=reserved+$2::bigint,updated_at=$3::bigint
        WHERE principal_id=$1::text AND balance-reserved>=$2::bigint RETURNING balance,reserved`, [input.principalId, input.micros, input.ts]);
      if (held.length === 0) {
        const w = (await tx.query("SELECT balance,reserved FROM agent_wallets WHERE principal_id=$1::text FOR UPDATE", [input.principalId]))[0];
        return { kind: "insufficient_funds", availableMicros: Math.max(0, num(w?.balance) - num(w?.reserved)) };
      }
      for (const counter of capCounters(input)) {
        // Check and increment in one statement; no row back = this ceiling refused.
        const booked = await tx.query(`INSERT INTO agent_spend_periods(subject,period,micros)
          SELECT $1::text,$2::text,$3::bigint WHERE $3::bigint <= $4::bigint
          ON CONFLICT(subject,period) DO UPDATE SET micros=agent_spend_periods.micros+excluded.micros
          WHERE agent_spend_periods.micros+excluded.micros <= $4::bigint
          RETURNING micros`, [counter.subject, counter.periodKey, input.micros, counter.limit]);
        if (booked.length === 0) {
          const spent = num((await tx.query("SELECT micros FROM agent_spend_periods WHERE subject=$1::text AND period=$2::text", [counter.subject, counter.periodKey]))[0]?.micros);
          // Undo the wallet hold and any counters already incremented with the transaction.
          throw new Rollback<ReserveOutcome>({ kind: "cap_exceeded", scope: counter.scope, period: counter.period, limitMicros: counter.limit, spentMicros: spent });
        }
      }
      await insertReceipt(0);
      return { kind: "ok" };
    }).catch(() => ({ kind: "unavailable" } as const));
  }

  settle(receiptId: string, actualMicros: number, listMicros: number = actualMicros, settledQuantity?: number): Promise<void> {
    return this.tx(async (tx) => {
      if (!Number.isSafeInteger(actualMicros) || actualMicros < 0) return;
      const list = Number.isSafeInteger(listMicros) && listMicros >= actualMicros ? listMicros : actualMicros;
      // The settled quantity is computed in SQL from the row's reserved quantity (`settledQuantityFor`):
      // the reported figure capped at the reservation, or all of it when charged and none when not.
      const reported = settledQuantity !== undefined && Number.isSafeInteger(settledQuantity) && settledQuantity >= 0 ? settledQuantity : null;
      // The flag flip is the once-only guard: a second settle finds no unsettled row.
      const rows = await tx.query(`UPDATE agent_receipts SET settled=1,cost=$2::bigint,list_cost=$3::bigint,
        settled_quantity=CASE WHEN $4::bigint IS NOT NULL THEN CASE WHEN $4::bigint < quantity THEN $4::bigint ELSE quantity END
          WHEN $2::bigint > 0 THEN quantity ELSE 0 END
        WHERE receipt_id=$1::text AND settled=0
        RETURNING principal_id,key_id,estimate,day,month,funding,commit_id`, [receiptId, actualMicros, list, reported]);
      const r = rows[0];
      if (!r) return;
      const estimate = num(r.estimate);
      const principalId = String(r.principal_id);
      const commitRow = r.funding === "commit" && optText(r.commit_id) !== undefined
        ? (await tx.query("SELECT amount_micros,drawn_micros FROM agent_commits WHERE commit_id=$1::text FOR UPDATE", [String(r.commit_id)]))[0]
        : undefined;
      if (commitRow) {
        const over = overagePart(num(commitRow.amount_micros), num(commitRow.drawn_micros), actualMicros);
        await tx.query("UPDATE agent_commits SET drawn_micros=drawn_micros+$2::bigint,overage_micros=overage_micros+$3::bigint WHERE commit_id=$1::text",
          [String(r.commit_id), actualMicros, over]);
      } else if (r.funding !== "x402") {
        await tx.query(`UPDATE agent_wallets SET
          reserved=CASE WHEN reserved<$2::bigint THEN 0 ELSE reserved-$2::bigint END,
          balance=CASE WHEN balance<$3::bigint THEN 0 ELSE balance-$3::bigint END
          WHERE principal_id=$1::text`, [principalId, estimate, actualMicros]);
      }
      const delta = actualMicros - estimate;
      for (const subject of [capSubjectKey(String(r.key_id)), capSubjectPrincipal(principalId)]) {
        for (const period of [String(r.day), String(r.month)]) {
          await tx.query(`UPDATE agent_spend_periods SET micros=CASE WHEN micros+$3::bigint<0 THEN 0 ELSE micros+$3::bigint END
            WHERE subject=$1::text AND period=$2::text`, [subject, period, delta]);
        }
      }
    });
  }

  async periodSpend(subject: string, period: string): Promise<number> {
    return num((await this.read("SELECT micros FROM agent_spend_periods WHERE subject=$1::text AND period=$2::text", [subject, period]))[0]?.micros);
  }

  async receipt(receiptId: string): Promise<Receipt | undefined> {
    const r = (await this.read(`SELECT ${RECEIPT_COLUMNS} FROM agent_receipts WHERE receipt_id=$1::text`, [receiptId]))[0];
    return r ? receiptFromRow(r) : undefined;
  }

  async receipts(principalId: string, sinceMs: number, limit: number): Promise<Receipt[]> {
    if (!Number.isSafeInteger(limit) || limit <= 0) return [];
    return (await this.read(`SELECT ${RECEIPT_COLUMNS} FROM agent_receipts WHERE principal_id=$1::text AND ts>=$2::bigint
      ORDER BY ts,receipt_id LIMIT $3::bigint`, [principalId, sinceMs, limit])).map(receiptFromRow);
  }

  dispute(receiptId: string, principalId: string, reason: string, disputeId: string, at: number): Promise<{ dispute: Dispute; created: boolean } | undefined> {
    return this.tx(async (tx) => {
      const owned = await tx.query("SELECT receipt_id FROM agent_receipts WHERE receipt_id=$1::text AND principal_id=$2::text FOR UPDATE", [receiptId, principalId]);
      if (owned.length === 0) return undefined;
      const existing = (await tx.query(`SELECT ${DISPUTE_COLUMNS} FROM agent_disputes WHERE receipt_id=$1::text`, [receiptId]))[0];
      if (existing) return { dispute: disputeFromRow(existing), created: false };
      await tx.query("INSERT INTO agent_disputes(dispute_id,receipt_id,principal_id,reason,status,ts) VALUES($1::text,$2::text,$3::text,$4::text,'open',$5::bigint)",
        [disputeId, receiptId, principalId, reason, at]);
      return { dispute: { disputeId, receiptId, principalId, reason, status: "open", ts: at }, created: true };
    });
  }

  // --- E19 ---

  createCommit(c: CommitRecord): Promise<"created" | "duplicate" | "overlap"> {
    return this.tx(async (tx) => {
      if ((await tx.query("SELECT commit_id FROM agent_commits WHERE commit_id=$1::text", [c.commitId])).length > 0) return "duplicate";
      // Lock the subject's commits while checking for an overlap.
      const clash = await tx.query(`SELECT commit_id FROM agent_commits WHERE subject_kind=$1::text AND subject_id=$2::text
        AND starts_at<$4::bigint AND $3::bigint<ends_at FOR UPDATE`, [c.subjectKind, c.subjectId, c.startsAt, c.endsAt]);
      if (clash.length > 0) return "overlap";
      await tx.query(`INSERT INTO agent_commits(${COMMIT_COLUMNS})
        VALUES($1::text,$2::text,$3::text,$4::bigint,$5::bigint,$6::bigint,$7::bigint,$8::bigint,$9::bigint,$10::bigint,$11::bigint,$12::text,$13::text)
        ON CONFLICT(commit_id) DO NOTHING`,
      [c.commitId, c.subjectKind, c.subjectId, c.amountMicros, c.discountBps, c.startsAt, c.endsAt, c.drawnMicros, c.overageMicros, c.overageReportedMicros,
        c.createdAt, c.stripeCustomerId ?? null, c.stripeSubscriptionItemId ?? null]);
      return "created";
    });
  }

  async commit(commitId: string): Promise<CommitRecord | undefined> {
    const r = (await this.read(`SELECT ${COMMIT_COLUMNS} FROM agent_commits WHERE commit_id=$1::text`, [commitId]))[0];
    return r ? commitFromRow(r) : undefined;
  }

  async activeCommit(principalId: string, tenantId: string, atMs: number): Promise<CommitRecord | undefined> {
    const rows = (await this.read(`SELECT ${COMMIT_COLUMNS} FROM agent_commits
      WHERE ((subject_kind='principal' AND subject_id=$1::text) OR (subject_kind='tenant' AND subject_id=$2::text))
      AND starts_at<=$3::bigint AND $3::bigint<ends_at ORDER BY starts_at`, [principalId, tenantId, atMs])).map(commitFromRow);
    return rows.find((c) => c.subjectKind === "principal") ?? rows[0];
  }

  setCommitSubscriptionItem(commitId: string, customerId: string, itemId: string): Promise<boolean> {
    return this.tx(async (tx) => (await tx.query(`UPDATE agent_commits SET stripe_customer_id=$2::text,stripe_subscription_item=$3::text
      WHERE commit_id=$1::text RETURNING commit_id`, [commitId, customerId, itemId])).length > 0);
  }

  claimOverageReports(nowMs: number, limit: number): Promise<OverageReport[]> {
    return this.tx(async (tx) => {
      const pending = await tx.query(`SELECT commit_id,stripe_subscription_item,overage_micros,overage_reported_micros FROM agent_commits
        WHERE stripe_subscription_item IS NOT NULL AND overage_micros-overage_reported_micros>=10000 FOR UPDATE`, []);
      for (const r of pending) {
        const reported = num(r.overage_reported_micros);
        const cents = Math.floor((num(r.overage_micros) - reported) / 10_000);
        if (cents <= 0) continue;
        const reportId = overageReportId(String(r.commit_id), reported);
        await tx.query(`INSERT INTO agent_commit_overage_reports(report_id,commit_id,subscription_item,cents,ts) VALUES($1::text,$2::text,$3::text,$4::bigint,$5::bigint)
          ON CONFLICT(report_id) DO NOTHING`, [reportId, String(r.commit_id), String(r.stripe_subscription_item), cents, nowMs]);
        await tx.query("UPDATE agent_commits SET overage_reported_micros=overage_reported_micros+$2::bigint WHERE commit_id=$1::text AND overage_reported_micros=$3::bigint",
          [String(r.commit_id), cents * 10_000, reported]);
      }
      const rows = await tx.query(`SELECT report_id,commit_id,subscription_item,cents,ts,sent_at FROM agent_commit_overage_reports
        WHERE sent_at IS NULL ORDER BY ts,report_id LIMIT $1::bigint`, [Math.max(0, limit)]);
      return rows.map((r) => ({ reportId: String(r.report_id), commitId: String(r.commit_id), subscriptionItemId: String(r.subscription_item), cents: num(r.cents), ts: num(r.ts) }));
    });
  }

  markOverageReportSent(reportId: string, at: number): Promise<void> {
    return this.tx(async (tx) => {
      await tx.query("UPDATE agent_commit_overage_reports SET sent_at=$2::bigint WHERE report_id=$1::text AND sent_at IS NULL", [reportId, at]);
    });
  }

  private async adjustIn(tx: SqlExecutor, a: Omit<Adjustment, "appliedMicros">): Promise<{ outcome: "applied" | "duplicate"; adjustment: Adjustment }> {
    const existing = (await tx.query("SELECT entry_id,principal_id,micros,applied_micros,kind,reason,ref,ts FROM agent_adjustments WHERE entry_id=$1::text", [a.entryId]))[0];
    if (existing) return { outcome: "duplicate", adjustment: adjustmentFromRow(existing) };
    await tx.query("INSERT INTO agent_wallets(principal_id,balance,reserved,updated_at) VALUES($1::text,0,0,$2::bigint) ON CONFLICT(principal_id) DO NOTHING", [a.principalId, a.ts]);
    const w = (await tx.query("SELECT balance FROM agent_wallets WHERE principal_id=$1::text FOR UPDATE", [a.principalId]))[0];
    const balance = num(w?.balance);
    const applied = a.micros >= 0 ? a.micros : -Math.min(balance, -a.micros);
    await tx.query("UPDATE agent_wallets SET balance=balance+$2::bigint,updated_at=$3::bigint WHERE principal_id=$1::text", [a.principalId, applied, a.ts]);
    await tx.query(`INSERT INTO agent_adjustments(entry_id,principal_id,micros,applied_micros,kind,reason,ref,ts)
      VALUES($1::text,$2::text,$3::bigint,$4::bigint,$5::text,$6::text,$7::text,$8::bigint)`,
    [a.entryId, a.principalId, a.micros, applied, a.kind, a.reason, a.ref ?? null, a.ts]);
    return { outcome: "applied", adjustment: { ...a, appliedMicros: applied } };
  }

  adjust(a: Omit<Adjustment, "appliedMicros">): Promise<{ outcome: "applied" | "duplicate"; adjustment: Adjustment }> {
    return this.tx((tx) => this.adjustIn(tx, a));
  }

  recordChargeRefund(principalId: string, chargeId: string, cumulativeMicros: number, at: number): Promise<{ debitedMicros: number }> {
    return this.tx(async (tx) => {
      if (!Number.isSafeInteger(cumulativeMicros) || cumulativeMicros <= 0) return { debitedMicros: 0 };
      await tx.query("INSERT INTO agent_charge_refunds(charge_id,principal_id,refunded_micros) VALUES($1::text,$2::text,0) ON CONFLICT(charge_id) DO NOTHING", [chargeId, principalId]);
      const before = num((await tx.query("SELECT refunded_micros FROM agent_charge_refunds WHERE charge_id=$1::text FOR UPDATE", [chargeId]))[0]?.refunded_micros);
      if (cumulativeMicros <= before) return { debitedMicros: 0 };
      await tx.query("UPDATE agent_charge_refunds SET refunded_micros=$2::bigint WHERE charge_id=$1::text", [chargeId, cumulativeMicros]);
      const out = await this.adjustIn(tx, { entryId: chargeRefundEntryId(chargeId, cumulativeMicros), principalId, micros: -(cumulativeMicros - before), kind: "refund", reason: "refund", ref: chargeId, ts: at });
      return { debitedMicros: -out.adjustment.appliedMicros };
    });
  }

  recordPayment(paymentIntentId: string, principalId: string, sessionId: string, micros: number, at: number): Promise<void> {
    return this.tx(async (tx) => {
      await tx.query(`INSERT INTO agent_stripe_payments(payment_intent,principal_id,session_id,micros,ts) VALUES($1::text,$2::text,$3::text,$4::bigint,$5::bigint)
        ON CONFLICT(payment_intent) DO NOTHING`, [paymentIntentId, principalId, sessionId, micros, at]);
    });
  }

  async paymentPrincipal(paymentIntentId: string): Promise<string | undefined> {
    return optText((await this.read("SELECT principal_id FROM agent_stripe_payments WHERE payment_intent=$1::text", [paymentIntentId]))[0]?.principal_id);
  }

  recordCheckoutSession(sessionId: string, principalId: string, micros: number, at: number): Promise<void> {
    return this.tx(async (tx) => {
      await tx.query(`INSERT INTO agent_checkout_sessions(session_id,principal_id,micros,status,created_at,updated_at) VALUES($1::text,$2::text,$3::bigint,'open',$4::bigint,$4::bigint)
        ON CONFLICT(session_id) DO NOTHING`, [sessionId, principalId, micros, at]);
    });
  }

  setCheckoutStatus(sessionId: string, status: "completed" | "expired", at: number): Promise<boolean> {
    return this.tx(async (tx) => (await tx.query(`UPDATE agent_checkout_sessions SET status=$2::text,updated_at=$3::bigint
      WHERE session_id=$1::text AND status<>'completed' AND status<>$2::text RETURNING session_id`, [sessionId, status, at])).length > 0);
  }

  async checkoutSession(sessionId: string): Promise<CheckoutSessionRecord | undefined> {
    const r = (await this.read("SELECT session_id,principal_id,micros,status,created_at,updated_at FROM agent_checkout_sessions WHERE session_id=$1::text", [sessionId]))[0];
    if (!r) return undefined;
    const status = r.status === "completed" || r.status === "expired" ? r.status : "open";
    return { sessionId: String(r.session_id), principalId: String(r.principal_id), micros: num(r.micros), status, createdAt: num(r.created_at), updatedAt: num(r.updated_at) };
  }

  async disputeById(disputeId: string): Promise<Dispute | undefined> {
    const r = (await this.read(`SELECT ${DISPUTE_COLUMNS} FROM agent_disputes WHERE dispute_id=$1::text`, [disputeId]))[0];
    return r ? disputeFromRow(r) : undefined;
  }

  resolveDispute(disputeId: string, status: "resolved" | "rejected", creditMicros: number, at: number): Promise<Dispute | undefined> {
    return this.tx(async (tx) => {
      await tx.query(`UPDATE agent_disputes SET status=$2::text,credit_micros=$3::bigint,resolved_at=$4::bigint WHERE dispute_id=$1::text AND status='open'`,
        [disputeId, status, status === "resolved" ? Math.max(0, creditMicros) : 0, at]);
      const r = (await tx.query(`SELECT ${DISPUTE_COLUMNS} FROM agent_disputes WHERE dispute_id=$1::text`, [disputeId]))[0];
      return r ? disputeFromRow(r) : undefined;
    });
  }

  async statementInputs(principalId: string, month: string, nowMs: number): Promise<StatementInputs> {
    const bounds = monthBounds(month) ?? { start: 0, end: 0 };
    const usage = await this.read(`SELECT unit,COUNT(*) AS calls,SUM(quantity) AS qty,SUM(COALESCE(list_cost,cost)) AS list,SUM(cost) AS charged,
      SUM(CASE WHEN funding='commit' THEN cost ELSE 0 END) AS commit_funded,
      SUM(CASE WHEN funding='x402' THEN cost ELSE 0 END) AS x402_funded
      FROM agent_receipts WHERE principal_id=$1::text AND month=$2::text AND settled=1 GROUP BY unit`, [principalId, month]);
    const unsettled = num((await this.read("SELECT COUNT(*) AS n FROM agent_receipts WHERE principal_id=$1::text AND month=$2::text AND settled=0", [principalId, month]))[0]?.n);
    const credits = await this.read("SELECT topup_id,principal_id,micros,source,ts FROM agent_topups WHERE principal_id=$1::text AND ts>=$2::bigint AND ts<$3::bigint",
      [principalId, bounds.start, bounds.end]);
    const adjustments = await this.read("SELECT entry_id,principal_id,micros,applied_micros,kind,reason,ref,ts FROM agent_adjustments WHERE principal_id=$1::text AND ts>=$2::bigint AND ts<$3::bigint",
      [principalId, bounds.start, bounds.end]);
    const disputes = await this.read(`SELECT d.dispute_id,d.receipt_id,d.principal_id,d.reason,d.status,d.ts,d.credit_micros,d.resolved_at
      FROM agent_disputes d JOIN agent_receipts r ON r.receipt_id=d.receipt_id WHERE d.principal_id=$1::text AND r.month=$2::text`, [principalId, month]);
    const tenantId = (await this.principal(principalId))?.tenantId ?? principalId;
    const commit = await this.activeCommit(principalId, tenantId, Math.min(nowMs, bounds.end - 1));
    return {
      usage: usage.map((r) => ({ unit: String(r.unit) as MeterUnit, calls: num(r.calls), quantity: num(r.qty), listMicros: num(r.list), chargedMicros: num(r.charged), commitFundedMicros: num(r.commit_funded), ...(num(r.x402_funded) > 0 ? { x402FundedMicros: num(r.x402_funded) } : {}) })),
      unsettled,
      credits: credits.map((r) => ({ topupId: String(r.topup_id), principalId: String(r.principal_id), micros: num(r.micros), source: String(r.source), ts: num(r.ts) })),
      adjustments: adjustments.map(adjustmentFromRow),
      disputes: disputes.map(disputeFromRow).map((d) => ({ disputeId: d.disputeId, receiptId: d.receiptId, status: d.status, creditMicros: d.creditMicros ?? 0, openedAt: d.ts, ...(d.resolvedAt !== undefined ? { resolvedAt: d.resolvedAt } : {}) })),
      ...(commit ? { commit } : {}),
    };
  }

  async storedStatement(principalId: string, month: string): Promise<StoredStatement | undefined> {
    const r = (await this.read("SELECT principal_id,month,body,hash,closed_at FROM agent_statements WHERE principal_id=$1::text AND month=$2::text", [principalId, month]))[0];
    return r ? { principalId: String(r.principal_id), month: String(r.month), body: String(r.body), hash: String(r.hash), closedAt: num(r.closed_at) } : undefined;
  }

  async saveStatement(s: StoredStatement): Promise<StoredStatement> {
    await this.tx(async (tx) => {
      await tx.query(`INSERT INTO agent_statements(principal_id,month,body,hash,closed_at) VALUES($1::text,$2::text,$3::text,$4::text,$5::bigint)
        ON CONFLICT(principal_id,month) DO NOTHING`, [s.principalId, s.month, s.body, s.hash, s.closedAt]);
    });
    return (await this.storedStatement(s.principalId, s.month)) ?? s;
  }

  linkLicence(link: LicenceLink): Promise<void> {
    return this.tx(async (tx) => {
      await tx.query(`INSERT INTO agent_licence_links(licence_subject,principal_id,plan,verified_at) VALUES($1::text,$2::text,$3::text,$4::bigint)
        ON CONFLICT(licence_subject) DO UPDATE SET principal_id=excluded.principal_id,plan=excluded.plan,verified_at=excluded.verified_at`,
      [link.licenceSubject, link.principalId, link.plan, link.verifiedAt]);
    });
  }

  async licenceLinks(plan: string, sinceMs: number): Promise<LicenceLink[]> {
    return (await this.read("SELECT licence_subject,principal_id,plan,verified_at FROM agent_licence_links WHERE plan=$1::text AND verified_at>=$2::bigint ORDER BY licence_subject",
      [plan, sinceMs])).map((r) => ({ licenceSubject: String(r.licence_subject), principalId: String(r.principal_id), plan: String(r.plan), verifiedAt: num(r.verified_at) }));
  }

  spendChallenge(challengeHash: string, expiresAt: number, nowMs: number): Promise<boolean> {
    return this.tx(async (tx) => {
      // Spend, or re-spend an expired row: one conditional upsert, so two registrations with the
      // same challenge cannot both pass.
      const rows = await tx.query(`INSERT INTO agent_pow_spent(challenge_hash,expires_at) VALUES($1::text,$2::bigint)
        ON CONFLICT(challenge_hash) DO UPDATE SET expires_at=excluded.expires_at WHERE agent_pow_spent.expires_at<=$3::bigint
        RETURNING challenge_hash`, [challengeHash, expiresAt, nowMs]);
      return rows.length > 0;
    });
  }

  maintenance(nowMs: number): Promise<void> {
    return this.tx(async (tx) => {
      await tx.query("DELETE FROM agent_pow_spent WHERE expires_at<=$1::bigint", [nowMs]);
      // Settled receipts past the statement window. Unsettled ones are never deleted: they hold money.
      await tx.query("DELETE FROM agent_receipts WHERE settled=1 AND ts<$1::bigint", [nowMs - RECEIPT_KEEP_MS]);
    });
  }
}
