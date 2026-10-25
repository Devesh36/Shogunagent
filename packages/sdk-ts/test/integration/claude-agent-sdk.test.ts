/**
 * E36b: `withShogunAgent(query)` around the real Claude Agent SDK
 * (`@anthropic-ai/claude-agent-sdk`). The SDK normally spawns the Claude Code process; here
 * `spawnClaudeCodeProcess` hands it an in-memory fake that speaks the SDK's stream-json protocol
 * (control `initialize`, one user message in, `system` / `assistant` / `result` out). The SDK's
 * own transport, initialize handshake and message parsing run for real; no process, no model,
 * no network.
 */
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";

import { query } from "@anthropic-ai/claude-agent-sdk";
import { describe, expect, it } from "vitest";

import { withShogunAgent } from "../../src/adapters/claude-agent-sdk.js";
import { PaymentRequiredError } from "../../src/index.js";
import { FakeFetch, PAYMENT_402, memoryFake, recallPayload, toolText } from "../fake-fetch.js";
import { blockCount, installNetworkGuard } from "./net-guard.js";

installNetworkGuard();

interface Seen {
  initialize?: Record<string, unknown>;
  user?: Record<string, unknown>;
  args?: string[];
  spawned: number;
}

/** A Claude Code stand-in: answers the control handshake and one turn with `reply`. */
function fakeClaudeCode(reply: string, seen: Seen, sessionId = "sess-1", resultUuid = "res-uuid-1") {
  return (options: { args: string[] }) => {
    seen.spawned += 1;
    seen.args = options.args;
    const stdin = new PassThrough();
    const stdout = new PassThrough();
    const events = new EventEmitter();
    let exitCode: number | null = null;
    let killed = false;
    const exit = (code: number | null) => {
      if (exitCode !== null || killed) return;
      exitCode = code ?? 0;
      stdout.end();
      queueMicrotask(() => events.emit("exit", code, null));
    };
    const send = (message: unknown) => stdout.write(`${JSON.stringify(message)}\n`);
    const handle = (message: Record<string, unknown>) => {
      if (message.type === "control_request") {
        const request = message.request as Record<string, unknown>;
        if (request.subtype === "initialize") seen.initialize = request;
        send({
          type: "control_response",
          response: { subtype: "success", request_id: message.request_id, response: { commands: [], agents: [], models: [], account: {}, output_style: "default", available_output_styles: ["default"] } },
        });
        return;
      }
      if (message.type === "user") {
        seen.user = message;
        const base = { session_id: sessionId, parent_tool_use_id: null };
        send({ type: "system", subtype: "init", ...base, uuid: "00000000-0000-4000-8000-000000000001", cwd: "/", tools: [], mcp_servers: [], model: "mock", permissionMode: "default", slash_commands: [], apiKeySource: "none", output_style: "default", skills: [], plugins: [] });
        send({ type: "assistant", ...base, uuid: "00000000-0000-4000-8000-000000000002", message: { id: "msg_1", type: "message", role: "assistant", model: "mock", content: [{ type: "text", text: reply }], stop_reason: "end_turn", stop_sequence: null, usage: { input_tokens: 3, output_tokens: 5 } } });
        send({ type: "result", subtype: "success", ...base, uuid: resultUuid, is_error: false, result: reply, duration_ms: 1, duration_api_ms: 1, num_turns: 1, total_cost_usd: 0, usage: { input_tokens: 3, output_tokens: 5 }, modelUsage: {}, permission_denials: [] });
      }
    };
    let buffer = "";
    stdin.on("data", (chunk: Buffer) => {
      buffer += chunk.toString("utf8");
      for (let i = buffer.indexOf("\n"); i >= 0; i = buffer.indexOf("\n")) {
        const line = buffer.slice(0, i).trim();
        buffer = buffer.slice(i + 1);
        if (line) handle(JSON.parse(line) as Record<string, unknown>);
      }
    });
    stdin.on("finish", () => exit(0));
    return {
      stdin,
      stdout,
      get killed() { return killed; },
      get exitCode() { return exitCode; },
      kill: () => { if (exitCode === null) { killed = true; stdout.end(); queueMicrotask(() => events.emit("exit", null, "SIGTERM")); } return true; },
      on: (event: string, listener: (...args: unknown[]) => void) => { events.on(event, listener); },
      once: (event: string, listener: (...args: unknown[]) => void) => { events.once(event, listener); },
      off: (event: string, listener: (...args: unknown[]) => void) => { events.off(event, listener); },
    };
  };
}

