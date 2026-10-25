/**
 * `pnpm --filter @shogunagent/router listing:check <provider-dir>... | --all [--out report.json] [--markdown summary.md]`
 *
 * The provider self-serve listing pipeline (E21; src/listing/check.ts). For each provider
 * directory (a folder under connectors/ holding provider.ts and provider.json) it runs schema
 * validation, fixture replay, the scope-classification check, the description lint and the price
 * sanity check, and prints one machine-readable JSON document:
 *
 *   { "format": "shogunagent-listing-reports/1", "ok": bool, "reports": [ListingReport, …] }
 *
 * `--all` checks every directory under connectors/. A directory without provider.ts that holds an
 * imported catalog (connectors/monid) is reported as skipped: it is a passthrough, listed through
 * its own catalog, not a self-serve listing. When GITHUB_STEP_SUMMARY is set (Actions), the
 * markdown rendering is appended to it. Exit 1 when any report fails.
 *
 * Relative paths resolve against the current directory, then against this package's root (pnpm
 * runs the script from apps/router).
 */
import { appendFileSync, existsSync, readdirSync, statSync, writeFileSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import type { ProviderDefinition } from "../connectors/types.js";
import { checkListing, failedReport, renderMarkdown, skippedReport, type ListingReport } from "../src/listing/check.js";

const pkgRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const connectorsDir = join(pkgRoot, "connectors");

function usage(): never {
  console.error("usage: listing:check <provider-dir>... | --all [--out <report.json>] [--markdown <summary.md>]");
  process.exit(2);
}

function parseArgs(argv: string[]): { dirs: string[]; all: boolean; out?: string; markdown?: string } {
  const dirs: string[] = [];
  let all = false;
  let out: string | undefined;
  let markdown: string | undefined;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--all") all = true;
    else if (a === "--out") out = argv[++i] ?? usage();
    else if (a === "--markdown") markdown = argv[++i] ?? usage();
    else if (a === "--") continue;
    else if (a?.startsWith("--")) usage();
    else if (a) dirs.push(a);
  }
  if (!all && dirs.length === 0) usage();
  return { dirs, all, ...(out ? { out } : {}), ...(markdown ? { markdown } : {}) };
}

function resolveDir(d: string): string {
  const fromCwd = resolve(process.cwd(), d);
  if (existsSync(fromCwd)) return fromCwd;
  const fromPkg = resolve(pkgRoot, d.replace(/^apps\/router\//, ""));
  return existsSync(fromPkg) ? fromPkg : fromCwd;
}

function isProvider(v: unknown): v is ProviderDefinition {
  return !!v && typeof v === "object" && typeof (v as ProviderDefinition).id === "string" && Array.isArray((v as ProviderDefinition).endpoints) && typeof (v as ProviderDefinition).request === "object";
}

async function checkDir(abs: string): Promise<ListingReport> {
  const shown = relative(pkgRoot, abs) || ".";
  if (!existsSync(abs) || !statSync(abs).isDirectory()) return failedReport(shown, "not a directory");
  const providerTs = join(abs, "provider.ts");
  if (!existsSync(providerTs)) {
    if (readdirSync(abs).some((f) => f.endsWith("catalog.fixture.json"))) return skippedReport(shown, "passthrough catalog import (src/passthrough/monid.ts), listed through its own catalog; not a self-serve listing");
    return failedReport(shown, "no provider.ts in this directory (connectors/schema.md §1)");
  }
  let mod: Record<string, unknown>;
  try {
    mod = (await import(pathToFileURL(providerTs).href)) as Record<string, unknown>;
  } catch (e) {
    return failedReport(shown, `provider.ts does not load: ${e instanceof Error ? e.message.split("\n")[0] : "unknown error"}`);
  }
  const providers = Object.values(mod).filter(isProvider);
  if (providers.length !== 1) return failedReport(shown, `provider.ts must export exactly one provider definition (found ${providers.length})`);
  return checkListing(providers[0] as ProviderDefinition, shown);
}

const args = parseArgs(process.argv.slice(2));
const targets = args.all
  ? readdirSync(connectorsDir)
      .map((n) => join(connectorsDir, n))
      .filter((p) => statSync(p).isDirectory())
      .sort()
  : args.dirs.map(resolveDir);

const reports: ListingReport[] = [];
for (const t of targets) reports.push(await checkDir(t));
const doc = { format: "shogunagent-listing-reports/1", ok: reports.every((r) => r.ok), reports };
const json = `${JSON.stringify(doc, null, 2)}\n`;
process.stdout.write(json);
if (args.out) writeFileSync(resolve(process.cwd(), args.out), json);
const md = renderMarkdown(reports);
if (args.markdown) writeFileSync(resolve(process.cwd(), args.markdown), md);
if (process.env["GITHUB_STEP_SUMMARY"]) appendFileSync(process.env["GITHUB_STEP_SUMMARY"], md);
for (const r of reports) console.error(`listing ${r.skipped ? "skip" : r.ok ? "pass" : "FAIL"}: ${r.provider_dir}${r.provider_id ? ` (${r.provider_id})` : ""} errors=${r.problems.filter((p) => p.severity === "error").length} warnings=${r.problems.filter((p) => p.severity === "warning").length}`);
process.exit(doc.ok ? 0 : 1);
