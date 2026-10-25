/**
 * `POST /v1/handoff` (plan E16, docs/api/handoff.md): the state of work for a task, as a typed
 * object one agent hands to another over A2A, plus the A2A data part that carries it.
 *
 * The object is a strict subset of export v1 (docs/api/export-v1.md): entities (people, projects)
 * and facts (commitments, open loops) the task touches, each with its confidence band and the
 * provenance pointers (event id + quote) that prove it. It is built from one `memory.recall` the
 * gateway sends to the principal's core through the same upstream seam `/mcp` uses
 * (`mcpUpstream`), under the caller's own key: the core decides what the principal may read, and
 * applies the confidence rules (Low never comes back). The gateway maps the recall's items onto the
 * handoff shape in memory, answers, and keeps nothing: no body is logged, stored or cached
 * (invariants 1 and 2 — the composition is a pass-through, the same bytes the caller could fetch
 * with `memory.recall` itself, reshaped).
 *
 * Scope `read`; free (a read, invariant 10). The receiver accepts it into its own `agent_scratch`
 * namespace with the SDKs' `acceptHandoff()`, never into the principal's world.
 */
import type { Context } from "hono";
import { randomUUID } from "node:crypto";

import { agentError, type AgentPrincipal } from "./agent-keys.js";

export const HANDOFF_FORMAT = "shogunagent-handoff/v1";
export const HANDOFF_SCHEMA_URL = "https://shogunagent.dev/schemas/handoff.schema.json";
export const HANDOFF_PATH = "/v1/handoff";
export const MIN_BUDGET_TOKENS = 100;
export const MAX_BUDGET_TOKENS = 32_000;
export const DEFAULT_BUDGET_TOKENS = 2_000;
export const MIN_TTL_SECONDS = 60;
export const MAX_TTL_SECONDS = 86_400;
export const DEFAULT_TTL_SECONDS = 3_600;
export const MAX_TEXT = 2_000;
/** Most entities and most facts one handoff carries (each). */
export const MAX_ROWS = 200;
const AGENT_ID_RE = /^[A-Za-z0-9_.:@/-]{1,128}$/;
const MCP_PROTOCOL_VERSION = "2026-07-28";

export interface HandoffProvenance {
  event_id: number;
  quote: string | null;
}

export interface HandoffEntity {
  type: "entity";
  kind: "person" | "project";
  id: number;
  name: string;
  confidence: number;
  band: "high" | "medium";
  provenance: HandoffProvenance[];
}

export interface HandoffFact {
  type: "fact";
  kind: "commitment" | "open_loop";
  id: number;
  text: string;
  confidence: number;
  band: "high" | "medium";
  provenance: HandoffProvenance[];
}

export interface Handoff {
  format: typeof HANDOFF_FORMAT;
  handoff_id: string;
  created_at: number;
  expires_at: number;
  from_agent: string;
  to_agent: string;
  task: { summary: string; query: string; thread_key: string | null };
  entities: HandoffEntity[];
  facts: HandoffFact[];
  truncated: boolean;
}

export interface HandoffRequest {
  task: string;
  toAgent: string;
  fromAgent: string;
  query: string;
  budgetTokens: number;
  ttlSeconds: number;
  threadKey: string | null;
}

/** The recall result the builder reads (`render_recall` in crates/shogun-mcp/src/ingest_api.rs). */
export interface RecallLike {
  items?: Array<{ kind?: unknown; id?: unknown; text?: unknown; confidence?: unknown; band?: unknown; quote?: unknown; event_id?: unknown }>;
  truncated?: unknown;
}

function isObj(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function boundedText(v: unknown, field: string, required: boolean, max = MAX_TEXT): string | null | Error {
  if (v === undefined || v === null) return required ? new Error(`missing_${field}: pass ${field}, a non-empty string of at most ${max} characters.`) : null;
  if (typeof v !== "string") return new Error(`invalid_${field}: ${field} must be a string.`);
  const t = v.trim();
  if (!t) return required ? new Error(`missing_${field}: pass ${field}, a non-empty string of at most ${max} characters.`) : null;
  if (t.length > max) return new Error(`invalid_${field}: ${field} is at most ${max} characters.`);
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/.test(t)) return new Error(`invalid_${field}: ${field} must not contain control characters.`);
  return t;
}

function boundedInt(v: unknown, field: string, min: number, max: number, fallback: number): number | Error {
  if (v === undefined || v === null) return fallback;
  if (typeof v !== "number" || !Number.isSafeInteger(v) || v < min || v > max) return new Error(`invalid_${field}: ${field} must be an integer between ${min} and ${max}.`);
  return v;
}

