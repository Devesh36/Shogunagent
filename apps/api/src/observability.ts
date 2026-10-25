/**
 * Server operations telemetry for the gateway (plan E37): body-free request metrics in the
 * Prometheus text format, and an opt-in OpenTelemetry (OTLP/HTTP JSON) trace export.
 *
 * This is the operator's view of the service, not user telemetry: it counts requests, it never
 * describes them. Every metric series and every span carries exactly the labels in
 * `METRIC_LABELS` / `SPAN_ATTRIBUTES`, and every label value comes from a closed set:
 *
 * - `route`       the route template (`logging.ts` `routeTemplate`), never the raw path;
 * - `method`      one of the HTTP methods below, else `OTHER`;
 * - `status`      the numeric status code;
 * - `mcp_method`  the `Mcp-Method` header when it names a known MCP method, `none` when absent,
 *                 `other` otherwise — never the caller's string;
 * - `mcp_name`    the `Mcp-Name` header when it names a known tool (the tool table and the
 *                 pricing table), `none` when absent, `other` otherwise.
 *
 * Nothing here reads a request or response body, a query string, the `Authorization` header or
 * any principal, licence or key id (CLAUDE.md invariant 2). `scripts/check-log-hygiene.py` holds
 * the label and attribute lists to that allowlist and refuses a body or credential read in this
 * file, so a new label cannot quietly carry content.
 *
 * Exposure:
 * - Metrics: `GET /metrics?format=prometheus` (or `Accept: text/plain`), behind the existing
 *   `METRICS_TOKEN` bearer; 404 when the token is unset (app.ts).
 * - Traces: off unless `OTEL_EXPORTER_OTLP_ENDPOINT` (or `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`) is
 *   set (boot.ts `otlpConfigFromEnv`). Spans are buffered in memory (bounded), posted in batches
 *   and dropped, counted, when the collector is unreachable. A failed export never fails a request.
 *
 * Scope: per process (Node) or per isolate (Workers), like `metrics.ts`. Sum across instances.
 *
 * Keep the closed sets and the label names in step with apps/router/src/observability.ts.
 */
import { randomBytes } from "node:crypto";
import type { MiddlewareHandler } from "hono";

import toolTableJson from "../fixtures/tool-table.json" with { type: "json" };
import { FREE_TOOLS, INGEST_TOOLS, RUN_TOOLS } from "./pricing.js";

/** The label names of every metric series. A change here is a change to the guard's allowlist. */
export const METRIC_LABELS = ["route", "method", "status", "mcp_method", "mcp_name"] as const;
/** The attribute keys of every exported span (OpenTelemetry semantic-convention names). */
export const SPAN_ATTRIBUTES = ["http.route", "http.request.method", "http.response.status_code", "mcp.method.name", "mcp.tool.name"] as const;

export type MetricLabel = (typeof METRIC_LABELS)[number];
export type RequestLabels = Readonly<Record<MetricLabel, string>>;

const HTTP_METHODS: ReadonlySet<string> = new Set(["GET", "POST", "HEAD", "OPTIONS", "PUT", "PATCH", "DELETE"]);

/** The MCP methods a label may name (MCP 2025-06-18 and 2026-07-28). Anything else is `other`. */
export const MCP_METHODS: ReadonlySet<string> = new Set([
  "initialize", "ping", "tools/list", "tools/call", "resources/list", "resources/read", "resources/templates/list",
  "resources/subscribe", "resources/unsubscribe", "prompts/list", "prompts/get", "completion/complete",
  "logging/setLevel", "server/discover", "notifications/initialized", "notifications/cancelled", "notifications/progress",
]);

/** Every tool name a label may carry: the core tool table, the pricing table's named tools and
 * the router's verbs. A name outside this set is `other`, whatever its shape. */
export const KNOWN_TOOL_NAMES: ReadonlySet<string> = new Set([
  ...(toolTableJson as { tools: Array<{ name: string }> }).tools.map((t) => t.name),
  ...FREE_TOOLS, ...INGEST_TOOLS, ...RUN_TOOLS,
  "router.discover", "router.inspect", "router.run",
]);

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

/** Histogram bucket upper bounds in ms (the Prometheus `le` values; `+Inf` is implicit). */
export const DURATION_BOUNDS_MS = [5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10_000, 30_000] as const;

/** The most distinct label sets kept. Every value is from a closed set, so this is a backstop
 * against a template bug, not a normal limit; past it, observations are counted as overflow. */
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

