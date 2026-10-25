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
  type AgentStore, type Dispute, type PrincipalRecord, type Receipt, type ReserveInput, type ReserveOutcome, type Wallet,
} from "./agent-store.js";
import type { MeterUnit } from "./pricing.js";
import type { Row, SqlClient, SqlExecutor } from "./sql.js";

export const AGENT_STORE_TABLES = [
  "agent_principals", "agent_keys", "agent_wallets", "agent_topups", "agent_receipts", "agent_spend_periods", "agent_disputes", "agent_pow_spent",
] as const;

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

const RECEIPT_COLUMNS = "receipt_id,principal_id,key_id,method,name,unit,quantity,estimate,cost,settled,day,month,ts";

function receiptFromRow(r: Row): Receipt {
  return {
    receiptId: String(r.receipt_id), principalId: String(r.principal_id), keyId: String(r.key_id), method: String(r.method),
    ...(optText(r.name) !== undefined ? { name: String(r.name) } : {}),
    unit: String(r.unit) as MeterUnit, quantity: num(r.quantity), estimateMicros: num(r.estimate), costMicros: num(r.cost),
    settled: num(r.settled) === 1, day: String(r.day), month: String(r.month), ts: num(r.ts),
  };
}

function disputeFromRow(r: Row): Dispute {
  return { disputeId: String(r.dispute_id), receiptId: String(r.receipt_id), principalId: String(r.principal_id), reason: String(r.reason), status: "open", ts: num(r.ts) };
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
      const insertReceipt = (settled: 0 | 1): Promise<Row[]> => tx.query(`INSERT INTO agent_receipts(${RECEIPT_COLUMNS})
        VALUES($1::text,$2::text,$3::text,$4::text,$5::text,$6::text,$7::bigint,$8::bigint,0,$9::smallint,$10::text,$11::text,$12::bigint)`,
      [input.receiptId, input.principalId, input.keyId, input.method, input.name ?? null, input.unit, input.quantity, input.micros, settled, input.day, input.month, input.ts]);
      if (input.micros === 0) {
        await insertReceipt(1);
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

  settle(receiptId: string, actualMicros: number): Promise<void> {
    return this.tx(async (tx) => {
      if (!Number.isSafeInteger(actualMicros) || actualMicros < 0) return;
      // The flag flip is the once-only guard: a second settle finds no unsettled row.
      const rows = await tx.query(`UPDATE agent_receipts SET settled=1,cost=$2::bigint WHERE receipt_id=$1::text AND settled=0
        RETURNING principal_id,key_id,estimate,day,month`, [receiptId, actualMicros]);
      const r = rows[0];
      if (!r) return;
      const estimate = num(r.estimate);
      const principalId = String(r.principal_id);
      await tx.query(`UPDATE agent_wallets SET
        reserved=CASE WHEN reserved<$2::bigint THEN 0 ELSE reserved-$2::bigint END,
        balance=CASE WHEN balance<$3::bigint THEN 0 ELSE balance-$3::bigint END
        WHERE principal_id=$1::text`, [principalId, estimate, actualMicros]);
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
      const existing = (await tx.query("SELECT dispute_id,receipt_id,principal_id,reason,ts FROM agent_disputes WHERE receipt_id=$1::text", [receiptId]))[0];
      if (existing) return { dispute: disputeFromRow(existing), created: false };
      await tx.query("INSERT INTO agent_disputes(dispute_id,receipt_id,principal_id,reason,status,ts) VALUES($1::text,$2::text,$3::text,$4::text,'open',$5::bigint)",
        [disputeId, receiptId, principalId, reason, at]);
      return { dispute: { disputeId, receiptId, principalId, reason, status: "open", ts: at }, created: true };
    });
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
