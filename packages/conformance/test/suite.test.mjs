// @ts-check
/**
 * The suite itself, against an in-process fake MCP server (no Rust, no network beyond loopback):
 * a conformant fake passes every check, and each deliberate deviation is caught by the check that
 * names it. This is what keeps the suite from passing a broken server.
 */
import { createServer } from "node:http";

import { afterEach, describe, expect, it } from "vitest";

import { loadToolTable } from "../src/config.mjs";
import { passed, runSuite, summarize } from "../src/suite.mjs";

const TOKEN = "t0ken";
const table = loadToolTable();

/**
 * @typedef {{
 *   noProtocolHeader?: boolean; getAllowed?: boolean; acceptMismatch?: boolean; badAnnotations?: string;
 *   replayInserts?: boolean; quantityFromRequest?: boolean; noApprovalUrl?: boolean; l2RunsOnDecline?: boolean;
 *   readOnlyDestructive?: boolean; http401?: boolean; nextActions?: boolean; envelopes?: boolean;
 * }} Faults
 */

/** @param {Faults} faults */
function fakeServer(faults = {}) {
  /** @type {Map<string, number[]>} */
  const ingested = new Map();
  let approvals = 0;
  const level = new Map(table.tools.map((t) => [t.name, t.level]));
  level.set("export.write_file", "L2");
  const annotations = (/** @type {string} */ name) => {
    const l = level.get(name);
    if (faults.badAnnotations === name) return { readOnlyHint: false, destructiveHint: false, idempotentHint: false, openWorldHint: false };
    if (l === "read") return { readOnlyHint: true, destructiveHint: !!faults.readOnlyDestructive, idempotentHint: true, openWorldHint: false };
    if (l === "L3") return { readOnlyHint: false, destructiveHint: true, idempotentHint: false, openWorldHint: true };
    return { readOnlyHint: false, destructiveHint: false, idempotentHint: false, openWorldHint: false };
  };
  const text = (/** @type {unknown} */ payload, isError = false, extra = {}) => ({ content: [{ type: "text", text: JSON.stringify(payload) }], isError, ...extra });
  const envelope = (/** @type {string} */ tool, /** @type {unknown} */ result) => (faults.envelopes === false ? { tool, result } : { tool, result, usage: { unit: "read", quantity: 1, cost_micro_usd: 0 }, latency_ms: 1, next_action: null });
  const data = (/** @type {string} */ next) => (faults.nextActions ? { data: { next_action: next } } : {});

  /** @returns {{ result?: any; error?: any; quantity?: number }} */
  const toolsCall = (/** @type {any} */ params) => {
    const { name, arguments: args = {} } = params;
    switch (name) {
      case "state.commitments.list":
      case "profile.whoami":
      case "memory.recall":
        if (name === "memory.recall" || faults.envelopes !== false) return { result: text(envelope(name, { items: [] }), false, { _meta: { ttlMs: 15000, cacheScope: "private" } }) };
        return { result: text({ tool: name, results: [] }, false, { _meta: { ttlMs: 15000, cacheScope: "private" } }) };
      case "memory.search":
        return { error: { code: -32602, message: "invalid_type: query must be a string", ...data("fix_arguments") } };
      case "state.people.get":
        return { result: text({ error: "not_found", message: "No record has that id.", tool: name, ...(faults.nextActions ? { next_action: "list_first" } : {}) }, true) };
      case "memory.ingest": {
        const items = (args.turns ?? []).length;
        if (!args.idempotency_key) return { error: { code: -32602, message: "missing_argument: idempotency_key is required", ...data("add_idempotency_key") }, quantity: faults.quantityFromRequest ? items : 0 };
        const seen = ingested.get(args.idempotency_key);
        const replayed = !!seen && !faults.replayInserts;
        const ids = seen ?? [ingested.size + 1];
        ingested.set(args.idempotency_key, ids);
        const quantity = replayed && !faults.quantityFromRequest ? 0 : items;
        return { result: text({ tool: name, result: { replayed, inserted: replayed ? 0 : items, event_ids: ids }, usage: { unit: "events", quantity }, latency_ms: 1, next_action: null }), quantity };
      }
      case "memory.append_note":
        return { result: text({ accepted: true, note_id: 1 }) };
      case "export.write_file": {
        const response = params.inputResponses?.[0];
        if (!response) return { result: { resultType: "input_required", inputRequests: [{ id: "l2_confirm", mode: "form", message: "Confirm", requestedSchema: { type: "object", properties: { confirm: { type: "boolean" } }, required: ["confirm"] } }], ...text({ input_required: true }) } };
        const ran = faults.l2RunsOnDecline || (response.action === "accept" && response.content?.confirm === true);
        return { result: text({ ran, declined: !ran }) };
      }
      case "actions.execute": {
        approvals += 1;
        const url = faults.noApprovalUrl ? null : `https://approve.test/approve/tok-${approvals}`;
        return {
          result: {
            resultType: "approval_required",
            approval: { approval_id: approvals, level: "L3", url, poll: { tool: "actions.status", arguments: { approval_id: approvals } } },
            ...(url ? { inputRequests: [{ id: "l3_approval", mode: "url", url, message: "m" }] } : {}),
            ...text({ pending: true, approval_id: approvals, level: "L3", url, next_action: url ? "approve_at_url" : "poll_status" }),
          },
        };
      }
      case "actions.status":
        return { result: text({ approval_id: args.approval_id, status: "pending" }) };
      default:
        return { error: { code: -32602, message: "unknown_tool: no tool has that name; call tools/list", ...data("list_tools") } };
    }
  };

  const server = createServer((req, res) => {
    let raw = "";
    req.on("data", (chunk) => (raw += chunk));
    req.on("end", () => {
      /** @type {Record<string, string>} */
      const headers = faults.noProtocolHeader ? {} : { "mcp-protocol-version": "2026-07-28" };
      const send = (/** @type {number} */ status, /** @type {any} */ body, extra = {}) => {
        const json = body === undefined ? "" : JSON.stringify(body);
        const sseOnly = /text\/event-stream/.test(req.headers.accept ?? "") && !/application\/json/.test(req.headers.accept ?? "");
        res.writeHead(status, { ...headers, ...extra, "content-type": sseOnly && json ? "text/event-stream" : "application/json" });
        res.end(sseOnly && json ? `event: message\ndata: ${json}\n\n` : json);
      };
      if (req.method !== "POST") {
        if (faults.getAllowed && req.method === "GET") return send(200, {});
        return send(405, { error: "method_not_allowed" }, { allow: "POST" });
      }
      let msg;
      try {
        msg = JSON.parse(raw);
      } catch {
        return send(400, { jsonrpc: "2.0", id: null, error: { code: -32700, message: "parse error" } });
      }
      if (Array.isArray(msg)) return send(400, { jsonrpc: "2.0", id: null, error: { code: -32600, message: "not a batch" } });
      const method = req.headers["mcp-method"];
      const name = req.headers["mcp-name"];
      if (!method) return send(400, { jsonrpc: "2.0", id: msg.id, error: { code: -32600, message: "missing_header: Mcp-Method" } });
      if (method !== msg.method && !faults.acceptMismatch) return send(400, { jsonrpc: "2.0", id: msg.id, error: { code: -32600, message: "mcp_method_mismatch: x" } });
      if (msg.method === "tools/call") {
        if (!name) return send(400, { jsonrpc: "2.0", id: msg.id, error: { code: -32600, message: "missing_header: Mcp-Name" } });
        if (name !== msg.params?.name) return send(400, { jsonrpc: "2.0", id: msg.id, error: { code: -32600, message: "mcp_name_mismatch: x" } });
      }
      const echo = { "mcp-method": String(msg.method), ...(msg.method === "tools/call" ? { "mcp-name": String(name) } : {}) };
      if (msg.id === undefined) return send(202, undefined, echo);
      const authed = req.headers.authorization === `Bearer ${TOKEN}`;
      const reply = (/** @type {any} */ result, extra = {}) => send(200, { jsonrpc: "2.0", id: msg.id, result }, { ...echo, ...extra });
      const fail = (/** @type {any} */ error, extra = {}) => send(200, { jsonrpc: "2.0", id: msg.id, error }, { ...echo, ...extra });
      switch (msg.method) {
        case "ping":
          return reply({});
        case "initialize":
          return reply({ protocolVersion: "2026-07-28", capabilities: { tools: {} }, serverInfo: { name: "fake", version: "0" } });
        case "server/discover":
          return reply({ protocolVersion: "2026-07-28", stateless: true, capabilities: { tools: {} }, serverInfo: { name: "fake", version: "0" } });
        case "tools/list": {
          if (!authed) return fail({ code: -32001, message: "unauthorized" });
          const tools = [...level.keys()].map((n) => ({ name: n, description: `${n} does a thing.`, inputSchema: { type: "object", properties: {} }, annotations: annotations(n) }));
          return reply({ tools, ttlMs: 300000, cacheScope: "private" });
        }
        case "tools/call": {
          if (!authed) {
            if (faults.http401) return send(401, { jsonrpc: "2.0", id: msg.id, error: { code: -32001, message: "unauthorized" } }, { "www-authenticate": 'Bearer realm="fake"' });
            return fail({ code: -32001, message: "unauthorized" });
          }
          const out = toolsCall(msg.params);
          const quantity = msg.params.name === "memory.ingest" ? { "mcp-quantity": String(out.quantity ?? 0) } : {};
          return out.error ? fail(out.error, quantity) : reply(out.result, quantity);
        }
        default:
          return fail({ code: -32601, message: "method not found" });
      }
    });
  });
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve(server)));
}