/** Request counters and latency histograms per label set, rendered as Prometheus text. */
export class OpsMetrics {
  private readonly series = new Map<string, Series>();
  private overflow = 0;
  /** OTLP export health, filled in by the exporter. */
  spansDropped = 0;
  exportFailures = 0;

  constructor(private readonly prefix = "shogun_gateway") {}

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

  /** The number of distinct label sets (tests, the overflow alert). */
  get seriesCount(): number {
    return this.series.size;
  }

  /** Prometheus text exposition format 0.0.4. */
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

// --- OTLP trace export (opt-in) ---

/** What `otlpConfigFromEnv` reads. The headers are a credential for the collector: held here,
 * sent only to the endpoint, never logged or rendered. */
export interface OtlpConfig {
  /** The full traces URL (`…/v1/traces`). */
  tracesUrl: string;
  headers: Readonly<Record<string, string>>;
  serviceName: string;
  /** 0–1: the share of requests that become spans. */
  sampleRatio: number;
}

/** Thrown with a message that names a variable and never echoes its value. */
export class OtlpConfigError extends Error {}

function isLoopbackHost(hostname: string): boolean {
  return hostname === "localhost" || hostname === "127.0.0.1" || hostname === "[::1]" || hostname === "::1";
}

/**
 * The OTLP settings, or `undefined` when export is off (no endpoint set: the default).
 *
 * - `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` — the full traces URL; or
 * - `OTEL_EXPORTER_OTLP_ENDPOINT`        — the collector base URL (`/v1/traces` is appended).
 *   https only, except a loopback collector (`http://127.0.0.1:4318`).
 * - `OTEL_EXPORTER_OTLP_HEADERS`         — secret, optional: `k=v,k2=v2` (e.g. the collector's auth).
 * - `OTEL_SERVICE_NAME`                  — default `defaultService`.
 * - `OTEL_TRACES_SAMPLER_ARG`            — sample ratio 0–1 (default 1).
 */
export function otlpConfigFromEnv(env: Readonly<Record<string, string | undefined>>, defaultService: string): OtlpConfig | undefined {
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
  const headers: Record<string, string> = {};
  const rawHeaders = env.OTEL_EXPORTER_OTLP_HEADERS?.trim();
  if (rawHeaders) {
    for (const pair of rawHeaders.split(",")) {
      const i = pair.indexOf("=");
      const key = i > 0 ? pair.slice(0, i).trim().toLowerCase() : "";
      const value = i > 0 ? decodeURIComponentSafe(pair.slice(i + 1).trim()) : undefined;
      if (!/^[a-z0-9-]+$/.test(key) || value === undefined || /[\r\n]/.test(value)) throw new OtlpConfigError("invalid OTEL_EXPORTER_OTLP_HEADERS: expected k=v,k2=v2");
      headers[key] = value;
    }
  }
  const serviceName = env.OTEL_SERVICE_NAME?.trim() || defaultService;
  if (!/^[A-Za-z0-9._-]{1,64}$/.test(serviceName)) throw new OtlpConfigError("invalid OTEL_SERVICE_NAME: 1-64 of A-Z a-z 0-9 . _ -");
  const ratioRaw = env.OTEL_TRACES_SAMPLER_ARG?.trim();
  const sampleRatio = ratioRaw ? Number(ratioRaw) : 1;
  if (!Number.isFinite(sampleRatio) || sampleRatio < 0 || sampleRatio > 1) throw new OtlpConfigError("invalid OTEL_TRACES_SAMPLER_ARG: a number from 0 to 1");
  return { tracesUrl: url.toString(), headers, serviceName, sampleRatio };
}

function decodeURIComponentSafe(v: string): string | undefined {
  try {
    return decodeURIComponent(v);
  } catch {
    return undefined;
  }
}

/** Posts one OTLP JSON body. Resolves with the status; rejects on a network failure. */
export type OtlpPost = (body: string) => Promise<{ status: number }>;

/** The default poster: `fetch` to the configured URL with the configured headers, 5 s timeout. */
export function fetchOtlpPost(config: OtlpConfig, doFetch: typeof fetch = fetch): OtlpPost {
  return async (body) => {
    const res = await doFetch(config.tracesUrl, {
      method: "POST",
      headers: { ...config.headers, "content-type": "application/json" },
      body,
      signal: AbortSignal.timeout(5_000),
    });
    // The collector's answer is not read beyond its status.
    await res.body?.cancel().catch(() => undefined);
    return { status: res.status };
  };
}

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

/** Buffers body-free spans and posts them to an OTLP/HTTP collector in batches. */
export class OtlpTraceExporter {
  private pending: PendingSpan[] = [];
  private lastFlushMs = 0;
  private inFlight: Promise<void> | undefined;

