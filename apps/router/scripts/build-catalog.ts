/**
 * `pnpm --filter @shogunagent/router build:catalog` (E8-T2): compiles every connector definition,
 * fails on the first invalid one, and writes the content-hashed catalog JSON to
 * `dist/catalog.json` and `dist/catalog.<hash12>.json`. The manifest is what `server-card.json`,
 * `pricing.json` and the human docs derive from (invariant 12) — never the reverse.
 *
 * Prints counts and the hash only: catalog descriptions are public, but the habit of printing
 * nothing but numbers and ids is the one the rest of the router lives by.
 */
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { allProviders } from "../connectors/index.js";
import { CatalogCompileError, compileCatalog } from "../src/catalog.js";

const here = dirname(fileURLToPath(import.meta.url));
const outDir = join(here, "..", "dist");

try {
  const catalog = compileCatalog(allProviders());
  mkdirSync(outDir, { recursive: true });
  const text = `${JSON.stringify(catalog.manifest, null, 2)}\n`;
  const short = catalog.manifest.hash.slice(0, 12);
  writeFileSync(join(outDir, "catalog.json"), text);
  writeFileSync(join(outDir, `catalog.${short}.json`), text);
  const firstParty = catalog.entries().filter((e) => e.first_party).length;
  console.log(`catalog ok: providers=${catalog.manifest.provider_count} entries=${catalog.manifest.entry_count} first_party=${firstParty} hash=${catalog.manifest.hash}`);
  console.log(`wrote dist/catalog.json and dist/catalog.${short}.json`);
} catch (e) {
  if (e instanceof CatalogCompileError) {
    console.error(e.message);
    process.exit(1);
  }
  throw e;
}
