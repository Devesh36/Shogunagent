import { defineConfig } from "vitest/config";

// E36b: withShogunAgent inside the real frameworks (Vercel AI SDK, OpenAI Agents SDK, Claude Agent
// SDK, LangGraph.js), each with its own mock model or a fake model process. No network: every test
// installs test/integration/net-guard.ts, and ShogunAgent is reached through the fake fetch.
export default defineConfig({
  test: {
    include: ["test/integration/**/*.test.ts"],
    exclude: ["node_modules/**", "dist/**"],
    testTimeout: 20_000,
  },
});
