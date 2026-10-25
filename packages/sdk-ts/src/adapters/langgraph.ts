/**
 * LangGraph.js adapter: wrap a graph node that calls the model. Before the node runs, the node
 * sees `state.messages` with the block in its first system message (or a new one prepended; any
 * earlier block replaced). After it returns, the last human message and the AI reply it produced
 * are ingested under the graph's `thread_id` (or `threadKey`).
 *
 * ```ts
 * import { StateGraph, MessagesAnnotation } from "@langchain/langgraph";
 * import { withShogunAgent } from "@shogunagent/sdk/langgraph";
 * const callModel = withShogunAgent(async (state) => ({ messages: [await model.invoke(state.messages)] }));
 * const graph = new StateGraph(MessagesAnnotation).addNode("agent", callModel).addEdge("__start__", "agent").compile();
 * ```
 *
 * Only the messages handed to the node change; the graph state is never written with the block.
 * Structural types only; no dependency on LangChain. A system message is replaced by a plain
 * `{ role: "system", content }` message-like, which every LangChain chat model accepts.
 */
import { replaceContext, stripContext } from "../context.js";
import { ShogunAgentMemory, textOf, type WithShogunAgentOptions } from "../middleware.js";

export { type WithShogunAgentOptions } from "../middleware.js";

export interface MessageLike {
  role?: string;
  type?: string;
  content: unknown;
  id?: string;
  _getType?: () => string;
  getType?: () => string;
  [key: string]: unknown;
}

export interface MessagesStateLike {
  messages: MessageLike[];
  [key: string]: unknown;
}

export interface RunnableConfigLike {
  configurable?: { thread_id?: string; [key: string]: unknown };
  [key: string]: unknown;
}

/** `system`, `human`, `ai`, `tool` — from a class instance or a plain message-like. */
export function roleOf(m: MessageLike): string {
  const t = (typeof m.getType === "function" ? m.getType() : typeof m._getType === "function" ? m._getType() : undefined) ?? m.type ?? m.role ?? "";
  if (t === "user") return "human";
  if (t === "assistant") return "ai";
  return t;
}

export function injectIntoMessages(messages: MessageLike[], block: string): MessageLike[] {
  const first = messages.findIndex((m) => roleOf(m) === "system");
  if (first < 0) return block ? [{ role: "system", content: block }, ...messages] : messages;
  return messages.map((m, i) => {
    if (roleOf(m) !== "system") return m;
    const text = textOf(m.content);
    const next = i === first ? replaceContext(text, block) : stripContext(text);
    return next === text ? m : { role: "system", content: next };
  });
}

function lastHuman(messages: MessageLike[]): string {
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i];
    if (m && roleOf(m) === "human") return textOf(m.content);
  }
  return "";
}

/** Wrap a node `(state, config) => update`. */
export function withShogunAgent<S extends MessagesStateLike, U extends { messages?: unknown }>(
  node: (state: S, config?: RunnableConfigLike) => Promise<U> | U,
  options: WithShogunAgentOptions = {},
): (state: S, config?: RunnableConfigLike) => Promise<U> {
  const memory = new ShogunAgentMemory(options, "langgraph");
  return async (state, config) => {
    const user = lastHuman(state.messages);
    const block = await memory.block(user);
    const update = await node({ ...state, messages: injectIntoMessages(state.messages, block) }, config);
    const produced = Array.isArray(update?.messages) ? (update.messages as MessageLike[]) : update?.messages ? [update.messages as MessageLike] : [];
    let reply: MessageLike | undefined;
    for (let i = produced.length - 1; i >= 0; i--) {
      const m = produced[i];
      if (m && roleOf(m) === "ai") { reply = m; break; }
    }
    if (reply) {
      await memory.remember({
        turnId: typeof reply.id === "string" ? reply.id : undefined,
        threadKey: options.threadKey ?? config?.configurable?.thread_id,
        user,
        assistant: textOf(reply.content),
      });
    }
    return update;
  };
}
