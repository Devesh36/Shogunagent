// The Node suite (`pnpm test:node`). The Workers-runtime suite under test/workers runs in workerd
// through vitest.workers.config.ts (`pnpm test:workers`); `pnpm test` runs both.
//
// The suites assert the Anthropic-shaped rows (Haiku 5.5 prices, effort, cache breakpoints), so
// they run on the config-selected `anthropic` provider; openai-gateway.test.ts re-imports
// models.ts under the default (luna) where it checks that routing.
import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["test/**/*.test.ts"],
    exclude: ["test/workers/**", "node_modules/**"],
    env: { BATCH_PROVIDER: "anthropic" },
  },
});
