// Node suite (`pnpm test`). The engine is runtime-neutral (one `node:crypto` import that the
// Workers `nodejs_compat` shim provides); a workerd suite is E4's job once `/mcp` is conformant.
import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["test/**/*.test.ts"],
    exclude: ["node_modules/**"],
  },
});
