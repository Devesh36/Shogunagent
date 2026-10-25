/**
 * The REST face: `POST /v1/router/{discover,inspect,run}` plus `/mcp`, `/health` and the
 * catalog manifest. Built from one `RouterDeps` so tests inject replay transports and stub
 * stores; src/worker.ts builds the production deps from the Worker environment.
 */
import { Hono, type Context } from "hono";
import type { StatusCode } from "hono/utils/http-status";
import { callVerb, type RouterDeps } from "./engine.js";
import { requestLogger } from "./logging.js";
import { handleMcp } from "./mcp.js";
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

  app.get("/health", (c) => c.json({ ok: true, catalog_hash: deps.catalog.manifest.hash, entries: deps.catalog.manifest.entry_count }));

  app.get("/v1/router/catalog", (c) => sendJson(c, deps.catalog.manifest, 200));

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
