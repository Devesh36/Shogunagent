/**
 * Operational metrics for the relay (issue #29 FR-SVC-06).
 *
 * What is measured, per route template (`/v1/batch`, `/v1/batch/:id`, …): request rate, p95
 * latency, 429 rate and 5xx rate over a rolling five-minute window, plus lifetime totals. For the
 * Batch lane: reservation outcomes, and today's per-plan cap consumption. For the spend breaker:
 * today's booked spend against the cap.
 *
 * What is never measured: bodies, headers, batch ids, licence ids or device ids. Licences are
 * counted per plan through a hash (`logId`) held only in memory for today's UTC date and dropped
 * at the rollover; nothing identifying leaves the process in a snapshot.
 *
 * Scope: in-process. With the shared store each instance — on Workers, each isolate — reports its
 * own traffic; sum rates across instances, and take the per-plan consumption from any one of them
 * as a floor (each sees only the reservations it made). The ledger, not this window, is the
 * authority on spend.
 */
import { logId } from "./logging.js";
import { OpsMetrics } from "./observability.js";

/** Histogram bucket upper bounds in ms. p95 is reported as the bucket bound it falls in — coarse,
 * but it answers "is it over 2 s" (the alert) without keeping every sample. */
const BOUNDS_MS = [5, 10, 25, 50, 100, 250, 500, 1000, 2000, 5000, 10_000, 30_000, Infinity] as const;
const WINDOW_MINUTES = 5;

interface MinuteBucket {
  minute: number;
  count: number;
  s429: number;
  s5xx: number;
  hist: number[];
}

interface RouteStats {
  window: MinuteBucket[];
  total: number;
  total429: number;
  total5xx: number;
}

export type ReservationOutcome = "ok" | "capped" | "spend_capped" | "license_spend_capped" | "unavailable" | "unknown";

interface PlanDay {
  /** Hashed licence → chunks reserved today by this instance. */
  licences: Map<string, number>;
  outcomes: Record<ReservationOutcome, number>;
}

export interface RouteSnapshot {
  rps: number;
  p95_ms: number | null;
  rate_429: number;
  rate_5xx: number;
  window_requests: number;
  total_requests: number;
  total_429: number;
  total_5xx: number;
}

export interface PlanSnapshot {
  reservations: Record<ReservationOutcome, number>;
  active_licences: number;
  chunks_reserved: number;
  /** Σ chunks ÷ (active licences × per-licence cap): how much of the plan's aggregate daily
   * allowance its active licences have used. Alert threshold 0.8. */
  cap_consumption: number;
  licences_over_80pct: number;
}

export interface MetricsSnapshot {
  window_seconds: number;
  routes: Record<string, RouteSnapshot>;
  batch: { date: string; daily_chunk_cap: number; plans: Record<string, PlanSnapshot> };
  spend?: { date: string; spent_micros: number; cap_micros: number; consumption: number };
}

function emptyOutcomes(): Record<ReservationOutcome, number> {
  return { ok: 0, capped: 0, spend_capped: 0, license_spend_capped: 0, unavailable: 0, unknown: 0 };
}

export class RelayMetrics {
  private readonly routes = new Map<string, RouteStats>();
  private date = "";
  private plans = new Map<string, PlanDay>();
  /** E37: per-label-set counters and latency histograms (route, method, status, `Mcp-Method`,
   * `Mcp-Name`), rendered as Prometheus text. Lives here so it is shared exactly like this window
   * (one per Node process / Worker isolate). */
  readonly ops = new OpsMetrics();

  /** `planCaps` overrides the per-licence cap for a plan that has its own (the trial's). */
  constructor(
    private readonly dailyChunkCap: number,
    private readonly planCaps: Readonly<Record<string, number>> = {},
  ) {}

