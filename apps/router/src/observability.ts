/**
 * Server operations telemetry for the router (plan E37), the router-side twin of
 * apps/api/src/observability.ts: body-free request metrics in the Prometheus text format and an
 * opt-in OpenTelemetry (OTLP/HTTP JSON) span export.
 *
 * Every metric series and span carries exactly the labels in `METRIC_LABELS` /
 * `SPAN_ATTRIBUTES`, with values from closed sets: the route template (`logging.ts`), the HTTP
 * method, the status code, and the `Mcp-Method` / `Mcp-Name` headers only when they name a known
 * MCP method or one of the three verbs (`none` / `other` otherwise). No body, query, input,
 * result, tool argument, header value or key reaches a label (invariant 2);
 * `scripts/check-log-hygiene.py` holds both twins to the same allowlist.
 *
 * This module does not call `fetch` and reads no secret: the OTLP post and the `/metrics` bearer
 * check live in src/transport.ts, the router's only egress and only reader of the secret map
 * (invariant 7). Off by default: `/metrics` answers 404 without `ROUTER_METRICS_TOKEN`, and no
 * span is recorded without `OTEL_EXPORTER_OTLP_ENDPOINT` (src/env.ts).
 */
import { randomBytes } from "node:crypto";
import type { MiddlewareHandler } from "hono";

import { VERB_SCHEMAS } from "./verbs.js";

/** Same names and order as apps/api/src/observability.ts. */
export const METRIC_LABELS = ["route", "method", "status", "mcp_method", "mcp_name"] as const;
export const SPAN_ATTRIBUTES = ["http.route", "http.request.method", "http.response.status_code", "mcp.method.name", "mcp.tool.name"] as const;

export type MetricLabel = (typeof METRIC_LABELS)[number];
export type RequestLabels = Readonly<Record<MetricLabel, string>>;

const HTTP_METHODS: ReadonlySet<string> = new Set(["GET", "POST", "HEAD", "OPTIONS", "PUT", "PATCH", "DELETE"]);

export const MCP_METHODS: ReadonlySet<string> = new Set([
  "initialize", "ping", "tools/list", "tools/call", "resources/list", "resources/read", "resources/templates/list",
  "resources/subscribe", "resources/unsubscribe", "prompts/list", "prompts/get", "completion/complete",
  "logging/setLevel", "server/discover", "notifications/initialized", "notifications/cancelled", "notifications/progress",
]);

/** The router's tools: its three verbs. Catalog tool ids are not labels (unbounded, caller-chosen). */
export const KNOWN_TOOL_NAMES: ReadonlySet<string> = new Set(Object.keys(VERB_SCHEMAS));

export function methodLabel(method: string): string {
  return HTTP_METHODS.has(method) ? method : "OTHER";
}

export function statusLabel(status: number): string {
  return Number.isInteger(status) && status >= 100 && status <= 599 ? String(status) : "other";
}

export function mcpMethodLabel(raw: string | null | undefined): string {
  if (raw === null || raw === undefined || raw.trim() === "") return "none";
  return MCP_METHODS.has(raw.trim()) ? raw.trim() : "other";
}

export function mcpNameLabel(raw: string | null | undefined, known: ReadonlySet<string> = KNOWN_TOOL_NAMES): string {
  if (raw === null || raw === undefined || raw.trim() === "") return "none";
  return known.has(raw.trim()) ? raw.trim() : "other";
}

export const DURATION_BOUNDS_MS = [5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10_000, 30_000] as const;
export const MAX_SERIES = 2000;

interface Series {
  labels: RequestLabels;
  count: number;
  sumMs: number;
  buckets: number[];
}

