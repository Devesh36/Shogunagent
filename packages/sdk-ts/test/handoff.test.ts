import { describe, expect, it } from "vitest";

import { HANDOFF_FORMAT, HANDOFF_SCHEMA_URL, HandoffError, ShogunAgent, type Handoff } from "../src/index.js";
import { FakeFetch, memoryFake } from "./fake-fetch.js";

const NOW = Date.parse("2026-10-25T12:00:00Z");

function sample(overrides: Partial<Handoff> = {}): Handoff {
  return {
    format: HANDOFF_FORMAT,
    handoff_id: "hof_1",
    created_at: NOW,
    expires_at: NOW + 3_600_000,
    from_agent: "agent:planner",
    to_agent: "agent:writer",
    task: { summary: "Draft the Heron rollout note", query: "Heron rollout", thread_key: "task-42" },
    entities: [
      { type: "entity", kind: "person", id: 12, name: "Oren Vale", confidence: 0.9, band: "high", provenance: [{ event_id: 481, quote: "Oren Vale: I'll send the rollout plan by Friday." }] },
    ],
    facts: [
      { type: "fact", kind: "commitment", id: 7, text: "send the rollout plan by Friday", confidence: 0.9, band: "high", provenance: [{ event_id: 481, quote: "I'll send the rollout plan by Friday." }] },
      { type: "fact", kind: "open_loop", id: 3, text: "waiting on budget numbers", confidence: 0.6, band: "medium", provenance: [{ event_id: 482, quote: "budget next week" }] },
    ],
    truncated: false,
    ...overrides,
  };
}

describe("handoff()", () => {
  it("posts the task to /v1/handoff and returns the object and the A2A part", async () => {
    const h = sample();
    const fake = new FakeFetch().route("/v1/handoff", (c) => ({
      body: {
        result: { handoff: { ...h, from_agent: c.body.from_agent }, a2a: { part: { kind: "data", data: h, metadata: { schema: HANDOFF_SCHEMA_URL } }, message: { kind: "message", role: "agent", messageId: "m", parts: [] } } },
        usage: { unit: "read", quantity: 1, cost_micro_usd: 0 }, latency_ms: 5, next_action: null,
      },
    }));
    const c = new ShogunAgent({ fetch: fake.fetch, apiKey: "k", actor: "agent:planner" });
    const r = await c.handoff({ task: "Draft the Heron rollout note", toAgent: "agent:writer", query: "Heron rollout", budgetTokens: 800, ttlSeconds: 600, threadKey: "task-42" });
    expect(fake.calls[0]!.body).toEqual({ task: "Draft the Heron rollout note", to_agent: "agent:writer", from_agent: "agent:planner", query: "Heron rollout", budget_tokens: 800, ttl_seconds: 600, thread_key: "task-42" });
    expect(r.handoff.format).toBe(HANDOFF_FORMAT);
    expect(r.a2a.part.metadata.schema).toBe(HANDOFF_SCHEMA_URL);
    expect(r.envelope.usage?.cost_micro_usd).toBe(0);
  });
});

describe("acceptHandoff()", () => {
  it("ingests into the receiver's agent_scratch (never world), idempotently, keeping possibly:", async () => {
    const fake = memoryFake();
    const c = new ShogunAgent({ fetch: fake.fetch, actor: "agent:writer", now: () => NOW });
    const part = { kind: "data", data: sample(), metadata: { schema: HANDOFF_SCHEMA_URL } };
    const first = await c.acceptHandoff({ kind: "message", role: "agent", parts: [{ kind: "text", text: "hi" }, part] });
    await c.acceptHandoff(part);
    const calls = fake.toolCalls("memory.ingest").map((x) => x.body.params.arguments);
    expect(calls).toHaveLength(2);
    expect(calls[0]).toMatchObject({ namespace: "agent_scratch", actor: "agent:writer", source: "handoff", thread_key: "handoff:hof_1", idempotency_key: "handoff:hof_1:0" });
    expect(calls[1].idempotency_key).toBe(calls[0].idempotency_key);
    const contents = (calls[0].events as Array<{ content: string }>).map((e) => e.content);
    expect(contents[0]).toContain("Draft the Heron rollout note");
    expect(contents.some((t) => t.startsWith("possibly: open loop: waiting on budget numbers"))).toBe(true);
    expect(contents.some((t) => t.startsWith("commitment: send the rollout plan by Friday") && t.includes("(event 481)"))).toBe(true);
    expect(first.handoff.handoff_id).toBe("hof_1");
  });

  it("refuses an expired handoff, one for another agent, and a non-handoff, before any network call", async () => {
    const fake = memoryFake();
    const c = new ShogunAgent({ fetch: fake.fetch, actor: "agent:writer", now: () => NOW });
    await expect(c.acceptHandoff(sample({ expires_at: NOW - 1 }))).rejects.toMatchObject({ code: "handoff_expired" });
    await expect(c.acceptHandoff(sample({ to_agent: "agent:other" }))).rejects.toMatchObject({ code: "handoff_not_for_you" });
    await expect(c.acceptHandoff({ hello: 1 })).rejects.toBeInstanceOf(HandoffError);
    expect(fake.calls).toHaveLength(0);
    await c.acceptHandoff(sample({ to_agent: "agent:other" }), { checkAddressee: false });
    expect(fake.calls).toHaveLength(1);
  });

  it("splits a handoff larger than one ingest", async () => {
    const fake = memoryFake();
    const c = new ShogunAgent({ fetch: fake.fetch, actor: "agent:writer", now: () => NOW });
    const facts = Array.from({ length: 250 }, (_, i) => ({ type: "fact" as const, kind: "commitment" as const, id: i + 1, text: `item ${i}`, confidence: 0.9, band: "high" as const, provenance: [{ event_id: i + 1, quote: null }] }));
    const r = await c.acceptHandoff(sample({ facts }));
    expect(r.ingested).toHaveLength(2);
    expect(fake.toolCalls("memory.ingest").map((x) => x.body.params.arguments.idempotency_key)).toEqual(["handoff:hof_1:0", "handoff:hof_1:1"]);
  });
});
