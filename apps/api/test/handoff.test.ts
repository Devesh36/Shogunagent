/**
 * `POST /v1/handoff` (E16): the handoff object validates against docs/api/handoff.schema.json, is a
 * strict subset of export v1, carries no Low row and no row without provenance, and is built from
 * one recall through the upstream seam (a fake core here) under the caller's key. The route needs
 * `read`, is free, and logs no body.
 */
import Ajv2020 from "ajv/dist/2020.js";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

import { HANDOFF_FORMAT, HANDOFF_SCHEMA_URL, buildHandoff, parseHandoffRequest, type Handoff, type HandoffRequest } from "../src/handoff.js";
import { agentCard } from "../src/wellknown/agent-card.js";
import { BASE, authed, makeAgentRig, registerAgent } from "./agent-helpers.js";

const repo = join(dirname(fileURLToPath(import.meta.url)), "../../..");
const readJson = (rel: string): Record<string, any> => JSON.parse(readFileSync(join(repo, rel), "utf8")) as Record<string, any>;
const handoffSchema = readJson("docs/api/handoff.schema.json");
const exportSchema = readJson("docs/api/export-v1.schema.json");

const ajv = new (Ajv2020 as unknown as new (o: object) => { compile(s: object): ((v: unknown) => boolean) & { errors?: unknown } })({ strict: false, allErrors: true });
const validate = ajv.compile(handoffSchema);

const SECRET = "SENTINEL-BODY-7f3a";

function recallEnvelope(items: unknown[], truncated = false): string {
  const payload = {
    tool: "memory.recall",
    result: { items, facts: [`fact ${SECRET}`], possibly: [], evidence: [], truncated, budget_tokens: 2000, used_tokens: 10, include_scratch: false },
    usage: { unit: "read", quantity: 1, cost_micro_usd: 0 }, latency_ms: 2, next_action: null,
  };
  return JSON.stringify({ jsonrpc: "2.0", id: 1, result: { content: [{ type: "text", text: JSON.stringify(payload) }], isError: false } });
}

const ITEMS = [
  { kind: "commitment", id: 7, text: `send the rollout plan by Friday ${SECRET}`, confidence: 0.9, band: "high", quote: "I'll send the rollout plan by Friday.", event_id: 481 },
  { kind: "open_loop", id: 3, text: "waiting on budget numbers", confidence: 0.6, band: "medium", quote: "budget next week", event_id: 482 },
  { kind: "person", id: 12, text: "Oren Vale", confidence: 0.92, band: "high", quote: "Oren Vale: I'll send it.", event_id: 481 },
  { kind: "person", id: 12, text: "Oren Vale", confidence: 0.92, band: "high", quote: "Oren again", event_id: 490 },
  { kind: "project", id: 4, text: "Heron", confidence: 0.85, band: "high", quote: null, event_id: 470 },
  { kind: "project", id: 5, text: "LOW-PROJECT", confidence: 0.3, band: "low", quote: null, event_id: 471 },
  { kind: "commitment", id: 8, text: "no provenance", confidence: 0.9, band: "high", quote: null, event_id: null },
];

const REQ: HandoffRequest = { task: "Draft the Heron rollout note", toAgent: "agent:writer", fromAgent: "agent:planner", query: "Heron rollout", budgetTokens: 2000, ttlSeconds: 3600, threadKey: "task-42" };

