import { afterEach, expect, it, vi } from "vitest";
import { OpenAiBatchGateway, RoutingGateway, fromOutputLine, toChatLine } from "../src/openai-gateway.js";
import { parseResultLine } from "../src/app.js";
import type { AnthropicBatchRequestItem } from "../src/types.js";

afterEach(() => vi.unstubAllGlobals());

const req = (extra: Partial<AnthropicBatchRequestItem["params"]> = {}): AnthropicBatchRequestItem => ({
  custom_id: "c1",
  params: { model: "gpt-6-luna", max_tokens: 100, messages: [{ role: "user", content: "hi" }], output_config: { effort: "low" }, ...extra },
});

it("routes every class, the brief included, to luna; config selects the fallback", async () => {
  vi.resetModules();
  vi.stubEnv("BATCH_PROVIDER", "openai");
  const { MODEL_BY_CLASS, MIN_CACHEABLE_TOKENS_BY_CLASS, MAX_TOKENS_BY_CLASS } = await import("../src/models.js");
  expect(MIN_CACHEABLE_TOKENS_BY_CLASS.classify).toBe(Infinity);
  expect(MIN_CACHEABLE_TOKENS_BY_CLASS.brief).toBe(Infinity);
  expect(MAX_TOKENS_BY_CLASS.brief).toBe(16384);
  for (const c of ["classify", "summarize", "brief", "about_profile", "daily_activity_summary"] as const) expect(MODEL_BY_CLASS[c]).toBe("gpt-6-luna");
  vi.resetModules();
  vi.stubEnv("BATCH_PROVIDER", "anthropic");
  const fallback = (await import("../src/models.js")).MODEL_BY_CLASS;
  expect(fallback.classify).toBe("claude-haiku-5-5");
  expect(fallback.brief).toBe("claude-sonnet-5-5");
  vi.unstubAllEnvs();
});

it("translates a request into a chat completions batch line", () => {
  const line = JSON.parse(toChatLine(req({
    system: [{ type: "text", text: "sys", cache_control: { type: "ephemeral", ttl: "1h" } }],
    output_config: { effort: "medium", format: { type: "json_schema", schema: { type: "object" } } },
  })));
  expect(line).toMatchObject({ custom_id: "c1", method: "POST", url: "/v1/chat/completions" });
  // The relay's per-class effort is forwarded as `reasoning_effort`; a request naming none gets `low`.
  expect(line.body).toMatchObject({ model: "gpt-6-luna", reasoning_effort: "medium", max_completion_tokens: 100, store: false });
  expect(JSON.parse(toChatLine(req())).body.reasoning_effort).toBe("low");
  expect(line.body.messages).toEqual([{ role: "system", content: "sys" }, { role: "user", content: "hi" }]);
  expect(line.body.response_format.json_schema.schema).toEqual({ type: "object" });
});

it("maps output lines back to the result shape the device reads", () => {
  const ok = JSON.stringify({ custom_id: "c1", response: { status_code: 200, body: { model: "gpt-6-luna",
    choices: [{ message: { content: "{\"a\":1}" } }],
    usage: { prompt_tokens: 50, completion_tokens: 7, prompt_tokens_details: { cached_tokens: 20 } } } } });
  expect(parseResultLine(fromOutputLine(ok))).toEqual({
    result: { custom_id: "c1", text: "{\"a\":1}", usage: { input_tokens: 30, output_tokens: 7, cache_read_input_tokens: 20, cache_creation_input_tokens: 0 } },
    model: "gpt-6-luna",
  });
  const bad = JSON.stringify({ custom_id: "c2", response: null, error: { code: "x" } });
  expect(parseResultLine(fromOutputLine(bad)).result).toEqual({ custom_id: "c2", error: "errored" });
});

