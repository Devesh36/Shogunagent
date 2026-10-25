/**
 * Vercel AI SDK adapter: a language-model middleware (`wrapLanguageModel({ model, middleware })`)
 * and `withShogunAgent(model)`, which does that wrapping without importing `ai`.
 *
 * ```ts
 * import { wrapLanguageModel, generateText } from "ai";
 * import { shogunAgentMiddleware } from "@shogunagent/sdk/ai-sdk";
 * const model = wrapLanguageModel({ model: openai("gpt-5"), middleware: shogunAgentMiddleware({ threadKey: "task-42" }) });
 * ```
 *
 * Structural types only (no dependency on `ai` / `@ai-sdk/provider`); works with the
 * LanguageModelV2/V3 call options (`prompt: [{ role, content }]`) and both the V1 (`text`,
 * `textDelta`) and V2 (`content[]`, `delta`) result shapes.
 */
import { replaceContext, stripContext } from "../context.js";
import { ShogunAgentMemory, textOf, type WithShogunAgentOptions } from "../middleware.js";

export { type WithShogunAgentOptions } from "../middleware.js";

/** A prompt message of the language-model spec. */
export interface LmMessage {
  role: string;
  content: unknown;
  [key: string]: unknown;
}

export interface LmCallOptions {
  prompt: LmMessage[];
  [key: string]: unknown;
}

/**
 * The language-model middleware shape. The methods are generic over the call options, the
 * generate result and the stream result, so the object is assignable to `ai`'s
 * `LanguageModelMiddleware` under `strict` + `exactOptionalPropertyTypes` without this package
 * depending on `ai`: `wrapLanguageModel({ model, middleware: shogunAgentMiddleware() })`
 * typechecks as written (test/integration/ai-sdk.test.ts).
 */
export interface LmMiddleware {
  middlewareVersion?: string;
  transformParams?<P extends LmCallOptions>(args: { type: "generate" | "stream"; params: P; model?: unknown }): PromiseLike<P>;
  wrapGenerate?<R>(args: { doGenerate: () => PromiseLike<R>; doStream: () => PromiseLike<unknown>; params: LmCallOptions; model: unknown }): PromiseLike<R>;
  wrapStream?<S extends { stream: ReadableStream<unknown> }>(args: { doGenerate: () => PromiseLike<unknown>; doStream: () => PromiseLike<S>; params: LmCallOptions; model: unknown }): PromiseLike<S>;
}

/** The last user message's text. */
export function lastUserText(prompt: LmMessage[]): string {
  for (let i = prompt.length - 1; i >= 0; i--) {
    const m = prompt[i];
    if (m?.role === "user") return textOf(m.content);
  }
  return "";
}

/** Put `block` in the first system message (stripping earlier blocks from every system message),
 * or prepend a system message when there is none. An empty block only strips. */
export function injectIntoPrompt(prompt: LmMessage[], block: string): LmMessage[] {
  const firstSystem = prompt.findIndex((m) => m.role === "system");
  if (firstSystem < 0) return block ? [{ role: "system", content: block }, ...prompt] : prompt;
  return prompt.map((m, i) => {
    if (m.role !== "system") return m;
    const text = typeof m.content === "string" ? m.content : textOf(m.content);
    return { ...m, content: i === firstSystem ? replaceContext(text, block) : stripContext(text) };
  });
}

function resultText(result: unknown): string {
  if (!result || typeof result !== "object") return "";
  const r = result as Record<string, unknown>;
  if (typeof r.text === "string") return r.text;
  if (Array.isArray(r.content)) return textOf(r.content);
  return "";
}

function responseId(result: unknown): string | undefined {
  if (!result || typeof result !== "object") return undefined;
  const r = result as Record<string, unknown>;
  const resp = (r.response ?? r.rawResponse) as Record<string, unknown> | undefined;
  return typeof resp?.id === "string" ? resp.id : undefined;
}

/** The middleware for `wrapLanguageModel`. */
export function shogunAgentMiddleware(options: WithShogunAgentOptions = {}): LmMiddleware & { memory: ShogunAgentMemory } {
  const memory = new ShogunAgentMemory(options, "ai-sdk");
  return {
    memory,
    middlewareVersion: "v2",
    async transformParams({ params }) {
      const query = lastUserText(params.prompt);
      const block = await memory.block(query);
      return { ...params, prompt: injectIntoPrompt(params.prompt, block) as typeof params.prompt };
    },
    async wrapGenerate({ doGenerate, params }) {
      const result = await doGenerate();
      await memory.remember({ turnId: responseId(result), user: lastUserText(params.prompt), assistant: resultText(result) });
      return result;
    },
    async wrapStream({ doStream, params }) {
      const out = await doStream();
      let text = "";
      let id: string | undefined;
      const user = lastUserText(params.prompt);
      const tap = new TransformStream<unknown, unknown>({
        transform(part, controller) {
          if (part && typeof part === "object") {
            const p = part as Record<string, unknown>;
            if (p.type === "text-delta") text += typeof p.delta === "string" ? p.delta : typeof p.textDelta === "string" ? p.textDelta : "";
            if (p.type === "response-metadata" && typeof p.id === "string") id = p.id;
          }
          controller.enqueue(part);
        },
        async flush() {
          await memory.remember({ turnId: id, user, assistant: text });
        },
      });
      return { ...out, stream: out.stream.pipeThrough(tap) } as typeof out;
    },
  };
}

/** A language model (any object with `doGenerate` / `doStream`). */
export interface LanguageModelLike {
  doGenerate(options: LmCallOptions): PromiseLike<unknown>;
  doStream(options: LmCallOptions): PromiseLike<{ stream: ReadableStream<unknown> }>;
}

/**
 * Wrap a model so every call recalls before and ingests after. Equivalent to
 * `wrapLanguageModel({ model, middleware: shogunAgentMiddleware(options) })`, without importing `ai`.
 */
export function withShogunAgent<M extends LanguageModelLike>(model: M, options: WithShogunAgentOptions = {}): M & LanguageModelLike {
  const mw = shogunAgentMiddleware(options);
  const wrapped = Object.create(model) as M & LanguageModelLike;
  const transform = async (type: "generate" | "stream", params: LmCallOptions) =>
    mw.transformParams ? mw.transformParams({ type, params, model }) : params;
  Object.assign(wrapped, {
    doGenerate: async (params: LmCallOptions) => {
      const p = await transform("generate", params);
      return mw.wrapGenerate!({ doGenerate: () => model.doGenerate(p), doStream: () => model.doStream(p), params: p, model });
    },
    doStream: async (params: LmCallOptions) => {
      const p = await transform("stream", params);
      return mw.wrapStream!({ doGenerate: () => model.doGenerate(p), doStream: () => model.doStream(p), params: p, model });
    },
  });
  return wrapped;
}
