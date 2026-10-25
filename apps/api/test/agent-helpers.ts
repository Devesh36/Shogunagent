/** Shared rig for the agent routes (agent-app.ts): an in-memory store, a fake core behind
 * `/mcp`, an inspectable log, and a registration helper that solves the (cheap, 8-bit) PoW. */
import { Hono } from "hono";

import { createAgentApp, type AgentDeps } from "../src/agent-app.js";
import type { AgentVars } from "../src/agent-keys.js";
import { InMemoryAgentStore, type AgentStore } from "../src/agent-store.js";
import { redactingLogger } from "../src/logging.js";
import { solvePow } from "../src/pow.js";

export const BASE = "https://api.test";
export const POW_SECRET = "test-pow-secret";
export const TEST_POW_BITS = 8;
/** PBKDF2 rounds for test keys: enough to exercise the path, cheap enough for hundreds of logins. */
export const TEST_KEY_ITERATIONS = 1_000;

export interface AgentRig {
  app: Hono<{ Variables: AgentVars }>;
  store: AgentStore;
  logLines: string[];
  /** What the fake core saw: method, headers it cares about. Never bodies — the test observes
   * the passthrough, it does not retain content. */
  upstreamCalls: Array<{ method: string; principalId: string }>;
  /** The fake core's next answer. */
  upstream: { status: number; body: string };
  now: () => Date;
  setNow(d: Date): void;
}

/** Overrides for the rig. `mcpUpstream: undefined` removes the fake core (a gateway with no core wired). */
export type RigOverrides = Partial<Omit<AgentDeps, "mcpUpstream">> & { mcpUpstream?: AgentDeps["mcpUpstream"] | undefined };

export function makeAgentRig(overrides: RigOverrides = {}, store: AgentStore = new InMemoryAgentStore()): AgentRig {
  const logLines: string[] = [];
  const upstreamCalls: AgentRig["upstreamCalls"] = [];
  const upstream = { status: 200, body: JSON.stringify({ jsonrpc: "2.0", id: 1, result: { ok: true } }) };
  let current = new Date("2026-10-25T12:00:00Z");
  const now = () => current;
  const log = (line: string) => logLines.push(line);
  const agentApp = createAgentApp({
    store,
    log,
    now,
    publicBaseUrl: BASE,
    powSecret: POW_SECRET,
    powBits: TEST_POW_BITS,
    keyIterations: TEST_KEY_ITERATIONS,
    ...("mcpUpstream" in overrides ? {} : {
      mcpUpstream: (req, principal) => {
        upstreamCalls.push({ method: req.method, principalId: principal.principalId });
        return Promise.resolve(new Response(upstream.body, { status: upstream.status, headers: { "content-type": "application/json" } }));
      },
    }),
    ...Object.fromEntries(Object.entries(overrides).filter(([, v]) => v !== undefined)),
  } as AgentDeps);
  // The same logger `createApp` puts in front of everything (app.ts); the rig mounts the agent
  // app the way production does.
  const app = new Hono<{ Variables: AgentVars }>();
  app.use("*", redactingLogger(log));
  app.route("/", agentApp);
  return { app, store, logLines, upstreamCalls, upstream, now, setNow: (d) => { current = d; } };
}

export interface Registered {
  key: string;
  keyId: string;
  principalId: string;
  body: Record<string, unknown>;
}

/** GET a challenge, solve it, POST /v1/agents. */
export async function registerAgent(rig: AgentRig, body: Record<string, unknown> = {}, ip = "203.0.113.1"): Promise<Registered> {
  const res = await register(rig, body, ip);
  if (res.status !== 201) throw new Error(`registration failed: ${res.status} ${await res.text()}`);
  const json = (await res.json()) as Record<string, unknown>;
  return { key: String(json.key), keyId: String(json.key_id), principalId: String(json.principal_id), body: json };
}

export async function solvedPowHeader(rig: AgentRig): Promise<string> {
  const challenge = ((await (await rig.app.request("/v1/agents/challenge")).json()) as { challenge: string }).challenge;
  const nonce = solvePow(challenge, TEST_POW_BITS);
  if (nonce === undefined) throw new Error("unsolvable test challenge");
  return `${challenge}:${nonce}`;
}

export async function register(rig: AgentRig, body: Record<string, unknown> = {}, ip = "203.0.113.1", pow?: string): Promise<Response> {
  return rig.app.request("/v1/agents", {
    method: "POST",
    headers: { "content-type": "application/json", "cf-connecting-ip": ip, "x-agent-pow": pow ?? (await solvedPowHeader(rig)) },
    body: JSON.stringify(body),
  });
}

/** One metered call through `/mcp`. The body is a fixed JSON-RPC envelope the fake core ignores. */
export function mcp(rig: AgentRig, key: string, headers: Record<string, string>, method = "POST"): Promise<Response> {
  return Promise.resolve(rig.app.request("/mcp", {
    method,
    headers: { authorization: `Bearer ${key}`, "content-type": "application/json", ...headers },
    ...(method === "POST" ? { body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: headers["mcp-method"] ?? "ping", params: {} }) } : {}),
  }));
}

export function authed(rig: AgentRig, key: string, path: string, init: { method?: string; body?: unknown; headers?: Record<string, string> } = {}): Promise<Response> {
  return Promise.resolve(rig.app.request(path, {
    method: init.method ?? "GET",
    headers: { authorization: `Bearer ${key}`, "content-type": "application/json", ...(init.headers ?? {}) },
    ...(init.body !== undefined ? { body: JSON.stringify(init.body) } : {}),
  }));
}
