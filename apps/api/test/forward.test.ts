/** §4.2–§4.4: the forward is a passthrough for custom_id and chunk content, and the model is a
 * server decision the client cannot influence. */
import { describe, expect, it } from "vitest";

import { EFFORT_BY_CLASS, MAX_TOKENS_BY_CLASS, MODEL_BY_CLASS } from "../src/models.js";
import { UpstreamError } from "../src/gateway.js";
import { makeRig, postBatch, sampleBody } from "./helpers.js";

describe("forwarding", () => {
  it("preserves custom_ids and chunks, and picks the model server-side", async () => {
    const rig = await makeRig();
    const res = await postBatch(rig, sampleBody);
    expect(res.status).toBe(202);

    const forwarded = rig.gateway.createCalls[0];
    expect(forwarded).toBeDefined();
    expect(forwarded?.map((r) => r.custom_id)).toEqual(["1234", "5678"]);
    expect(forwarded?.[0]?.params.messages[0]?.content).toBe("TOP-SECRET-CHUNK-ALPHA");
    expect(forwarded?.[0]?.params.model).toBe(MODEL_BY_CLASS.classify);
  });

  it("forwards the Haiku 5.5 request shape for classify: model, room for thinking, low effort, no sampling params", async () => {
    const rig = await makeRig();
    expect((await postBatch(rig, sampleBody)).status).toBe(202);
    const params = rig.gateway.createCalls[0]?.[0]?.params;
    expect(params?.model).toBe("claude-haiku-5-5");
    expect(MODEL_BY_CLASS.classify).toBe("claude-haiku-5-5");
    // Thinking counts toward max_tokens on this model: a cap sized for the ~200-token verdict
    // alone would end in stop_reason max_tokens before any text.
    expect(params?.max_tokens).toBe(2048);
    expect(params?.max_tokens).toBeGreaterThanOrEqual(2 * 1024);
    expect(params?.output_config?.effort).toBe("low");
    expect(params?.output_config?.format?.type).toBe("json_schema");
    // Any non-default value of these, a thinking budget, or a prefill is a 400 on Haiku 5.5.
    for (const forbidden of ['"temperature"', '"top_p"', '"top_k"', '"thinking"', '"budget_tokens"', '"assistant"', '"tools"']) {
      expect(JSON.stringify(params)).not.toContain(forbidden);
    }
    expect(Object.keys(params ?? {}).sort()).toEqual(["max_tokens", "messages", "model", "output_config"]);
  });

  it("sets the effort per class server-side: low for extraction, medium for recaps and the brief", async () => {
    expect(EFFORT_BY_CLASS).toEqual({ classify: "low", summarize: "medium", brief: "medium", about_profile: "low", daily_activity_summary: "low" });
    for (const [purpose, model_class, effort] of [
      ["meeting_recap", "summarize", "medium"],
      ["about_profile", "about_profile", "low"],
      ["daily_activity_summary", "daily_activity_summary", "low"],
      ["morning_brief", "brief", "medium"],
      ["evening_wrap", "brief", "medium"],
    ] as const) {
      const rig = await makeRig();
      expect((await postBatch(rig, { ...sampleBody, purpose, model_class })).status).toBe(202);
      const params = rig.gateway.createCalls[0]?.[0]?.params;
      expect(params?.output_config?.effort).toBe(effort);
      expect(params?.max_tokens).toBe(MAX_TOKENS_BY_CLASS[model_class]);
    }
    // Every Haiku lane moved; only the brief stays on Sonnet.
    for (const cls of ["classify", "summarize", "about_profile", "daily_activity_summary"] as const) {
      expect(MODEL_BY_CLASS[cls]).toBe("claude-haiku-5-5");
    }
    expect(MODEL_BY_CLASS.brief).toBe("claude-sonnet-5-5");
  });

  it("never forwards a device-sent sampling parameter, thinking budget or prefill, at body or item level", async () => {
    const rig = await makeRig();
    const res = await postBatch(rig, {
      ...sampleBody,
      temperature: 0,
      top_p: 1,
      top_k: 5,
      thinking: { type: "enabled", budget_tokens: 4000 },
      output_config: { effort: "max" },
      params: { temperature: 0 },
      items: [
        { custom_id: "1", chunk: "c", temperature: 0, top_p: 0.5, top_k: 1, thinking: { type: "disabled" }, output_config: { effort: "max" },
          messages: [{ role: "assistant", content: "{" }] },
      ],
    });
    expect(res.status).toBe(202);
    const text = JSON.stringify(rig.gateway.createCalls[0]);
    for (const forbidden of ['"temperature"', '"top_p"', '"top_k"', '"thinking"', '"budget_tokens"', '"assistant"', '"max"']) {
      expect(text).not.toContain(forbidden);
    }
    expect(rig.gateway.createCalls[0]?.[0]?.params.output_config?.effort).toBe("low");
    expect(rig.gateway.createCalls[0]?.[0]?.params.messages).toEqual([{ role: "user", content: "c" }]);
  });

  it("attaches a json_schema for the purpose and never a tools array (#315)", async () => {
    const rig = await makeRig();
    const res = await postBatch(rig, sampleBody);
    expect(res.status).toBe(202);
    const params = rig.gateway.createCalls[0]?.[0]?.params;
    expect(params?.output_config?.format?.type).toBe("json_schema");
    expect(params?.output_config?.format?.schema).toMatchObject({
      type: "object",
      required: expect.arrayContaining(["commitments", "open_loops", "entities"]),
    });
    expect(JSON.stringify(params)).not.toContain('"tools"');
  });

  it("lets every action and entity carry its line anchor and never asks for a quote (PR #1022)", async () => {
    // The device resolves `line` / `anchor` / `anchor_len` back to a verbatim span itself. A
    // schema without them cannot be answered with them (`additionalProperties: false`), so the
    // device would drop every item as unresolvable — after the batch was billed. An entity with a
    // `quote` is rejected by the device's strict wire type for the same reason in reverse.
    const rig = await makeRig();
    expect((await postBatch(rig, sampleBody)).status).toBe(202);
    const schema = rig.gateway.createCalls[0]?.[0]?.params.output_config?.format?.schema as {
      properties: Record<string, { items: { required: string[]; properties: Record<string, unknown> } }>;
    };
    for (const array of ["commitments", "open_loops", "entities"]) {
      const item = schema.properties[array]?.items;
      expect(item?.required, array).toEqual(expect.arrayContaining(["line", "anchor", "anchor_len"]));
      expect(item?.properties, array).not.toHaveProperty("quote");
      expect(item?.properties, array).not.toHaveProperty("confidence");
    }
  });

  it("picks the mail triage verdict schema for mail_triage, not the consolidation shape", async () => {
    // mail_triage shares the `classify` cost class with consolidation. Selecting the schema by
    // class used to force `{commitments, open_loops, entities}` on a prompt that asks for
    // `{label, attention, confidence, reasons}`; the device then dropped every verdict as
    // malformed after the batch had been billed.
    const rig = await makeRig();
    const res = await postBatch(rig, { ...sampleBody, purpose: "mail_triage", model_class: "classify" });
    expect(res.status).toBe(202);
    const schema = rig.gateway.createCalls[0]?.[0]?.params.output_config?.format?.schema as {
      required: string[];
      additionalProperties: boolean;
      properties: {
        attention: { enum: string[] };
        commitments: { type: string; items: { required: string[] } };
        open_loops: { type: string; items: { required: string[] } };
        entities?: unknown;
      };
    };
    expect(schema.required).toEqual(["label", "attention", "confidence", "reasons", "commitments", "open_loops"]);
    expect(schema.additionalProperties).toBe(false);
    expect(schema.properties.attention.enum).toEqual(["needs_reply", "waiting_on_them", "fyi", "none"]);
    // L0(b): a Gmail thread's commitments and open loops come from this lane alone, anchored the
    // same way the consolidation lane's are. The mail prompt asks for no entities.
    expect(schema.properties.commitments.items.required).toEqual(expect.arrayContaining(["direction", "description", "line", "anchor", "anchor_len"]));
    expect(schema.properties.open_loops.items.required).toEqual(expect.arrayContaining(["kind", "description", "line", "anchor", "anchor_len"]));
    expect(schema.properties.entities).toBeUndefined();
  });

  it("rejects a custom_id the upstream Batch API would refuse, before any spend", async () => {
    const rig = await makeRig();
    const res = await postBatch(rig, {
      ...sampleBody,
      items: [{ custom_id: "gmail:19a3f", chunk: "x" }],
    });
    expect(res.status).toBe(400);
    expect(rig.gateway.createCalls).toHaveLength(0);
  });

  it("sends meeting recaps without a forced shape and with room for evidence", async () => {
    // A forced shape had no `evidence` field and made Haiku loop or return empty minutes.
    const rig = await makeRig();
    const res = await postBatch(rig, { ...sampleBody, purpose: "meeting_recap", model_class: "summarize" });
    expect(res.status).toBe(202);
    const params = rig.gateway.createCalls[0]?.[0]?.params;
    // No forced shape, but the relay's effort still rides in output_config.
    expect(params?.output_config).toEqual({ effort: "medium" });
    expect(params?.max_tokens).toBe(8192);
    expect(JSON.stringify(params)).not.toContain('"tools"');
  });

  it("picks the brief schema for morning briefs, with thinking headroom and an explicit medium effort", async () => {
    const rig = await makeRig();
    const res = await postBatch(rig, { ...sampleBody, purpose: "morning_brief", model_class: "brief" });
    expect(res.status).toBe(202);
    const params = rig.gateway.createCalls[0]?.[0]?.params;
    const schema = params?.output_config?.format?.schema as { required: string[] };
    expect(schema.required).toEqual(["what_happened"]);
    // Sonnet 5.5 thinks adaptively (default effort high) and the thinking counts toward
    // max_tokens: at 2,048 the brief JSON was cut off after the thinking block. The relay sets a
    // medium effort and the recap's 8,192 headroom.
    expect(params?.output_config?.effort).toBe("medium");
    expect(params?.max_tokens).toBe(8192);
    expect(MAX_TOKENS_BY_CLASS.brief).toBe(8192);
  });

  it.each([
    ["about_profile", ["about"]],
    ["daily_activity_summary", ["summary", "evidence"]],
  ] as const)("accepts %s only with its grounded response schema", async (purpose, required) => {
    const rig = await makeRig();
    const res = await postBatch(rig, { ...sampleBody, purpose, model_class: purpose });
    expect(res.status).toBe(202);
    const params = rig.gateway.createCalls[0]?.[0]?.params;
    expect(params?.model).toBe(MODEL_BY_CLASS[purpose]);
    expect((params?.output_config?.format?.schema as { required: string[] }).required).toEqual(
      expect.arrayContaining([...required]),
    );
  });

  it("rejects a permitted purpose paired with the wrong response contract", async () => {
    const rig = await makeRig();
    const res = await postBatch(rig, { ...sampleBody, purpose: "about_profile", model_class: "summarize" });
    expect(res.status).toBe(400);
    expect(rig.gateway.createCalls).toHaveLength(0);
  });

  it("keeps review-only purposes out of /v1/batch", async () => {
    const rig = await makeRig();
    const res = await postBatch(rig, { ...sampleBody, purpose: "dictionary_terms", model_class: "classify" });
    expect(res.status).toBe(400);
    expect(rig.gateway.createCalls).toHaveLength(0);
  });

  it("ignores a client-sent model id — the intent is all the device gets to say (§4.4)", async () => {
    const rig = await makeRig();
    const res = await postBatch(rig, {
      ...sampleBody,
      model: "claude-opus-4-6", // not part of the contract; must not be read
      items: [{ custom_id: "1", chunk: "c", model: "claude-opus-4-6" }],
    });
    expect(res.status).toBe(202);
    const forwarded = rig.gateway.createCalls[0];
    expect(forwarded?.[0]?.params.model).toBe(MODEL_BY_CLASS.classify);
    expect(JSON.stringify(forwarded)).not.toContain("opus");
  });

  it("rejects an unknown model_class with 400 before touching the gateway", async () => {
    const rig = await makeRig();
    const res = await postBatch(rig, { ...sampleBody, model_class: "claude-opus-4-6" });
    expect(res.status).toBe(400);
    expect(rig.gateway.createCalls).toHaveLength(0);
  });

  it("passes an in-progress status through as {status, completed, total} (§4.3)", async () => {
    const rig = await makeRig();
    // The ownership check reads the store before the gateway — register rb_1 as this licence's.
    await rig.usage.attachBatch("rb_1", "lic_test_1", "2026-08-13", 1, "dev_test_device");
    rig.gateway.status = {
      id: "rb_1",
      processing_status: "in_progress",
      request_counts: { processing: 512, succeeded: 280, errored: 20, canceled: 0, expired: 0 },
    };
    const res = await rig.app.request("/v1/batch/rb_1", {
      headers: { authorization: `Bearer ${rig.token}` },
    });
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({ status: "in_progress", state: "running", completed: 300, total: 812 });
    expect(rig.gateway.getCalls).toEqual(["rb_1"]);
  });

  it("streams ended results as relay-shaped JSON keyed by custom_id (§4.3)", async () => {
    const rig = await makeRig();
    // The ownership check reads the store before the gateway — register rb_1 as this licence's.
    await rig.usage.attachBatch("rb_1", "lic_test_1", "2026-08-13", 1, "dev_test_device");
    rig.gateway.status = {
      id: "rb_1",
      processing_status: "ended",
      request_counts: { processing: 0, succeeded: 2, errored: 1, canceled: 0, expired: 0 },
    };
    rig.gateway.resultLines = [
      JSON.stringify({
        custom_id: "b",
        result: { type: "succeeded", message: { content: [{ type: "text", text: "B-label" }] } },
      }),
      JSON.stringify({
        custom_id: "a",
        result: {
          type: "succeeded",
          // Multi-block content concatenates, mirroring the device-side parser.
          message: { content: [{ type: "text", text: "A-" }, { type: "text", text: "label" }] },
        },
      }),
      JSON.stringify({ custom_id: "c", result: { type: "errored", error: { type: "invalid_request" } } }),
    ];
    const res = await rig.app.request("/v1/batch/rb_1", {
      headers: { authorization: `Bearer ${rig.token}` },
    });
    expect(res.status).toBe(200);
    const body = (await res.json()) as { status: string; results: Array<Record<string, string>> };
    expect(body.status).toBe("ended");
    const byId = new Map(body.results.map((r) => [r.custom_id, r]));
    expect(byId.get("a")?.text).toBe("A-label");
    expect(byId.get("b")?.text).toBe("B-label");
    expect(byId.get("c")?.error).toBe("errored");
    expect(rig.gateway.resultsCalls).toEqual(["rb_1"]);
  });

  it("answers a retryable 503, not an aborted 200, when results cannot be read", async () => {
    const rig = await makeRig();
    await rig.usage.attachBatch("rb_1", "lic_test_1", "2026-08-13", 1, "dev_test_device");
    rig.gateway.status = { id: "rb_1", processing_status: "ended", request_counts: { processing: 0, succeeded: 1, errored: 0, canceled: 0, expired: 0 } };
    // eslint-disable-next-line require-yield
    rig.gateway.streamResults = async function* () { throw new UpstreamError("results", 404); };
    const res = await rig.app.request("/v1/batch/rb_1", { headers: { authorization: `Bearer ${rig.token}` } });
    expect(res.status).toBe(410);
    rig.gateway.streamResults = async function* () { throw new UpstreamError("results", 500); };
    expect((await rig.app.request("/v1/batch/rb_1", { headers: { authorization: `Bearer ${rig.token}` } })).status).toBe(503);
  });

  it("answers an all-errored batch with no result file as ended and failed, not 410", async () => {
    const rig = await makeRig();
    await rig.usage.attachBatch("rb_1", "lic_test_1", "2026-08-13", 1, "dev_test_device");
    rig.gateway.status = { id: "rb_1", processing_status: "ended", request_counts: { processing: 0, succeeded: 0, errored: 1, canceled: 0, expired: 0 } };
    // eslint-disable-next-line require-yield
    rig.gateway.streamResults = async function* () { throw new UpstreamError("results", 404); };
    const res = await rig.app.request("/v1/batch/rb_1", { headers: { authorization: `Bearer ${rig.token}` } });
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({ status: "ended", state: "failed", results: [] });
  });

  it("releases provider result files only after the device has fully read them", async () => {
    const rig = await makeRig();
    await rig.usage.attachBatch("rb_1", "lic_test_1", "2026-08-13", 1, "dev_test_device");
    rig.gateway.status = { id: "rb_1", processing_status: "ended", request_counts: { processing: 0, succeeded: 1, errored: 0, canceled: 0, expired: 0 } };
    rig.gateway.resultLines = [JSON.stringify({ custom_id: "a", result: { type: "succeeded", message: { content: [{ type: "text", text: "x" }] } } })];
    const released: string[] = [];
    rig.gateway.releaseResults = (id: string) => { released.push(id); return Promise.resolve(); };
    const res = await rig.app.request("/v1/batch/rb_1", { headers: { authorization: `Bearer ${rig.token}` } });
    await res.text();
    expect(released).toEqual(["rb_1"]);
  });

  it("hands the provider cleanup to waitUntil before the results response completes (D11)", async () => {
    // On Workers a promise still pending after the response completes may be cancelled, so the
    // deletion must not run after controller.close() unprotected.
    const rig = await makeRig();
    await rig.usage.attachBatch("rb_1", "lic_test_1", "2026-08-13", 1, "dev_test_device");
    rig.gateway.status = { id: "rb_1", processing_status: "ended", request_counts: { processing: 0, succeeded: 1, errored: 0, canceled: 0, expired: 0 } };
    rig.gateway.resultLines = [JSON.stringify({ custom_id: "a", result: { type: "succeeded", message: { content: [{ type: "text", text: "x" }] } } })];
    let release: () => void = () => undefined;
    const started: string[] = [];
    rig.gateway.releaseResults = (id: string) => { started.push(id); return new Promise<void>((done) => { release = done; }); };
    const handed: Promise<unknown>[] = [];
    const ctx = { waitUntil: (p: Promise<unknown>) => { handed.push(p); }, passThroughOnException: () => undefined, props: {} };
    const res = await rig.app.request("/v1/batch/rb_1", { headers: { authorization: `Bearer ${rig.token}` } }, undefined, ctx);
    // The body completes while the deletion is still pending: it was not awaited inline...
    const body = JSON.parse(await res.text()) as { results: unknown[] };
    expect(body.results).toHaveLength(1);
    expect(started).toEqual(["rb_1"]);
    // ...but handed to the runtime, which keeps it alive past the response.
    expect(handed).toHaveLength(1);
    release();
    await Promise.all(handed);
  });

  it("without an execution context (Node), finishes the provider cleanup before closing the stream", async () => {
    const rig = await makeRig();
    await rig.usage.attachBatch("rb_1", "lic_test_1", "2026-08-13", 1, "dev_test_device");
    rig.gateway.status = { id: "rb_1", processing_status: "ended", request_counts: { processing: 0, succeeded: 1, errored: 0, canceled: 0, expired: 0 } };
    rig.gateway.resultLines = [JSON.stringify({ custom_id: "a", result: { type: "succeeded", message: { content: [{ type: "text", text: "x" }] } } })];
    const order: string[] = [];
    rig.gateway.releaseResults = async () => { await Promise.resolve(); order.push("released"); };
    const res = await rig.app.request("/v1/batch/rb_1", { headers: { authorization: `Bearer ${rig.token}` } });
    await res.text();
    order.push("body complete");
    expect(order).toEqual(["released", "body complete"]);
  });

  it("a failed provider cleanup is a fixed log line and never fails the read", async () => {
    const rig = await makeRig();
    await rig.usage.attachBatch("rb_1", "lic_test_1", "2026-08-13", 1, "dev_test_device");
    rig.gateway.status = { id: "rb_1", processing_status: "ended", request_counts: { processing: 0, succeeded: 1, errored: 0, canceled: 0, expired: 0 } };
    rig.gateway.resultLines = [JSON.stringify({ custom_id: "a", result: { type: "succeeded", message: { content: [{ type: "text", text: "x" }] } } })];
    rig.gateway.releaseResults = () => Promise.reject(new Error("file-abc123 gone"));
    const res = await rig.app.request("/v1/batch/rb_1", { headers: { authorization: `Bearer ${rig.token}` } });
    expect(res.status).toBe(200);
    expect((JSON.parse(await res.text()) as { results: unknown[] }).results).toHaveLength(1);
    expect(rig.logLines).toContain("batch_cleanup_failure");
    expect(rig.logLines.join()).not.toContain("file-abc123");
  });
});
