/**
 * `POST /v1/handoff` (E16, E35): the handoff object validates against docs/api/handoff.schema.json
 * and is a strict subset of export v1. The core assembles it (crates/shogun-mcp/src/handoff_api.rs);
 * the route relays the body to the sidecar and the answer back without reading either (a fake core
 * here). It needs `read`, is free, and logs no body.
 */
import Ajv2020 from "ajv/dist/2020.js";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

import { HANDOFF_FORMAT, HANDOFF_SCHEMA_URL } from "../src/handoff.js";
import { createMcpRelay } from "../src/mcp-proxy.js";
import { agentCard } from "../src/wellknown/agent-card.js";
import { BASE, authed, makeAgentRig, registerAgent } from "./agent-helpers.js";

const repo = join(dirname(fileURLToPath(import.meta.url)), "../../..");
const readJson = (rel: string): Record<string, any> => JSON.parse(readFileSync(join(repo, rel), "utf8")) as Record<string, any>;
const handoffSchema = readJson("docs/api/handoff.schema.json");
const exportSchema = readJson("docs/api/export-v1.schema.json");

const ajv = new (Ajv2020 as unknown as new (o: object) => { compile(s: object): ((v: unknown) => boolean) & { errors?: unknown } })({ strict: false, allErrors: true });
const validate = ajv.compile(handoffSchema);

const SECRET = "SENTINEL-BODY-7f3a";

/** A handoff as the core builds it (crates/shogun-mcp/src/handoff_api.rs `build`, pinned in
 * `the_built_object_has_the_schema_fields` there): the object the schema and subset tests check. */
const h = {
  format: HANDOFF_FORMAT,
  handoff_id: "hof_00000000-0000-4000-8000-000000000000",
  created_at: 1_792_000_000_000,
  expires_at: 1_792_003_600_000,
  from_agent: "agent:planner",
  to_agent: "agent:writer",
  task: { summary: "Draft the Heron rollout note", query: "Heron rollout", thread_key: "task-42" },
  entities: [
    { type: "entity", kind: "person", id: 12, name: "Oren Vale", confidence: 0.92, band: "high", provenance: [{ event_id: 481, quote: "Oren Vale: I'll send it." }, { event_id: 490, quote: "Oren again" }] },
    { type: "entity", kind: "project", id: 4, name: "Heron", confidence: 0.85, band: "high", provenance: [{ event_id: 470, quote: null }] },
  ],
  facts: [
    { type: "fact", kind: "commitment", id: 7, text: "send the rollout plan by Friday", confidence: 0.9, band: "high", provenance: [{ event_id: 481, quote: "I'll send the rollout plan by Friday." }] },
    { type: "fact", kind: "open_loop", id: 3, text: "waiting on budget numbers", confidence: 0.6, band: "medium", provenance: [{ event_id: 482, quote: "budget next week" }] },
  ],
  truncated: false,
};
type Handoff = typeof h;

describe("the handoff object", () => {

  it("validates against handoff.schema.json", () => {
    expect(validate(h), JSON.stringify((validate as { errors?: unknown }).errors)).toBe(true);
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

});

describe("POST /v1/handoff relays to the core", () => {
  function rig(answer: { status?: number; body: string } = { body: JSON.stringify({ tool: "handoff.create", result: { handoff: h }, usage: { unit: "read", quantity: 1, cost_micro_usd: 0 } }) }) {
    const seen: Array<{ url: string; method: string; body: string; principalId: string }> = [];
    const r = makeAgentRig({
      mcpUpstream: async (req, principal) => {
        seen.push({ url: req.url, method: req.method, body: await req.text(), principalId: principal.principalId });
        return new Response(answer.body, { status: answer.status ?? 200, headers: { "content-type": "application/json" } });
      },
    });
    return { r, seen };
  }

  it("hands the caller's body to the core unchanged and returns the core's answer unchanged", async () => {
    const { r, seen } = rig();
    const agent = await registerAgent(r);
    const sent = { task: `Draft the Heron rollout note ${SECRET}`, to_agent: "agent:writer", budget_tokens: 900 };
    const res = await authed(r, agent.key, "/v1/handoff", { method: "POST", body: sent });
    expect(res.status).toBe(200);
    expect(res.headers.get("cache-control")).toBe("no-store");
    const body = (await res.json()) as { result: { handoff: Handoff }; usage: unknown };
    expect(body.result.handoff).toEqual(h);
    expect(body.usage).toEqual({ unit: "read", quantity: 1, cost_micro_usd: 0 });
    // One relay, as this principal, to the same path, with the body as the caller sent it.
    expect(seen).toHaveLength(1);
    expect(seen[0]!.principalId).toBe(agent.principalId);
    expect(new URL(seen[0]!.url).pathname).toBe("/v1/handoff");
    expect(JSON.parse(seen[0]!.body)).toEqual(sent);
    // Nothing from either body reaches the log; the route and a fixed word do.
    const log = r.logLines.join("\n");
    expect(log).not.toContain(SECRET);
    expect(log).not.toContain("Heron");
    expect(r.logLines.some((l) => l.includes("/v1/handoff"))).toBe(true);
  });

  it("relays the core's refusal with its status, and is free (nothing reserved)", async () => {
    const refusal = JSON.stringify({ error: "missing_task", message: "missing_task: pass task.", next_action: "fix_input" });
    const { r } = rig({ status: 400, body: refusal });
    const agent = await registerAgent(r);
    const before = await r.store.wallet(agent.principalId);
    const res = await authed(r, agent.key, "/v1/handoff", { method: "POST", body: { to_agent: "agent:w" } });
    expect(res.status).toBe(400);
    expect(await res.text()).toBe(refusal);
    expect(await r.store.wallet(agent.principalId)).toEqual(before);
  });

  it("refuses without a key, and answers 503 without a core or when it is down", async () => {
    const { r, seen } = rig();
    expect((await r.app.request("/v1/handoff", { method: "POST", body: "{}" })).status).toBe(401);
    expect(seen).toHaveLength(0);
    const none = makeAgentRig({ mcpUpstream: undefined });
    const a2 = await registerAgent(none);
    const res = await authed(none, a2.key, "/v1/handoff", { method: "POST", body: { task: "t", to_agent: "agent:w" } });
    expect(res.status).toBe(503);
    expect(((await res.json()) as { error: { code: string } }).error.code).toBe("upstream_unavailable");
    const down = makeAgentRig({ mcpUpstream: () => Promise.reject(new Error("down")) });
    const b = await registerAgent(down);
    expect((await authed(down, b.key, "/v1/handoff", { method: "POST", body: { task: "t", to_agent: "agent:w" } })).status).toBe(503);
  });

  it("the sidecar relay sends /v1/handoff to the sidecar's /v1/handoff and everything else to /mcp", async () => {
    const urls: string[] = [];
    const relay = createMcpRelay({ url: "http://127.0.0.1:7464", tokens: new Map([["p1", "sidecar-token-1"]]) }, async (input) => {
      urls.push(input);
      return new Response("{}", { status: 200, headers: { "content-type": "application/json" } });
    });
    const principal = { principalId: "p1", keyId: "k1" } as Parameters<typeof relay.upstream>[1];
    for (const path of ["/v1/handoff", "/mcp", "/v1/handoff/accept", "/v1/anything"]) {
      await relay.upstream(new Request(`${BASE}${path}`, { method: "POST", body: "{}" }), principal);
    }
    expect(urls).toEqual(["http://127.0.0.1:7464/v1/handoff", "http://127.0.0.1:7464/mcp", "http://127.0.0.1:7464/mcp", "http://127.0.0.1:7464/mcp"]);
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
