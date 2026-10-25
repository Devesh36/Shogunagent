import { afterEach, expect, it, vi } from "vitest";
import { FetchAnthropicGateway, MAX_RESULT_LINE_BYTES, UpstreamError } from "../src/gateway.js";
import { makeRig, postBatch, sampleBody } from "./helpers.js";

afterEach(() => vi.unstubAllGlobals());
it("bounds a result line and closes its upstream stream on failure", async () => {
  const cancel = vi.fn();
  let sent = false;
  vi.stubGlobal("fetch", vi.fn(async (_url, options: RequestInit) => {
    expect(options.signal).toBeInstanceOf(AbortSignal);
    return new Response(new ReadableStream({
      pull(controller) {
        if (!sent) { sent = true; controller.enqueue(new TextEncoder().encode("x".repeat(MAX_RESULT_LINE_BYTES + 1))); }
      }, cancel,
    }));
  }));
  const iterator = new FetchAnthropicGateway("provider-key").streamResults("provider-job")[Symbol.asyncIterator]();
  await expect(iterator.next()).rejects.toBeInstanceOf(UpstreamError);
  expect(cancel).toHaveBeenCalledOnce();
});
it("deletes an Anthropic batch once the device has read it, and treats an already-deleted batch as done", async () => {
  const calls: Array<{ url: string; method: string | undefined; key: string | undefined }> = [];
  const statuses = [200, 404, 500];
  vi.stubGlobal("fetch", vi.fn(async (url: string, options: RequestInit) => {
    calls.push({ url, method: options.method, key: (options.headers as Record<string, string>)["x-api-key"] });
    return new Response("{}", { status: statuses.shift() ?? 500 });
  }));
  const gateway = new FetchAnthropicGateway("provider-key", "https://provider.test");
  await gateway.releaseResults("msgbatch_1");
  await gateway.releaseResults("msgbatch_2");
  await expect(gateway.releaseResults("msgbatch_3")).rejects.toBeInstanceOf(UpstreamError);
  expect(calls).toEqual([
    { url: "https://provider.test/v1/messages/batches/msgbatch_1", method: "DELETE", key: "provider-key" },
    { url: "https://provider.test/v1/messages/batches/msgbatch_2", method: "DELETE", key: "provider-key" },
    { url: "https://provider.test/v1/messages/batches/msgbatch_3", method: "DELETE", key: "provider-key" },
  ]);
});
it("does not log identifiers supplied through known or unknown URL paths", async () => {
  const rig = await makeRig();
  for (const path of ["/v1/batch/PROMPT-SECRET", "/BEARER-SECRET"]) await rig.app.request(path, { headers: { authorization: `Bearer ${rig.token}` } });
  const logs = rig.logLines.join("\n");
  expect(logs).not.toContain("PROMPT-SECRET");
  expect(logs).not.toContain("BEARER-SECRET");
});
it("rejects unknown purposes and duplicate item IDs before reserving", async () => {
  const rig = await makeRig();
  for (const body of [{ ...sampleBody, purpose: "user transcript" }, { ...sampleBody, items: [sampleBody.items[0], sampleBody.items[0]] }]) {
    expect((await postBatch(rig, body)).status).toBe(400);
  }
  expect(rig.gateway.createCalls).toHaveLength(0);
});
