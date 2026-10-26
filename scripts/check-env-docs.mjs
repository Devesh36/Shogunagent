#!/usr/bin/env node
// E41: every environment variable the hosted service reads must be named in the production
// runbook (docs/deploy/production.md), so an operator never meets a setting the runbook does not
// explain. Fails when a name read in apps/api/src or apps/router/src (or named as a connector
// credential in apps/router/connectors) is not written in the runbook as `NAME` (in backticks).
//
// What counts as "read" (comments are stripped first; *.test.ts and *.generated.ts are skipped):
//   env.NAME / env?.NAME / process.env.NAME / settings.NAME / envRecord(env).NAME
//   env["NAME"] / env['NAME']
//   helper calls whose first string argument is the name:
//     required("NAME")  intFromEnv(env, "NAME", …)  nodeInt("NAME", …)  usd("NAME", …)  usdMicros("NAME", …)
//   connector credentials: credential_env: "NAME", baseUrlEnv: "NAME", *_CREDENTIAL_ENV = "NAME"
//   the router's ENV_NAMES block (apps/router/src/env.ts)
//   the sidecar's hosted connect settings (E51): `pub const *_VAR: &str = "NAME";` in the Rust
//   files of RUST_SOURCES (`SHOGUN_CONNECT_*`, read by `HostedClients::from_vars`)
//
//   node scripts/check-env-docs.mjs              check the tree
//   node scripts/check-env-docs.mjs --list       print every name found, with where it is read
//   node scripts/check-env-docs.mjs --self-test  check the extractor against fixed samples
import { readFileSync, readdirSync, statSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const RUNBOOK = "docs/deploy/production.md";
const SOURCE_DIRS = ["apps/api/src", "apps/router/src", "apps/router/connectors"];
/** Rust files whose `*_VAR` constants name environment variables the hosted sidecar reads. */
const RUST_SOURCES = ["crates/shogun-integrations/src/hosted_connect.rs"];

const NAME = "[A-Z][A-Z0-9_]*[A-Z0-9]";
const PATTERNS = [
  // member access on an env-like object
  new RegExp(`\\b(?:process\\.env|env|settings|envRecord\\([^)]*\\))\\??\\.(${NAME})\\b`, "g"),
  // bracket access with a literal
  new RegExp(`\\b(?:env|settings|process\\.env)\\[\\s*["'\`](${NAME})["'\`]\\s*\\]`, "g"),
  // helpers that take the variable name as a string
  new RegExp(`\\b(?:required|nodeInt|usd|usdMicros)\\(\\s*["'\`](${NAME})["'\`]`, "g"),
  new RegExp(`\\bintFromEnv\\(\\s*[\\w.]+\\s*,\\s*["'\`](${NAME})["'\`]`, "g"),
  // connector credential / base-URL names, injected at the transport
  new RegExp(`\\b(?:credential_env|baseUrlEnv)\\s*:\\s*["'\`](${NAME})["'\`]`, "g"),
  new RegExp(`\\b\\w*_CREDENTIAL_ENV\\s*=\\s*["'\`](${NAME})["'\`]`, "g"),
];

/** Remove block and line comments, leaving string literals (and the `//` inside them) intact. */
export function stripComments(src) {
  let out = "";
  let quote = "";
  for (let i = 0; i < src.length; i++) {
    const ch = src[i];
    if (quote) {
      out += ch;
      if (ch === "\\") { out += src[i + 1] ?? ""; i++; }
      else if (ch === quote || (ch === "\n" && quote !== "`")) quote = "";
      continue;
    }
    if (ch === "/" && src[i + 1] === "/") { while (i < src.length && src[i] !== "\n") i++; out += "\n"; continue; }
    if (ch === "/" && src[i + 1] === "*") { const end = src.indexOf("*/", i + 2); i = end === -1 ? src.length : end + 1; out += " "; continue; }
    if (ch === "\"" || ch === "'" || ch === "`") quote = ch;
    out += ch;
  }
  return out;
}

/** Every env name the source text reads. */
export function namesIn(src) {
  const code = stripComments(src);
  const found = new Set();
  for (const re of PATTERNS) for (const m of code.matchAll(re)) found.add(m[1]);
  const block = code.match(/ENV_NAMES\s*=\s*\{([\s\S]*?)\}\s*as\s+const/);
  if (block) for (const m of block[1].matchAll(new RegExp(`["'\`](${NAME})["'\`]`, "g"))) found.add(m[1]);
  return found;
}

/** Every `pub const X_VAR: &str = "NAME";` in Rust source (comments stripped the same way). */
export function rustVarNamesIn(src) {
  const code = stripComments(src);
  const found = new Set();
  for (const m of code.matchAll(new RegExp(`\\bconst\\s+\\w+_VAR\\s*:\\s*&(?:'static\\s+)?str\\s*=\\s*"(${NAME})"`, "g"))) found.add(m[1]);
  return found;
}

/** Names written as `NAME` in the runbook. */
export function documentedIn(markdown) {
  const out = new Set();
  for (const m of markdown.matchAll(new RegExp(`\`(${NAME})\``, "g"))) out.add(m[1]);
  return out;
}

function walk(dir, out = []) {
  for (const entry of readdirSync(dir)) {
    const path = join(dir, entry);
    if (statSync(path).isDirectory()) {
      if (entry === "node_modules" || entry === "dist") continue;
      walk(path, out);
    } else if (/\.(ts|mts|js|mjs)$/.test(entry) && !/\.test\.(ts|js)$/.test(entry) && !/\.generated\.ts$/.test(entry)) {
      out.push(path);
    }
  }
  return out;
}

function selfTest() {
  const sample = `
    // env.NOT_READ_IN_A_COMMENT
    /* process.env.ALSO_A_COMMENT */
    const a = env.ALPHA_ONE?.trim(); const b = env?.BETA; const c = process.env.GAMMA;
    const d = env["DELTA"]; const e = required("EPSILON"); const f = intFromEnv(env, "ZETA_N", 1);
    const g = nodeInt("ETA", 8787); const h = usd("THETA_USD", 1); const i = usdMicros("IOTA_USD", 1);
    if (settings.KAPPA === "1") {} if (envRecord(env).LAMBDA === "1") {}
    const url = "https://example.com//x"; const cfg = { credential_env: "MU_TOKEN", baseUrlEnv: "NU_URL" };
    export const XI_CREDENTIAL_ENV = "XI_KEY";
    const notEnv = config.OMICRON; const utc = Date.UTC(2026, 1, 1);
    export const ENV_NAMES = { secrets: ["PI_SECRET"], vars: ["RHO"], bindings: ["SIGMA"] } as const;
  `;
  const got = [...namesIn(sample)].sort();
  const want = ["ALPHA_ONE", "BETA", "DELTA", "EPSILON", "ETA", "GAMMA", "IOTA_USD", "KAPPA", "LAMBDA", "MU_TOKEN", "NU_URL", "PI_SECRET", "RHO", "SIGMA", "THETA_USD", "XI_KEY", "ZETA_N"];
  const docs = documentedIn("| `ALPHA_ONE` | x |\nBETA without backticks\n`GAMMA`");
  const failures = [];
  if (JSON.stringify(got) !== JSON.stringify(want)) failures.push(`extractor: got  ${got.join(",")}\n           want ${want.join(",")}`);
  const rust = [...rustVarNamesIn(`
    /// The callback.
    pub const REDIRECT_URI_VAR: &str = "TAU_REDIRECT";
    // pub const COMMENTED_VAR: &str = "NOT_READ";
    pub const UPSILON_VAR: &'static str = "UPSILON_ID";
    const PHI: &str = "NOT_A_VAR_CONST";
  `)].sort();
  if (JSON.stringify(rust) !== JSON.stringify(["TAU_REDIRECT", "UPSILON_ID"])) failures.push(`rust extractor: got ${rust.join(",")}`);
  if (!docs.has("ALPHA_ONE") || docs.has("BETA") || !docs.has("GAMMA")) failures.push("documentedIn: the backtick rule is broken");
  if (failures.length) {
    console.error(`check-env-docs self-test FAILED\n${failures.join("\n")}`);
    process.exit(1);
  }
  console.log("check-env-docs self-test ok");
}

function main() {
  const args = new Set(process.argv.slice(2));
  if (args.has("--self-test")) return selfTest();
  const readAt = new Map();
  for (const dir of SOURCE_DIRS) {
    for (const file of walk(join(ROOT, dir))) {
      for (const name of namesIn(readFileSync(file, "utf8"))) {
        if (!readAt.has(name)) readAt.set(name, new Set());
        readAt.get(name).add(relative(ROOT, file));
      }
    }
  }
  for (const file of RUST_SOURCES) {
    for (const name of rustVarNamesIn(readFileSync(join(ROOT, file), "utf8"))) {
      if (!readAt.has(name)) readAt.set(name, new Set());
      readAt.get(name).add(file);
    }
  }
  if (args.has("--list")) {
    for (const name of [...readAt.keys()].sort()) console.log(`${name}\t${[...readAt.get(name)].sort().join(", ")}`);
    console.log(`${readAt.size} names`);
    return;
  }
  let markdown;
  try {
    markdown = readFileSync(join(ROOT, RUNBOOK), "utf8");
  } catch {
    console.error(`check-env-docs: ${RUNBOOK} is missing`);
    process.exit(1);
  }
  const documented = documentedIn(markdown);
  const missing = [...readAt.keys()].filter((n) => !documented.has(n)).sort();
  if (missing.length) {
    console.error(`check-env-docs: ${missing.length} environment variable(s) read by the code are not in ${RUNBOOK}.`);
    for (const n of missing) console.error(`  ${n}  (read in ${[...readAt.get(n)].sort().join(", ")})`);
    console.error("Add each one, in backticks, to the variable tables of the runbook: which component reads it, secret or var, required or optional.");
    process.exit(1);
  }
  console.log(`check-env-docs: all ${readAt.size} environment variables read by apps/api, apps/router and the sidecar's connect flow are documented in ${RUNBOOK}.`);
}

main();
