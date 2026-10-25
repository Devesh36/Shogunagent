import { afterEach, expect, it, vi } from "vitest";
import { openAiReviewer, ReviewRefused } from "../src/review.js";
import { UpstreamError } from "../src/gateway.js";
import { TokenBucketLimiter } from "../src/ratelimit.js";
import { makeRig, sampleBody } from "./helpers.js";
import { MAX_CHUNK_BYTES, MAX_SYSTEM_BYTES } from "../src/types.js";

afterEach(() => vi.unstubAllGlobals());

const answer = (content: string) =>
  Response.json({ choices: [{ message: { content } }], usage: { prompt_tokens: 100, completion_tokens: 20, prompt_tokens_details: { cached_tokens: 10 } } });

it("returns json + usage, retries once on empty, sends luna low-effort json_schema request", async () => {
  const bodies: any[] = [];
  const replies = [answer(""), answer('{"ok":1}')];
  vi.stubGlobal("fetch", vi.fn(async (_u: string, init: RequestInit) => { bodies.push(JSON.parse(init.body as string)); return replies.shift()!; }));
  const out = await openAiReviewer("k")("sys", "usr", { type: "object" });
  expect(out.json).toEqual({ ok: 1 });
  // Both attempts were billed: the empty answer too (audit 2026-10-20 C1).
  expect(out.usage).toEqual({ input_tokens: 180, output_tokens: 40, cache_read_input_tokens: 20, cache_creation_input_tokens: 0 });
  expect(out.billing).toEqual({ sent: true, complete: true, usage: out.usage });
  expect(bodies).toHaveLength(2);
  expect(bodies[0]).toMatchObject({ model: "gpt-6-luna", reasoning_effort: "low", max_completion_tokens: 16000, store: false });
  expect(bodies[0].response_format.type).toBe("json_schema");
});

it("/v1/review caps system and user at the Batch lane's ceilings, before any reservation", async () => {
  const reviewer = vi.fn(async () => ({ json: { a: 1 }, usage: { input_tokens: 1, output_tokens: 1, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 } }));
  const rig = await makeRig({ reviewer, licenseSpendCapMicros: 5_000_000 });
  const post = (body: unknown) => rig.app.request("/v1/review", {
    method: "POST", headers: { authorization: `Bearer ${rig.token}`, "content-type": "application/json" }, body: JSON.stringify(body),
  });
  // Exactly at the ceilings (multi-byte text counts in UTF-8 bytes): served.
  const atSystem = "あ".repeat(Math.floor(MAX_SYSTEM_BYTES / 3));
  const atUser = "x".repeat(MAX_CHUNK_BYTES);
  expect((await post({ purpose: "consolidation", system: atSystem, user: atUser })).status).toBe(200);
  expect(reviewer).toHaveBeenCalledTimes(1);
  const records = rig.usage.records.length;
  const tooBigSystem = await post({ purpose: "consolidation", system: "あ".repeat(Math.floor(MAX_SYSTEM_BYTES / 3) + 1), user: "u" });
  expect(tooBigSystem.status).toBe(400);
  expect(await tooBigSystem.json()).toEqual({ error: "system is too large" });
  const tooBigUser = await post({ purpose: "consolidation", system: "s", user: "x".repeat(MAX_CHUNK_BYTES + 1) });
  expect(tooBigUser.status).toBe(400);
  expect(await tooBigUser.json()).toEqual({ error: "user is too large" });
  // Refused before the reviewer and before the ledger.
  expect(reviewer).toHaveBeenCalledTimes(1);
  expect(rig.usage.records).toHaveLength(records);
});

it("gives up after the second invalid answer", async () => {
  vi.stubGlobal("fetch", vi.fn(async () => answer("not json")));
  await expect(openAiReviewer("k")("s", "u", {})).rejects.toThrow();
});

