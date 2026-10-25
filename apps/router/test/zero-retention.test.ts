/**
 * Invariant 2 (E8-T6 / E8-T7): no request or response body reaches the logger or the run record,
 * through the REST face, the MCP face and the Monid passthrough, on success and on failure.
 */
import { describe, expect, it } from "vitest";
import { createApp } from "../src/app.js";
import { formatEvent } from "../src/logging.js";
import { TOOL_RUN_RECORD_KEYS } from "../src/runlog.js";
import { AGENT_KEY, makeDeps } from "./helpers.js";

const IN = "SENTINEL-INPUT-7f3a9c";
const OUT = "SENTINEL-OUTPUT-b81e2d";
const ERR = "SENTINEL-ERROR-55d0aa";
const SECRET_VALUE = "SENTINEL-MONID-KEY-0c1f";

describe("zero retention", () => {
  it("the logger never receives bodies, keys or inputs — success, vendor failure, REST and MCP", async () => {
    const deps = makeDeps({
      responder: (call) => {
        const body = call.request.body as { input?: { query?: string } };
        if (body.input?.query?.includes("fail")) return { status: 500, body: { status: "failed", error: { code: ERR, message: ERR } } };
        return { status: 200, body: { status: "succeeded", output: { results: [{ title: OUT }] } } };
      },
    });
    const app = createApp(deps);
    const headers = { "content-type": "application/json", authorization: `Bearer ${AGENT_KEY}` };

    const ok = await app.request("/v1/router/run", { method: "POST", headers, body: JSON.stringify({ tool_id: "monid.tinyfish.web-search", input: { query: IN }, idempotency_key: "zr1", task_hint: `${IN} hint` }) });
    expect(ok.status).toBe(200);
    expect(JSON.stringify(await ok.json())).toContain(OUT);

    const failed = await app.request("/v1/router/run", { method: "POST", headers, body: JSON.stringify({ tool_id: "monid.tinyfish.web-search", input: { query: `${IN} fail` }, idempotency_key: "zr2" }) });
    expect(failed.status).toBe(502);

    const mcp = await app.request("/mcp", {
      method: "POST",
      headers,
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name: "router.run", arguments: { tool_id: "monid.tinyfish.web-search", input: { query: IN }, idempotency_key: "zr3" } } }),
    });
    expect(mcp.status).toBe(200);

    const dumped = JSON.stringify(deps.logs) + deps.logs.map(formatEvent).join("\n");
    for (const s of [IN, OUT, ERR, AGENT_KEY, SECRET_VALUE, "Bearer", "query", "results"]) expect(dumped).not.toContain(s);
    expect(deps.logs.length).toBeGreaterThan(0);
    for (const e of deps.logs) {
      for (const k of Object.keys(e)) expect(["kind", "route", "method", "status", "latency_ms", "tool_id", "provider_id", "outcome", "count", "actor_hash"]).toContain(k);
    }
  });

  it("the run record has exactly the specified keys and no body; task_hint is truncated and the key is hashed", async () => {
    const deps = makeDeps({ responder: () => ({ status: 200, body: { status: "succeeded", output: { results: [{ title: OUT }] } } }) });
    const app = createApp(deps);
    const longHint = `${IN} `.repeat(10); // 230 chars: inside the verb's 500 limit, beyond the record's 120
    await app.request("/v1/router/run", {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${AGENT_KEY}` },
      body: JSON.stringify({ tool_id: "monid.tinyfish.web-search", input: { query: IN }, idempotency_key: "rr1", task_hint: longHint, hint_followed: true }),
    });
    expect(deps.runLog.records).toHaveLength(1);
    const rec = deps.runLog.records[0];
    if (!rec) throw new Error("no record");
    expect(Object.keys(rec).sort()).toEqual([...TOOL_RUN_RECORD_KEYS].sort());
    expect(rec.outcome).toBe("ok");
    expect(rec.cost_micro_usd).toBe(1365);
    expect(rec.hint_followed).toBe(true);
    expect(rec.actor).not.toContain(AGENT_KEY);
    expect(rec.actor).toMatch(/^[0-9a-f]{12}$/);
    expect(rec.task_hint?.length).toBeLessThanOrEqual(120);
    const dumped = JSON.stringify(rec);
    expect(dumped).not.toContain(OUT);
    expect(dumped).not.toContain(AGENT_KEY);
  });

  it("a Secret serialises as a redaction marker", async () => {
    const { Secret } = await import("../src/secret.js");
    const s = new Secret(SECRET_VALUE);
    expect(JSON.stringify({ s })).toBe('{"s":"[redacted]"}');
    expect(`${s}`).toBe("[redacted]");
    expect(s.expose()).toBe(SECRET_VALUE);
  });
});