it("uploads, creates, polls and downloads, with prefixed ids", async () => {
  const calls: string[] = [];
  vi.stubGlobal("fetch", vi.fn(async (url: string, init: RequestInit = {}) => {
    const path = new URL(url).pathname;
    calls.push(`${init.method ?? "GET"} ${path}`);
    if (path === "/v1/files") { expect(init.body).toBeInstanceOf(FormData); return Response.json({ id: "file_in" }); }
    if (path === "/v1/batches") {
      expect(JSON.parse(init.body as string)).toMatchObject({ input_file_id: "file_in", endpoint: "/v1/chat/completions" });
      return Response.json({ id: "batch_1", status: "validating", request_counts: { total: 1, completed: 0, failed: 0 } });
    }
    if (path === "/v1/batches/batch_1") return Response.json({ id: "batch_1", status: "completed", output_file_id: "file_out", error_file_id: "file_err", request_counts: { total: 2, completed: 1, failed: 1 } });
    if (path === "/v1/files/file_out/content") return new Response(JSON.stringify({ custom_id: "a", response: { status_code: 200, body: { choices: [{ message: { content: "t" } }], usage: {} } } }) + "\n");
    if (path === "/v1/files/file_err/content") return new Response(JSON.stringify({ custom_id: "b", error: { code: "e" } }) + "\n");
    return new Response("no", { status: 404 });
  }));
  const gw = new OpenAiBatchGateway("k");
  const created = await gw.createBatch([req()]);
  expect(created).toMatchObject({ id: "openai_batch_1", processing_status: "in_progress" });
  const polled = await gw.getBatch("openai_batch_1");
  expect(polled).toMatchObject({ processing_status: "ended", request_counts: { succeeded: 1, errored: 1, processing: 0 } });
  const out: string[] = [];
  for await (const l of gw.streamResults("openai_batch_1")) out.push(l);
  expect(out.map((l) => parseResultLine(l).result)).toEqual([
    { custom_id: "a", text: "t", usage: { input_tokens: 0, output_tokens: 0, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 } },
    { custom_id: "b", error: "errored" },
  ]);
  expect(calls[0]).toBe("POST /v1/files");
});

it("routes create by model and recovery/poll by id prefix", async () => {
  const mk = (tag: string) => ({
    createBatch: vi.fn(async () => ({ id: tag, processing_status: "in_progress", request_counts: { processing: 0, succeeded: 0, errored: 0, canceled: 0, expired: 0 } })),
    getBatch: vi.fn(async (id: string) => ({ id, processing_status: "ended", request_counts: { processing: 0, succeeded: 0, errored: 0, canceled: 0, expired: 0 } })),
    streamResults: vi.fn(async function* () { yield tag; }),
  });
  const a = mk("anthropic"), o = mk("openai_x");
  const r = new RoutingGateway(a, o);
  expect((await r.createBatch([req()])).id).toBe("openai_x");
  expect((await r.createBatch([req({ model: "claude-sonnet-5-5" })])).id).toBe("anthropic");
  await r.getBatch("openai_batch_9"); await r.getBatch("msgbatch_9");
  expect(o.getBatch).toHaveBeenCalledWith("openai_batch_9");
  expect(a.getBatch).toHaveBeenCalledWith("msgbatch_9");
  await expect(new RoutingGateway(a, undefined).createBatch([req()])).rejects.toThrow();
});

function fileApi(batch: Record<string, unknown>, files: Record<string, string> = {}) {
  const deleted: string[] = [];
  vi.stubGlobal("fetch", vi.fn(async (url: string, init: RequestInit = {}) => {
    const path = new URL(url).pathname;
    if (init.method === "DELETE") { deleted.push(path.split("/").pop() as string); return Response.json({ deleted: true }); }
    if (path.startsWith("/v1/batches/")) return Response.json(batch);
    const m = /^\/v1\/files\/(.+)\/content$/.exec(path);
    if (m) return new Response(files[m[1] as string] ?? "");
    return new Response("no", { status: 404 });
  }));
  return deleted;
}
const line = JSON.stringify({ custom_id: "a", response: { status_code: 200, body: { choices: [{ message: { content: "t" } }], usage: {} } } }) + "\n";
const drain = async (gw: OpenAiBatchGateway, id: string) => { for await (const _ of gw.streamResults(id)) void _; };

it("deletes only the input after a full read; result files stay for the device's own read", async () => {
  const deleted = fileApi({ id: "b", status: "completed", input_file_id: "fi", output_file_id: "fo", error_file_id: "fe" }, { fo: line, fe: "" });
  await drain(new OpenAiBatchGateway("k"), "openai_b");
  expect(deleted).toEqual(["fi"]);
});

