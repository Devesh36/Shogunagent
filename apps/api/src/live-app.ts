/** Live provider boundary: server-selected lanes; bounded SSE; no provider error bodies. */
import { Hono } from "hono";
import { bodyLimit } from "hono/body-limit";
import { createHmac, type KeyObject } from "node:crypto";
import { AuthError, deniedByOperator, isExpiredTrialToken, meterSubject, verifyLicense, type License, type TrustedKeys } from "./auth.js";
import { isLiveLane, type LiveConfig, type LiveLane, type LivePlan, type LivePolicy } from "./live-config.js";
import { LIVE_MAX_MS, LiveRefusal, type LiveStore } from "./live-store.js";
import { redactingLogger, type LogFn } from "./logging.js";

const encoder = new TextEncoder();
const record = (v: unknown): v is Record<string, unknown> => !!v && typeof v === "object" && !Array.isArray(v);
const isTokenCount = (n: unknown): n is number => typeof n === "number" && Number.isSafeInteger(n) && n >= 0;
export function reportedCost(data: unknown, policy: LivePolicy): number | undefined {
  if (!record(data)) return undefined;
  const response = record(data.response) ? data.response : data;
  const u = record(response.usage) ? response.usage : record(data.x_groq) && record(data.x_groq.usage) ? data.x_groq.usage : undefined;
  if (!u) return undefined;
  const input = policy.provider === "openai" ? u.input_tokens : u.prompt_tokens;
  const output = policy.provider === "openai" ? u.output_tokens : u.completion_tokens;
  if (!isTokenCount(input) || !isTokenCount(output)) return undefined;
  // Price cached input at the full rate. Never assume a discount the price config cannot prove.
  const cost = Math.ceil(input * policy.inputMicrosPerToken + output * policy.outputMicrosPerToken);
  return Number.isSafeInteger(cost) ? cost : undefined;
}

