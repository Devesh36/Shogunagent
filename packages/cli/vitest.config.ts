import { defineConfig } from "vitest/config";

// No network and no real filesystem writes in tests: every test injects an in-memory `Fs`, a fake
// `spawn` and a fake `fetch` (src/types.ts). A test that reaches the network fails on purpose.
export default defineConfig({
  test: {
    include: ["test/**/*.test.ts"],
    exclude: ["node_modules/**", "dist/**"],
  },
});
