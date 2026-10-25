/**
 * The REST face: `POST /v1/router/{discover,inspect,run}` plus `/mcp`, `/health` and the
 * catalog manifest. Built from one `RouterDeps` so tests inject replay transports and stub
 * stores; src/worker.ts builds the production deps from the Worker environment.
 */
import { Hono, type Context } from "hono";
import type { StatusCode } from "hono/utils/http-status";
import { callVerb, type RouterDeps } from "./engine.js";
import { requestLogger, routeTemplate } from "./logging.js";
import { handleMcp } from "./mcp.js";
import { OpsMetrics, opsTelemetry } from "./observability.js";
import { openCatalog, openCatalogDetail, openCatalogHtml, type OpenCatalogFilter } from "./open-catalog.js";
import { errorBody } from "./responses.js";
import { Secret } from "./secret.js";
import type { VerbName } from "./verbs.js";

function bearerOf(header: string | undefined): Secret | undefined {
  if (!header) return undefined;
  const m = /^Bearer\s+(\S+)$/i.exec(header);
  return m?.[1] ? new Secret(m[1]) : undefined;
}

/** JSON response without Hono's typed-status generics (the recursive `Json` type blows them up). */
function sendJson(c: Context, body: unknown, status: number): Response {
  return c.newResponse(JSON.stringify(body), status as StatusCode, { "content-type": "application/json; charset=UTF-8" });
}

async function readJson(req: Request): Promise<{ ok: true; value: unknown } | { ok: false }> {
  try {
    return { ok: true, value: await req.json() };
  } catch {
    return { ok: false };
  }
}

export function createApp(deps: RouterDeps): Hono {
  const app = new Hono();
  app.use("*", requestLogger(deps.logger));
  // E37: closed-set request metrics and the opt-in spans (src/observability.ts). Never a body.
  const ops = deps.ops ?? { metrics: new OpsMetrics() };
  app.use("*", opsTelemetry({ metrics: ops.metrics, ...(ops.exporter ? { exporter: ops.exporter } : {}), routeTemplate, ...(deps.now ? { now: deps.now } : {}) }));

  // Ops-only Prometheus text, behind `ROUTER_METRICS_TOKEN` (checked in src/transport.ts). 404
  // when the token is unset: the body describes the deployment's traffic.
  app.get("/metrics", (c) => {
    const verdict = ops.authorize?.(c.req.header("authorization")) ?? "disabled";
    if (verdict === "disabled") return sendJson(c, errorBody("invalid_request", "no such route", "discover", 0), 404);
    if (verdict === "unauthorized") return sendJson(c, { error: "unauthorized" }, 401);
    return c.newResponse(ops.metrics.render(), 200, { "content-type": "text/plain; version=0.0.4; charset=utf-8", "cache-control": "no-store" });
  });

  app.get("/health", (c) => c.json({ ok: true, catalog_hash: deps.catalog.manifest.hash, entries: deps.catalog.manifest.entry_count }));

  app.get("/v1/router/catalog", (c) => sendJson(c, deps.catalog.manifest, 200));

  // E29: the open public catalog. No key; generated from the compiled catalog and live health.
  const publicCache = { "cache-control": "public, max-age=60" };
  app.get("/v1/catalog", (c) => {
    const fp = c.req.query("first_party");
    const filter: OpenCatalogFilter = {
      ...(c.req.query("category") ? { category: c.req.query("category") as string } : {}),
      ...(c.req.query("provider") ? { provider: c.req.query("provider") as string } : {}),
      ...(fp === "true" || fp === "false" ? { first_party: fp === "true" } : {}),
    };
    for (const [k, v] of Object.entries(publicCache)) c.header(k, v);
    return sendJson(c, openCatalog(deps.catalog, deps.health, filter), 200);
  });
  app.get("/v1/catalog/:tool_id", (c) => {
    const detail = openCatalogDetail(deps.catalog, deps.health, c.req.param("tool_id"));
    if (!detail) return sendJson(c, errorBody("tool_not_found", `no tool "${c.req.param("tool_id")}" in the catalog; GET /v1/catalog lists every listed endpoint`, "discover", 0), 404);
    for (const [k, v] of Object.entries(publicCache)) c.header(k, v);
    return sendJson(c, detail, 200);
  });
  app.get("/catalog", (c) =>
    c.newResponse(openCatalogHtml(openCatalog(deps.catalog, deps.health)), 200, {
      "content-type": "text/html; charset=UTF-8",
      // No script runs on this page; the policy says so to the browser too.
      "content-security-policy": "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
      "x-content-type-options": "nosniff",
      ...publicCache,
    }),
  );

  const verb = (path: string, name: VerbName) => {
    app.post(path, async (c) => {
      const body = await readJson(c.req.raw);
      c.header("Mcp-Method", "tools/call");
      c.header("Mcp-Name", name);
      if (!body.ok) return sendJson(c, errorBody("invalid_request", "the request body must be JSON", "fix_input", 0), 400);
      const out = await callVerb(deps, name, body.value, bearerOf(c.req.header("authorization")));
      return sendJson(c, out.body, out.status);
    });
  };
  verb("/v1/router/discover", "router.discover");
  verb("/v1/router/inspect", "router.inspect");
  verb("/v1/router/run", "router.run");

  app.post("/mcp", async (c) => {
    const body = await readJson(c.req.raw);
    const reply = await handleMcp(deps, body.ok ? body.value : null, bearerOf(c.req.header("authorization")));
    for (const [k, v] of Object.entries(reply.headers)) c.header(k, v);
    return sendJson(c, reply.body, reply.status);
  });

  app.notFound((c) => sendJson(c, errorBody("invalid_request", "no such route; the router serves POST /v1/router/{discover,inspect,run} and POST /mcp", "discover", 0), 404));

  return app;
}