  /** One finished request. `route` must already be a template, never a raw path. */
  observe(route: string, status: number, ms: number, nowMs: number): void {
    let stats = this.routes.get(route);
    if (!stats) {
      stats = { window: [], total: 0, total429: 0, total5xx: 0 };
      this.routes.set(route, stats);
    }
    const minute = Math.floor(nowMs / 60_000);
    let bucket = stats.window[stats.window.length - 1];
    if (!bucket || bucket.minute !== minute) {
      bucket = { minute, count: 0, s429: 0, s5xx: 0, hist: BOUNDS_MS.map(() => 0) };
      stats.window.push(bucket);
      while (stats.window.length > 0 && stats.window[0]!.minute <= minute - WINDOW_MINUTES) stats.window.shift();
    }
    bucket.count += 1;
    stats.total += 1;
    if (status === 429) { bucket.s429 += 1; stats.total429 += 1; }
    if (status >= 500) { bucket.s5xx += 1; stats.total5xx += 1; }
    const i = BOUNDS_MS.findIndex((b) => ms <= b);
    bucket.hist[i] = (bucket.hist[i] ?? 0) + 1;
  }

  /** One reservation attempt on the Batch lane. Only `ok` adds chunks. */
  reservation(date: string, plan: string, licenseId: string, outcome: ReservationOutcome, chunks: number): void {
    if (date !== this.date) {
      this.date = date;
      this.plans = new Map();
    }
    let day = this.plans.get(plan);
    if (!day) {
      day = { licences: new Map(), outcomes: emptyOutcomes() };
      this.plans.set(plan, day);
    }
    day.outcomes[outcome] += 1;
    if (outcome === "ok") {
      const id = logId(licenseId);
      day.licences.set(id, (day.licences.get(id) ?? 0) + chunks);
    }
  }

  snapshot(nowMs: number, spend?: { date: string; spentMicros: number; capMicros: number }): MetricsSnapshot {
    const minute = Math.floor(nowMs / 60_000);
    const routes: Record<string, RouteSnapshot> = {};
    for (const [route, stats] of this.routes) {
      const live = stats.window.filter((b) => b.minute > minute - WINDOW_MINUTES);
      const count = live.reduce((n, b) => n + b.count, 0);
      const hist = BOUNDS_MS.map((_, i) => live.reduce((n, b) => n + (b.hist[i] ?? 0), 0));
      routes[route] = {
        rps: count / (WINDOW_MINUTES * 60),
        p95_ms: percentile(hist, 0.95),
        rate_429: count ? live.reduce((n, b) => n + b.s429, 0) / count : 0,
        rate_5xx: count ? live.reduce((n, b) => n + b.s5xx, 0) / count : 0,
        window_requests: count,
        total_requests: stats.total,
        total_429: stats.total429,
        total_5xx: stats.total5xx,
      };
    }
    const plans: Record<string, PlanSnapshot> = {};
    for (const [plan, day] of this.plans) {
      const values = [...day.licences.values()];
      const chunks = values.reduce((a, b) => a + b, 0);
      const cap = this.planCaps[plan] ?? this.dailyChunkCap;
      const allowance = values.length * cap;
      plans[plan] = {
        reservations: { ...day.outcomes },
        active_licences: values.length,
        chunks_reserved: chunks,
        cap_consumption: allowance > 0 ? chunks / allowance : 0,
        licences_over_80pct: values.filter((v) => cap > 0 && v > 0.8 * cap).length,
      };
    }
    const snap: MetricsSnapshot = {
      window_seconds: WINDOW_MINUTES * 60,
      routes,
      batch: { date: this.date, daily_chunk_cap: this.dailyChunkCap, plans },
    };
    if (spend) {
      snap.spend = {
        date: spend.date,
        spent_micros: spend.spentMicros,
        cap_micros: spend.capMicros,
        // A zero cap means the breaker is closed for everything: report it as fully consumed.
        consumption: spend.capMicros > 0 ? spend.spentMicros / spend.capMicros : 1,
      };
    }
    return snap;
  }
}

/** The upper bound (ms) of the bucket holding quantile `q`; null with no samples. Samples past
 * the last finite bound report 30 001 ("over 30 s") so the value stays a finite JSON number. */
function percentile(hist: number[], q: number): number | null {
  const total = hist.reduce((a, b) => a + b, 0);
  if (total === 0) return null;
  const target = Math.ceil(total * q);
  let seen = 0;
  for (let i = 0; i < hist.length; i++) {
    seen += hist[i] ?? 0;
    if (seen >= target) {
      const bound = BOUNDS_MS[i]!;
      return Number.isFinite(bound) ? bound : 30_001;
    }
  }
  return null;
}