describe("the handoff object", () => {
  const h = buildHandoff(JSON.parse(JSON.parse(recallEnvelope(ITEMS)).result.content[0].text).result, REQ, 1_792_000_000_000, "hof_00000000-0000-0000-0000-000000000000");

  it("validates against handoff.schema.json", () => {
    expect(validate(h), JSON.stringify((validate as { errors?: unknown }).errors)).toBe(true);
  });

  it("drops Low rows and rows without provenance, merges duplicates, keeps bands and quotes", () => {
    expect(h.entities.map((e) => `${e.kind}:${e.id}`)).toEqual(["person:12", "project:4"]);
    expect(h.facts.map((f) => `${f.kind}:${f.id}:${f.band}`)).toEqual(["commitment:7:high", "open_loop:3:medium"]);
    expect(h.entities[0]!.provenance).toEqual([{ event_id: 481, quote: "Oren Vale: I'll send it." }, { event_id: 490, quote: "Oren again" }]);
    expect(h.entities[1]!.provenance).toEqual([{ event_id: 470, quote: null }]);
    expect(JSON.stringify(h)).not.toContain("LOW-PROJECT");
    expect(h.expires_at - h.created_at).toBe(3_600_000);
    expect(h.task).toEqual({ summary: REQ.task, query: REQ.query, thread_key: "task-42" });
  });

  it("the schema rejects Low, missing provenance, extra fields and a wrong format", () => {
    const bad = (mut: (x: any) => void) => { const x = structuredClone(h) as any; mut(x); return validate(x); };
    expect(bad((x) => { x.facts[0].band = "low"; })).toBe(false);
    expect(bad((x) => { x.facts[0].provenance = []; })).toBe(false);
    expect(bad((x) => { x.facts[0].evidence = "x"; })).toBe(false);
    expect(bad((x) => { x.format = "shogunagent-export/v1"; })).toBe(false);
    expect(bad((x) => { x.to_agent = "has space"; })).toBe(false);
  });

  it("is a strict subset of export v1: every record field is the export field of the same name and schema", () => {
    // Resolve local `$ref`s so the two documents compare by structure, not by ref names.
    const resolve = (doc: Record<string, any>, node: unknown): unknown => {
      if (Array.isArray(node)) return node.map((n) => resolve(doc, n));
      if (node && typeof node === "object") {
        const o = node as Record<string, unknown>;
        if (typeof o.$ref === "string" && o.$ref.startsWith("#/$defs/")) return resolve(doc, doc.$defs[o.$ref.slice(8)]);
        return Object.fromEntries(Object.entries(o).filter(([k]) => k !== "description").map(([k, v]) => [k, resolve(doc, v)]));
      }
      return node;
    };
    for (const def of ["entity", "fact"]) {
      const mine = resolve(handoffSchema, handoffSchema.$defs[def]) as { properties: Record<string, any>; required: string[]; additionalProperties: boolean };
      const theirs = resolve(exportSchema, exportSchema.$defs[def]) as { properties: Record<string, any>; required: string[] };
      expect(mine.additionalProperties).toBe(false);
      for (const [name, schema] of Object.entries(mine.properties)) {
        expect(Object.keys(theirs.properties), `${def}.${name} exists in export v1`).toContain(name);
        if (name === "band") {
          // Narrower: the same type, a subset of the values.
          expect(schema.type).toBe(theirs.properties.band.type);
          for (const v of schema.enum) expect(theirs.properties.band.enum).toContain(v);
          expect(schema.enum).not.toContain("low");
        } else if (name === "provenance") {
          // Narrower: at least one pointer; the pointer itself is export v1's.
          expect(schema.minItems).toBe(1);
          expect(schema.items).toEqual(theirs.properties.provenance.items);
        } else {
          expect(schema, `${def}.${name}`).toEqual(theirs.properties[name]);
        }
      }
      // Every field the handoff requires is a field export v1 requires too.
      for (const r of mine.required) expect(theirs.required).toContain(r);
    }
    // And every built row validates as its export v1 counterpart once the export-only fields are filled.
    const exportValidate = ajv.compile({ ...exportSchema, $id: "export-v1-test" });
    for (const e of h.entities) {
      expect(exportValidate({ ...e, aliases: [], summary: null, status: null, valid_from: 1, valid_to: null, updated_at: 1, actor: null, namespace: "world" })).toBe(true);
    }
    for (const f of h.facts) {
      expect(exportValidate({ ...f, subtype: "other", status: "open", due_at: null, counterparty_id: null, project_id: null, valid_from: 1, valid_to: null, updated_at: 1, actor: null, namespace: "world" })).toBe(true);
    }
  });

  it("parses and bounds the request", () => {
    expect(parseHandoffRequest({ task: " t ", to_agent: "agent:w" }, { keyId: "ak_1" })).toMatchObject({ task: "t", query: "t", fromAgent: "agent:ak_1", budgetTokens: 2000, ttlSeconds: 3600, threadKey: null });
    expect((parseHandoffRequest({ to_agent: "agent:w" }, { keyId: "k" }) as Error).message).toMatch(/^missing_task:/);
    expect((parseHandoffRequest({ task: "t", to_agent: "bad id" }, { keyId: "k" }) as Error).message).toMatch(/^invalid_to_agent:/);
    expect((parseHandoffRequest({ task: "t", to_agent: "a", budget_tokens: 5 }, { keyId: "k" }) as Error).message).toMatch(/^invalid_budget_tokens:/);
    expect((parseHandoffRequest({ task: "t", to_agent: "a", ttl_seconds: 10 ** 6 }, { keyId: "k" }) as Error).message).toMatch(/^invalid_ttl_seconds:/);
  });
});

