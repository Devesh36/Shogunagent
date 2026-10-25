// The Workers-runtime suite: test/workers/*.test.ts run INSIDE workerd (the same runtime the
// deployed Worker uses), through @cloudflare/vitest-pool-workers. This is what proves that
// `node:crypto` Ed25519 verification, the redacting logger and the Worker entry's fail-closed boot
// behave in the real runtime, not in Node. The compatibility date/flags mirror wrangler.jsonc.
import { defineWorkersConfig } from "@cloudflare/vitest-pool-workers/config";

export default defineWorkersConfig({
  test: {
    include: ["test/workers/**/*.test.ts"],
    poolOptions: {
      workers: {
        // Bindings are handed to the Worker entry by each test (a plain `env` object), so no
        // wrangler config is loaded here and no Hyperdrive/Postgres is needed.
        miniflare: {
          compatibilityDate: "2026-03-01",
          compatibilityFlags: ["nodejs_compat"],
        },
      },
    },
  },
});
