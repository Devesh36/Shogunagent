/**
 * Integration tests run the real frameworks, which own HTTP clients of their own (the OpenAI
 * tracing exporter, LangSmith, provider SDKs). Nothing may leave the machine: every test installs
 * this guard, which replaces `globalThis.fetch` with one that records the attempt and throws, and
 * asserts at the end that nothing was attempted. ShogunAgent itself is reached only through the
 * injected fake fetch (`../fake-fetch.ts`), never through the global one.
 */
import { afterEach, beforeEach, expect } from "vitest";

const FRAMEWORK_NETWORK_ENV = [
  "OPENAI_API_KEY",
  "ANTHROPIC_API_KEY",
  "LANGCHAIN_TRACING_V2",
  "LANGCHAIN_TRACING",
  "LANGSMITH_TRACING",
  "LANGCHAIN_API_KEY",
  "LANGSMITH_API_KEY",
];

export function installNetworkGuard(): { attempts: string[] } {
  const state = { attempts: [] as string[] };
  let original: typeof globalThis.fetch;
  const saved: Record<string, string | undefined> = {};
  beforeEach(() => {
    state.attempts.length = 0;
    original = globalThis.fetch;
    globalThis.fetch = (async (input: unknown) => {
      const url = typeof input === "string" ? input : input instanceof URL ? input.href : String((input as { url?: string }).url ?? input);
      state.attempts.push(url);
      throw new Error(`network access attempted in an offline test: ${url}`);
    }) as typeof globalThis.fetch;
    for (const name of FRAMEWORK_NETWORK_ENV) {
      saved[name] = process.env[name];
      delete process.env[name];
    }
    process.env.OPENAI_AGENTS_DISABLE_TRACING = "1";
  });
  afterEach(() => {
    globalThis.fetch = original;
    for (const name of FRAMEWORK_NETWORK_ENV) {
      if (saved[name] === undefined) delete process.env[name];
      else process.env[name] = saved[name];
    }
    expect(state.attempts, "a framework tried to reach the network").toEqual([]);
  });
  return state;
}

/** How many context blocks a text carries. */
export const blockCount = (s: string): number => (s.match(/<shogunagent context="state-of-work" readonly>/g) ?? []).length;