/** @type {import("node:http").Server | undefined} */
let running;
afterEach(() => running?.close());

/** @param {Faults} [faults] @param {Partial<import("../src/suite.mjs").SuiteOptions>} [options] */
async function run(faults = {}, options = {}) {
  running = /** @type {import("node:http").Server} */ (await fakeServer(faults));
  const address = /** @type {import("node:net").AddressInfo} */ (running.address());
  const results = await runSuite({ url: `http://127.0.0.1:${address.port}/mcp`, token: TOKEN, writes: true, approvalUrlPrefix: "https://approve.test/approve/", l2Tool: "export.write_file", l2Args: { path: "x" }, toolTable: table, ...options });
  return { results, outcome: (/** @type {string} */ id) => results.find((r) => r.id === id)?.outcome };
}

describe("the conformance suite", () => {
  it("passes every check against a conformant server", async () => {
    const { results } = await run({ http401: true, nextActions: true });
    expect(results.filter((r) => r.outcome !== "pass")).toEqual([]);
    expect(passed(results, true)).toBe(true);
  });

  it("skips (never passes) the write checks when writes are off", async () => {
    const { results, outcome } = await run({ http401: true, nextActions: true }, { writes: false });
    for (const id of ["write.ingest-idempotent", "write.ingest-requires-key", "write.l1-direct", "mrtr.l2-input-required", "mrtr.l3-approval-required", "mrtr.l3-poll"]) expect(outcome(id)).toBe("skip");
    expect(summarize(results).fail).toBe(0);
  });

  it.each(/** @type {Array<[string, Faults, string, string]>} */ ([
    ["no Mcp-Protocol-Version", { noProtocolHeader: true }, "transport.protocol-version-header", "fail"],
    ["GET opens something", { getAllowed: true }, "transport.get-delete-405", "fail"],
    ["Mcp-Method not checked", { acceptMismatch: true }, "headers.method-mismatch", "fail"],
    ["an L3 tool without destructive hints", { badAnnotations: "actions.execute" }, "tools.list.annotations-by-level", "fail"],
    ["a read marked destructive", { readOnlyDestructive: true }, "tools.list.annotation-consistency", "fail"],
    ["a replay that inserts again", { replayInserts: true }, "write.ingest-idempotent", "fail"],
    ["quantity from the request", { quantityFromRequest: true }, "write.ingest-requires-key", "fail"],
    ["an approval without URL", { noApprovalUrl: true }, "mrtr.l3-approval-required", "fail"],
    ["an L2 tool that runs on decline", { l2RunsOnDecline: true }, "mrtr.l2-input-required", "fail"],
    ["a 200 for a missing bearer", { nextActions: true }, "auth.http-401", "warn"],
    ["refusals without next_action", { http401: true }, "errors.next-action", "warn"],
    ["reads without the metered envelope", { http401: true, nextActions: true, envelopes: false }, "call.metered-envelope", "warn"],
  ]))("catches %s", async (_label, faults, id, want) => {
    const { outcome } = await run(faults);
    expect(outcome(id)).toBe(want);
  });

  it("a warning fails only a strict run", async () => {
    const { results } = await run({ http401: true });
    expect(passed(results)).toBe(true);
    expect(passed(results, true)).toBe(false);
  });
});