it("releaseResults deletes output+error files; sweep-style read before it leaves them readable", async () => {
  const deleted = fileApi({ id: "b", status: "completed", input_file_id: "fi", output_file_id: "fo", error_file_id: "fe" }, { fo: line, fe: "" });
  const gw = new OpenAiBatchGateway("k");
  await drain(gw, "openai_b"); // sweep reads first
  expect(deleted).toEqual(["fi"]);
  await drain(gw, "openai_b"); // device reads after: files still there
  await gw.releaseResults("openai_b");
  expect(deleted.sort()).toEqual(["fe", "fi", "fi", "fo"]);
});

it("deletes the input of failed/expired/cancelled batches that have no output", async () => {
  for (const status of ["failed", "expired", "cancelled"]) {
    const deleted = fileApi({ id: "b", status, input_file_id: "fi" });
    await drain(new OpenAiBatchGateway("k"), "openai_b");
    expect(deleted).toEqual(["fi"]);
  }
});

it("keeps the files when the reader stops early", async () => {
  const deleted = fileApi({ id: "b", status: "completed", input_file_id: "fi", output_file_id: "fo" }, { fo: line + line });
  const it = new OpenAiBatchGateway("k").streamResults("openai_b")[Symbol.asyncIterator]();
  await it.next(); await it.return?.();
  expect(deleted).toEqual([]);
});

it("logs a failed delete by status only, never a file or batch id or a body", async () => {
  const logs: string[] = [];
  vi.stubGlobal("fetch", vi.fn(async (url: string, init: RequestInit = {}) =>
    init.method === "DELETE" ? new Response("secret body", { status: 500 }) : Response.json({ id: "b", status: "failed", input_file_id: "file-abc123" })));
  await drain(new OpenAiBatchGateway("k", undefined, (l) => logs.push(l)), "openai_b");
  expect(logs).toEqual(["openai_file_delete_failed status=500"]);
  // A transport failure: the same line, status 0.
  logs.length = 0;
  vi.stubGlobal("fetch", vi.fn(async (url: string, init: RequestInit = {}) => {
    if (init.method === "DELETE") throw new Error("socket hang up file-abc123");
    return Response.json({ id: "b", status: "failed", input_file_id: "file-abc123" });
  }));
  await drain(new OpenAiBatchGateway("k", undefined, (l) => logs.push(l)), "openai_b");
  expect(logs).toEqual(["openai_file_delete_failed status=0"]);
  expect(logs.join()).not.toMatch(/file-abc123|secret/);
});

it("sets minimum retention on upload and batch, with our file-name prefix", async () => {
  let form: FormData | undefined; let batchBody: Record<string, unknown> = {};
  vi.stubGlobal("fetch", vi.fn(async (url: string, init: RequestInit = {}) => {
    const path = new URL(url).pathname;
    if (path === "/v1/files") { form = init.body as FormData; return Response.json({ id: "fi" }); }
    batchBody = JSON.parse(init.body as string);
    return Response.json({ id: "b", status: "validating" });
  }));
  await new OpenAiBatchGateway("k").createBatch([req()]);
  expect((form?.get("file") as File).name).toMatch(/^shogun-relay-.+\.jsonl$/);
  expect(form?.get("expires_after[seconds]")).toBe("172800");
  expect(batchBody.output_expires_after).toEqual({ anchor: "created_at", seconds: 86400 });
});

it("sweeps old terminal batches with our prefix and leaves others alone", async () => {
  const now = 1_000_000;
  const deleted: string[] = [];
  vi.stubGlobal("fetch", vi.fn(async (url: string, init: RequestInit = {}) => {
    const path = new URL(url).pathname;
    if (init.method === "DELETE") { deleted.push(path.split("/").pop() as string); return Response.json({}); }
    if (path === "/v1/batches") return Response.json({ data: [
      { id: "old", status: "cancelled", created_at: now - 90_000, input_file_id: "oi", output_file_id: "oo" },
      { id: "fresh", status: "completed", created_at: now - 100, input_file_id: "fi" },
      { id: "running", status: "in_progress", created_at: now - 90_000, input_file_id: "ri" },
      { id: "foreign", status: "completed", created_at: now - 90_000, input_file_id: "xi" },
    ] });
    if (path === "/v1/files/oi") return Response.json({ filename: "shogun-relay-1.jsonl" });
    if (path === "/v1/files/xi") return Response.json({ filename: "someone-else.jsonl" });
    return new Response("no", { status: 404 });
  }));
  expect(await new OpenAiBatchGateway("k").sweepOrphans(now)).toBe(1);
  expect(deleted.sort()).toEqual(["oi", "oo"]);
});