/** The system prompt the SDK handed to Claude Code (initialize request or CLI flag). */
function systemPromptSeen(seen: Seen): string {
  const init = seen.initialize ?? {};
  const fromInit = [init.systemPrompt, init.appendSystemPrompt, init.append_system_prompt]
    .flat()
    .filter((v): v is string => typeof v === "string");
  const args = seen.args ?? [];
  const fromArgs = args.flatMap((a, i) => (a === "--system-prompt" || a === "--append-system-prompt") && args[i + 1] ? [args[i + 1]!] : []);
  return [...fromInit, ...fromArgs].join("\n\n");
}

async function drive(wrapped: ReturnType<typeof withShogunAgent>, prompt: string, options: Record<string, unknown>) {
  const messages: Array<Record<string, unknown>> = [];
  for await (const m of wrapped({ prompt, options })) messages.push(m as Record<string, unknown>);
  return messages;
}

describe("Claude Agent SDK (@anthropic-ai/claude-agent-sdk) integration", () => {
  it("query: recall before the process starts (block in the system prompt), every message passes through, ingest after", async () => {
    const fake = memoryFake();
    const seen: Seen = { spawned: 0 };
    const wrapped = withShogunAgent(query as never, { fetch: fake.fetch, threadKey: "task-1" });
    const messages = await drive(wrapped, "What do I owe Oren?", { systemPrompt: "You are helpful.", spawnClaudeCodeProcess: fakeClaudeCode("I will draft the plan.", seen), persistSession: false });

    expect(seen.spawned).toBe(1);
    expect(messages.map((m) => m.type)).toEqual(["system", "assistant", "result"]);
    expect(messages.at(-1)).toMatchObject({ type: "result", result: "I will draft the plan." });
    const system = systemPromptSeen(seen);
    expect(system).toContain("You are helpful.");
    expect(blockCount(system)).toBe(1);
    expect(system).toContain("send the rollout plan by Friday");
    expect(system).not.toContain("SECRET-LOW-ITEM");
    expect(JSON.stringify(seen.user)).toContain("What do I owe Oren?");
    expect(fake.calls.map((c) => c.headers["mcp-name"])).toEqual(["memory.recall", "memory.ingest"]);
    const ingest = fake.toolCalls("memory.ingest")[0]!.body.params.arguments;
    expect(ingest).toMatchObject({ thread_key: "task-1", turns: [{ role: "user", content: "What do I owe Oren?" }, { role: "assistant", content: "I will draft the plan." }] });
    expect(ingest.idempotency_key).toMatch(/^turn:[0-9a-f]{40}$/);
  });

  it("without a threadKey the turn lands under the Claude session; the same result uuid replays its key", async () => {
    const fake = memoryFake();
    const wrapped = withShogunAgent(query as never, { fetch: fake.fetch });
    for (const uuid of ["res-a", "res-a", "res-b"]) {
      await drive(wrapped, "status?", { spawnClaudeCodeProcess: fakeClaudeCode("ok", { spawned: 0 }, "sess-9", uuid), persistSession: false });
    }
    const ingests = fake.toolCalls("memory.ingest").map((c) => c.body.params.arguments);
    expect(ingests.map((i) => i.thread_key)).toEqual(["claude:sess-9", "claude:sess-9", "claude:sess-9"]);
    expect(ingests[1].idempotency_key).toBe(ingests[0].idempotency_key);
    expect(ingests[2].idempotency_key).not.toBe(ingests[0].idempotency_key);
  });

  it("the claude_code preset gets the block in append", async () => {
    const seen: Seen = { spawned: 0 };
    const wrapped = withShogunAgent(query as never, { fetch: memoryFake().fetch });
    await drive(wrapped, "q", { systemPrompt: { type: "preset", preset: "claude_code", append: "Be brief." }, spawnClaudeCodeProcess: fakeClaudeCode("ok", seen), persistSession: false });
    const system = systemPromptSeen(seen);
    expect(system).toContain("Be brief.");
    expect(blockCount(system)).toBe(1);
  });

  it("a 402 on ingest is surfaced once; the messages were already delivered", async () => {
    const fake = new FakeFetch().tool("memory.recall", toolText(recallPayload())).tool("memory.ingest", PAYMENT_402);
    const errors: Array<[unknown, string]> = [];
    const wrapped = withShogunAgent(query as never, { fetch: fake.fetch, onError: (e, p) => errors.push([e, p]) });
    const messages = await drive(wrapped, "q", { spawnClaudeCodeProcess: fakeClaudeCode("fine", { spawned: 0 }), persistSession: false });
    expect(messages.at(-1)).toMatchObject({ type: "result", result: "fine" });
    expect(fake.toolCalls("memory.ingest")).toHaveLength(1);
    expect(errors).toHaveLength(1);
    expect(errors[0]![0]).toBeInstanceOf(PaymentRequiredError);
    expect(errors[0]![1]).toBe("ingest");
  });
});
