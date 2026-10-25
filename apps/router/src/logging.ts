/**
 * The router's only logger (invariant 2, zero retention; same discipline as apps/api/src/logging.ts).
 *
 * `LogEvent` is a closed set of typed fields — route template, status, latency, tool id, outcome,
 * provider id, a count. There is no field that could hold a body, a header, an input, a result or
 * a credential, and `consoleLogger` formats only these fields. test/zero-retention.test.ts drives
 * sentinel strings through every path and asserts none reaches a logger.
 */
import type { MiddlewareHandler } from "hono";

export interface LogEvent {
  kind: "request" | "run" | "catalog" | "metering";
  route?: string;
  method?: string;
  status?: number;
  latency_ms?: number;
  tool_id?: string;
  provider_id?: string;
  outcome?: string;
  count?: number;
  /** A stable short hash of the actor id, never the key. */
  actor_hash?: string;
}

export interface Logger {
  log(event: LogEvent): void;
}

const ALLOWED_KEYS: ReadonlySet<string> = new Set([
  "kind",
  "route",
  "method",
  "status",
  "latency_ms",
  "tool_id",
  "provider_id",
  "outcome",
  "count",
  "actor_hash",
]);

/** Formats an event as `key=value` pairs. Unknown keys are dropped, so a caller cannot smuggle a field in. */
export function formatEvent(event: LogEvent): string {
  const parts: string[] = [];
  for (const [k, v] of Object.entries(event)) {
    if (!ALLOWED_KEYS.has(k) || v === undefined) continue;
    const s = typeof v === "number" ? String(v) : String(v).replace(/[\s=]/g, "_").slice(0, 80);
    parts.push(`${k}=${s}`);
  }
  return parts.join(" ");
}

export const consoleLogger: Logger = {
  log(event) {
    console.log(formatEvent(event));
  },
};

export const silentLogger: Logger = { log() {} };

/** The route template for a path — never the raw path. */
export function routeTemplate(pathname: string): string {
  if (pathname === "/v1/router/discover" || pathname === "/v1/router/inspect" || pathname === "/v1/router/run") return pathname;
  if (pathname === "/mcp" || pathname === "/health" || pathname === "/v1/router/catalog") return pathname;
  return "/unmatched";
}

export function requestLogger(logger: Logger): MiddlewareHandler {
  return async (c, next) => {
    const start = Date.now();
    try {
      await next();
    } finally {
      logger.log({
        kind: "request",
        method: c.req.method,
        route: routeTemplate(new URL(c.req.url).pathname),
        status: c.res.status,
        latency_ms: Date.now() - start,
      });
    }
  };
}
