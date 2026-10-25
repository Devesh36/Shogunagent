/**
 * Mastra adapter: an input processor (recall before the model; the block becomes one system
 * message with a fixed id, replaced on every call) and an output processor (ingest the turn after
 * the model), added to an agent config.
 *
 * ```ts
 * import { Agent } from "@mastra/core/agent";
 * import { withShogunAgent } from "@shogunagent/sdk/mastra";
 * const agent = new Agent(withShogunAgent({ name: "helper", instructions: "…", model }, { threadKey: "task-42" }));
 * ```
 *
 * Structural types only; no dependency on `@mastra/core`. Messages may carry `content` as a
 * string, a parts array, or Mastra's `{ format: 2, parts }`.
 */
import { CONTEXT_OPEN, stripContext } from "../context.js";
import { ShogunAgentMemory, textOf, type WithShogunAgentOptions } from "../middleware.js";

export { type WithShogunAgentOptions } from "../middleware.js";

export const CONTEXT_MESSAGE_ID = "shogunagent-state-of-work";

export interface MastraMessageLike {
  id?: string;
  role: string;
  content: unknown;
  threadId?: string;
  createdAt?: Date;
  [key: string]: unknown;
}

export interface ProcessorLike {
  name: string;
  processInput?: (args: { messages: MastraMessageLike[]; [key: string]: unknown }) => Promise<MastraMessageLike[]> | MastraMessageLike[];
  processOutputResult?: (args: { messages: MastraMessageLike[]; [key: string]: unknown }) => Promise<MastraMessageLike[]> | MastraMessageLike[];
}

export interface AgentConfigLike {
  inputProcessors?: ProcessorLike[];
  outputProcessors?: ProcessorLike[];
  [key: string]: unknown;
}

function lastOf(messages: MastraMessageLike[], role: string): MastraMessageLike | undefined {
  for (let i = messages.length - 1; i >= 0; i--) if (messages[i]?.role === role) return messages[i];
  return undefined;
}

/** The messages without any block: our own block message dropped, a block inside another system
 * message stripped from its text. */
function withoutContext(messages: MastraMessageLike[]): MastraMessageLike[] {
  const out: MastraMessageLike[] = [];
  for (const m of messages) {
    if (m.id === CONTEXT_MESSAGE_ID) continue;
    if (m.role === "system" && textOf(m.content).includes(CONTEXT_OPEN)) {
      const text = stripContext(textOf(m.content));
      if (text) out.push({ ...m, content: text });
      continue;
    }
    out.push(m);
  }
  return out;
}

/** The two processors, sharing the user text of the call between input and output by thread. */
export function shogunAgentProcessors(options: WithShogunAgentOptions = {}): { input: ProcessorLike; output: ProcessorLike; memory: ShogunAgentMemory } {
  const memory = new ShogunAgentMemory(options, "mastra");
  const pending = new Map<string, string>();
  const threadOf = (messages: MastraMessageLike[]) =>
    options.threadKey ?? messages.find((m) => typeof m.threadId === "string")?.threadId ?? "mastra:default";
  const input: ProcessorLike = {
    name: "shogunagent-recall",
    async processInput({ messages }) {
      const user = lastOf(messages, "user");
      const query = user ? textOf(user.content) : "";
      if (query) pending.set(threadOf(messages), query);
      const block = await memory.block(query);
      const rest = withoutContext(messages);
      if (!block) return rest;
      return [{ id: CONTEXT_MESSAGE_ID, role: "system", content: block, createdAt: new Date(0) }, ...rest];
    },
  };
  const output: ProcessorLike = {
    name: "shogunagent-ingest",
    async processOutputResult({ messages }) {
      const reply = lastOf(messages, "assistant");
      const thread = threadOf(messages);
      const user = pending.get(thread);
      pending.delete(thread);
      await memory.remember({ turnId: reply?.id, threadKey: thread, user, assistant: reply ? textOf(reply.content) : "" });
      return messages;
    },
  };
  return { input, output, memory };
}

/** Add the processors to an agent config (existing processors kept, ours first in, last out). */
export function withShogunAgent<C extends AgentConfigLike>(config: C, options: WithShogunAgentOptions = {}): C & { inputProcessors: ProcessorLike[]; outputProcessors: ProcessorLike[] } {
  const { input, output } = shogunAgentProcessors(options);
  const strip = (list: ProcessorLike[] | undefined) => (list ?? []).filter((p) => p.name !== input.name && p.name !== output.name);
  return {
    ...config,
    inputProcessors: [input, ...strip(config.inputProcessors)],
    outputProcessors: [...strip(config.outputProcessors), output],
  };
}