export function upstreamBody(raw: unknown, lane: LiveLane, policy: LivePolicy, identity: string, hmacKey: string): Record<string, unknown> | undefined {
  if (!record(raw)) return undefined;
  const input = raw.input;
  let normalized: unknown;
  if (typeof input === "string" && input.length > 0) normalized = input;
  // Chat history (up to 40 turns) plus tool calls and their outputs (up to 8 per turn) and reasoning.
  else if (Array.isArray(input) && input.length > 0 && input.length <= 256) {
    const messages: Array<Record<string, unknown>> = [];
    for (const message of input) {
      if (!record(message)) return undefined;
      if (typeof message.content === "string" && ["user", "assistant", "system", "developer"].includes(String(message.role))) {
        messages.push({ role: message.role, content: message.content });
      } else if (policy.provider === "openai" && message.type === "message" && message.role === "assistant" && Array.isArray(message.content) && message.content.every(part => record(part) && part.type === "output_text" && typeof part.text === "string")) {
        messages.push({ role: "assistant", content: message.content.map(part => (part as { text: string }).text).join("") });
      } else if (policy.provider === "openai" && message.type === "reasoning" && typeof message.id === "string" && message.id.length > 0 && message.id.length <= 128 && typeof message.encrypted_content === "string") {
        messages.push({ type: "reasoning", id: message.id, encrypted_content: message.encrypted_content, summary: [] });
      } else if (policy.provider === "openai" && message.type === "function_call" && typeof message.call_id === "string" && typeof message.name === "string" && typeof message.arguments === "string") {
        messages.push({ type: "function_call", call_id: message.call_id, name: message.name, arguments: message.arguments });
      } else if (policy.provider === "openai" && message.type === "function_call_output" && typeof message.call_id === "string" && typeof message.output === "string") {
        messages.push({ type: "function_call_output", call_id: message.call_id, output: message.output });
      } else return undefined;
    }
    normalized = messages;
  } else return undefined;
  if (raw.instructions !== undefined && typeof raw.instructions !== "string") return undefined;
  // Image/remote attachments are not admitted by this first text lane contract.
  if (raw.attachments !== undefined) return undefined;
  let functions: Array<Record<string, unknown>> | undefined;
  if (raw.tools !== undefined) {
    if (!(["chat", "agent"] as string[]).includes(lane) || !Array.isArray(raw.tools) || raw.tools.length > 64) return undefined;
    functions = [];
    for (const tool of raw.tools) {
      if (!record(tool) || tool.type !== "function" || typeof tool.name !== "string" || !/^[a-zA-Z0-9_-]{1,64}$/.test(tool.name) || !record(tool.parameters) || (tool.description !== undefined && typeof tool.description !== "string")) return undefined;
      functions.push({ type: "function", name: tool.name, parameters: tool.parameters, strict: false, ...(typeof tool.description === "string" ? { description: tool.description } : {}) });
    }
  }
  if (policy.provider === "groq") {
    const messages = typeof normalized === "string" ? [{ role: "user", content: normalized }] : normalized as unknown[];
    // A configured effort marks a reasoning model (gpt-oss): keep its reasoning short and out of the answer.
    const reasoning = policy.effort === "low" ? { reasoning_effort: "low", include_reasoning: false } : {};
    return { model: policy.model, stream: true, stream_options: { include_usage: true }, max_completion_tokens: policy.maxOutputTokens, ...reasoning, messages: [...(raw.instructions ? [{ role: "system", content: raw.instructions }] : []), ...messages] };
  }
  const hash = (text: string) => createHmac("sha256", hmacKey).update(text).digest("hex");
  return {
    model: policy.model, input: normalized, stream: true, store: false, include: ["reasoning.encrypted_content"], max_output_tokens: policy.maxOutputTokens,
    safety_identifier: hash(identity), prompt_cache_key: hash(`${identity}\u0000${lane}`),
    ...(raw.instructions !== undefined ? { instructions: raw.instructions } : {}),
    ...(functions ? { tools: functions } : {}), ...(policy.effort ? { reasoning: { effort: policy.effort } } : {}),
    ...(lane === "rewrite" ? { service_tier: "priority" } : {}),
  };
}
const safeErrorEvent = encoder.encode('event: error\ndata: {"type":"error","code":"upstream_stream_failed"}\n\n');
export function liveStream(body: ReadableStream<Uint8Array>, policy: LivePolicy, controller: AbortController, settle: (cost?: number) => Promise<void>): ReadableStream<Uint8Array> {
  const reader = body.getReader(); const decoder = new TextDecoder();
  let buffer = ""; let cost: number | undefined; let finished = false;
  let pending: ReturnType<typeof reader.read> | undefined;
  let terminal = false;
  const timeout = setTimeout(() => controller.abort(), LIVE_MAX_MS);
  const finish = async () => { if (finished) return; finished = true; clearTimeout(timeout); await settle(cost).catch(() => undefined); };
  return new ReadableStream<Uint8Array>({
    async pull(out) {
      try {
        for (;;) {
          const end = buffer.indexOf("\n\n");
          if (end >= 0) {
            const frame = buffer.slice(0, end); buffer = buffer.slice(end+2);
            const data = frame.split("\n").filter(line => line.startsWith("data:")).map(line => line.slice(5).trimStart()).join("\n");
            if (!data) continue;
            if (data === "[DONE]") { terminal = true; out.enqueue(encoder.encode("data: [DONE]\n\n")); return; }
            let parsed: unknown; try { parsed = JSON.parse(data); } catch { throw new Error("invalid upstream event"); }
            if (record(parsed) && (parsed.error || parsed.type === "error" || parsed.type === "response.failed")) throw new Error("upstream error");
            const reported = reportedCost(parsed, policy); if (reported !== undefined) cost = reported;
            if (record(parsed) && (parsed.type === "response.completed" || parsed.type === "response.incomplete")) terminal = true;
            out.enqueue(encoder.encode(`${frame}\n\n`)); return;
          }
          if (controller.signal.aborted) throw new Error("stream deadline");
          pending ??= reader.read();
          let timer: ReturnType<typeof setTimeout> | undefined;
          const next = await Promise.race([pending, new Promise<"heartbeat">((resolve) => { timer = setTimeout(() => resolve("heartbeat"), 15_000); })]);
          if (timer) clearTimeout(timer);
          if (next === "heartbeat") { out.enqueue(encoder.encode(": heartbeat\n\n")); return; }
          pending = undefined;
          if (next.done) {
            if (!terminal || buffer.trim()) throw new Error("incomplete upstream stream");
            await finish(); out.close(); return;
          }
          buffer = (buffer + decoder.decode(next.value, { stream: true })).replace(/\r\n/g, "\n");
          if (buffer.length > 256*1024) throw new Error("upstream event too large");
        }
      } catch {
        controller.abort(); await reader.cancel().catch(() => undefined); await finish(); out.enqueue(safeErrorEvent); out.close();
      }
    },
    async cancel() { controller.abort(); await reader.cancel().catch(() => undefined); await finish(); },
  });
}

