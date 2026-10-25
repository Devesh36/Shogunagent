/** A fake `fetch` that plays the gateway / sidecar: MCP `tools/call` by tool name, REST by path. */
import type { FetchLike, ResponseLike } from "../src/types.js";

export interface Call {
  url: string;
  method: string;
  headers: Record<string, string>;
  body: any;
}

export interface Reply {
  status?: number;
  body?: unknown;
  headers?: Record<string, string>;
  /** Raw text instead of JSON. */
  text?: string;
}

type Handler = (call: Call) => Reply | Promise<Reply>;

export function response(reply: Reply): ResponseLike {
  const status = reply.status ?? 200;
  const headers = new Map(Object.entries({ "content-type": "application/json", ...(reply.headers ?? {}) }).map(([k, v]) => [k.toLowerCase(), v]));
  return {
    status,
    ok: status >= 200 && status < 300,
    headers: { get: (name: string) => headers.get(name.toLowerCase()) ?? null },
    text: async () => reply.text ?? (reply.body === undefined ? "" : JSON.stringify(reply.body)),
  };
}

/** A `tools/call` result whose text is `payload` (what the Rust core answers). */
export function toolText(payload: unknown, isError = false): Reply {
  return { body: { jsonrpc: "2.0", id: 1, result: { content: [{ type: "text", text: JSON.stringify(payload) }], isError } } };
}

export function rpcResult(result: unknown): Reply {
  return { body: { jsonrpc: "2.0", id: 1, result } };
}

export class FakeFetch {
  calls: Call[] = [];
  private tools = new Map<string, Handler>();
  private routes = new Map<string, Handler>();
  fallback: Handler = (c) => ({ status: 404, body: { error: { code: "not_found", message: `no fake for ${c.url}`, next_action: null } } });

  tool(name: string, handler: Handler | Reply): this {
    this.tools.set(name, typeof handler === "function" ? handler : () => handler);
    return this;
  }

  route(path: string, handler: Handler | Reply): this {
    this.routes.set(path, typeof handler === "function" ? handler : () => handler);
    return this;
  }

  /** Calls to one tool (by `Mcp-Name`). */
  toolCalls(name: string): Call[] {
    return this.calls.filter((c) => c.headers["mcp-name"] === name);
  }

  fetch: FetchLike = async (url, init = {}) => {
    const call: Call = {
      url,
      method: init.method ?? "GET",
      headers: Object.fromEntries(Object.entries(init.headers ?? {}).map(([k, v]) => [k.toLowerCase(), v])),
      body: init.body ? JSON.parse(init.body) : undefined,
    };
    this.calls.push(call);
    const path = new URL(url).pathname;
    if (path === "/mcp") {
      const name = call.body?.params?.name as string;
      const h = this.tools.get(name);
      return response(h ? await h(call) : { status: 200, body: { jsonrpc: "2.0", id: 1, error: { code: -32602, message: `unknown_tool: ${name}` } } });
    }
    const h = this.routes.get(path);
    return response(h ? await h(call) : await this.fallback(call));
  };
}

/** A recall payload with one item per band, a fact, a possibly line and an evidence line. */
export function recallPayload(overrides: Record<string, unknown> = {}): unknown {
  return {
    tool: "memory.recall",
    result: {
      items: [
        { kind: "commitment", id: 7, text: "send the rollout plan by Friday", confidence: 0.9, band: "high", quote: "I'll send the rollout plan by Friday.", event_id: 481 },
        { kind: "open_loop", id: 3, text: "waiting on Oren's budget numbers", confidence: 0.6, band: "medium", quote: "Oren: budget numbers next week", event_id: 482 },
        { kind: "person", id: 12, text: "SECRET-LOW-ITEM", confidence: 0.3, band: "low", quote: null, event_id: 483 },
      ],
      facts: ["Oren Vale leads the Heron rollout."],
      possibly: ["possibly: Heron ships in November."],
      evidence: [{ event_id: 481, ts_ms: 1792000000000, source: "agent", title: "standup", excerpt: "I'll send the rollout plan by Friday." }],
      truncated: false,
      budget_tokens: 2000,
      used_tokens: 120,
      include_scratch: false,
      ...overrides,
    },
    usage: { unit: "read", quantity: 1, cost_micro_usd: 0 },
    latency_ms: 4,
    next_action: null,
  };
}

export function ingestPayload(key = "k"): unknown {
  return {
    tool: "memory.ingest",
    result: { event_ids: [1, 2], inserted: 2, deduplicated: 0, replayed: false, thread_key: "t", idempotency_key: key },
    usage: { unit: "ingest", quantity: 2, cost_micro_usd: 1000 },
    latency_ms: 3,
    next_action: null,
  };
}

/** A fake that answers recall and ingest, with ingest replaying by idempotency key. */
export function memoryFake(recall: unknown = recallPayload()): FakeFetch {
  const seen = new Map<string, unknown>();
  return new FakeFetch()
    .tool("memory.recall", toolText(recall))
    .tool("memory.ingest", (c) => {
      const key = c.body.params.arguments.idempotency_key as string;
      const replayed = seen.has(key);
      seen.set(key, true);
      return toolText({ ...(ingestPayload(key) as object), result: { event_ids: [1], inserted: replayed ? 0 : 1, deduplicated: 0, replayed, thread_key: "t" } });
    })
    .tool("handoff.accept", (c) => {
      const args = c.body.params.arguments as { handoff: { handoff_id: string }; actor: string };
      const key = `handoff:${args.handoff.handoff_id}:${args.actor}`;
      const replayed = seen.has(key);
      seen.set(key, true);
      return toolText({
        tool: "handoff.accept",
        result: { handoff_id: args.handoff.handoff_id, accepted: 3, inserted: replayed ? 0 : 3, deduplicated: 0, rejected: 0, skipped: 0, replayed, event_ids: [1, 2, 3], thread_key: `handoff:${args.handoff.handoff_id}`, namespace: "agent_scratch", idempotency_key: key },
        usage: { unit: "events", quantity: replayed ? 0 : 3 }, latency_ms: 1, next_action: null,
      });
    });
}

export const PAYMENT_402: Reply = {
  status: 402,
  headers: {
    "payment-required": Buffer.from(JSON.stringify({ x402Version: 2, error: "payment_required", accepts: [{ rail: "card", url: "https://api.test/v1/wallet/topup", method: "POST" }], next_action: "top_up" })).toString("base64url"),
  },
  body: {
    error: { code: "payment_required", message: "This call costs 6000 micro-USD and the wallet has 1000 spendable.", next_action: "top_up" },
    payment: { amount_micro_usd: 6000, unit: "ingest", accepts: [{ rail: "card", url: "https://api.test/v1/wallet/topup", method: "POST" }], retry_after: 0, balance_micro_usd: 1000 },
    usage: { unit: "ingest", quantity: 0, cost_micro_usd: 0 },
    latency_ms: 3,
  },
};