/** Parse the body. `Error.message` is `<code>: <sentence>`, the caller's to fix (400). */
export function parseHandoffRequest(body: Record<string, unknown>, principal: Pick<AgentPrincipal, "keyId">): HandoffRequest | Error {
  const task = boundedText(body.task, "task", true);
  if (task instanceof Error) return task;
  const query = boundedText(body.query, "query", false);
  if (query instanceof Error) return query;
  const threadKey = boundedText(body.thread_key, "thread_key", false, 256);
  if (threadKey instanceof Error) return threadKey;
  const toAgent = body.to_agent;
  if (typeof toAgent !== "string" || !AGENT_ID_RE.test(toAgent)) return new Error("invalid_to_agent: to_agent is the receiving agent's id: letters, digits and `_ . : @ / -`, at most 128 characters.");
  const fromRaw = body.from_agent ?? `agent:${principal.keyId}`;
  if (typeof fromRaw !== "string" || !AGENT_ID_RE.test(fromRaw)) return new Error("invalid_from_agent: from_agent is your agent id: letters, digits and `_ . : @ / -`, at most 128 characters.");
  const budgetTokens = boundedInt(body.budget_tokens, "budget_tokens", MIN_BUDGET_TOKENS, MAX_BUDGET_TOKENS, DEFAULT_BUDGET_TOKENS);
  if (budgetTokens instanceof Error) return budgetTokens;
  const ttlSeconds = boundedInt(body.ttl_seconds, "ttl_seconds", MIN_TTL_SECONDS, MAX_TTL_SECONDS, DEFAULT_TTL_SECONDS);
  if (ttlSeconds instanceof Error) return ttlSeconds;
  return { task: task as string, query: (query as string | null) ?? (task as string), toAgent, fromAgent: fromRaw, budgetTokens, ttlSeconds, threadKey: threadKey as string | null };
}

/**
 * Map a recall onto the handoff shape. Only High and Medium rows with at least one provenance
 * event travel (a row with no event id cannot be proven to the receiver); duplicates of one
 * `(kind, id)` merge their provenance. Pure.
 */
export function buildHandoff(recall: RecallLike, req: HandoffRequest, nowMs: number, handoffId: string = `hof_${randomUUID()}`): Handoff {
  const entities = new Map<string, HandoffEntity>();
  const facts = new Map<string, HandoffFact>();
  let capped = false;
  for (const item of recall.items ?? []) {
    if (!isObj(item)) continue;
    const band = item.band;
    if (band !== "high" && band !== "medium") continue; // Low never travels.
    const id = item.id;
    const eventId = item.event_id;
    const text = typeof item.text === "string" ? item.text.trim() : "";
    const confidence = typeof item.confidence === "number" && item.confidence >= 0 && item.confidence <= 1 ? item.confidence : null;
    if (typeof id !== "number" || !Number.isSafeInteger(id) || typeof eventId !== "number" || !Number.isSafeInteger(eventId) || !text || confidence === null) continue;
    const prov: HandoffProvenance = { event_id: eventId, quote: typeof item.quote === "string" && item.quote.trim() ? item.quote : null };
    const kind = item.kind;
    const key = `${String(kind)}:${id}`;
    if (kind === "person" || kind === "project") {
      const existing = entities.get(key);
      if (existing) {
        if (!existing.provenance.some((p) => p.event_id === prov.event_id)) existing.provenance.push(prov);
      } else if (entities.size < MAX_ROWS) {
        entities.set(key, { type: "entity", kind, id, name: text, confidence, band, provenance: [prov] });
      } else capped = true;
    } else if (kind === "commitment" || kind === "open_loop") {
      const existing = facts.get(key);
      if (existing) {
        if (!existing.provenance.some((p) => p.event_id === prov.event_id)) existing.provenance.push(prov);
      } else if (facts.size < MAX_ROWS) {
        facts.set(key, { type: "fact", kind, id, text, confidence, band, provenance: [prov] });
      } else capped = true;
    }
  }
  return {
    format: HANDOFF_FORMAT,
    handoff_id: handoffId,
    created_at: nowMs,
    expires_at: nowMs + req.ttlSeconds * 1000,
    from_agent: req.fromAgent,
    to_agent: req.toAgent,
    task: { summary: req.task, query: req.query, thread_key: req.threadKey },
    entities: [...entities.values()],
    facts: [...facts.values()],
    truncated: recall.truncated === true || capped,
  };
}

/** The A2A data part (and a ready message) that carries a handoff. */
export function a2aPart(handoff: Handoff): { part: Record<string, unknown>; message: Record<string, unknown> } {
  const part = { kind: "data", data: handoff, metadata: { schema: HANDOFF_SCHEMA_URL, format: HANDOFF_FORMAT } };
  return {
    part,
    message: { kind: "message", role: "agent", messageId: `msg_${randomUUID()}`, parts: [part], metadata: { handoff_id: handoff.handoff_id } },
  };
}

