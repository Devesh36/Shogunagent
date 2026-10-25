/** E37: body-free operational telemetry. Every label is a closed-set value; nothing a caller sends
 * (body, bearer, an unknown method or tool name, an id in the path) reaches a metric or a span. */
import { Hono } from "hono";
import { describe, expect, it } from "vitest";

import { routeTemplate } from "../src/logging.js";
import {
  KNOWN_TOOL_NAMES, MAX_PENDING_SPANS, MAX_SERIES, METRIC_LABELS, OpsMetrics, OtlpConfigError, OtlpTraceExporter, SPAN_ATTRIBUTES,
  mcpMethodLabel, mcpNameLabel, opsTelemetry, otlpConfigFromEnv, type OtlpConfig, type RequestLabels,
} from "../src/observability.js";
import { BootError, newMetrics, newTraceExporter, relayConfigFromEnv } from "../src/boot.js";
import releaseKey from "../../../packages/config/license-public-key.json";
import { makeRig, postBatch, sampleBody } from "./helpers.js";

const BODY_SENTINEL = "SENTINEL-BODY-9c41";
const KEY_SENTINEL = "SENTINEL-KEY-77ab";
const NAME_SENTINEL = "secret.SENTINEL_NAME";
const PATH_SENTINEL = "rb_SENTINELBATCHID";

const otlp: OtlpConfig = { tracesUrl: "https://collector.example/v1/traces", headers: { authorization: "Bearer collector" }, serviceName: "test", sampleRatio: 1 };

function labels(over: Partial<RequestLabels> = {}): RequestLabels {
  return { route: "/mcp", method: "POST", status: "200", mcp_method: "tools/call", mcp_name: "memory.search", ...over };
}

describe("closed-set labels", () => {
  it("names known MCP methods and tools, and collapses everything else", () => {
    expect(mcpMethodLabel("tools/call")).toBe("tools/call");
    expect(mcpMethodLabel(undefined)).toBe("none");
    expect(mcpMethodLabel("  ")).toBe("none");
    expect(mcpMethodLabel(BODY_SENTINEL)).toBe("other");
    expect(mcpNameLabel("memory.search")).toBe("memory.search");
    expect(mcpNameLabel("state.commitments.list")).toBe("state.commitments.list");
    expect(mcpNameLabel("router.run")).toBe("router.run");
    expect(mcpNameLabel(NAME_SENTINEL)).toBe("other");
    expect(mcpNameLabel(null)).toBe("none");
    // The 21 core tools are all known.
    expect(KNOWN_TOOL_NAMES.size).toBeGreaterThanOrEqual(21);
  });

  it("random caller strings never become a label value", () => {
    let seed = 0x5eed;
    const rnd = (): number => { seed = (seed * 1103515245 + 12345) & 0x7fffffff; return seed; };
    for (let i = 0; i < 2000; i++) {
      const len = rnd() % 40;
      const s = Array.from({ length: len }, () => String.fromCharCode(32 + (rnd() % 95))).join("");
      const m = mcpMethodLabel(s);
      const n = mcpNameLabel(s);
      expect(m === "none" || m === "other" || m === s.trim()).toBe(true);
      expect(n === "none" || n === "other" || KNOWN_TOOL_NAMES.has(n)).toBe(true);
    }
  });
});

