/** E37: the router's request metrics and spans carry closed-set labels only — never a body, an
 * input, a tool argument, a key or a caller-chosen method or tool name. */
import { describe, expect, it } from "vitest";

import { createApp } from "../src/app.js";
import { routeTemplate } from "../src/logging.js";
import { METRIC_LABELS as API_METRIC_LABELS, SPAN_ATTRIBUTES as API_SPAN_ATTRIBUTES } from "../../api/src/observability.js";
import { METRIC_LABELS, OpsMetrics, OtlpConfigError, OtlpTraceExporter, SPAN_ATTRIBUTES, otlpSettingsFromEnv } from "../src/observability.js";
import { createMetricsAuthorizer, createOtlpPoster, parseOtlpHeaders } from "../src/transport.js";
import { AGENT_KEY, makeDeps } from "./helpers.js";

const IN = "SENTINEL-INPUT-31be";
const OUT = "SENTINEL-OUTPUT-4c0d";

describe("router telemetry", () => {
  it("uses the same label and attribute names as the gateway", () => {
    expect([...METRIC_LABELS]).toEqual([...API_METRIC_LABELS]);
    expect([...SPAN_ATTRIBUTES]).toEqual([...API_SPAN_ATTRIBUTES]);
  });

  it("counts REST and MCP calls by verb, with no body, input, key or unknown name", async () => {
    const deps = makeDeps({ responder: () => ({ status: 200, body: { status: "succeeded", output: { results: [{ title: OUT }] } } }) });
    const metrics = new OpsMetrics();
    const posted: string[] = [];
    const exporter = new OtlpTraceExporter({ serviceName: "router-test", sampleRatio: 1 }, async (b) => { posted.push(b); return { status: 200 }; }, metrics);
    const app = createApp({ ...deps, ops: { metrics, exporter, authorize: createMetricsAuthorizer({ ROUTER_METRICS_TOKEN: "ops-token" }) } });
    const headers = { "content-type": "application/json", authorization: `Bearer ${AGENT_KEY}` };

    await app.request("/v1/router/run", { method: "POST", headers, body: JSON.stringify({ tool_id: "monid.tinyfish.web-search", input: { query: IN }, idempotency_key: "o1" }) });
    await app.request("/mcp", { method: "POST", headers, body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name: "router.discover", arguments: { task: IN } } }) });
    await app.request("/mcp", { method: "POST", headers, body: JSON.stringify({ jsonrpc: "2.0", id: 2, method: `${IN}/method`, params: {} }) });
    await app.request(`/v1/catalog/${IN}`);

    expect((await app.request("/metrics")).status).toBe(401);
    const res = await app.request("/metrics", { headers: { authorization: "Bearer ops-token" } });
    expect(res.status).toBe(200);
    const text = await res.text();
    expect(text).toContain('route="/v1/router/run",method="POST",status="200",mcp_method="tools/call",mcp_name="router.run"');
    expect(text).toContain('route="/mcp",method="POST",status="200",mcp_method="tools/call",mcp_name="router.discover"');
    expect(text).toContain('mcp_method="other",mcp_name="none"');
    expect(text).toContain('route="/v1/catalog/:tool_id"');
    await exporter.flush();
    const dumped = text + posted.join("\n");
    for (const s of [IN, OUT, AGENT_KEY, "ops-token", "query", "monid.tinyfish"]) expect(dumped).not.toContain(s);
  });

  it("answers 404 for /metrics without a token configured", async () => {
    expect((await createApp(makeDeps()).request("/metrics")).status).toBe(404);
    const app = createApp({ ...makeDeps(), ops: { metrics: new OpsMetrics(), authorize: createMetricsAuthorizer({}) } });
    expect((await app.request("/metrics", { headers: { authorization: "Bearer x" } })).status).toBe(404);
  });

  it("export is off by default; the poster sends the collector headers only to the collector", async () => {
    expect(otlpSettingsFromEnv({})).toBeUndefined();
    expect(() => otlpSettingsFromEnv({ OTEL_EXPORTER_OTLP_ENDPOINT: "http://remote.example" })).toThrow(OtlpConfigError);
    expect(otlpSettingsFromEnv({ OTEL_EXPORTER_OTLP_ENDPOINT: "http://localhost:4318" })?.tracesUrl).toBe("http://localhost:4318/v1/traces");
    expect(parseOtlpHeaders("x-honeycomb-team=abc%3D")).toEqual({ "x-honeycomb-team": "abc=" });
    expect(() => parseOtlpHeaders("SENTINEL")).toThrow(/OTEL_EXPORTER_OTLP_HEADERS/);
    const seen: Array<{ url: string; auth: string | null }> = [];
    const post = createOtlpPoster({
      env: { OTEL_EXPORTER_OTLP_HEADERS: "authorization=Bearer%20collector" },
      url: "https://collector.example/v1/traces",
      fetch: (async (url: string | URL | Request, init?: RequestInit) => {
        seen.push({ url: String(url), auth: new Headers(init?.headers).get("authorization") });
        return new Response("ignored", { status: 200 });
      }) as typeof fetch,
    });
    expect(await post("{}")).toEqual({ status: 200 });
    expect(seen).toEqual([{ url: "https://collector.example/v1/traces", auth: "Bearer collector" }]);
  });

  it("route templates collapse every catalog id", () => {
    expect(routeTemplate("/v1/catalog/anything-at-all")).toBe("/v1/catalog/:tool_id");
    expect(routeTemplate("/v1/catalog/a/b")).toBe("/unmatched");
  });
});
