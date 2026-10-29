import { defineConfig } from "vitest/config";

// Unit tests inject an in-memory Fs/spawn/fetch. The local-launcher smoke uses isolated temporary
// files and a synthetic stdio executable; it never opens user stores/configs or needs the network.
export default defineConfig({
  test: {
    include: ["test/**/*.test.ts"],
    exclude: ["node_modules/**", "dist/**"],
  },
});
