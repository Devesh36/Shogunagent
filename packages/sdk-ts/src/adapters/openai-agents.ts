/**
 * OpenAI Agents SDK (JS, `@openai/agents`) adapter. Wrap `run` once; every run recalls before the
 * first model call (the block goes into the agent's instructions through the run's
 * `callModelInputFilter`, composed with yours) and ingests the turn when the run completes
 * (streamed runs: when `result.completed` settles).
 *
 * ```ts
 * import { Agent, run } from "@openai/agents";
 * import { withShogunAgent } from "@shogunagent/sdk/openai-agents";
 * const runWithMemory = withShogunAgent(run, { threadKey: "task-42" });
 * const result = await runWithMemory(new Agent({ name: "helper", instructions: "…" }), "What do I owe Oren?");
 * ```
 *
 * Structural types only; no dependency on `@openai/agents`.
 */
import { replaceContext } from "../context.js";
import { ShogunAgentMemory, textOf, type WithShogunAgentOptions } from "../middleware.js";

export { type WithShogunAgentOptions } from "../middleware.js";

/** What `callModelInputFilter` receives and returns (`CallModelData` / `ModelInputData`). */
export interface ModelInputData {
  instructions?: string | undefined;
  input: unknown;
  [key: string]: unknown;
}

export interface CallModelData {
  modelData: ModelInputData;
  agent?: unknown;
  context?: unknown;
}

export type CallModelInputFilter = (args: CallModelData) => ModelInputData | Promise<ModelInputData>;

export interface RunOptionsLike {
  callModelInputFilter?: CallModelInputFilter;
  stream?: boolean;
  conversationId?: string;
  [key: string]: unknown;
}

export interface RunResultLike {
  finalOutput?: unknown;
  lastResponseId?: string | undefined;
  completed?: Promise<unknown>;
  [key: string]: unknown;
}

/** The last user message of a run input (a string, or `AgentInputItem[]`). */
export function lastUserInput(input: unknown): string {
  if (typeof input === "string") return input;
  if (!Array.isArray(input)) return "";
  for (let i = input.length - 1; i >= 0; i--) {
    const item = input[i] as Record<string, unknown> | undefined;
    if (item && item.role === "user") return textOf(item.content);
  }
  return "";
}

function outputText(output: unknown): string {
  if (output === undefined || output === null) return "";
  if (typeof output === "string") return output;
  try {
    return JSON.stringify(output);
  } catch {
    return "";
  }
}

/** A `callModelInputFilter` that writes `block` into the instructions, after `previous`. */
export function injectingFilter(block: string, previous?: CallModelInputFilter): CallModelInputFilter {
  return async (args) => {
    const data = previous ? await previous(args) : args.modelData;
    return { ...data, instructions: replaceContext(data.instructions ?? "", block) };
  };
}

/**
 * A standalone `callModelInputFilter` that recalls on every model call of a run (from the last
 * user item of the model input). Use it when you cannot wrap `run`; it does not ingest.
 */
export function shogunAgentInputFilter(options: WithShogunAgentOptions = {}): CallModelInputFilter {
  const memory = new ShogunAgentMemory({ ...options, ingest: false }, "openai-agents");
  return async ({ modelData }) => ({
    ...modelData,
    instructions: await memory.inject(modelData.instructions ?? "", lastUserInput(modelData.input)),
  });
}

/** Wrap `run` (`(agent, input, options) => result`). */
export function withShogunAgent<A, R extends RunResultLike>(
  run: (agent: A, input: string | unknown[], options?: RunOptionsLike) => Promise<R>,
  options: WithShogunAgentOptions = {},
): (agent: A, input: string | unknown[], runOptions?: RunOptionsLike) => Promise<R> {
  const memory = new ShogunAgentMemory(options, "openai-agents");
  return async (agent, input, runOptions = {}) => {
    const user = lastUserInput(input);
    const block = await memory.block(user);
    const result = await run(agent, input, { ...runOptions, callModelInputFilter: injectingFilter(block, runOptions.callModelInputFilter) });
    const threadKey = options.threadKey ?? runOptions.conversationId;
    const finish = () => memory.remember({ turnId: result.lastResponseId, threadKey, user, assistant: outputText(result.finalOutput) });
    if (result.completed && typeof result.completed.then === "function") {
      // Streamed: ingest once the stream is done; the caller consumes the stream meanwhile.
      void result.completed.then(finish, () => undefined);
    } else {
      await finish();
    }
    return result;
  };
}
