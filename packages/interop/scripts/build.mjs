#!/usr/bin/env node
// Build ESM (dist/esm), CommonJS (dist/cjs) and declarations (dist/types) with tsc, then mark
// each output directory with its module type: the package is `"type": "module"`, so Node needs a
// `{"type":"commonjs"}` package.json next to the CommonJS files to load them as CommonJS.
import { execFileSync } from "node:child_process";
import { createRequire } from "node:module";
import { rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const tsc = createRequire(import.meta.url).resolve("typescript/bin/tsc");

rmSync(join(root, "dist"), { recursive: true, force: true });
for (const config of ["tsconfig.esm.json", "tsconfig.cjs.json", "tsconfig.types.json"]) {
  execFileSync(process.execPath, [tsc, "-p", join(root, config)], { stdio: "inherit", cwd: root });
}
writeFileSync(join(root, "dist/cjs/package.json"), JSON.stringify({ type: "commonjs" }) + "\n");
writeFileSync(join(root, "dist/esm/package.json"), JSON.stringify({ type: "module" }) + "\n");
console.log("built dist/esm, dist/cjs, dist/types");
