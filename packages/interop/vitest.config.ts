import { defineConfig } from "vitest/config";

// No network, no I/O beyond reading the published schemas in docs/api.
export default defineConfig({
  test: {
    include: ["test/**/*.test.ts"],
    exclude: ["node_modules/**", "dist/**"],
  },
});
