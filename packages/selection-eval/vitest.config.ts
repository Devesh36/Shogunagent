import { defineConfig } from "vitest/config";

// Tests never touch the network: every adapter test injects a fake fetch (test/adapters.test.ts).
export default defineConfig({
  test: {
    include: ["test/**/*.test.ts"],
    exclude: ["node_modules/**"],
  },
});
