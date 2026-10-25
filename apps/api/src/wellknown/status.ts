/**
 * `/.well-known/status.json` and `/health` (E10-T7): live p50 / p95, error rate and incident
 * history, so a health-weighted selection loop (README §11 principle 6) can rank this service
 * on measurements it can repeat.
 *
 * The window is in-process: a five-minute ring of one-minute buckets over the agent routes
 * (`/mcp` and `/v1/*` of the agent API, observed by `observe()` in the agent app). Each bucket
 * keeps its latencies (capped; past the cap the request is still counted but its latency is
 * not sampled) and status counts. Nothing identifying is stored: no path beyond the template,
 * no key, no body. On Workers each isolate reports its own traffic, like `metrics.ts`.
 *
 * Incidents come from `apps/api/incidents.json` (committed; the operator edits it and deploys).
 * The list is the public incident history the README promises; an empty array is the honest
 * default, not a placeholder.
 */
import { wellknownLinks, type WellknownContext } from "./identity.js";

export const WINDOW_MS = 5 * 60_000;
const BUCKET_MS = 60_000;
const MAX_SAMPLES_PER_BUCKET = 20_000;
/** Fewer requests than this and the window says nothing about health (`status: "ok"`, nulls). */
export const MIN_REQUESTS_FOR_JUDGEMENT = 20;
export const DEGRADED_ERROR_RATE = 0.05;
export const DEGRADED_P95_MS = 2_000;

interface Bucket {
  minute: number;
  count: number;
  s4xx: number;
  s5xx: number;
  latencies: number[];
}

export interface WindowSnapshot {
  window_seconds: number;
  requests: number;
  rps: number;
  p50_ms: number | null;
  p95_ms: number | null;
  error_rate: number;
  rate_4xx: number;
  sampled: number;
}

export class RollingWindow {
  private buckets: Bucket[] = [];

  constructor(private readonly windowMs = WINDOW_MS) {}

  /** Record one request. `status` is the HTTP status; `latencyMs` wall time in the handler. */
  observe(status: number, latencyMs: number, nowMs: number): void {
    const minute = Math.floor(nowMs / BUCKET_MS);
    let b = this.buckets[this.buckets.length - 1];
    if (!b || b.minute !== minute) {
      b = { minute, count: 0, s4xx: 0, s5xx: 0, latencies: [] };
      this.buckets.push(b);
      this.evict(nowMs);
    }
    b.count += 1;
    if (status >= 500) b.s5xx += 1;
    else if (status >= 400) b.s4xx += 1;
    if (b.latencies.length < MAX_SAMPLES_PER_BUCKET) b.latencies.push(Math.max(0, latencyMs));
  }

  private evict(nowMs: number): void {
    const oldest = Math.floor((nowMs - this.windowMs) / BUCKET_MS) + 1;
    while (this.buckets.length > 0 && (this.buckets[0]?.minute ?? oldest) < oldest) this.buckets.shift();
  }

  snapshot(nowMs: number): WindowSnapshot {
    this.evict(nowMs);
    let requests = 0, s4xx = 0, s5xx = 0;
    const samples: number[] = [];
    for (const b of this.buckets) {
      requests += b.count;
      s4xx += b.s4xx;
      s5xx += b.s5xx;
      for (const l of b.latencies) samples.push(l);
    }
    samples.sort((a, b) => a - b);
    const pct = (p: number): number | null => (samples.length === 0 ? null : samples[Math.min(samples.length - 1, Math.max(0, Math.ceil(p * samples.length) - 1))] ?? null);
    return {
      window_seconds: Math.round(this.windowMs / 1000),
      requests,
      rps: Math.round((requests / (this.windowMs / 1000)) * 1000) / 1000,
      p50_ms: pct(0.5),
      p95_ms: pct(0.95),
      error_rate: requests === 0 ? 0 : Math.round((s5xx / requests) * 10_000) / 10_000,
      rate_4xx: requests === 0 ? 0 : Math.round((s4xx / requests) * 10_000) / 10_000,
      sampled: samples.length,
    };
  }
}

export type IncidentSeverity = "minor" | "major";

export interface Incident {
  id: string;
  title: string;
  severity: IncidentSeverity;
  started_at: string;
  resolved_at: string | null;
  affected: string[];
  updates: { at: string; text: string }[];
}

const ISO_RE = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z$/;

