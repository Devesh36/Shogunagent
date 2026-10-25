/**
 * `POST /v1/handoff` (plan E16, E35; docs/api/handoff.md): the state of work for a task, as a
 * typed object one agent hands to another over A2A, plus the A2A data part that carries it.
 *
 * The core assembles it (`handoff.create` in crates/shogun-mcp/src/handoff_api.rs, served by the
 * sidecar at the same path): which rows the principal may read, the confidence rules, the subset of
 * export v1, the ids and the clock are all the core's. This route is a pure relay (CLAUDE.md
 * invariants 1 and 2; threat model R5, resolved): the caller's request body is handed to the
 * principal's sidecar as the incoming stream through the same relay `/mcp` uses (`mcp-proxy.ts`,
 * which picks the sidecar path from this request's own path), and the sidecar's reply is returned
 * as its stream. Neither body is read, parsed, buffered, logged or stored here; only the status
 * and a fixed event word reach the log.
 *
 * Scope `read`; free (a read, invariant 10), so nothing is reserved or billed. Accepting a handoff
 * is `handoff.accept` over the metered `/mcp` (an ingest into the receiver's own scratch).
 */
import type { Context } from "hono";

import { agentError, type AgentPrincipal } from "./agent-keys.js";
import { McpRelayRefused } from "./mcp-proxy.js";

export const HANDOFF_FORMAT = "shogunagent-handoff/v1";
export const HANDOFF_SCHEMA_URL = "https://shogunagent.dev/schemas/handoff.schema.json";
export const HANDOFF_PATH = "/v1/handoff";

export interface HandoffDeps {
  /** The sidecar relay (`McpRelay.upstream`): forwards `request` to the sidecar path of the same
   * name. Absent when no core is wired. */
  mcpUpstream?: (request: Request, principal: AgentPrincipal) => Promise<Response>;
}

/** The route handler (mounted by `agent-app.ts` behind `requireKey(["read"])`). */
export async function handleHandoff(c: Context, principal: AgentPrincipal, deps: HandoffDeps): Promise<Response> {
  if (!deps.mcpUpstream) {
    c.set("relayLogEvent", "upstream_unavailable");
    return agentError(c, 503, "upstream_unavailable", "The core is not reachable from this gateway yet. Nothing was charged. Retry shortly.", "retry_after", { "retry-after": "30" });
  }
  let reply: Response;
  try {
    reply = await deps.mcpUpstream(c.req.raw, principal);
  } catch (e) {
    if (e instanceof McpRelayRefused) {
      const r = e.refusal;
      c.set("relayLogEvent", r.code === "wrong_shard" ? "wrong_shard" : "upstream_unavailable");
      return c.json({ error: { code: r.code, message: r.message, next_action: r.nextAction, ...(r.extra ?? {}) } }, r.status, { "cache-control": "no-store", ...(r.status === 503 ? { "retry-after": "30" } : {}) });
    }
    c.set("relayLogEvent", "upstream_failure");
    return agentError(c, 503, "upstream_unavailable", "The core did not answer. Nothing was charged. Retry shortly.", "retry_after", { "retry-after": "5" });
  }
  c.set("relayLogEvent", reply.ok ? "handoff_relayed" : "upstream_failure");
  const headers = new Headers(reply.headers);
  headers.set("cache-control", "no-store");
  return new Response(reply.body, { status: reply.status, headers });
}