it("/v1/review needs a licence, validates, reserves spend and settles it", async () => {
  const reviewer = vi.fn(async () => ({ json: { a: 1 }, usage: { input_tokens: 1000, output_tokens: 500, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 } }));
  const rig = await makeRig({ reviewer, licenseSpendCapMicros: 5_000_000 });
  const post = (body: unknown, token: string | null = rig.token) => rig.app.request("/v1/review", {
    method: "POST", headers: { ...(token ? { authorization: `Bearer ${token}` } : {}), "content-type": "application/json" }, body: JSON.stringify(body),
  });
  expect((await post({}, null)).status).toBe(401);
  expect((await post({ purpose: "consolidation", system: "s" })).status).toBe(400);
  const ok = await post({ purpose: "consolidation", system: "s", user: "u" });
  expect(ok.status).toBe(200);
  expect(await ok.json()).toMatchObject({ json: { a: 1 }, usage: { output_tokens: 500 } });
  expect(reviewer).toHaveBeenCalledTimes(1);
});

it("a reviewer failure is a 502", async () => {
  const rig = await makeRig({ reviewer: async () => { throw new UpstreamError("review", 500); } });
  const res = await rig.app.request("/v1/review", { method: "POST", headers: { authorization: `Bearer ${rig.token}` }, body: JSON.stringify({ purpose: "consolidation", system: "s", user: "u" }) });
  expect(res.status).toBe(502);
});

it("/v1/review forces the entity_dedupe groups shape and the entity resolution fields on consolidation", async () => {
  const schemas: any[] = [];
  const reviewer = vi.fn(async (_s: string, _u: string, schema: object) => { schemas.push(schema); return { json: { groups: [] }, usage: { input_tokens: 1, output_tokens: 1, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 } }; });
  const rig = await makeRig({ reviewer });
  const post = (purpose: string) => rig.app.request("/v1/review", {
    method: "POST", headers: { authorization: `Bearer ${rig.token}`, "content-type": "application/json" }, body: JSON.stringify({ purpose, system: "s", user: "u" }),
  });
  expect((await post("entity_dedupe")).status).toBe(200);
  expect(schemas[0].required).toEqual(["groups"]);
  expect((await post("consolidation")).status).toBe(200);
  const entity = schemas[1].properties.entities.items.properties;
  expect(Object.keys(entity)).toEqual(expect.arrayContaining(["existing_id", "aliases", "description", "person_class", "channel"]));
  expect(schemas[1].properties.entities.items.required).not.toContain("existing_id");
  // One call per hourly window: each session's answer keyed by its id.
  expect((await post("consolidation_window")).status).toBe(200);
  const session = schemas[2].properties.sessions.items;
  expect(schemas[2].required).toEqual(["sessions"]);
  expect(session.required).toEqual(["id", "commitments", "open_loops", "entities"]);
  expect(session.properties.entities.items.properties).toHaveProperty("existing_id");
  // A person / project page: a summary and claims, each with its basis, weight and events.
  expect((await post("entity_page")).status).toBe(200);
  expect(schemas[3].required).toEqual(["summary", "claims"]);
  expect(schemas[3].properties.claims.items.required).toEqual(["text", "basis", "weight", "event_ids"]);
});

it("/v1/review forces the loop consolidator's short-list shape", async () => {
  const schemas: any[] = [];
  const reviewer = vi.fn(async (_s: string, _u: string, schema: object) => { schemas.push(schema); return { json: { open: [], resolved: [], duplicates: [] }, usage: { input_tokens: 1, output_tokens: 1, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 } }; });
  const rig = await makeRig({ reviewer });
  const res = await rig.app.request("/v1/review", {
    method: "POST", headers: { authorization: `Bearer ${rig.token}`, "content-type": "application/json" }, body: JSON.stringify({ purpose: "loops_consolidate", system: "s", user: "u" }),
  });
  expect(res.status).toBe(200);
  expect(schemas[0].required).toEqual(["open", "resolved", "duplicates"]);
  expect(schemas[0].properties.open.maxItems).toBe(6);
  expect(schemas[0].properties.open.items.required).toEqual(["id", "kind", "horizon", "source_event_id", "source_quote", "confidence", "text"]);
  expect(schemas[0].properties.resolved.items.required).toEqual(["id", "event_id", "quote"]);
});

