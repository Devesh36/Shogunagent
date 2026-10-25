/**
 * Body-redacting request logging (docs/batch-relay-design.md §3.2 / NFR-PRV-04).
 *
 * Written FIRST, before any route handler: the relay's core invariant is that chunk content is
 * never persisted, and logs are the easiest place to break that by accident. This middleware is
 * therefore the only sanctioned way a request leaves a trace, and it is structurally unable to
 * leak: it reads nothing but method, path, status, duration, and (on POST) the *count* of
 * accepted items a handler chose to expose via `relay-log-*` context vars. No body, no headers
 * (the Authorization header is a bearer credential), no query strings beyond the path.
 *
 * Runtime-neutral: `createHash` is the only import, and it is one of the `node:crypto` functions
 * the Workers runtime provides under `nodejs_compat` (exercised by test/workers).
 */
import { createHash } from "node:crypto";
import type { MiddlewareHandler } from "hono";

export type LogFn = (line: string) => void;

/**
 * The relay's console writers — the ONLY place `console.*` is called in the relay's own code
 * (`index.ts`, `worker.ts` and `boot.ts` all route through here). Every line is a fixed string
 * plus counters, route templates or env-variable *names*: never a body, a header, an id, a token
 * or an environment *value*. On Cloudflare Workers, Workers Logs captures all console output, so
 * this is also the complete inventory of what can reach it. Keep it that way.
 */
export const operational = {
  info: (line: string): void => console.log(line),
  warn: (line: string): void => console.warn(line),
  error: (line: string): void => console.error(line),
} as const;

/** Stable short hash of a licence id, for the log line.
 *
 * The id is not user content, but `license=<sub>` still turns the console into a per-customer
 * activity timeline, and the log sink is usually less protected than the usage ledger. A hash
 * keeps what the line is actually for — correlating requests within an incident — without the
 * identifier itself. */
export function logId(licenseId: string): string {
  return createHash("sha256").update(licenseId).digest("hex").slice(0, 12);
}

/** Context vars a handler may set for the log line. Numbers only — never content. */
export interface LogVars {
  /** Number of chunks accepted (set by POST /v1/batch). */
  relayLogChunks?: number;
  relayLogReservedChunks?: number;
  relayLogEvent?: "cap_reached" | "spend_cap_reached" | "license_spend_cap_reached" | "duplicate_prevented" | "payload_conflict" | "submission_unknown" | "provider_accepted_unbound" | "provider_rejected" | "metering_unavailable" | "running" | "batch_failed_no_results"
    // `/v1/ai/form_decide` (issue #1144).
    | "form_decide_unavailable" | "in_flight_capped" | "typesafe_key_rejected" | "upstream_rate_limited" | "upstream_overloaded";
  /** Server-chosen provider and model of the accepted batch (ids, not content). */
  relayLogModel?: string;
  /** License id (an opaque token id, not user content). */
  relayLogLicense?: string;
}

/** The route template for a path — never the raw path, which for `/v1/batch/{id}` carries a batch
 * id. Shared by the log line and the metrics (FR-SVC-06) so both name routes the same way. */
export function routeTemplate(pathname: string): string {
  if (pathname === "/v1/batch" || pathname === "/v1/review" || pathname === "/v1/batch/recover" || pathname === "/v1/ai/form_decide" || pathname === "/healthz" || pathname === "/metrics") return pathname;
  if (["/v1/asr/grant", "/v1/asr/refresh", "/v1/asr/finish"].includes(pathname)) return pathname;
  if (pathname === "/v1/ai/usage") return pathname;
  if (/^\/v1\/ai\/[^/]+$/.test(pathname)) return "/v1/ai/:lane";
  return /^\/v1\/batch\/[^/]+$/.test(pathname) ? "/v1/batch/:id" : "/unmatched";
}

export function redactingLogger(log: LogFn): MiddlewareHandler<{ Variables: LogVars }> {
  return async (c, next) => {
    const start = Date.now();
    try {
      await next();
    } finally {
      const ms = Date.now() - start;
      const chunks = c.get("relayLogChunks");
      const reserved = c.get("relayLogReservedChunks");
      const license = c.get("relayLogLicense");
      const model = c.get("relayLogModel");
      const event = c.get("relayLogEvent");
      const extras = [
        event ? `event=${event}` : null,
        typeof reserved === "number" ? `reserved_chunks=${reserved}` : null,
        typeof chunks === "number" ? `chunks=${chunks}` : null,
        typeof model === "string" ? `provider=${model.startsWith("gpt-") ? "openai" : model.startsWith("jev-") ? "typesafe" : "anthropic"} model=${model}` : null,
        typeof license === "string" ? `license=${logId(license)}` : null,
      ]
        .filter((v): v is string => v !== null)
        .join(" ");
      // Path only — never the body, never a header value.
      const route = routeTemplate(new URL(c.req.url).pathname);
      const method = ["GET", "POST", "HEAD", "OPTIONS", "PUT", "PATCH", "DELETE"].includes(c.req.method) ? c.req.method : "OTHER";
      log(`${method} ${route} ${c.res.status} ${ms}ms${extras ? " " + extras : ""}`);
    }
  };
}