export function createLiveApp(deps: { config: LiveConfig; store: LiveStore; publicKey: KeyObject | TrustedKeys; log: LogFn; fetch?: typeof fetch; denySubjects?: readonly string[] }) {
  const { config, store } = deps;
  const app = new Hono<{ Variables: { license: License } }>();
  app.use("*", redactingLogger(deps.log));
  app.onError((error, c) => {
    if (error instanceof AuthError) return c.json({ error: error.status === 401 ? "invalid_token" : "plan_required" }, error.status);
    if (error instanceof LiveRefusal) return c.json({ error: error.reason, resets_at: new Date(Date.now()+error.retryAfter*1000).toISOString() }, error.status, { "retry-after": String(error.retryAfter) });
    return c.json({ error: "live_unavailable" }, 503, { "retry-after": "5" });
  });
  app.use("*", async (c, next) => {
    c.header("cache-control", "no-store");
    if (c.req.header("origin") !== undefined) return c.json({ error: "browser_origin_refused" }, 403);
    const authorization = c.req.header("authorization"); const now = Math.floor(Date.now()/1000);
    let license: License;
    try { license = verifyLicense(authorization, deps.publicKey, now); }
    catch (error) {
      // An ended trial is 402 `plan_required`, as on every relay route (app.ts `authorize`, #940).
      if (error instanceof AuthError && error.status === 401 && isExpiredTrialToken(authorization, deps.publicKey, now)) throw new AuthError("plan_required", 402);
      throw error;
    }
    // The operator's emergency denylist (`RELAY_DENY_SUBJECTS`, audit 2026-10-20 L3).
    if (deniedByOperator(license, deps.denySubjects)) throw new AuthError("invalid or expired license token", 401);
    c.set("license", license);
    await next();
  });
  app.use("*", bodyLimit({ maxSize: 1024*1024, onError: c => c.json({ error: "body_too_large" }, 413) }));
  app.get("/v1/ai/usage", async c => {
    const license = c.get("license");
    return c.json({ ...await store.usage(meterSubject(license)), caps: config.caps[license.plan as LivePlan], unit: "micro_usd" });
  });
  app.post("/v1/ai/:lane", async c => {
    const lane = c.req.param("lane"); if (!isLiveLane(lane)) return c.json({ error: "unknown_lane" }, 404);
    const rawText = await c.req.text();
    const bytes = encoder.encode(rawText).byteLength;
    if (lane === "rewrite" && bytes > 64*1024) return c.json({ error: "body_too_large" }, 413);
    let raw: unknown; try { raw = JSON.parse(rawText); } catch { return c.json({ error: "invalid_request" }, 400); }
    // Live budgets meter the payer exactly as every other relay budget does (`meterSubject`,
    // owner decision 2026-10-21): a trial by its account (or its device-only `lic`), a paid plan
    // by its licence id even when the token also carries `acc`.
    const license = c.get("license"); const subject = meterSubject(license); const policy = config.policies[lane];
    const upstream = upstreamBody(raw, lane, policy, subject, config.hmacKey);
    if (!upstream) return c.json({ error: "invalid_request" }, 400);
    const caps = config.caps[license.plan as LivePlan];
    // Byte-bound conservative input estimate, including message/function framing. Output is
    // capped by the server. No token estimator supplied by the device is trusted.
    const reserve = Math.ceil((bytes*2+4096)*policy.inputMicrosPerToken + policy.maxOutputTokens*policy.outputMicrosPerToken);
    const ticket = await store.reserve({ subject, device: license.deviceId, provider: policy.provider, lane, reserve, daily: caps.daily, monthly: caps.monthly, global: config.globalDailyMicros });
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 30_000);
    const abort = () => controller.abort(); c.req.raw.signal.addEventListener("abort", abort, { once: true });
    const settle = async (cost?: number) => { c.req.raw.signal.removeEventListener("abort", abort); await store.settle(ticket, cost); };
    try {
      const response = await (deps.fetch ?? fetch)(policy.provider === "openai" ? "https://api.openai.com/v1/responses" : "https://api.groq.com/openai/v1/chat/completions", {
        method: "POST", redirect: "error", signal: controller.signal,
        headers: { authorization: `Bearer ${policy.provider === "openai" ? config.openaiKey : config.groqKey}`, "content-type": "application/json" }, body: JSON.stringify(upstream),
      });
      clearTimeout(timer);
      if (!response.ok || !response.body || !response.headers.get("content-type")?.includes("text/event-stream")) {
        await response.body?.cancel(); await settle();
        return c.json({ error: "upstream_failed" }, 502);
      }
      return new Response(liveStream(response.body, policy, controller, settle), { headers: { "content-type": "text/event-stream", "cache-control": "no-store", "x-accel-buffering": "no" } });
    } catch { await settle(); return c.json({ error: "upstream_failed" }, 502); }
    finally { clearTimeout(timer); }
  });
  return app;
}
