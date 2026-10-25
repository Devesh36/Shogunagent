/**
 * The agent-readable documents (plan E10; README §11 "Day-one artifacts"), as one Hono app the
 * agent app mounts next to `/.well-known/pricing.json`:
 *
 *   GET /.well-known/mcp/server-card.json   E10-T1  from the tool table (tool-table.ts)
 *   GET /.well-known/agent-card.json        E10-T3  A2A v1.0, signed when a key is configured
 *   GET /.well-known/status.json            E10-T7  rolling p50 / p95 / error rate + incidents
 *   GET /.well-known/mcp-config             E10-T9  Smithery config schema
 *   GET /health                             E10-T7  the short form (503 in a major incident)
 *   GET /openapi.json                       E10-T6  OpenAPI 3.1 of the REST face
 *   GET /llms.txt, /llms-full.txt           E10-T4  generated from the docs
 *   GET /privacy, /terms                    E23     generated from docs/legal/*.md (DRAFT)
 *   GET /legal/dpa, /legal/zdr, /legal/sla  E26     the enterprise contract drafts, same generator
 *
 * Every document is computed from code and config (invariant 12) and cached per process; none
 * reads a request body. `observe()` is the middleware the agent app puts in front of its routes
 * so the status window sees the metered traffic.
 */
import { Hono, type Context, type MiddlewareHandler } from "hono";
import type { KeyObject } from "node:crypto";

import incidentsJson from "../../incidents.json" with { type: "json" };
import toolTableJson from "../../fixtures/tool-table.json" with { type: "json" };
import { agentCard, signAgentCard } from "./agent-card.js";
import type { WellknownContext } from "./identity.js";
import { DPA_HTML, PRIVACY_HTML, SLA_HTML, TERMS_HTML, ZDR_HTML } from "./legal-content.generated.js";
import { LLMS_FULL_TXT, LLMS_TXT } from "./llms-content.generated.js";
import { mcpConfigDocument } from "./mcp-config.js";
import { openapiDocument } from "./openapi.js";
import { serverCard } from "./server-card.js";
import { RollingWindow, healthDocument, incidentProblems, statusDocument, type Incident } from "./status.js";
import { loadToolTable, type ToolTable } from "./tool-table.js";

export interface WellknownDeps extends WellknownContext {
  now?: () => Date;
  /** Ed25519 private key for the agent card (`AGENT_CARD_SIGNING_KEY`, parsed in boot.ts). Absent = unsigned. */
  agentCardSigningKey?: KeyObject;
  /** The tool table to render from (default: the committed fixture, pending E4's export). */
  toolTable?: ToolTable;
  /** The incident history (default: `apps/api/incidents.json`). */
  incidents?: Incident[];
  /** The window the agent routes are observed into (default: a fresh one). */
  window?: RollingWindow;
}

/** The committed tool table, validated at import so a bad fixture fails every test, not one route. */
export const FIXTURE_TOOL_TABLE: ToolTable = loadToolTable(toolTableJson);

/** The committed incident history, validated the same way. */
export const COMMITTED_INCIDENTS: Incident[] = (() => {
  const problems = incidentProblems(incidentsJson);
  if (problems.length > 0) throw new Error(`incidents.json: ${problems.join("; ")}`);
  return incidentsJson as Incident[];
})();

const JSON_CACHE = { "cache-control": "public, max-age=300" } as const;
const TEXT_CACHE = { "cache-control": "public, max-age=3600" } as const;

export interface Wellknown {
  app: Hono;
  /** Record a request into the status window. Mount it before the routes to be measured. */
  observe: MiddlewareHandler;
  window: RollingWindow;
}

export function createWellknown(deps: WellknownDeps): Wellknown {
  const now = deps.now ?? (() => new Date());
  const window = deps.window ?? new RollingWindow();
  const table = deps.toolTable ?? FIXTURE_TOOL_TABLE;
  const incidents = deps.incidents ?? COMMITTED_INCIDENTS;
  const ctx: WellknownContext = { publicBaseUrl: deps.publicBaseUrl, ...(deps.mcpUrl ? { mcpUrl: deps.mcpUrl } : {}) };

  // Static per deployment: rendered once, on first request.
  let card: string | undefined;
  let a2a: string | undefined;
  let openapi: string | undefined;
  let mcpConfig: string | undefined;
  const once = (slot: () => string | undefined, set: (v: string) => void, render: () => Record<string, unknown>): string => {
    const have = slot();
    if (have !== undefined) return have;
    const text = JSON.stringify(render());
    set(text);
    return text;
  };

  const app = new Hono();
  app.get("/.well-known/mcp/server-card.json", (c) => c.body(once(() => card, (v) => { card = v; }, () => serverCard(table, ctx)), 200, { ...JSON_CACHE, "content-type": "application/json; charset=utf-8" }));
  app.get("/.well-known/agent-card.json", (c) => c.body(once(() => a2a, (v) => { a2a = v; }, () => {
    const unsigned = agentCard(ctx);
    return deps.agentCardSigningKey ? signAgentCard(unsigned, deps.agentCardSigningKey) : unsigned;
  }), 200, { ...JSON_CACHE, "content-type": "application/json; charset=utf-8" }));
  app.get("/.well-known/mcp-config", (c) => c.body(once(() => mcpConfig, (v) => { mcpConfig = v; }, () => mcpConfigDocument(ctx)), 200, { ...JSON_CACHE, "content-type": "application/schema+json; charset=utf-8" }));
  app.get("/openapi.json", (c) => c.body(once(() => openapi, (v) => { openapi = v; }, () => openapiDocument(table, ctx)), 200, { ...JSON_CACHE, "content-type": "application/json; charset=utf-8" }));
  app.get("/llms.txt", (c) => c.text(LLMS_TXT, 200, TEXT_CACHE));
  app.get("/llms-full.txt", (c) => c.text(LLMS_FULL_TXT, 200, TEXT_CACHE));
  // E23: the privacy policy and terms, rendered from docs/legal/*.md (gen-legal-pages.mjs).
  const legal = (html: string) => (c: Context) => c.html(html, 200, { ...TEXT_CACHE, "content-security-policy": "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'", "x-content-type-options": "nosniff" });
  app.get("/privacy", legal(PRIVACY_HTML));
  app.get("/terms", legal(TERMS_HTML));
  // E26: the enterprise contract drafts (DRAFT for legal review).
  app.get("/legal/dpa", legal(DPA_HTML));
  app.get("/legal/zdr", legal(ZDR_HTML));
  app.get("/legal/sla", legal(SLA_HTML));

  // Live: never cached beyond a few seconds.
  const statusDeps = { window, incidents, now, ctx };
  app.get("/.well-known/status.json", (c) => c.json(statusDocument(statusDeps), 200, { "cache-control": "public, max-age=15" }));
  app.get("/health", (c) => {
    const { body, status } = healthDocument(statusDeps);
    return c.json(body, status as 200 | 503, { "cache-control": "public, max-age=5" });
  });

  const observe: MiddlewareHandler = async (c, next) => {
    const start = now().getTime();
    try {
      await next();
    } finally {
      window.observe(c.res.status, now().getTime() - start, now().getTime());
    }
  };

  return { app, observe, window };
}