describe("relay /metrics in Prometheus text", () => {
  it("is behind the metrics bearer, and carries no body, key, id or unknown name", async () => {
    const rig = await makeRig({ metricsToken: "ops-secret" });
    await postBatch(rig, { ...sampleBody, items: [{ custom_id: "c1", chunk: BODY_SENTINEL }] });
    await rig.app.request("/v1/batch", { method: "POST", headers: { authorization: `Bearer ${KEY_SENTINEL}`, "mcp-method": BODY_SENTINEL, "mcp-name": NAME_SENTINEL, "content-type": "application/json" }, body: JSON.stringify({ x: BODY_SENTINEL }) });
    await rig.app.request(`/v1/batch/${PATH_SENTINEL}?q=${BODY_SENTINEL}`, { headers: { authorization: `Bearer ${KEY_SENTINEL}` } });
    await rig.app.request("/v1/batch", { method: "POST", headers: { authorization: `Bearer ${rig.token}`, "mcp-method": "tools/call", "mcp-name": "memory.search", "content-type": "application/json" }, body: "{}" });

    expect((await rig.app.request("/metrics?format=prometheus")).status).toBe(401);
    const res = await rig.app.request("/metrics?format=prometheus", { headers: { authorization: "Bearer ops-secret" } });
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toContain("text/plain");
    const text = await res.text();
    for (const s of [BODY_SENTINEL, KEY_SENTINEL, NAME_SENTINEL, PATH_SENTINEL, "ops-secret", rig.token, "lic_test_1"]) expect(text).not.toContain(s);
    expect(text).toContain('route="/v1/batch/:id"');
    expect(text).toContain('mcp_method="other",mcp_name="other"');
    expect(text).toContain('mcp_method="tools/call",mcp_name="memory.search"');
    expect(text).toMatch(/shogun_gateway_http_requests_total\{route="\/v1\/batch",method="POST",status="401",mcp_method="other",mcp_name="other"\} 1/);
    expect(text).toContain("shogun_gateway_http_request_duration_ms_bucket{");
    // Only the five labels (plus `le` on buckets) ever appear.
    for (const m of text.matchAll(/\{([^}]*)\}/g)) {
      const keys = m[1]!.split(/,(?=[a-z_]+=")/).map((kv) => kv.split("=")[0]);
      for (const k of keys) expect([...METRIC_LABELS, "le"]).toContain(k);
    }
    // The JSON snapshot is still the default, and `Accept: text/plain` also selects the text.
    expect((await rig.app.request("/metrics", { headers: { authorization: "Bearer ops-secret" } })).headers.get("content-type")).toContain("application/json");
    expect((await rig.app.request("/metrics", { headers: { authorization: "Bearer ops-secret", accept: "text/plain" } })).headers.get("content-type")).toContain("text/plain");
  });

  it("answers 404 when no metrics token is configured", async () => {
    const rig = await makeRig();
    expect((await rig.app.request("/metrics?format=prometheus")).status).toBe(404);
  });
});

describe("OpsMetrics", () => {
  it("bounds the number of series", () => {
    const m = new OpsMetrics();
    for (let i = 0; i < MAX_SERIES + 10; i++) m.observe(labels({ route: `/r${i}` }), 1);
    expect(m.seriesCount).toBe(MAX_SERIES);
    expect(m.render()).toContain("shogun_gateway_metrics_series_overflow_total 10");
  });

  it("escapes label values even though they come from closed sets", () => {
    const m = new OpsMetrics();
    m.observe(labels({ route: 'a"b\\c\nd' }), 3);
    expect(m.render()).toContain('route="a\\"b\\\\c\\nd"');
  });
});

describe("OTLP export (opt-in)", () => {
  it("is off without an endpoint and validates what is set, never echoing a value", () => {
    expect(otlpConfigFromEnv({}, "svc")).toBeUndefined();
    expect(otlpConfigFromEnv({ OTEL_EXPORTER_OTLP_ENDPOINT: "https://otel.example:4318/" }, "svc")).toMatchObject({ tracesUrl: "https://otel.example:4318/v1/traces", serviceName: "svc", sampleRatio: 1 });
    expect(otlpConfigFromEnv({ OTEL_EXPORTER_OTLP_TRACES_ENDPOINT: "http://127.0.0.1:4318/v1/traces" }, "svc")?.tracesUrl).toBe("http://127.0.0.1:4318/v1/traces");
    expect(otlpConfigFromEnv({ OTEL_EXPORTER_OTLP_ENDPOINT: "https://o.example", OTEL_EXPORTER_OTLP_HEADERS: "Authorization=Bearer%20abc,x-team=ops", OTEL_TRACES_SAMPLER_ARG: "0.25" }, "svc"))
      .toMatchObject({ headers: { authorization: "Bearer abc", "x-team": "ops" }, sampleRatio: 0.25 });
    const refused = (env: Record<string, string>): string => {
      try { otlpConfigFromEnv(env, "svc"); return "accepted"; } catch (e) { expect(e).toBeInstanceOf(OtlpConfigError); return (e as Error).message; }
    };
    expect(refused({ OTEL_EXPORTER_OTLP_ENDPOINT: "http://otel.example" })).toContain("https is required");
    expect(refused({ OTEL_EXPORTER_OTLP_ENDPOINT: "not a url" })).toContain("OTEL_EXPORTER_OTLP_ENDPOINT");
    expect(refused({ OTEL_EXPORTER_OTLP_ENDPOINT: "https://u:SENTINELPW@o.example" })).not.toContain("SENTINELPW");
    expect(refused({ OTEL_EXPORTER_OTLP_ENDPOINT: "https://o.example", OTEL_EXPORTER_OTLP_HEADERS: "SENTINELHDR" })).not.toContain("SENTINELHDR");
    expect(refused({ OTEL_EXPORTER_OTLP_ENDPOINT: "https://o.example", OTEL_TRACES_SAMPLER_ARG: "2" })).toContain("OTEL_TRACES_SAMPLER_ARG");
  });

  it("posts spans with only the five attributes, and drops (counted) on failure", async () => {
    const posted: string[] = [];
    let status = 200;
    const metrics = new OpsMetrics();
    const exporter = new OtlpTraceExporter(otlp, async (body) => { posted.push(body); return { status }; }, metrics);
    const app = new Hono();
    app.use("*", opsTelemetry({ metrics, exporter, routeTemplate }));
    app.post("/mcp", async (c) => { await c.req.text(); c.header("mcp-method", "tools/call"); return c.json({ echoed: BODY_SENTINEL }); });
    app.get("/v1/batch/:id", (c) => c.json({ error: "boom" }, 500));

    await app.request("/mcp", { method: "POST", headers: { authorization: `Bearer ${KEY_SENTINEL}`, "mcp-name": "memory.ingest" }, body: BODY_SENTINEL });
    await app.request(`/v1/batch/${PATH_SENTINEL}`);
    expect(exporter.pendingCount).toBe(2);
    await exporter.flush();
    expect(posted).toHaveLength(1);
    const doc = JSON.parse(posted[0]!) as { resourceSpans: Array<{ scopeSpans: Array<{ spans: Array<{ name: string; traceId: string; attributes: Array<{ key: string; value: Record<string, string> }>; status: { code: number } }> }> }> };
    const spans = doc.resourceSpans[0]!.scopeSpans[0]!.spans;
    expect(spans.map((s) => s.name)).toEqual(["POST /mcp", "GET /v1/batch/:id"]);
    expect(spans[0]!.traceId).toMatch(/^[0-9a-f]{32}$/);
    for (const s of spans) expect(s.attributes.map((a) => a.key)).toEqual([...SPAN_ATTRIBUTES]);
    expect(spans[0]!.attributes.find((a) => a.key === "mcp.tool.name")?.value.stringValue).toBe("memory.ingest");
    expect(spans[1]!.status.code).toBe(2);
    for (const s of [BODY_SENTINEL, KEY_SENTINEL, PATH_SENTINEL, "collector"]) expect(posted[0]).not.toContain(s);

    status = 503;
    await app.request("/mcp", { method: "POST", body: "{}" });
    await exporter.flush();
    expect(metrics.exportFailures).toBe(1);
    expect(metrics.spansDropped).toBe(1);
    const throwing = new OtlpTraceExporter(otlp, () => Promise.reject(new Error(BODY_SENTINEL)), metrics);
    throwing.record(labels(), Date.now(), 1);
    await expect(throwing.flush()).resolves.toBeUndefined();
    expect(metrics.exportFailures).toBe(2);
  });

  it("bounds the buffer and honours the sample ratio", () => {
    const metrics = new OpsMetrics();
    const full = new OtlpTraceExporter(otlp, async () => ({ status: 200 }), metrics);
    for (let i = 0; i < MAX_PENDING_SPANS + 5; i++) full.record(labels(), 0, 1);
    expect(full.pendingCount).toBe(MAX_PENDING_SPANS);
    expect(metrics.spansDropped).toBe(5);
    const none = new OtlpTraceExporter({ ...otlp, sampleRatio: 0 }, async () => ({ status: 200 }), metrics);
    none.record(labels(), 0, 1);
    expect(none.pendingCount).toBe(0);
  });
});

describe("boot wiring", () => {
  const minimal = {
    ANTHROPIC_API_KEY: "sk-ant-test-not-a-real-key",
    LICENSE_PUBKEY_B64: (releaseKey as { ed25519: string }).ed25519,
    DAILY_SPEND_CAP_USD: "10",
    BATCH_PROVIDER: "anthropic",
    OPENAI_API_KEY: "sk-test-not-a-real-key",
  };

  it("leaves export off by default, refuses a bad endpoint by name, and wires the exporter when set", () => {
    expect(relayConfigFromEnv(minimal).otlp).toBeUndefined();
    expect(() => relayConfigFromEnv({ ...minimal, OTEL_EXPORTER_OTLP_ENDPOINT: "http://otel.example" })).toThrow(BootError);
    const config = relayConfigFromEnv({ ...minimal, OTEL_EXPORTER_OTLP_ENDPOINT: "https://otel.example", OTEL_EXPORTER_OTLP_HEADERS: "authorization=Bearer%20x" });
    const metrics = newMetrics(config);
    expect(newTraceExporter(config, metrics)).toBeInstanceOf(OtlpTraceExporter);
    expect(newTraceExporter(relayConfigFromEnv(minimal), metrics)).toBeUndefined();
  });
});
