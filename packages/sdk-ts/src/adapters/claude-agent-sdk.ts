/**
 * Claude Agent SDK (TS, `@anthropic-ai/claude-agent-sdk`) adapter. Wrap `query` once; each query
 * with a string prompt recalls first and puts the block in `options.systemPrompt` (a string is
 * replaced in place; the `claude_code` preset gets it in `append`), then passes every message
 * through and ingests the prompt and the final `result` message when the stream ends.
 *
 * ```ts
 * import { query } from "@anthropic-ai/claude-agent-sdk";
 * import { withShogunAgent } from "@shogunagent/sdk/claude-agent-sdk";
 * const q = withShogunAgent(query, { threadKey: "task-42" });
 * for await (const m of q({ prompt: "What is still open with Oren?" })) if (m.type === "result") console.log(m.result);
 * ```
 *
 * A streaming-input prompt (an async iterable) is passed through untouched unless `recallQuery`
 * names what to recall. Structural types only; no dependency on the SDK.
 */
import { replaceContext } from "../context.js";
import { ShogunAgentMemory, textOf, type WithShogunAgentOptions } from "../middleware.js";

export { type WithShogunAgentOptions } from "../middleware.js";

export type SystemPromptLike =
  | string
  | { type: "preset"; preset: string; append?: string; [key: string]: unknown }
  | undefined;

export interface QueryOptionsLike {
  systemPrompt?: SystemPromptLike;
  resume?: string;
  [key: string]: unknown;
}

export interface QueryParamsLike {
  prompt: string | AsyncIterable<unknown>;
  options?: QueryOptionsLike;
}

export interface ClaudeAdapterOptions extends WithShogunAgentOptions {
  /** What to recall when the prompt is a stream of messages. */
  recallQuery?: string;
}

/** Put the block in a system prompt of either shape. */
export function injectSystemPrompt(system: SystemPromptLike, block: string): SystemPromptLike {
  if (system === undefined) return block || undefined;
  if (typeof system === "string") return replaceContext(system, block);
  const append = replaceContext(system.append ?? "", block);
  return { ...system, ...(append ? { append } : {}) };
}

/** Wrap `query`. The returned function has the same call shape and yields the same messages. */
export function withShogunAgent<M>(
  query: (params: QueryParamsLike) => AsyncIterable<M> & { interrupt?: () => Promise<void> },
  options: ClaudeAdapterOptions = {},
): (params: QueryParamsLike) => AsyncGenerator<M, void, undefined> {
  const memory = new ShogunAgentMemory(options, "claude-agent-sdk");
  return async function* wrappedQuery(params: QueryParamsLike) {
    const user = typeof params.prompt === "string" ? params.prompt : options.recallQuery;
    const block = await memory.block(user);
    const opts: QueryOptionsLike = { ...(params.options ?? {}) };
    const system = injectSystemPrompt(opts.systemPrompt, block);
    if (system !== undefined) opts.systemPrompt = system;
    let resultText = "";
    let assistantText = "";
    let turnId: string | undefined;
    let sessionId: string | undefined;
    for await (const message of query({ prompt: params.prompt, options: opts })) {
      const m = message as Record<string, unknown>;
      if (m.type === "assistant" && m.message && typeof m.message === "object") {
        const text = textOf((m.message as Record<string, unknown>).content);
        if (text) assistantText = text;
      }
      if (m.type === "result") {
        if (typeof m.result === "string") resultText = m.result;
        if (typeof m.uuid === "string") turnId = m.uuid;
        if (typeof m.session_id === "string") sessionId = m.session_id;
      }
      yield message;
    }
    await memory.remember({
      turnId,
      threadKey: options.threadKey ?? (sessionId ? `claude:${sessionId}` : undefined),
      user: typeof params.prompt === "string" ? params.prompt : undefined,
      assistant: resultText || assistantText,
    });
  };
}