describe("POST /v1/handoff", () => {
  function rig(answer: { status?: number; body: string } = { body: recallEnvelope(ITEMS) }) {
    const seen: Array<{ url: string; headers: Record<string, string>; body: any; principalId: string }> = [];
    const r = makeAgentRig({
      mcpUpstream: async (req, principal) => {
        seen.push({ url: req.url, headers: Object.fromEntries(req.headers.entries()), body: JSON.parse(await req.text()), principalId: principal.principalId });
        return new Response(answer.body, { status: answer.status ?? 200, headers: { "content-type": "application/json" } });
      },
    });
    return { r, seen };
  }

  it("builds the handoff from one recall under the caller's key and returns the A2A part", async () => {
    const { r, seen } = rig();
    const agent = await registerAgent(r);
    const res = await authed(r, agent.key, "/v1/handoff", { method: "POST", body: { task: "Draft the Heron rollout note", to_agent: "agent:writer", from_agent: "agent:planner", query: "Heron rollout", budget_tokens: 900, ttl_seconds: 600, thread_key: "task-42" } });
    expect(res.status).toBe(200);
    const body = (await res.json()) as { result: { handoff: Handoff; a2a: { part: any; message: any } }; usage: any; next_action: unknown };
    const h = body.result.handoff;
    expect(validate(h)).toBe(true);
    expect(h.format).toBe(HANDOFF_FORMAT);
    expect(h.handoff_id).toMatch(/^hof_/);
    expect(h.from_agent).toBe("agent:planner");
    expect(h.expires_at - h.created_at).toBe(600_000);
    expect(body.result.a2a.part).toEqual({ kind: "data", data: h, metadata: { schema: HANDOFF_SCHEMA_URL, format: HANDOFF_FORMAT } });
    expect(body.result.a2a.message).toMatchObject({ kind: "message", role: "agent", parts: [body.result.a2a.part] });
    expect(body.usage).toEqual({ unit: "read", quantity: 1, cost_micro_usd: 0 });
    expect(body.next_action).toBeNull();
    // One recall, through the seam, as this principal, with the caller's own bearer and the metering headers.
    expect(seen).toHaveLength(1);
    expect(seen[0]!.principalId).toBe(agent.principalId);
    expect(seen[0]!.url).toBe(`${BASE}/mcp`);
    expect(seen[0]!.headers).toMatchObject({ "mcp-method": "tools/call", "mcp-name": "memory.recall", authorization: `Bearer ${agent.key}` });
    expect(seen[0]!.body.params).toEqual({ name: "memory.recall", arguments: { query: "Heron rollout", budget_tokens: 900 } });
    // Nothing from either body reaches the log.
    expect(r.logLines.join("\n")).not.toContain(SECRET);
    expect(r.logLines.join("\n")).not.toContain("Heron");
    expect(r.logLines.some((l) => l.includes("/v1/handoff"))).toBe(true);
  });

  it("defaults from_agent to the key and query to the task; truncation says narrow_query", async () => {
    const { r, seen } = rig({ body: recallEnvelope(ITEMS, true) });
    const agent = await registerAgent(r);
    const res = await authed(r, agent.key, "/v1/handoff", { method: "POST", body: { task: "status of Heron", to_agent: "agent:writer" } });
    const body = (await res.json()) as { result: { handoff: Handoff }; next_action: unknown };
    expect(body.result.handoff.from_agent).toBe(`agent:${agent.keyId}`);
    expect(body.result.handoff.truncated).toBe(true);
    expect(body.next_action).toBe("narrow_query");
    expect(seen[0]!.body.params.arguments).toEqual({ query: "status of Heron", budget_tokens: 2000 });
  });

  it("refuses without a key, with bad input, and without a core", async () => {
    const { r, seen } = rig();
    expect((await r.app.request("/v1/handoff", { method: "POST", body: "{}" })).status).toBe(401);
    const agent = await registerAgent(r);
    const bad = await authed(r, agent.key, "/v1/handoff", { method: "POST", body: { task: "t", to_agent: "has space" } });
    expect(bad.status).toBe(400);
    expect(((await bad.json()) as { error: { code: string; next_action: string } }).error).toMatchObject({ code: "invalid_to_agent", next_action: "fix_input" });
    expect(seen).toHaveLength(0);
    const none = makeAgentRig({ mcpUpstream: undefined });
    const a2 = await registerAgent(none);
    const res = await authed(none, a2.key, "/v1/handoff", { method: "POST", body: { task: "t", to_agent: "agent:w" } });
    expect(res.status).toBe(503);
    expect(((await res.json()) as { error: { code: string } }).error.code).toBe("upstream_unavailable");
  });

  it("maps a core refusal to 502 with next_action and an unreachable core to 503", async () => {
    const refused = rig({ body: JSON.stringify({ jsonrpc: "2.0", id: 1, error: { code: -32000, message: "memory_update_pending: the store is being upgraded" } }) });
    const a = await registerAgent(refused.r);
    const res = await authed(refused.r, a.key, "/v1/handoff", { method: "POST", body: { task: "t", to_agent: "agent:w" } });
    expect(res.status).toBe(502);
    expect(((await res.json()) as { error: { code: string; next_action: string } }).error).toMatchObject({ code: "memory_update_pending", next_action: "retry_after" });
    const down = rig({ status: 500, body: "oops" });
    const b = await registerAgent(down.r);
    expect((await authed(down.r, b.key, "/v1/handoff", { method: "POST", body: { task: "t", to_agent: "agent:w" } })).status).toBe(503);
  });
});

describe("the agent card advertises handoff", () => {
  it("has a handoff skill from SKILL.md and the endpoint, schema and A2A part shape", () => {
    const card = agentCard({ publicBaseUrl: BASE }) as { skills: Array<{ id: string }>; "x-shogunagent": { handoff: Record<string, unknown> } };
    expect(card.skills.map((s) => s.id)).toContain("handoff");
    expect(card["x-shogunagent"].handoff).toMatchObject({ url: `${BASE}/v1/handoff`, method: "POST", scope: "read", format: HANDOFF_FORMAT, schema: HANDOFF_SCHEMA_URL });
    expect(handoffSchema.$id).toBe(HANDOFF_SCHEMA_URL);
  });
});