  constructor(
    private readonly config: OtlpConfig,
    private readonly post: OtlpPost,
    private readonly counters: Pick<OpsMetrics, "spansDropped" | "exportFailures">,
    private readonly random: () => number = Math.random,
  ) {}

  /** One finished request. `labels` must already be closed-set values. */
  record(labels: RequestLabels, startMs: number, durationMs: number): void {
    if (this.config.sampleRatio < 1 && this.random() >= this.config.sampleRatio) return;
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

  /** Whether a flush is worth scheduling now (a full batch, or the interval passed). */
  due(nowMs: number): boolean {
    return this.pending.length > 0 && this.inFlight === undefined && (this.pending.length >= MAX_SPANS_PER_EXPORT || nowMs - this.lastFlushMs >= FLUSH_INTERVAL_MS);
  }

  /** The OTLP JSON body for a batch of spans. Exported for the tests and the guard's self-check. */
  body(spans: readonly PendingSpan[]): string {
    const attr = (key: string, value: string | number) =>
      typeof value === "number" ? { key, value: { intValue: String(value) } } : { key, value: { stringValue: value } };
    return JSON.stringify({
      resourceSpans: [{
        resource: { attributes: [attr("service.name", this.config.serviceName)] },
        scopeSpans: [{
          scope: { name: "shogunagent.ops" },
          spans: spans.map((s) => ({
            traceId: s.traceId,
            spanId: s.spanId,
            name: s.name,
            kind: 2, // SERVER
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

  /** Post what is pending, in batches. Never throws; a failed batch is dropped and counted. */
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

/** Where the middleware schedules a flush on Workers (`ExecutionContext.waitUntil`). */
export type WaitUntil = (p: Promise<unknown>) => void;

export interface OpsTelemetryDeps {
  metrics: OpsMetrics;
  exporter?: OtlpTraceExporter;
  routeTemplate: (pathname: string) => string;
  knownTools?: ReadonlySet<string>;
  now?: () => number;
}

/** What the labels are made of: nothing but these five reads of a finished request. */
export interface RequestFacts {
  method: string;
  url: string;
  status: number;
  /** `Mcp-Method` of the request, else of the response the handler set. */
  mcpMethod: string | null | undefined;
  /** `Mcp-Name` of the request, else of the response. */
  mcpName: string | null | undefined;
}

/** The closed-set labels of one finished request. */
export function requestLabels(facts: RequestFacts, routeTemplate: (pathname: string) => string, knownTools: ReadonlySet<string> = KNOWN_TOOL_NAMES): RequestLabels {
  return {
    route: routeTemplate(new URL(facts.url).pathname),
    method: methodLabel(facts.method),
    status: statusLabel(facts.status),
    mcp_method: mcpMethodLabel(facts.mcpMethod),
    mcp_name: mcpNameLabel(facts.mcpName, knownTools),
  };
}

/** The middleware: one observation (and, when export is on, one span) per request. */
export function opsTelemetry(deps: OpsTelemetryDeps): MiddlewareHandler {
  const now = deps.now ?? (() => Date.now());
  return async (c, next) => {
    const startWall = now();
    const start = performance.now();
    try {
      await next();
    } finally {
      const ms = performance.now() - start;
      const labels = requestLabels(
        {
          method: c.req.method,
          url: c.req.url,
          status: c.res.status,
          mcpMethod: c.req.header("mcp-method") ?? c.res.headers.get("mcp-method"),
          mcpName: c.req.header("mcp-name") ?? c.res.headers.get("mcp-name"),
        },
        deps.routeTemplate,
        deps.knownTools,
      );
      deps.metrics.observe(labels, ms);
      const exporter = deps.exporter;
      if (exporter) {
        exporter.record(labels, startWall, ms);
        if (exporter.due(now())) {
          // Workers: keep the isolate alive for the post. Node: the entry's interval flushes, and
          // reading `executionCtx` outside Workers throws.
          let waitUntil: WaitUntil | undefined;
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
