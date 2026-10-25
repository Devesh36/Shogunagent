import { randomUUID } from "node:crypto";
import type { SqlClient, SqlExecutor } from "./sql.js";
import type { LiveLane, LiveProvider } from "./live-config.js";

export class LiveRefusal extends Error {
  constructor(readonly reason: "blocked" | "budget_exceeded" | "rate_limited" | "concurrency_limited" | "provider_budget_exceeded", readonly status: 403 | 429 | 503, readonly retryAfter = 1) { super(reason); }
}
export interface LiveAdmission {
  subject: string; device: string; provider: LiveProvider; lane: LiveLane;
  reserve: number; daily: number; monthly: number; global: number;
}
export interface LiveTicket { id: string; subject: string; provider: LiveProvider; day: string; month: string; reserved: number }
export interface LiveStore {
  reserve(input: LiveAdmission): Promise<LiveTicket>;
  settle(ticket: LiveTicket, actual?: number): Promise<void>;
  usage(subject: string): Promise<{ daily: number; monthly: number; blocked: boolean }>;
  block(subject: string, blocked: boolean, actor: string): Promise<void>;
}
const periods = (now: number) => { const day = new Date(now).toISOString().slice(0, 10); return { day, month: day.slice(0, 7) }; };
const count = (value: unknown): number => { const n = Number(value ?? 0); if (!Number.isSafeInteger(n) || n < 0) throw new Error("invalid live counter"); return n; };
// Timeout is shorter than the lease. A crashed request keeps its money reservation but cannot
// hold a stream slot forever. All isolates lock the same subject, then provider, then ticket.
export const LIVE_MAX_MS = 240_000;
export class SqlLiveStore implements LiveStore {
  constructor(private readonly db: SqlClient, private readonly now = Date.now) {}
  private async lock(tx: SqlExecutor, subject: string): Promise<boolean> {
    await tx.query("INSERT INTO relay_live_subjects(subject) VALUES($1::text) ON CONFLICT(subject) DO NOTHING", [subject]);
    const rows = await tx.query("SELECT blocked FROM relay_live_subjects WHERE subject=$1::text FOR UPDATE", [subject]);
    return Number(rows[0]?.blocked) === 1;
  }
  /** Bounded retention job, never on the hot path. Abandoned calls keep their full charge. */
  async maintenance(): Promise<void> {
    const cutoff = this.now() - 400 * 86_400_000;
    await this.db.transaction(async tx => {
      await tx.query("DELETE FROM relay_live_requests WHERE started<$1::bigint AND expires<$2::bigint", [cutoff, this.now()]);
      await tx.query("DELETE FROM relay_live_counters WHERE period<$1::text", [new Date(cutoff).toISOString().slice(0, 7)]);
    });
  }
  async reserve(input: LiveAdmission): Promise<LiveTicket> {
    for (const value of [input.reserve, input.daily, input.monthly, input.global]) if (!Number.isSafeInteger(value) || value < 0) throw new Error("invalid live budget");
    return this.db.transaction(async tx => {
      if (await this.lock(tx, input.subject)) throw new LiveRefusal("blocked", 403);
      await this.lock(tx, `provider:${input.provider}`);
      const now = this.now(); const { day, month } = periods(now);
      const active = await tx.query("SELECT device FROM relay_live_requests WHERE subject=$1::text AND settled=0 AND expires>$2::bigint", [input.subject, now]);
      if (active.length >= 3 || active.filter(row => row.device === input.device).length >= 2) throw new LiveRefusal("concurrency_limited", 429, 5);
      const recent = await tx.query("SELECT id FROM relay_live_requests WHERE subject=$1::text AND lane=$2::text AND started>$3::bigint", [input.subject, input.lane, now - 60_000]);
      // Live meeting lanes send one request per caption line; the rest are user presses.
      if (recent.length >= (input.lane === "translate_live" || input.lane === "summary_live" ? 40 : 12)) throw new LiveRefusal("rate_limited", 429, 60);
      const buckets: Array<[string, string, number]> = [[input.subject, day, input.daily], [input.subject, month, input.monthly], [`provider:${input.provider}`, day, input.global]];
      for (const [subject, period, cap] of buckets) {
        const rows = await tx.query("SELECT spent FROM relay_live_counters WHERE subject=$1::text AND period=$2::text", [subject, period]);
        if (count(rows[0]?.spent) + input.reserve > cap) {
          const reset = period.length === 7 ? Date.UTC(Number(day.slice(0,4)), Number(day.slice(5,7)), 1) : Date.parse(day) + 86400000;
          throw new LiveRefusal(subject.startsWith("provider:") ? "provider_budget_exceeded" : "budget_exceeded", subject.startsWith("provider:") ? 503 : 429, Math.max(1, Math.ceil((reset-now)/1000)));
        }
        await tx.query("INSERT INTO relay_live_counters(subject,period,spent) VALUES($1::text,$2::text,$3::bigint) ON CONFLICT(subject,period) DO UPDATE SET spent=relay_live_counters.spent+excluded.spent", [subject, period, input.reserve]);
      }
      const ticket: LiveTicket = { id: randomUUID(), subject: input.subject, provider: input.provider, day, month, reserved: input.reserve };
      await tx.query("INSERT INTO relay_live_requests(id,subject,device,provider,lane,day,month,reserved,started,expires) VALUES($1::text,$2::text,$3::text,$4::text,$5::text,$6::text,$7::text,$8::bigint,$9::bigint,$10::bigint)", [ticket.id, input.subject, input.device, input.provider, input.lane, day, month, input.reserve, now, now + LIVE_MAX_MS + 60_000]);
      return ticket;
    });
  }
  async settle(ticket: LiveTicket, actual?: number): Promise<void> {
    await this.db.transaction(async tx => {
      await this.lock(tx, ticket.subject); await this.lock(tx, `provider:${ticket.provider}`);
      const rows = await tx.query("SELECT * FROM relay_live_requests WHERE id=$1::text FOR UPDATE", [ticket.id]);
      const row = rows[0]; if (!row || Number(row.settled) === 1) return;
      // A malformed usage value never refunds a reservation. Even above-estimate actual cost
      // is recorded so the following request sees the true breaker balance.
      const reserved = count(row.reserved);
      const billed = actual !== undefined && Number.isSafeInteger(actual) && actual >= 0 ? actual : reserved;
      const delta = billed-reserved;
      for (const [subject, period] of [[String(row.subject), String(row.day)], [String(row.subject), String(row.month)], [`provider:${row.provider}`, String(row.day)]]) {
        await tx.query("UPDATE relay_live_counters SET spent=spent+$3::bigint WHERE subject=$1::text AND period=$2::text", [subject, period, delta]);
      }
      await tx.query("UPDATE relay_live_requests SET settled=1 WHERE id=$1::text", [ticket.id]);
    });
  }
  async usage(subject: string): Promise<{ daily: number; monthly: number; blocked: boolean }> {
    const { day, month } = periods(this.now());
    const rows = await this.db.query("SELECT period,spent FROM relay_live_counters WHERE subject=$1::text AND (period=$2::text OR period=$3::text)", [subject, day, month]);
    const status = await this.db.query("SELECT blocked FROM relay_live_subjects WHERE subject=$1::text", [subject]);
    return { daily: count(rows.find(row => row.period === day)?.spent), monthly: count(rows.find(row => row.period === month)?.spent), blocked: Number(status[0]?.blocked) === 1 };
  }
  async block(subject: string, blocked: boolean, actor: string): Promise<void> {
    await this.db.transaction(async tx => {
      await this.lock(tx, subject);
      await tx.query("UPDATE relay_live_subjects SET blocked=$2::integer WHERE subject=$1::text", [subject, blocked ? 1 : 0]);
      await tx.query("INSERT INTO relay_live_admin_actions(id,actor,subject,blocked,ts) VALUES($1::text,$2::text,$3::text,$4::integer,$5::bigint)", [randomUUID(), actor, subject, blocked ? 1 : 0, this.now()]);
    });
  }
}
