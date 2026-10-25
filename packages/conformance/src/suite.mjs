// @ts-check
/**
 * The MCP conformance suite (E36a): drives one MCP endpoint over Streamable HTTP and checks the
 * MCP 2026-07-28 behavior the ShogunAgent server claims (crates/shogun-mcp: `mcp_http.rs`,
 * `mrtr.rs`, `core_tools.rs`).
 *
 * Every check has a stable id and one of four outcomes:
 * - `pass`;
 * - `fail`: the server breaks the 2026-07-28 transport or a contract it publishes (headers,
 *   annotations by level, MRTR shapes, idempotency);
 * - `warn`: a deviation from a repository rule or a recommended behavior that is not a protocol
 *   MUST (fails the run only with `strict`);
 * - `skip`: the check could not run here, with the reason (never counted as a pass).
 *
 * Writes are opt-in (`writes: true`): they ingest one test turn, append one note and queue one L3
 * approval in the target store. Never point a write run at a store whose owner did not ask for it.
 *
 * The expected level of each core tool comes from the machine-readable tool table
 * (`apps/api/fixtures/tool-table.json`, invariant 12), not from a list in this file.
 */

export const PROTOCOL_VERSION = "2026-07-28";

/**
 * @typedef {"pass" | "fail" | "warn" | "skip"} Outcome
 * @typedef {{ id: string; title: string; outcome: Outcome; detail?: string }} CheckResult
 * @typedef {{ name: string; level: string }} TableTool
 * @typedef {{
 *   url: string;
 *   token?: string | undefined;
 *   writes?: boolean;
 *   approvalUrlPrefix?: string | undefined;
 *   l2Tool?: string | undefined;
 *   l2Args?: Record<string, unknown> | undefined;
 *   toolTable?: { tools: TableTool[] } | undefined;
 *   fetch?: typeof globalThis.fetch;
 *   timeoutMs?: number;
 * }} SuiteOptions
 * @typedef {{ status: number; headers: Headers; text: string; json: any }} Reply
 */

/** Thrown inside a check to fail it with a sentence. */
class CheckFailed extends Error {}
/** Thrown inside a check to downgrade it to a warning. */
class CheckWarned extends Error {}
/** Thrown inside a check to skip it with a reason. */
class CheckSkipped extends Error {}

/** @param {unknown} condition @param {string} message @returns {asserts condition} */
function expect(condition, message) {
  if (!condition) throw new CheckFailed(message);
}
/** @param {unknown} condition @param {string} message */
function recommend(condition, message) {
  if (!condition) throw new CheckWarned(message);
}
/** @param {string} reason @returns {never} */
function skip(reason) {
  throw new CheckSkipped(reason);
}

const short = (/** @type {unknown} */ value) => {
  const text = typeof value === "string" ? value : JSON.stringify(value);
  return text.length > 300 ? `${text.slice(0, 300)}…` : text;
};