/** Validate `incidents.json`. Returns the problems, empty when valid. */
export function incidentProblems(raw: unknown): string[] {
  if (!Array.isArray(raw)) return ["incidents.json must be a JSON array"];
  const problems: string[] = [];
  const ids = new Set<string>();
  raw.forEach((inc, i) => {
    const where = `incidents[${i}]`;
    if (typeof inc !== "object" || inc === null) { problems.push(`${where} must be an object`); return; }
    const r = inc as Record<string, unknown>;
    if (typeof r.id !== "string" || !/^inc_[A-Za-z0-9_-]+$/.test(r.id)) problems.push(`${where}.id must be inc_<slug>`);
    else if (ids.has(r.id)) problems.push(`${where}.id ${r.id} is listed twice`);
    else ids.add(r.id);
    if (typeof r.title !== "string" || r.title.length === 0) problems.push(`${where}.title is required`);
    if (r.severity !== "minor" && r.severity !== "major") problems.push(`${where}.severity must be minor or major`);
    if (typeof r.started_at !== "string" || !ISO_RE.test(r.started_at)) problems.push(`${where}.started_at must be an ISO-8601 UTC instant`);
    if (!(r.resolved_at === null || (typeof r.resolved_at === "string" && ISO_RE.test(r.resolved_at)))) problems.push(`${where}.resolved_at must be null or an ISO-8601 UTC instant`);
    if (!Array.isArray(r.affected) || !r.affected.every((a) => typeof a === "string")) problems.push(`${where}.affected must be an array of route templates`);
    if (!Array.isArray(r.updates) || !r.updates.every((u) => typeof u === "object" && u !== null && typeof (u as Record<string, unknown>).at === "string" && typeof (u as Record<string, unknown>).text === "string")) {
      problems.push(`${where}.updates must be an array of { at, text }`);
    }
  });
  return problems;
}

export type ServiceStatus = "ok" | "degraded" | "major_outage";

export interface StatusDeps {
  window: RollingWindow;
  incidents: Incident[];
  now: () => Date;
  ctx: WellknownContext;
}

export function serviceStatus(w: WindowSnapshot, incidents: Incident[]): ServiceStatus {
  if (incidents.some((i) => i.resolved_at === null && i.severity === "major")) return "major_outage";
  if (incidents.some((i) => i.resolved_at === null)) return "degraded";
  if (w.requests >= MIN_REQUESTS_FOR_JUDGEMENT && (w.error_rate > DEGRADED_ERROR_RATE || (w.p95_ms !== null && w.p95_ms > DEGRADED_P95_MS))) return "degraded";
  return "ok";
}

/** Open incidents first, then the most recent; nothing older than 90 days unless still open. */
export function visibleIncidents(incidents: Incident[], nowMs: number): Incident[] {
  const cutoff = nowMs - 90 * 86_400_000;
  return incidents
    .filter((i) => i.resolved_at === null || Date.parse(i.started_at) >= cutoff)
    .sort((a, b) => (a.resolved_at === null) === (b.resolved_at === null) ? Date.parse(b.started_at) - Date.parse(a.started_at) : a.resolved_at === null ? -1 : 1);
}

/** The body of `/.well-known/status.json`. */
export function statusDocument(deps: StatusDeps): Record<string, unknown> {
  const nowMs = deps.now().getTime();
  const w = deps.window.snapshot(nowMs);
  const incidents = visibleIncidents(deps.incidents, nowMs);
  const links = wellknownLinks(deps.ctx);
  return {
    schema: "shogunagent/status/v1",
    status: serviceStatus(w, deps.incidents),
    checked_at: new Date(nowMs).toISOString(),
    window: w,
    thresholds: { min_requests: MIN_REQUESTS_FOR_JUDGEMENT, degraded_error_rate: DEGRADED_ERROR_RATE, degraded_p95_ms: DEGRADED_P95_MS },
    scope: "this instance, agent routes (/mcp and /v1 of the agent API), last five minutes",
    incidents,
    links: { health: links.health, pricing: links.pricing, server_card: links.server_card },
  };
}

/** The body of `/health`: short, cacheable for seconds, enough to rank on. */
export function healthDocument(deps: StatusDeps): { body: Record<string, unknown>; status: number } {
  const nowMs = deps.now().getTime();
  const w = deps.window.snapshot(nowMs);
  const status = serviceStatus(w, deps.incidents);
  return {
    status: status === "major_outage" ? 503 : 200,
    body: {
      ok: status !== "major_outage",
      status,
      checked_at: new Date(nowMs).toISOString(),
      p50_ms: w.p50_ms,
      p95_ms: w.p95_ms,
      error_rate: w.error_rate,
      window_requests: w.requests,
      window_seconds: w.window_seconds,
      open_incidents: deps.incidents.filter((i) => i.resolved_at === null).length,
    },
  };
}
