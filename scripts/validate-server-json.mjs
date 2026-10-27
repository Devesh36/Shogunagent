#!/usr/bin/env node
// E10-T2: validate the MCP Registry entry `server.json` at the repository root.
//
// Prefers the registry's own tool when it is installed (`mcp-publisher validate`); otherwise
// validates against the published JSON Schema the file names in `$schema`, from the committed
// copy in scripts/fixtures/ (fetched from static.modelcontextprotocol.io on 2026-10-25; refresh
// it with `--refresh-schema` when the registry publishes a new version). No network otherwise.
//
//   pnpm registry:validate
//   node scripts/validate-server-json.mjs --refresh-schema
import { spawnSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import Ajv from "ajv";
import addFormats from "ajv-formats";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const serverJsonPath = join(root, "server.json");
const schemaPath = join(root, "scripts/fixtures/mcp-registry-server.schema.json");

const server = JSON.parse(readFileSync(serverJsonPath, "utf8"));

if (process.argv.includes("--refresh-schema")) {
  const url = server.$schema;
  const res = await fetch(url);
  if (!res.ok) {
    console.error(`could not fetch ${url}: ${res.status}`);
    process.exit(1);
  }
  writeFileSync(schemaPath, (await res.text()).trimEnd() + "\n");
  console.log(`refreshed ${schemaPath} from ${url}`);
}

// 1. The registry's own validator, when present on PATH.
const publisher = spawnSync("mcp-publisher", ["validate", serverJsonPath], { encoding: "utf8" });
if (!publisher.error) {
  process.stdout.write(publisher.stdout ?? "");
  process.stderr.write(publisher.stderr ?? "");
  if (publisher.status !== 0) process.exit(publisher.status ?? 1);
  console.log("server.json: mcp-publisher validate passed");
} else {
  console.log("server.json: mcp-publisher not installed; validating against the committed schema copy");
}

// 2. JSON Schema (draft-07) from the committed copy. Runs even after mcp-publisher so the two
// cannot drift apart silently.
const schema = JSON.parse(readFileSync(schemaPath, "utf8"));
if (schema.$id !== server.$schema) {
  console.error(`server.json names ${server.$schema} but the committed schema is ${schema.$id}; run --refresh-schema`);
  process.exit(1);
}
const ajv = new Ajv({ strict: false, allErrors: true });
addFormats(ajv);
const validate = ajv.compile(schema);
const problems = [];
if (!validate(server)) {
  for (const e of validate.errors ?? []) problems.push(`${e.instancePath || "$"} ${e.message ?? ""} ${e.params ? JSON.stringify(e.params) : ""}`.trim());
}

// 3. The registry's rules the schema cannot express, and this repository's own.
if (server.name !== "io.github.torutesu/shogunagent") problems.push(`name must be io.github.torutesu/shogunagent, got ${server.name}`);
const pkgJson = JSON.parse(readFileSync(join(root, "packages/cli/package.json"), "utf8"));
for (const p of server.packages ?? []) {
  if (p.registryType === "npm") {
    if (p.identifier !== pkgJson.name) problems.push(`npm package identifier ${p.identifier} is not packages/cli's ${pkgJson.name}`);
    if (p.version !== pkgJson.version) problems.push(`npm package version ${p.version} is not packages/cli's ${pkgJson.version}`);
  }
}
if (server.version !== pkgJson.version) problems.push(`server version ${server.version} is not packages/cli's ${pkgJson.version}`);
const remote = (server.remotes ?? []).find((r) => r.type === "streamable-http");
if (!remote || remote.url !== "https://api.shogunagent.dev/mcp") problems.push("remotes must name the Streamable HTTP endpoint https://api.shogunagent.dev/mcp");
const authorization = remote?.headers?.find((h) => h.name === "Authorization");
if (authorization?.isRequired !== true) problems.push("the hosted /mcp endpoint requires an Authorization header, including for free reads");
if (authorization?.isSecret !== true) problems.push("the Authorization header must be marked secret");

if (problems.length > 0) {
  console.error(`server.json: ${problems.length} problem(s)`);
  for (const p of problems) console.error(`  - ${p}`);
  process.exit(1);
}
console.log(`server.json: valid against ${schema.$id}`);