/** The `memory.recall` request the gateway sends upstream for a handoff. */
export function recallRequest(base: string, req: HandoffRequest, authorization: string | undefined): Request {
  return new Request(`${base.replace(/\/+$/, "")}/mcp`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      accept: "application/json",
      "mcp-method": "tools/call",
      "mcp-name": "memory.recall",
      "mcp-protocol-version": MCP_PROTOCOL_VERSION,
      ...(authorization ? { authorization } : {}),
    },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name: "memory.recall", arguments: { query: req.query, budget_tokens: req.budgetTokens } } }),
  });
}

/** Read the recall out of the core's answer (JSON, or one SSE event). `Error` when it is not one. */
export async function recallFromUpstream(res: Response): Promise<RecallLike | Error> {
  const text = await res.text();
  let raw = text;
  if ((res.headers.get("content-type") ?? "").includes("text/event-stream")) {
    const data = text.split(/\r?\n/).filter((l) => l.startsWith("data:")).map((l) => l.slice(5).trim());
    raw = data[data.length - 1] ?? "";
  }
  let rpc: unknown;
  try {
    rpc = JSON.parse(raw);
  } catch {
    return new Error("upstream_error: the core answered something that is not JSON-RPC.");
  }
  if (!isObj(rpc)) return new Error("upstream_error: the core answered something that is not JSON-RPC.");
  if (isObj(rpc.error)) {
    const msg = typeof rpc.error.message === "string" ? rpc.error.message : "";
    const code = /^([a-z_]+):/.exec(msg)?.[1] ?? "upstream_error";
    return new Error(`${code}: the core refused the recall behind this handoff.`);
  }
  const result = isObj(rpc.result) ? rpc.result : {};
  const first = Array.isArray(result.content) ? (result.content as unknown[])[0] : undefined;
  let payload: unknown = result.structuredContent;
  if (payload === undefined && isObj(first) && typeof first.text === "string") {
    try {
      payload = JSON.parse(first.text);
    } catch {
      payload = undefined;
    }
  }
  if (result.isError === true || !isObj(payload)) {
    const code = isObj(payload) && typeof payload.error === "string" ? payload.error : "upstream_error";
    return new Error(`${code}: the core refused the recall behind this handoff.`);
  }
  return (isObj(payload.result) ? payload.result : payload) as RecallLike;
}

export interface HandoffDeps {
  publicBaseUrl: string;
  mcpUpstream?: (request: Request, principal: AgentPrincipal) => Promise<Response>;
  now: () => Date;
}

/** The route handler (mounted by `agent-app.ts` behind `requireKey(["read"])`). */
export async function handleHandoff(c: Context, principal: AgentPrincipal, body: Record<string, unknown> | undefined, deps: HandoffDeps): Promise<Response> {
  const started = Date.now();
  if (body === undefined) return agentError(c, 400, "invalid_json", "The body must be JSON: {\"task\": \"…\", \"to_agent\": \"agent:…\"}.", "fix_input");
  const req = parseHandoffRequest(body, principal);
  if (req instanceof Error) {
    const [code, ...rest] = req.message.split(": ");
    return agentError(c, 400, code ?? "invalid_request", rest.join(": "), "fix_input");
  }
  if (!deps.mcpUpstream) {
    c.set("relayLogEvent", "upstream_unavailable");
    return agentError(c, 503, "upstream_unavailable", "The core is not reachable from this gateway yet. Retry shortly.", "retry_after", { "retry-after": "30" });
  }
  let upstream: Response;
  try {
    upstream = await deps.mcpUpstream(recallRequest(deps.publicBaseUrl, req, c.req.header("authorization")), principal);
  } catch {
    c.set("relayLogEvent", "upstream_failure");
    return agentError(c, 503, "upstream_unavailable", "The core did not answer. Retry shortly.", "retry_after", { "retry-after": "5" });
  }
  if (!upstream.ok) {
    c.set("relayLogEvent", "upstream_failure");
    return agentError(c, 503, "upstream_unavailable", `The core answered ${upstream.status} to the recall behind this handoff. Retry shortly.`, "retry_after", { "retry-after": "5" });
  }
  const recall = await recallFromUpstream(upstream);
  if (recall instanceof Error) {
    const code = recall.message.split(":")[0] ?? "upstream_error";
    c.set("relayLogEvent", "upstream_failure");
    const retryable = code === "memory_update_pending" || code === "upstream_error" || code.endsWith("_unavailable");
    return c.json({ error: { code, message: `${recall.message.slice(code.length + 2)} ${retryable ? "Retry shortly." : "Fix the request."}`, next_action: retryable ? "retry_after" : "fix_input" } }, 502, { "cache-control": "no-store" });
  }
  const handoff = buildHandoff(recall, req, deps.now().getTime());
  c.set("relayLogEvent", "handoff_built");
  return c.json({
    result: { handoff, a2a: a2aPart(handoff) },
    usage: { unit: "read", quantity: 1, cost_micro_usd: 0 },
    latency_ms: Date.now() - started,
    hints: [],
    next_action: handoff.truncated ? "narrow_query" : null,
  }, 200, { "cache-control": "no-store" });
}
