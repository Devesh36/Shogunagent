import { describe, expect, it } from "vitest";
import { liveConfig, LIVE_LANES } from "../../src/live-config.js";
import { upstreamBody, liveStream } from "../../src/live-app.js";
import worker, { resetBootStateForTests } from "../../src/worker.js";
import { createExecutionContext, waitOnExecutionContext } from "cloudflare:test";
import releaseKey from "../../../../packages/config/license-public-key.json";

function config() {
  return liveConfig({ OPENAI_LIVE_API_KEY: "test-live", GROQ_LIVE_API_KEY: "test-groq", LIVE_HMAC_KEY: "test-hmac-key-32-bytes-long-string", LIVE_GLOBAL_DAILY_CAP_USD: "10", LIVE_LANE_CONFIG_JSON: JSON.stringify(Object.fromEntries(LIVE_LANES.map(lane => [lane, { model: "gpt-6-luna", effort: "low", maxOutputTokens: 100, inputMicrosPerToken: 1, outputMicrosPerToken: 2 }]))) });
}
describe("live AI in workerd", () => {
  it("builds HMAC identifiers and priority request with no client overrides", () => {
    const c = config(); const body = upstreamBody({ input: "hello", store: true }, "rewrite", c.policies.rewrite, "lic_test", c.hmacKey);
    expect(body).toMatchObject({ store: false, service_tier: "priority", reasoning: { effort: "low" } });
    expect(body?.safety_identifier).toMatch(/^[a-f0-9]{64}$/);
  });
  it("streams and settles usage in the deployed runtime", async () => {
    const c = config(); const costs: Array<number | undefined> = [];
    const upstream = new Response('data: {"type":"response.completed","response":{"usage":{"input_tokens":4,"output_tokens":5}}}\n\n').body!;
    const out = new Response(liveStream(upstream, c.policies.chat, new AbortController(), async cost => { costs.push(cost); }));
    expect(await out.text()).toContain("response.completed"); expect(costs).toEqual([14]);
  });
  it("leaves live routes disabled on existing valid Batch deployments", async () => {
    resetBootStateForTests(); const ctx = createExecutionContext();
    const env = { ANTHROPIC_API_KEY: "test", OPENAI_API_KEY: "test", BATCH_PROVIDER: "anthropic", LICENSE_PUBKEY_B64: releaseKey.ed25519, DAILY_SPEND_CAP_USD: "10", HYPERDRIVE: { connectionString: "postgres://test:test@127.0.0.1:9/test" } };
    const response = await worker.fetch(new Request("https://api.shogunaios.com/v1/ai/chat", { method: "POST" }), env, ctx);
    expect(response.status).toBe(503); expect(await response.json()).toEqual({ error: "live_not_enabled" });
    await waitOnExecutionContext(ctx); resetBootStateForTests();
  });
  it("leaves the Jev form decision route on the Batch relay app (#1144)", async () => {
    resetBootStateForTests(); const ctx = createExecutionContext();
    const env = { ANTHROPIC_API_KEY: "test", OPENAI_API_KEY: "test", BATCH_PROVIDER: "anthropic", LICENSE_PUBKEY_B64: releaseKey.ed25519, DAILY_SPEND_CAP_USD: "10", HYPERDRIVE: { connectionString: "postgres://test:test@127.0.0.1:9/test" } };
    const response = await worker.fetch(new Request("https://api.shogunaios.com/v1/ai/form_decide", { method: "POST" }), env, ctx);
    expect(response.status).toBe(401);
    await waitOnExecutionContext(ctx); resetBootStateForTests();
  });
});
