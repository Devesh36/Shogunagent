import { defineConfig } from "vitest/config";

// No network: every test injects a fake fetch (test/fake-fetch.ts). A test that reaches the
// network fails on purpose.
export default defineConfig({
  test: {
    include: ["test/**/*.test.ts"],
    // The real-framework suite has its own config (vitest.integration.config.ts, `test:integration`).
    exclude: ["node_modules/**", "dist/**", "test/integration/**"],
  },
});
