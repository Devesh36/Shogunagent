#!/usr/bin/env node
/**
 * Release versions agree (E40, docs/release.md): for each published package, the manifest version,
 * the version constant the code reports, and a `## <version>` section in its CHANGELOG.md are the
 * same. `--package sdk|cli|py` checks one; `--expect <version>` also requires that exact version
 * (the publish workflows pass the version the owner typed). Reads files only; no network.
 */
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const read = (path) => readFileSync(join(root, path), "utf8");
const constant = (path, pattern) => {
  const match = pattern.exec(read(path));
  if (!match) throw new Error(`${path}: version constant not found (${pattern})`);
  return match[1];
};

const packages = {
  sdk: {
    name: "@shogunagent/sdk (npm)",
    manifest: () => JSON.parse(read("packages/sdk-ts/package.json")).version,
    code: () => constant("packages/sdk-ts/src/client.ts", /export const SDK_VERSION = "([^"]+)"/),
    changelog: "packages/sdk-ts/CHANGELOG.md",
  },
  cli: {
    name: "shogunagent (npm CLI)",
    manifest: () => JSON.parse(read("packages/cli/package.json")).version,
    code: () => constant("packages/cli/src/help.ts", /export const VERSION = "([^"]+)"/),
    changelog: "packages/cli/CHANGELOG.md",
  },
  py: {
    name: "shogunagent (PyPI)",
    manifest: () => constant("packages/sdk-py/pyproject.toml", /^version = "([^"]+)"/m),
    code: () => constant("packages/sdk-py/src/shogunagent/client.py", /^SDK_VERSION = "([^"]+)"/m),
    changelog: "packages/sdk-py/CHANGELOG.md",
  },
};

const args = process.argv.slice(2);
const pick = args.includes("--package") ? args[args.indexOf("--package") + 1] : undefined;
const expected = args.includes("--expect") ? args[args.indexOf("--expect") + 1] : undefined;
if (pick !== undefined && !(pick in packages)) {
  console.error(`unknown --package ${pick}; one of ${Object.keys(packages).join(", ")}`);
  process.exit(2);
}
const SEMVER = /^\d+\.\d+\.\d+(-(alpha|beta|rc)\.\d+)?$/;
const problems = [];
for (const [key, p] of Object.entries(packages)) {
  if (pick !== undefined && key !== pick) continue;
  const manifest = p.manifest();
  const code = p.code();
  if (!SEMVER.test(manifest)) problems.push(`${p.name}: version ${manifest} is not MAJOR.MINOR.PATCH[-alpha|beta|rc.N]`);
  if (code !== manifest) problems.push(`${p.name}: the manifest says ${manifest}, the code reports ${code}`);
  if (expected !== undefined && manifest !== expected) problems.push(`${p.name}: the manifest says ${manifest}, the release asked for ${expected}`);
  const changelog = read(p.changelog);
  if (!new RegExp(`^## ${manifest.replace(/\./g, "\\.")}( |$)`, "m").test(changelog)) problems.push(`${p.changelog}: no "## ${manifest}" section`);
  if (problems.length === 0) console.log(`ok  ${p.name} ${manifest}`);
}
if (problems.length) {
  for (const line of problems) console.error(`FAIL ${line}`);
  process.exit(1);
}