function escapeLabelValue(v: string): string {
  return v.replace(/\\/g, "\\\\").replace(/"/g, '\\"').replace(/\n/g, "\\n");
}

function labelText(labels: RequestLabels, extra?: string): string {
  const parts = METRIC_LABELS.map((k) => `${k}="${escapeLabelValue(labels[k])}"`);
  if (extra) parts.push(extra);
  return `{${parts.join(",")}}`;
}

export class OpsMetrics {
  private readonly series = new Map<string, Series>();
  private overflow = 0;
  spansDropped = 0;
  exportFailures = 0;

  constructor(private readonly prefix = "shogun_router") {}

  observe(labels: RequestLabels, ms: number): void {
    const key = METRIC_LABELS.map((k) => labels[k]).join("\u0000");
    let s = this.series.get(key);
    if (!s) {
      if (this.series.size >= MAX_SERIES) {
        this.overflow += 1;
        return;
      }
      s = { labels: { ...labels }, count: 0, sumMs: 0, buckets: DURATION_BOUNDS_MS.map(() => 0) };
      this.series.set(key, s);
    }
    const value = Number.isFinite(ms) && ms >= 0 ? ms : 0;
    s.count += 1;
    s.sumMs += value;
    for (let i = 0; i < DURATION_BOUNDS_MS.length; i++) if (value <= DURATION_BOUNDS_MS[i]!) s.buckets[i] = (s.buckets[i] ?? 0) + 1;
  }

  get seriesCount(): number {
    return this.series.size;
  }

  render(): string {
    const p = this.prefix;
    const out: string[] = [];
    out.push(`# HELP ${p}_http_requests_total Requests by route template, method, status and MCP method/tool. No bodies, no ids.`);
    out.push(`# TYPE ${p}_http_requests_total counter`);
    for (const s of this.series.values()) out.push(`${p}_http_requests_total${labelText(s.labels)} ${s.count}`);
    out.push(`# HELP ${p}_http_request_duration_ms Request latency in milliseconds, same labels.`);
    out.push(`# TYPE ${p}_http_request_duration_ms histogram`);
    for (const s of this.series.values()) {
      DURATION_BOUNDS_MS.forEach((bound, i) => out.push(`${p}_http_request_duration_ms_bucket${labelText(s.labels, `le="${bound}"`)} ${s.buckets[i] ?? 0}`));
      out.push(`${p}_http_request_duration_ms_bucket${labelText(s.labels, 'le="+Inf"')} ${s.count}`);
      out.push(`${p}_http_request_duration_ms_sum${labelText(s.labels)} ${Math.round(s.sumMs * 1000) / 1000}`);
      out.push(`${p}_http_request_duration_ms_count${labelText(s.labels)} ${s.count}`);
    }
    out.push(`# HELP ${p}_metrics_series_overflow_total Observations not recorded because the series limit was reached.`);
    out.push(`# TYPE ${p}_metrics_series_overflow_total counter`);
    out.push(`${p}_metrics_series_overflow_total ${this.overflow}`);
    out.push(`# HELP ${p}_otlp_spans_dropped_total Spans dropped (buffer full or export failed).`);
    out.push(`# TYPE ${p}_otlp_spans_dropped_total counter`);
    out.push(`${p}_otlp_spans_dropped_total ${this.spansDropped}`);
    out.push(`# HELP ${p}_otlp_export_failures_total OTLP export requests that failed.`);
    out.push(`# TYPE ${p}_otlp_export_failures_total counter`);
    out.push(`${p}_otlp_export_failures_total ${this.exportFailures}`);
    return out.join("\n") + "\n";
  }
}

/** The non-secret OTLP settings. The collector headers are read by src/transport.ts at send time. */
export interface OtlpTraceSettings {
  tracesUrl: string;
  serviceName: string;
  sampleRatio: number;
}

export class OtlpConfigError extends Error {}

function isLoopbackHost(hostname: string): boolean {
  return hostname === "localhost" || hostname === "127.0.0.1" || hostname === "[::1]" || hostname === "::1";
}

/** The OTLP settings from the router's vars, or `undefined` when export is off (the default).
 * Same variables and rules as apps/api (`OTEL_EXPORTER_OTLP_ENDPOINT`, …_TRACES_ENDPOINT,
 * `OTEL_SERVICE_NAME`, `OTEL_TRACES_SAMPLER_ARG`). Messages name a variable, never its value. */
export function otlpSettingsFromEnv(env: Readonly<Record<string, string | undefined>>, defaultService = "shogunagent-router"): OtlpTraceSettings | undefined {
  const tracesEndpoint = env.OTEL_EXPORTER_OTLP_TRACES_ENDPOINT?.trim();
  const base = env.OTEL_EXPORTER_OTLP_ENDPOINT?.trim();
  if (!tracesEndpoint && !base) return undefined;
  const name = tracesEndpoint ? "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT" : "OTEL_EXPORTER_OTLP_ENDPOINT";
  let url: URL;
  try {
    url = tracesEndpoint ? new URL(tracesEndpoint) : new URL(`${base!.replace(/\/+$/, "")}/v1/traces`);
  } catch {
    throw new OtlpConfigError(`invalid ${name}: not a URL`);
  }
  if (url.protocol !== "https:" && !(url.protocol === "http:" && isLoopbackHost(url.hostname))) {
    throw new OtlpConfigError(`invalid ${name}: https is required (http only for a loopback collector)`);
  }
  if (url.username || url.password) throw new OtlpConfigError(`invalid ${name}: put credentials in OTEL_EXPORTER_OTLP_HEADERS, not the URL`);
  const serviceName = env.OTEL_SERVICE_NAME?.trim() || defaultService;
  if (!/^[A-Za-z0-9._-]{1,64}$/.test(serviceName)) throw new OtlpConfigError("invalid OTEL_SERVICE_NAME: 1-64 of A-Z a-z 0-9 . _ -");
  const ratioRaw = env.OTEL_TRACES_SAMPLER_ARG?.trim();
  const sampleRatio = ratioRaw ? Number(ratioRaw) : 1;
  if (!Number.isFinite(sampleRatio) || sampleRatio < 0 || sampleRatio > 1) throw new OtlpConfigError("invalid OTEL_TRACES_SAMPLER_ARG: a number from 0 to 1");
  return { tracesUrl: url.toString(), serviceName, sampleRatio };
}

/** Posts one OTLP JSON body (src/transport.ts `createOtlpPoster`). */
export type OtlpPost = (body: string) => Promise<{ status: number }>;

interface PendingSpan {
  traceId: string;
  spanId: string;
  name: string;
  startNs: string;
  endNs: string;
  labels: RequestLabels;
  error: boolean;
}

export const MAX_PENDING_SPANS = 2048;
export const MAX_SPANS_PER_EXPORT = 512;
export const FLUSH_INTERVAL_MS = 5_000;

function nanos(ms: number): string {
  return (BigInt(Math.max(0, Math.round(ms * 1000))) * 1000n).toString();
}

export class OtlpTraceExporter {
  private pending: PendingSpan[] = [];
  private lastFlushMs = 0;
  private inFlight: Promise<void> | undefined;

  constructor(
    private readonly settings: Pick<OtlpTraceSettings, "serviceName" | "sampleRatio">,
    private readonly post: OtlpPost,
    private readonly counters: Pick<OpsMetrics, "spansDropped" | "exportFailures">,
    private readonly random: () => number = Math.random,
  ) {}

  record(labels: RequestLabels, startMs: number, durationMs: number): void {
    if (this.settings.sampleRatio < 1 && this.random() >= this.settings.sampleRatio) return;
    if (this.pending.length >= MAX_PENDING_SPANS) {
      this.counters.spansDropped += 1;
      return;
    }
    this.pending.push({
      traceId: randomBytes(16).toString("hex"),
      spanId: randomBytes(8).toString("hex"),
      name: `${labels.method} ${labels.route}`,
      startNs: nanos(startMs),
      endNs: nanos(startMs + Math.max(0, durationMs)),
      labels,
      error: Number(labels.status) >= 500,
    });
  }

  get pendingCount(): number {
    return this.pending.length;
  }

  due(nowMs: number): boolean {
    return this.pending.length > 0 && this.inFlight === undefined && (this.pending.length >= MAX_SPANS_PER_EXPORT || nowMs - this.lastFlushMs >= FLUSH_INTERVAL_MS);
  }

  body(spans: readonly PendingSpan[]): string {
    const attr = (key: string, value: string | number) =>
      typeof value === "number" ? { key, value: { intValue: String(value) } } : { key, value: { stringValue: value } };
    return JSON.stringify({
      resourceSpans: [{
        resource: { attributes: [attr("service.name", this.settings.serviceName)] },
        scopeSpans: [{
          scope: { name: "shogunagent.ops" },
          spans: spans.map((s) => ({
            traceId: s.traceId,
            spanId: s.spanId,
            name: s.name,
            kind: 2,
            startTimeUnixNano: s.startNs,
            endTimeUnixNano: s.endNs,
            attributes: [
              attr(SPAN_ATTRIBUTES[0], s.labels.route),
              attr(SPAN_ATTRIBUTES[1], s.labels.method),
              attr(SPAN_ATTRIBUTES[2], Number(s.labels.status) || 0),
              attr(SPAN_ATTRIBUTES[3], s.labels.mcp_method),
              attr(SPAN_ATTRIBUTES[4], s.labels.mcp_name),
            ],
            status: { code: s.error ? 2 : 0 },
          })),
        }],
      }],
    });
  }

  flush(nowMs: number = Date.now()): Promise<void> {
    if (this.inFlight) return this.inFlight;
    this.lastFlushMs = nowMs;
    this.inFlight = (async () => {
      try {
        while (this.pending.length > 0) {
          const batch = this.pending.splice(0, MAX_SPANS_PER_EXPORT);
          try {
            const { status } = await this.post(this.body(batch));
            if (status < 200 || status >= 300) {
              this.counters.exportFailures += 1;
              this.counters.spansDropped += batch.length;
            }
          } catch {
            this.counters.exportFailures += 1;
            this.counters.spansDropped += batch.length;
          }
        }
      } finally {
        this.inFlight = undefined;
      }
    })();
    return this.inFlight;
  }
}

export interface RequestFacts {
  method: string;
  url: string;
  status: number;
  mcpMethod: string | null | undefined;
  mcpName: string | null | undefined;
}

export function requestLabels(facts: RequestFacts, routeTemplate: (pathname: string) => string, knownTools: ReadonlySet<string> = KNOWN_TOOL_NAMES): RequestLabels {
  return {
    route: routeTemplate(new URL(facts.url).pathname),
    method: methodLabel(facts.method),
    status: statusLabel(facts.status),
    mcp_method: mcpMethodLabel(facts.mcpMethod),
    mcp_name: mcpNameLabel(facts.mcpName, knownTools),
  };
}

/** What the router's `/metrics` and the middleware need. `authorize` comes from src/transport.ts
 * (`createMetricsAuthorizer`), which alone reads `ROUTER_METRICS_TOKEN`. */
export interface RouterOps {
  metrics: OpsMetrics;
  exporter?: OtlpTraceExporter;
  authorize?: (authorization: string | undefined) => "ok" | "unauthorized" | "disabled";
}

export function opsTelemetry(deps: { metrics: OpsMetrics; exporter?: OtlpTraceExporter; routeTemplate: (pathname: string) => string; now?: () => number }): MiddlewareHandler {
  const now = deps.now ?? (() => Date.now());
  return async (c, next) => {
    const startWall = now();
    const start = performance.now();
    try {
      await next();
    } finally {
      const ms = performance.now() - start;
      // The router's handlers set `Mcp-Method` / `Mcp-Name` on the response from the parsed
      // request; the request's own headers win when present. Both pass the closed sets.
      const labels = requestLabels(
        {
          method: c.req.method,
          url: c.req.url,
          status: c.res.status,
          mcpMethod: c.req.header("mcp-method") ?? c.res.headers.get("mcp-method"),
          mcpName: c.req.header("mcp-name") ?? c.res.headers.get("mcp-name"),
        },
        deps.routeTemplate,
      );
      deps.metrics.observe(labels, ms);
      const exporter = deps.exporter;
      if (exporter) {
        exporter.record(labels, startWall, ms);
        if (exporter.due(now())) {
          let waitUntil: ((p: Promise<unknown>) => void) | undefined;
          try {
            const ctx = c.executionCtx;
            waitUntil = (p) => ctx.waitUntil(p);
          } catch {
            waitUntil = undefined;
          }
          if (waitUntil) waitUntil(exporter.flush(now()));
        }
      }
    }
  };
}
