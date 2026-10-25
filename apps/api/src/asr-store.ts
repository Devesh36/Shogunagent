import { randomUUID } from "node:crypto";
import type { SpeechUsage } from "./asr-usage.js";
import { operational } from "./logging.js";
import type { SqlClient } from "./sql.js";
import { DEEPGRAM_GRANT_TTL_SECONDS } from "./deepgram.js";

export const ASR_MONTH_SECONDS = 20 * 3600;
export const ASR_SESSION_SECONDS = 3600;
/** One grant or refresh per device per this long. */
export const ASR_MINT_PACING_MS = 20_000;
export class AsrRefused extends Error {
  constructor(readonly reason: "monthly_cap" | "rate_limited" | "concurrency_limited" | "blocked" | "spend_cap" | "session_expired") { super(reason); }
}
export interface AsrTicket { id: string; seconds: number }
export interface AsrStore {
  /** `capMicros`: the global spend ceiling this subject may reserve up to (the trial share for a
   * trial, audit 2026-10-20 H3); never above the store's own cap. Absent = the store's cap. */
  reserve(subject: string, device: string, streams?: number, capMicros?: number): Promise<AsrTicket>;
  refresh(subject: string, device: string, id: string, capMicros?: number): Promise<void>;
  finish(subject: string, device: string, id: string): Promise<void>;
  /** Refuse (`rate_limited`) when this device minted within `ASR_MINT_PACING_MS`. A read-only
   * pre-check the route runs BEFORE the provider usage reads, so a paced request costs no
   * Deepgram call and no ledger write; `reserve` / `refresh` check again inside their
   * transaction, which is what holds under concurrency. */
  checkPacing(subject: string, device: string): Promise<void>;
}
/** Admission is shared across isolates. Reserve the full session before minting; crashes keep
 * that reservation. Finish uses SERVER elapsed time, never a client-supplied usage duration.
 * This governs supported clients, not stolen JWTs: Deepgram JWTs can be reused and do not
 * terminate established sockets on expiry. Provider aggregate floors are checked before every mint and refresh.
 *
 * **Non-speech usage on the project (audit 2026-10-20 H3).** A grant can call any Deepgram API the
 * project allows, and usage tags are client-supplied. Usage tagged with a subject's own tag blocks
 * that subject. Project-wide usage that no tag attributes is an `ALERT` line for the operator, and
 * nothing more: an automatic project-wide latch would let any one grant holder switch speech off
 * for every user. The operator stop is the `('blocked','global')` row in
 * `relay_asr_provider_usage`, which only an operator writes (DEPLOY.md, "Company-funded speech").
 */