/** The JSON payload a tool put in its first text block, or undefined. */
export function payloadOf(/** @type {any} */ rpc) {
  const text = rpc?.result?.content?.[0]?.text;
  if (typeof text !== "string") return undefined;
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

/** @param {SuiteOptions} options */
export function client(options) {
  const doFetch = options.fetch ?? globalThis.fetch;
  const timeoutMs = options.timeoutMs ?? 15_000;
  let nextId = 1;
  /**
   * @param {string} method HTTP method
   * @param {{ body?: string; headers?: Record<string, string>; auth?: boolean }} [init]
   * @returns {Promise<Reply>}
   */
  const raw = async (method, init = {}) => {
    /** @type {Record<string, string>} */
    const headers = { accept: "application/json, text/event-stream", ...(init.headers ?? {}) };
    if (init.body !== undefined) headers["content-type"] = "application/json";
    if (init.auth !== false && options.token) headers.authorization = `Bearer ${options.token}`;
    const response = await doFetch(options.url, { method, headers, body: init.body, signal: AbortSignal.timeout(timeoutMs) });
    const text = await response.text();
    let json;
    try {
      json = text ? JSON.parse(text) : undefined;
    } catch {
      json = undefined;
    }
    return { status: response.status, headers: response.headers, text, json };
  };
  /**
   * One JSON-RPC request with the metering headers set from the body.
   * @param {string} method
   * @param {unknown} [params]
   * @param {{ headers?: Record<string, string>; auth?: boolean; id?: unknown }} [init]
   */
  const rpc = (method, params, init = {}) => {
    const id = "id" in init ? init.id : nextId++;
    const body = { jsonrpc: "2.0", ...(id === undefined ? {} : { id }), method, ...(params === undefined ? {} : { params }) };
    /** @type {Record<string, string>} */
    const headers = { "mcp-method": method };
    const name = /** @type {{ name?: unknown } | undefined} */ (params)?.name;
    if (method === "tools/call" && typeof name === "string") headers["mcp-name"] = name;
    return raw("POST", { body: JSON.stringify(body), headers: { ...headers, ...(init.headers ?? {}) }, ...(init.auth === undefined ? {} : { auth: init.auth }) });
  };
  /** @param {string} name @param {Record<string, unknown>} args @param {Record<string, unknown>} [extra] */
  const call = (name, args, extra = {}) => rpc("tools/call", { name, arguments: args, ...extra });
  return { raw, rpc, call };
}

/** The error code and message of a JSON-RPC error reply, or undefined. */
const rpcError = (/** @type {Reply} */ reply) => (reply.json && typeof reply.json === "object" && reply.json.error ? reply.json.error : undefined);

/** Where a refusal says what to do next: `error.data.next_action` or the payload's `next_action`. */
function nextActionOf(/** @type {Reply} */ reply) {
  const error = rpcError(reply);
  if (error) return error.data?.next_action;
  return payloadOf(reply.json)?.next_action;
}

/**
 * Run every check. Returns the results in a stable order.
 * @param {SuiteOptions} options
 * @returns {Promise<CheckResult[]>}
 */
export async function runSuite(options) {
  const c = client(options);
  /** @type {CheckResult[]} */
  const results = [];
  /** @type {any[] | undefined} */
  let listed;
  /** @type {number | undefined} */
  let approvalId;

  /** @param {string} id @param {string} title @param {() => Promise<string | void>} body */
  const check = async (id, title, body) => {
    try {
      const detail = await body();
      results.push({ id, title, outcome: "pass", ...(detail ? { detail } : {}) });
    } catch (e) {
      const outcome = e instanceof CheckSkipped ? "skip" : e instanceof CheckWarned ? "warn" : "fail";
      const detail = e instanceof Error ? e.message : String(e);
      results.push({ id, title, outcome, detail });
    }
  };

  const tools = async () => {
    if (listed) return listed;
    const reply = await c.rpc("tools/list");
    const list = reply.json?.result?.tools;
    if (!Array.isArray(list)) skip(`tools/list did not answer a tool list (${short(reply.text)})`);
    listed = list;
    return list;
  };

  // ---- Transport (stateless Streamable HTTP) ----
  await check("transport.no-handshake", "a POST without initialize is served (stateless)", async () => {
    const reply = await c.rpc("tools/list");
    expect(reply.status === 200, `tools/list answered HTTP ${reply.status}: ${short(reply.text)}`);
    expect((reply.headers.get("content-type") ?? "").includes("application/json"), `content-type is ${reply.headers.get("content-type")}`);
    expect(reply.json?.jsonrpc === "2.0", "the reply is not JSON-RPC 2.0");
    expect(Array.isArray(reply.json?.result?.tools), `no result.tools: ${short(reply.text)}`);
  });
  await check("transport.discover", "server/discover names 2026-07-28 and stateless", async () => {
    const reply = await c.rpc("server/discover");
    const result = reply.json?.result;
    expect(result, `server/discover failed: ${short(reply.text)}`);
    expect(result.protocolVersion === PROTOCOL_VERSION, `protocolVersion is ${result.protocolVersion}`);
    expect(result.stateless === true, "stateless is not true");
    expect(result.capabilities && typeof result.capabilities === "object" && "tools" in result.capabilities, "capabilities.tools missing");
    expect(typeof result.serverInfo?.name === "string", "serverInfo.name missing");
  });
  await check("transport.initialize-optional", "initialize still answers (older clients) and negotiates 2026-07-28", async () => {
    const reply = await c.rpc("initialize", { protocolVersion: PROTOCOL_VERSION, capabilities: {}, clientInfo: { name: "shogunagent-conformance", version: "1" } });
    expect(reply.json?.result?.protocolVersion === PROTOCOL_VERSION, `initialize answered ${short(reply.text)}`);
    expect(reply.headers.get("mcp-session-id") === null, "initialize opened a session (Mcp-Session-Id) on a stateless transport");
  });
  await check("transport.protocol-version-header", "every reply carries Mcp-Protocol-Version: 2026-07-28", async () => {
    const reply = await c.rpc("ping");
    expect(reply.headers.get("mcp-protocol-version") === PROTOCOL_VERSION, `Mcp-Protocol-Version is ${reply.headers.get("mcp-protocol-version")}`);
    expect(reply.headers.get("mcp-session-id") === null, "a reply carries Mcp-Session-Id");
  });
  await check("transport.get-delete-405", "GET and DELETE answer 405 Allow: POST (no stream, no session)", async () => {
    for (const method of ["GET", "DELETE"]) {
      const reply = await c.raw(method);
      expect(reply.status === 405, `${method} answered ${reply.status}`);
      expect((reply.headers.get("allow") ?? "").toUpperCase().includes("POST"), `${method} 405 has Allow: ${reply.headers.get("allow")}`);
    }
  });
  await check("transport.batch-rejected", "a JSON-RPC batch is 400 -32600", async () => {
    const reply = await c.raw("POST", { body: JSON.stringify([{ jsonrpc: "2.0", id: 1, method: "ping" }]), headers: { "mcp-method": "ping" } });
    expect(reply.status === 400, `a batch answered ${reply.status}`);
    expect(rpcError(reply)?.code === -32600, `error is ${short(reply.text)}`);
  });
  await check("transport.parse-error", "malformed JSON is 400 -32700 with id null", async () => {
    const reply = await c.raw("POST", { body: "{not json", headers: { "mcp-method": "ping" } });
    expect(reply.status === 400, `answered ${reply.status}`);
    expect(rpcError(reply)?.code === -32700 && reply.json.id === null, `error is ${short(reply.text)}`);
  });
  await check("transport.notification-202", "a notification is 202 with no body", async () => {
    const reply = await c.rpc("notifications/initialized", undefined, { id: undefined });
    expect(reply.status === 202, `answered ${reply.status}`);
    expect(reply.text === "", `body is ${short(reply.text)}`);
  });
  await check("transport.sse-only-accept", "Accept: text/event-stream alone gets one SSE message event", async () => {
    const reply = await c.raw("POST", { body: JSON.stringify({ jsonrpc: "2.0", id: 99, method: "ping" }), headers: { "mcp-method": "ping", accept: "text/event-stream" } });
    expect(reply.status === 200, `answered ${reply.status}`);
    expect((reply.headers.get("content-type") ?? "").includes("text/event-stream"), `content-type is ${reply.headers.get("content-type")}`);
    const match = /^event: message\ndata: (.+)\n\n$/s.exec(reply.text);
    expect(match, `not one SSE event: ${short(reply.text)}`);
    expect(JSON.parse(/** @type {string} */ (match[1])).id === 99, "the event is not the reply to the request");
  });
  await check("transport.unknown-method", "an unknown method is -32601", async () => {
    const reply = await c.rpc("conformance/no_such_method");
    expect(rpcError(reply)?.code === -32601, `answered ${short(reply.text)}`);
  });

  // ---- Metering headers (Mcp-Method / Mcp-Name) ----
  const ingestCall = { jsonrpc: "2.0", id: 7, method: "tools/call", params: { name: "memory.recall", arguments: { query: "conformance" } } };
  await check("headers.method-required", "a POST without Mcp-Method is refused 400 missing_header", async () => {
    const reply = await c.raw("POST", { body: JSON.stringify(ingestCall), headers: { "mcp-name": "memory.recall" } });
    expect(reply.status === 400, `answered ${reply.status}`);
    expect(rpcError(reply)?.code === -32600 && String(rpcError(reply)?.message).startsWith("missing_header"), `error is ${short(reply.text)}`);
  });
  await check("headers.method-mismatch", "Mcp-Method that disagrees with the body is refused 400", async () => {
    const reply = await c.raw("POST", { body: JSON.stringify(ingestCall), headers: { "mcp-method": "tools/list", "mcp-name": "memory.recall" } });
    expect(reply.status === 400 && String(rpcError(reply)?.message).startsWith("mcp_method_mismatch"), `answered ${reply.status} ${short(reply.text)}`);
  });
  await check("headers.name-required", "a tools/call without Mcp-Name is refused 400", async () => {
    const reply = await c.raw("POST", { body: JSON.stringify(ingestCall), headers: { "mcp-method": "tools/call" } });
    expect(reply.status === 400 && String(rpcError(reply)?.message).startsWith("missing_header"), `answered ${reply.status} ${short(reply.text)}`);
  });
  await check("headers.name-mismatch", "Mcp-Name that disagrees with params.name is refused 400", async () => {
    const reply = await c.raw("POST", { body: JSON.stringify(ingestCall), headers: { "mcp-method": "tools/call", "mcp-name": "memory.ingest" } });
    expect(reply.status === 400 && String(rpcError(reply)?.message).startsWith("mcp_name_mismatch"), `answered ${reply.status} ${short(reply.text)}`);
  });
  await check("headers.echoed", "replies echo Mcp-Method, and Mcp-Name on tools/call only", async () => {
    const list = await c.rpc("tools/list");
    expect(list.headers.get("mcp-method") === "tools/list", `tools/list reply Mcp-Method is ${list.headers.get("mcp-method")}`);
    expect(list.headers.get("mcp-name") === null, "tools/list reply carries Mcp-Name");
    const read = await c.call("memory.recall", { query: "conformance" });
    expect(read.headers.get("mcp-method") === "tools/call", `tools/call reply Mcp-Method is ${read.headers.get("mcp-method")}`);
    expect(read.headers.get("mcp-name") === "memory.recall", `tools/call reply Mcp-Name is ${read.headers.get("mcp-name")}`);
  });

  // ---- Auth ----
  await check("auth.bearer-required", "a tools/call without a bearer is refused before anything runs", async () => {
    const bare = await c.rpc("tools/call", { name: "memory.recall", arguments: { query: "conformance" } }, { auth: false });
    const error = rpcError(bare);
    expect(bare.status === 401 || (error && typeof error.code === "number"), `an unauthenticated call answered ${bare.status} ${short(bare.text)}`);
    expect(!bare.json?.result, "an unauthenticated call returned a result");
  });
  await check("auth.http-401", "an unauthenticated request is HTTP 401 with WWW-Authenticate (MCP authorization)", async () => {
    const bare = await c.rpc("tools/call", { name: "memory.recall", arguments: { query: "conformance" } }, { auth: false });
    recommend(bare.status === 401, `answered HTTP ${bare.status} with JSON-RPC ${short(rpcError(bare) ?? bare.text)}; MCP authorization expects 401 + WWW-Authenticate on the HTTP transport (the hosted gateway does this in front of the sidecar)`);
    recommend(bare.headers.get("www-authenticate") !== null, "401 without WWW-Authenticate");
  });

  // ---- tools/list ----
  await check("tools.list.shape", "every tool has a name, a description, an object inputSchema and the four hints", async () => {
    const list = await tools();
    expect(list.length > 0, "tools/list is empty");
    for (const tool of list) {
      expect(typeof tool.name === "string" && tool.name, `a tool without a name: ${short(tool)}`);
      expect(typeof tool.description === "string" && tool.description.length > 0, `${tool.name} has no description`);
      expect(tool.inputSchema?.type === "object", `${tool.name} inputSchema.type is ${tool.inputSchema?.type}`);
      for (const hint of ["readOnlyHint", "destructiveHint", "idempotentHint", "openWorldHint"]) {
        expect(typeof tool.annotations?.[hint] === "boolean", `${tool.name} annotations.${hint} is not a boolean`);
      }
    }
    return `${list.length} tools`;
  });
  await check("tools.list.cache-hints", "tools/list carries ttlMs and cacheScope (2026-07-28)", async () => {
    const reply = await c.rpc("tools/list");
    expect(Number.isInteger(reply.json?.result?.ttlMs) && reply.json.result.ttlMs > 0, `ttlMs is ${reply.json?.result?.ttlMs}`);
    expect(typeof reply.json?.result?.cacheScope === "string", `cacheScope is ${reply.json?.result?.cacheScope}`);
  });
  const table = options.toolTable?.tools ?? [];
  await check("tools.list.core-present", "every core tool of the tool table is listed", async () => {
    if (table.length === 0) skip("no tool table given");
    const names = new Set((await tools()).map((t) => t.name));
    const missing = table.filter((t) => !names.has(t.name)).map((t) => t.name);
    expect(missing.length === 0, `missing: ${missing.join(", ")}`);
    return `${table.length} core tools`;
  });
  await check("tools.list.annotations-by-level", "annotations follow the level: reads readOnlyHint, L1 writes none, L3 destructive + openWorld", async () => {
    if (table.length === 0) skip("no tool table given");
    const byName = new Map((await tools()).map((t) => [t.name, t.annotations ?? {}]));
    const wrong = [];
    for (const { name, level } of table) {
      const a = byName.get(name);
      if (!a) continue;
      const want =
        level === "read" ? { readOnlyHint: true, destructiveHint: false, openWorldHint: false }
        : level === "L3" ? { readOnlyHint: false, destructiveHint: true, openWorldHint: true }
        : { readOnlyHint: false, destructiveHint: false, openWorldHint: false };
      for (const [hint, value] of Object.entries(want)) if (a[hint] !== value) wrong.push(`${name} (${level}) ${hint}=${a[hint]}`);
    }
    expect(wrong.length === 0, wrong.join("; "));
  });
  await check("tools.list.annotation-consistency", "no tool is both readOnlyHint and destructiveHint or openWorldHint", async () => {
    const bad = (await tools()).filter((t) => t.annotations?.readOnlyHint && (t.annotations?.destructiveHint || t.annotations?.openWorldHint)).map((t) => t.name);
    expect(bad.length === 0, `contradictory hints: ${bad.join(", ")}`);
  });

  // ---- Calls ----
  await check("call.read", "an L1 read runs with no confirmation and carries the read cache hints", async () => {
    const reply = await c.call("state.commitments.list", {});
    const result = reply.json?.result;
    expect(result && result.isError === false, `answered ${short(reply.text)}`);
    expect(result.resultType === undefined, `a read answered resultType ${result.resultType}`);
    expect(payloadOf(reply.json) !== undefined, "the text block is not JSON");
    expect(Number.isInteger(result._meta?.ttlMs) && typeof result._meta?.cacheScope === "string", `_meta is ${short(result._meta)}`);
  });
  await check("call.metered-envelope", "core results carry usage, latency_ms and next_action (repository rule)", async () => {
    const missing = [];
    for (const [name, args] of /** @type {Array<[string, Record<string, unknown>]>} */ ([["memory.recall", { query: "conformance" }], ["state.commitments.list", {}], ["profile.whoami", {}]])) {
      const payload = payloadOf((await c.call(name, args)).json);
      for (const key of ["usage", "latency_ms", "next_action"]) if (!payload || !(key in payload)) missing.push(`${name}.${key}`);
    }
    recommend(missing.length === 0, `missing: ${missing.join(", ")}`);
  });
  await check("call.unknown-tool", "an unknown tool is -32602 `unknown_tool: …`", async () => {
    const reply = await c.call("conformance.no_such_tool", {});
    const error = rpcError(reply);
    expect(error?.code === -32602 && /^unknown_tool: \S/.test(String(error.message)), `answered ${short(reply.text)}`);
  });
  await check("call.invalid-argument", "a bad argument is -32602 `code: sentence`", async () => {
    const reply = await c.call("memory.search", { query: 5 });
    const error = rpcError(reply);
    expect(error?.code === -32602 && /^[a-z_]+: \S/.test(String(error.message)), `answered ${short(reply.text)}`);
  });
  await check("call.not-found-shape", "a tool that ran and found nothing is isError with {error, message, tool}", async () => {
    const reply = await c.call("state.people.get", { id: 987_654_321 });
    const payload = payloadOf(reply.json);
    expect(reply.json?.result?.isError === true, `answered ${short(reply.text)}`);
    expect(payload?.error === "not_found" && typeof payload.message === "string" && payload.tool === "state.people.get", `payload is ${short(payload)}`);
  });
  await check("errors.next-action", "refusals say what to do next (`next_action`, repository rule)", async () => {
    const replies = [await c.call("conformance.no_such_tool", {}), await c.call("memory.search", { query: 5 }), await c.call("state.people.get", { id: 987_654_321 })];
    const without = replies.filter((r) => typeof nextActionOf(r) !== "string").map((r) => short(rpcError(r)?.message ?? payloadOf(r.json)?.error ?? r.text));
    recommend(without.length === 0, `no next_action on: ${without.join(" | ")}`);
  });

  // ---- Writes (opt-in) ----
  const writeGate = () => {
    if (!options.writes) skip("writes are off (set SHOGUN_CONFORMANCE_WRITES=1 against a store that may take test rows)");
  };
  await check("write.ingest-idempotent", "memory.ingest with the same idempotency_key replays and adds nothing; Mcp-Quantity settles what was taken", async () => {
    writeGate();
    const key = `conformance-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
    const args = { turns: [{ role: "user", content: "conformance probe turn" }, { role: "assistant", content: "noted" }], source: "shogunagent-conformance", actor: "agent:conformance", thread_key: "conformance", idempotency_key: key };
    const first = await c.call("memory.ingest", args);
    const firstPayload = payloadOf(first.json);
    expect(first.json?.result?.isError === false && firstPayload?.result, `first ingest answered ${short(first.text)}`);
    expect(firstPayload.result.replayed === false, `first ingest replayed: ${short(firstPayload)}`);
    const again = await c.call("memory.ingest", args);
    const againPayload = payloadOf(again.json);
    expect(againPayload?.result?.replayed === true, `the replay answered ${short(againPayload)}`);
    expect(againPayload.result.inserted === 0, `the replay inserted ${againPayload.result.inserted}`);
    expect(JSON.stringify(againPayload.result.event_ids) === JSON.stringify(firstPayload.result.event_ids), "the replay returned other event ids");
    expect(first.headers.get("mcp-quantity") === "2", `first ingest Mcp-Quantity is ${first.headers.get("mcp-quantity")} (2 turns sent)`);
    expect(again.headers.get("mcp-quantity") === "0", `replay Mcp-Quantity is ${again.headers.get("mcp-quantity")}: a replay must not settle new events`);
  });
  await check("write.ingest-requires-key", "memory.ingest without idempotency_key is -32602 and settles nothing", async () => {
    writeGate();
    const reply = await c.call("memory.ingest", { turns: [{ role: "user", content: "x" }], source: "shogunagent-conformance", actor: "agent:conformance", thread_key: "conformance" });
    const error = rpcError(reply);
    expect(error?.code === -32602 && String(error.message).includes("idempotency_key"), `answered ${short(reply.text)}`);
    expect(reply.headers.get("mcp-quantity") === "0", `a refused ingest carries Mcp-Quantity ${reply.headers.get("mcp-quantity")}`);
  });
  await check("write.l1-direct", "an L1 write (memory.append_note) runs at once, no MRTR round trip", async () => {
    writeGate();
    const reply = await c.call("memory.append_note", { text: `conformance note ${Date.now()}` });
    const result = reply.json?.result;
    expect(result && result.isError === false, `answered ${short(reply.text)}`);
    expect(result.resultType === undefined && result.inputRequests === undefined, `an L1 write answered resultType ${result.resultType}`);
  });

  // ---- MRTR (SEP-2322) ----
  await check("mrtr.l2-input-required", "an L2 tool answers input_required with a confirm form; a decline runs nothing", async () => {
    writeGate();
    if (!options.l2Tool) skip("no L2 tool configured: this server's tools/list cannot tell L2 from L1 (both carry no hints), and the server build lists none of the desktop's L2 tools; set SHOGUN_CONFORMANCE_L2_TOOL / SHOGUN_CONFORMANCE_L2_ARGS");
    const args = options.l2Args ?? {};
    const first = await c.call(options.l2Tool, args);
    const result = first.json?.result;
    expect(result?.resultType === "input_required", `the first call answered ${short(first.text)}`);
    const form = result.inputRequests?.[0];
    expect(form?.mode === "form" && typeof form.id === "string", `inputRequests[0] is ${short(form)}`);
    expect(form.requestedSchema?.properties?.confirm?.type === "boolean" && form.requestedSchema.required?.includes("confirm"), `requestedSchema is ${short(form.requestedSchema)}`);
    for (const response of [{ id: form.id, action: "decline" }, { id: form.id, action: "accept", content: {} }]) {
      const retry = await c.call(options.l2Tool, args, { inputResponses: [response] });
      const payload = payloadOf(retry.json);
      expect(retry.json?.result?.resultType === undefined && payload?.ran === false, `${response.action} answered ${short(retry.text)}`);
    }
  });
  await check("mrtr.l3-approval-required", "an L3 action answers approval_required with an id, a poll and (when configured) a URL", async () => {
    writeGate();
    const reply = await c.call("actions.execute", { kind: "send_email", to: "conformance@example.invalid", subject: "conformance", body: "This approval request was queued by the MCP conformance suite. Reject it." });
    const result = reply.json?.result;
    expect(result?.resultType === "approval_required", `answered ${short(reply.text)}`);
    const approval = result.approval;
    expect(Number.isInteger(approval?.approval_id) && approval.approval_id > 0, `approval is ${short(approval)}`);
    expect(approval.level === "L3", `level is ${approval.level}`);
    expect(approval.poll?.tool === "actions.status" && approval.poll?.arguments?.approval_id === approval.approval_id, `poll is ${short(approval.poll)}`);
    const text = payloadOf(reply.json);
    expect(text?.approval_id === approval.approval_id && text?.pending === true, "the text block lost the approval id (older clients read it there)");
    approvalId = approval.approval_id;
    if (options.approvalUrlPrefix) {
      expect(typeof approval.url === "string" && approval.url.startsWith(options.approvalUrlPrefix), `approval.url is ${approval.url}`);
      const request = result.inputRequests?.[0];
      expect(request?.mode === "url" && request.url === approval.url, `inputRequests[0] is ${short(request)}`);
      expect(text.next_action === "approve_at_url", `next_action is ${text.next_action}`);
    } else {
      expect(typeof text.next_action === "string", "no next_action");
    }
  });
  await check("mrtr.l3-poll", "actions.status answers pending for the queued approval, never sent, and is not cached", async () => {
    writeGate();
    if (approvalId === undefined) skip("no approval was queued");
    const reply = await c.call("actions.status", { approval_id: approvalId });
    const payload = payloadOf(reply.json);
    expect(payload?.status === "pending", `status is ${short(payload)}`);
    expect(reply.json?.result?._meta?.ttlMs === undefined, "the approval poll carries a cache hint");
  });

  return results;
}

/** Totals by outcome. @param {CheckResult[]} results */
export function summarize(results) {
  /** @type {Record<Outcome, number>} */
  const totals = { pass: 0, fail: 0, warn: 0, skip: 0 };
  for (const r of results) totals[r.outcome] += 1;
  return totals;
}

/** Whether the run is green: no fail, and with `strict` no warn either. */
export function passed(/** @type {CheckResult[]} */ results, strict = false) {
  const totals = summarize(results);
  return totals.fail === 0 && (!strict || totals.warn === 0);
}

/** A plain-text report. @param {CheckResult[]} results @param {string} url */
export function formatReport(results, url) {
  const mark = { pass: "PASS", fail: "FAIL", warn: "WARN", skip: "SKIP" };
  const lines = [`MCP conformance (${PROTOCOL_VERSION}) against ${url}`, ""];
  for (const r of results) lines.push(`${mark[r.outcome]}  ${r.id}  ${r.title}${r.detail ? `\n      ${r.detail}` : ""}`);
  const t = summarize(results);
  lines.push("", `${t.pass} passed, ${t.fail} failed, ${t.warn} warnings, ${t.skip} skipped`);
  return lines.join("\n");
}