it("/v1/review draws from its own rate bucket, not the batch submissions' one", async () => {
  const reviewer = async () => ({ json: {}, usage: { input_tokens: 1, output_tokens: 1, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 } });
  const rig = await makeRig({ reviewer, rateLimit: new TokenBucketLimiter(1, 1), now: () => new Date("2026-10-01T00:00:00Z") });
  const review = () => rig.app.request("/v1/review", {
    method: "POST", headers: { authorization: `Bearer ${rig.token}`, "content-type": "application/json" }, body: JSON.stringify({ purpose: "consolidation", system: "s", user: "u" }),
  });
  const batch = () => rig.app.request("/v1/batch", {
    method: "POST", headers: { authorization: `Bearer ${rig.token}`, "content-type": "application/json" }, body: JSON.stringify(sampleBody),
  });
  expect((await review()).status).toBe(200);
  expect((await review()).status).toBe(429); // the review bucket is spent…
  expect((await batch()).status).not.toBe(429); // …the submission bucket is not
});

it("a content refusal is 422 upstream_refused; any other upstream failure stays a 502 outage", async () => {
  vi.stubGlobal("fetch", vi.fn(async () => Response.json({ error: { code: "content_filter" } }, { status: 400 })));
  await expect(openAiReviewer("k")("s", "u", {})).rejects.toBeInstanceOf(ReviewRefused);
  vi.stubGlobal("fetch", vi.fn(async () => Response.json({ choices: [{ message: { content: null, refusal: "no" }, finish_reason: "stop" }] })));
  await expect(openAiReviewer("k")("s", "u", {})).rejects.toMatchObject({ reason: "upstream_refused" });
  vi.stubGlobal("fetch", vi.fn(async () => Response.json({ error: { code: "invalid_request_error" } }, { status: 400 })));
  await expect(openAiReviewer("k")("s", "u", {})).rejects.toBeInstanceOf(UpstreamError);
  // An answer cut off at the budget is an outage (the hour is retried), never a refusal.
  vi.stubGlobal("fetch", vi.fn(async () => Response.json({ choices: [{ message: { content: "{\"sess" }, finish_reason: "length" }] })));
  await expect(openAiReviewer("k")("s", "u", {})).rejects.toBeInstanceOf(UpstreamError);

  const call = async (reviewer: () => Promise<never>) => {
    const rig = await makeRig({ reviewer });
    return rig.app.request("/v1/review", { method: "POST", headers: { authorization: `Bearer ${rig.token}` }, body: JSON.stringify({ purpose: "consolidation", system: "s", user: "u" }) });
  };
  const refused = await call(async () => { throw new ReviewRefused("upstream_refused"); });
  expect(refused.status).toBe(422);
  expect(await refused.json()).toEqual({ error: "upstream_refused" });
  expect((await call(async () => { throw new UpstreamError("review", 400); })).status).toBe(502);
});

it("/v1/review forces the dictionary_terms shape", async () => {
  const schemas: any[] = [];
  const reviewer = vi.fn(async (_s: string, _u: string, schema: object) => { schemas.push(schema); return { json: { terms: [] }, usage: { input_tokens: 1, output_tokens: 1, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 } }; });
  const rig = await makeRig({ reviewer });
  const res = await rig.app.request("/v1/review", {
    method: "POST", headers: { authorization: `Bearer ${rig.token}`, "content-type": "application/json" },
    body: JSON.stringify({ purpose: "dictionary_terms", system: "s", user: "{\"edits\":[]}" }),
  });
  expect(res.status).toBe(200);
  expect(schemas[0].required).toEqual(["terms"]);
  expect(schemas[0].properties.terms.items.required).toEqual(["heard", "term"]);
});