export class SqlAsrStore implements AsrStore {
  constructor(
    private readonly db: SqlClient,
    private readonly now = Date.now,
    private readonly dailyCapMicros = 15_000_000,
    private readonly hourlyReserveMicros = 1_000_000,
    /** Fixed-word alert lines (no ids). Default: the operational error stream. */
    private readonly alert: (line: string) => void = (line) => operational.error(line),
  ) {}
  /** The ceiling a reservation may reach: the caller's (trial share), never above the store's. */
  private capFor(capMicros: number | undefined): number {
    return capMicros === undefined ? this.dailyCapMicros : Math.min(this.dailyCapMicros, capMicros);
  }
  async checkLedger(): Promise<void> {
    await this.db.query("SELECT seconds FROM relay_asr_provider_usage LIMIT 1");
  }
  async reconcile(subject: string, usage: { day: SpeechUsage; month: SpeechUsage }): Promise<void> {
    const now = this.now(); const day = new Date(now).toISOString().slice(0, 10); const month = day.slice(0, 7);
    await this.db.transaction(async tx => {
      // Global row serializes overlapping observations; stale observations cannot refund usage.
      await tx.query("INSERT INTO relay_asr_provider_usage(scope,period,seconds) VALUES('global',$1::text,0) ON CONFLICT(scope,period) DO NOTHING", [day]);
      const previous = await tx.query("SELECT seconds FROM relay_asr_provider_usage WHERE scope='global' AND period=$1::text FOR UPDATE", [day]);
      const delta = Math.max(0, usage.day.seconds - Number(previous[0]?.seconds));
      if (delta > 0) {
        await tx.query("UPDATE relay_asr_provider_usage SET seconds=$2::bigint WHERE scope='global' AND period=$1::text", [day, usage.day.seconds]);
        // Conservative additional debit: provider usage is never refunded by client finish.
        await tx.query("INSERT INTO relay_spend_days(date,micros) VALUES($1::text,$2::bigint) ON CONFLICT(date) DO UPDATE SET micros=relay_spend_days.micros+excluded.micros", [day, Math.ceil(delta * this.hourlyReserveMicros / 3600)]);
      }
      await tx.query("INSERT INTO relay_asr_provider_usage(scope,period,seconds) VALUES($1::text,$2::text,$3::bigint) ON CONFLICT(scope,period) DO UPDATE SET seconds=CASE WHEN relay_asr_provider_usage.seconds<excluded.seconds THEN excluded.seconds ELSE relay_asr_provider_usage.seconds END", [subject, month, usage.month.seconds]);
      if (usage.month.unsupported) {
        await tx.query("INSERT INTO relay_asr_subjects(subject,blocked) VALUES($1::text,1) ON CONFLICT(subject) DO UPDATE SET blocked=1", [subject]);
      }
    });
    // After the transaction, so an alert never reports a reconciliation that rolled back.
    if (usage.month.unsupported) this.alert("ALERT speech_subject_blocked reason=tagged_non_speech_usage");
    else if (usage.day.unsupported) this.alert("ALERT speech_project_non_speech_usage scope=project action=investigate (operator stop: DEPLOY.md)");
  }
  async maintenance(): Promise<void> {
    const cutoff = this.now() - 400 * 86_400_000;
    await this.db.transaction(async tx => {
      await tx.query("DELETE FROM relay_asr_sessions WHERE expires<$1::bigint", [cutoff]);
      await tx.query("DELETE FROM relay_asr_months WHERE month<$1::text", [new Date(cutoff).toISOString().slice(0, 7)]);
      await tx.query("DELETE FROM relay_asr_provider_usage WHERE scope<>'blocked' AND period<$1::text", [new Date(cutoff).toISOString().slice(0, 7)]);
    });
  }
  async reserve(subject: string, device: string, streams = 1, capMicros?: number): Promise<AsrTicket> {
    if (streams !== 1 && streams !== 2) throw new Error("invalid stream count");
    const cap = this.capFor(capMicros);
    return await this.db.transaction(async tx => {
      const now = this.now();
      const day = new Date(now).toISOString().slice(0, 10);
      const month = day.slice(0, 7);
      await tx.query("INSERT INTO relay_asr_subjects(subject,blocked) VALUES($1::text,0) ON CONFLICT(subject) DO NOTHING", [subject]);
      const flags = await tx.query("SELECT blocked FROM relay_asr_subjects WHERE subject=$1::text FOR UPDATE", [subject]);
      if (Number(flags[0]?.blocked) !== 0) throw new AsrRefused("blocked");
      const recent = await tx.query("SELECT id FROM relay_asr_sessions WHERE subject=$1::text AND device=$2::text AND last_mint>$3::bigint", [subject, device, now - ASR_MINT_PACING_MS]);
      if (recent.length) throw new AsrRefused("rate_limited");
      const active = await tx.query("SELECT streams FROM relay_asr_sessions WHERE subject=$1::text AND finished=0 AND expires>$2::bigint", [subject, now]);
      if (active.reduce((n, row) => n + Number(row.streams), 0) + streams > 2) throw new AsrRefused("concurrency_limited");
      const totals = await tx.query("SELECT seconds FROM relay_asr_months WHERE subject=$1::text AND month=$2::text", [subject, month]);
      const provider = await tx.query("SELECT seconds FROM relay_asr_provider_usage WHERE scope=$1::text AND period=$2::text", [subject, month]);
      const stopped = await tx.query("SELECT seconds FROM relay_asr_provider_usage WHERE scope='blocked' AND period='global'");
      if (stopped.length) throw new AsrRefused("blocked");
      const used = Math.max(Number(totals[0]?.seconds ?? 0), Number(provider[0]?.seconds ?? 0));
      if (!Number.isSafeInteger(used) || used < 0) throw new Error("invalid speech usage");
      const seconds = Math.min(ASR_SESSION_SECONDS, Math.floor((ASR_MONTH_SECONDS - used) / streams));
      if (seconds <= 0) throw new AsrRefused("monthly_cap");
      await tx.query("INSERT INTO relay_asr_months(subject,month,seconds) VALUES($1::text,$2::text,$3::bigint) ON CONFLICT(subject,month) DO UPDATE SET seconds=excluded.seconds", [subject, month, used + seconds * streams]);
      const micros = Math.ceil(seconds * streams * this.hourlyReserveMicros / 3600);
      if (!Number.isSafeInteger(micros) || micros <= 0 || !Number.isSafeInteger(cap) || cap <= 0) throw new Error("invalid speech budget");
      const booked = await tx.query("INSERT INTO relay_spend_days(date,micros) SELECT $1::text,$2::bigint WHERE $2::bigint <= $3::bigint ON CONFLICT(date) DO UPDATE SET micros=relay_spend_days.micros+excluded.micros WHERE relay_spend_days.micros+excluded.micros <= $3::bigint RETURNING micros", [day, micros, cap]);
      if (!booked.length) throw new AsrRefused("spend_cap");
      const id = randomUUID();
      // Include the 60-second grant window: a handshake may begin just before JWT expiry.
      await tx.query("INSERT INTO relay_asr_sessions(id,subject,device,month,started,expires,seconds,streams,day,micros,last_mint,finished) VALUES($1::text,$2::text,$3::text,$4::text,$5::bigint,$6::bigint,$7::bigint,$8::integer,$9::text,$10::bigint,$5::bigint,0)", [id, subject, device, month, now, now + (seconds + 60) * 1000, seconds * streams, streams, day, micros]);
      return { id, seconds };
    });
  }
  async refresh(subject: string, device: string, id: string, capMicros?: number): Promise<void> {
    const cap = this.capFor(capMicros);
    await this.db.transaction(async tx => {
      const now = this.now();
      const stopped = await tx.query("SELECT seconds FROM relay_asr_provider_usage WHERE scope='blocked' AND period='global'");
      if (stopped.length) throw new AsrRefused("blocked");
      const spent = await tx.query("SELECT micros FROM relay_spend_days WHERE date=$1::text", [new Date(now).toISOString().slice(0, 10)]);
      if (Number(spent[0]?.micros ?? 0) >= cap) throw new AsrRefused("spend_cap");
      const monthly = await tx.query("SELECT seconds FROM relay_asr_provider_usage WHERE scope=$1::text AND period=$2::text", [subject, new Date(now).toISOString().slice(0, 7)]);
      if (Number(monthly[0]?.seconds ?? 0) >= ASR_MONTH_SECONDS) throw new AsrRefused("monthly_cap");
      const flags = await tx.query("SELECT blocked FROM relay_asr_subjects WHERE subject=$1::text FOR UPDATE", [subject]);
      if (Number(flags[0]?.blocked) !== 0) throw new AsrRefused("blocked");
      const rows = await tx.query("SELECT * FROM relay_asr_sessions WHERE id=$1::text AND subject=$2::text AND device=$3::text FOR UPDATE", [id, subject, device]);
      const row = rows[0];
      if (!row || Number(row.finished) !== 0 || now >= Number(row.started) + Number(row.seconds) / Number(row.streams) * 1000) throw new AsrRefused("session_expired");
      const recent = await tx.query("SELECT id FROM relay_asr_sessions WHERE subject=$1::text AND device=$2::text AND last_mint>$3::bigint", [subject, device, now - ASR_MINT_PACING_MS]);
      if (recent.length) throw new AsrRefused("rate_limited");
      // Debit pacing before contacting the provider, including ambiguous failures.
      await tx.query("UPDATE relay_asr_sessions SET last_mint=$2::bigint WHERE id=$1::text", [id, now]);
    });
  }
  async checkPacing(subject: string, device: string): Promise<void> {
    const recent = await this.db.query("SELECT id FROM relay_asr_sessions WHERE subject=$1::text AND device=$2::text AND last_mint>$3::bigint LIMIT 1", [subject, device, this.now() - ASR_MINT_PACING_MS]);
    if (recent.length) throw new AsrRefused("rate_limited");
  }
  /** Close a session on SERVER elapsed time, refunding the unused part of its reservation.
   *
   * **Never below the grant window.** A grant JWT opens sockets until it expires
   * (`DEEPGRAM_GRANT_TTL_SECONDS` after its mint), and an open socket outlives both the JWT and
   * this call. So the billed time runs at least to the expiry of the session's LAST mint
   * (`last_mint`: the grant or the latest refresh): a finish sent right after a grant is billed
   * that whole window, not one second. A supported Mac finishes when its meeting or dictation
   * ends, normally long after its last mint, so this changes little for it; a client that
   * finishes at once to be refunded keeps paying for every socket its JWT could still open. A
   * socket kept open past that point is beyond what the relay can see until the speech socket is
   * proxied; the provider-usage floors read before every grant and refresh (`reconcile`) and the
   * dedicated project's hard budget bound it (DEPLOY.md, "Company-funded speech"). The
   * concurrency slot is freed as before: one JWT can open any number of sockets, so holding the
   * slot would bound nothing against such a client and would only block a supported Mac's next
   * session. */
  async finish(subject: string, device: string, id: string): Promise<void> {
    await this.db.transaction(async tx => {
      await tx.query("SELECT blocked FROM relay_asr_subjects WHERE subject=$1::text FOR UPDATE", [subject]);
      const rows = await tx.query("SELECT * FROM relay_asr_sessions WHERE id=$1::text AND subject=$2::text AND device=$3::text FOR UPDATE", [id, subject, device]);
      const row = rows[0];
      if (!row || Number(row.finished) !== 0) return;
      const reserved = Number(row.seconds);
      const usableUntil = Math.max(this.now(), Number(row.last_mint) + DEEPGRAM_GRANT_TTL_SECONDS * 1000);
      const billed = Math.min(reserved, Math.max(1, Math.ceil((usableUntil - Number(row.started)) / 1000) * Number(row.streams)));
      await tx.query("UPDATE relay_asr_months SET seconds=seconds-$3::bigint WHERE subject=$1::text AND month=$2::text", [subject, String(row.month), reserved - billed]);
      const refund = Math.floor(Number(row.micros) * (reserved - billed) / reserved);
      await tx.query("UPDATE relay_spend_days SET micros=CASE WHEN micros<$2::bigint THEN 0 ELSE micros-$2::bigint END WHERE date=$1::text", [String(row.day), refund]);
      await tx.query("UPDATE relay_asr_sessions SET finished=1 WHERE id=$1::text", [id]);
    });
  }
}
