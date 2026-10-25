#!/usr/bin/env node
// Load test for the agent gateway (apps/api, plan E37). Manual, never in CI.
//
// Scenarios (each a closed loop of --concurrency requests for --duration seconds):
//   auth     POST /mcp with a key that does not exist          → 401 (key lookup + refusal)
//   pay402   POST /mcp memory.ingest with a capped key          → 402 (classify, quote, cap check)
//   read     POST /mcp tools/call memory.search (free)         → 200 (classify, reserve 0, proxy to core)
//   discover GET /.well-known/pricing.json                      → 200 (a public document)
//
// Targets:
//   --self-host           start apps/api/scripts/local-gateway.ts on 127.0.0.1 (in-memory store,
//                         fake core answering after --core-ms, tiny caps) — nothing external.
//   --url <base> --keys-file <file>
//                         an existing gateway; one agent key per line, capped so memory.ingest is
//                         402 there too (else pay402 reports what it got). Staging only.
//
// Output: one JSON object per scenario (statuses, p50/p95/p99 ms, rps) and the machine line.
// Bodies, keys and responses are never printed.
//
//   node bench/load/gateway.mjs --self-host
//   node bench/load/gateway.mjs --self-host --concurrency 32 --duration 20 --scenarios read,pay402
import { spawn } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { args as parseArgs, machine, run, waitForLine } from "./lib.mjs";

const a = parseArgs();
const concurrency = Number(a.concurrency ?? 16);
const durationSec = Number(a.duration ?? 10);
const scenarios = String(a.scenarios ?? "auth,pay402,read,discover").split(",");
const repo = join(dirname(fileURLToPath(import.meta.url)), "..", "..");

let base = a.url;
let keys = [];
let child;
let tmp;
if (a["self-host"]) {
  tmp = await mkdtemp(join(tmpdir(), "shogun-load-"));
  const keysFile = join(tmp, "keys.txt");
  const apiDir = join(repo, "apps", "api");
  child = spawn(join(apiDir, "node_modules", ".bin", "tsx"), ["scripts/local-gateway.ts", "--keys-out", keysFile, "--agents", "4", "--core-ms", String(a["core-ms"] ?? 5)], { cwd: apiDir, stdio: ["ignore", "pipe", "pipe"] });
  const m = await waitForLine(child, /LISTENING (\d+)/);
  base = `http://127.0.0.1:${m[1]}`;
  keys = (await readFile(keysFile, "utf8")).split("\n").filter(Boolean);
} else {
  if (!base || !a["keys-file"]) {
    console.error("pass --self-host, or --url <base> --keys-file <file>");
    process.exit(2);
  }
  keys = (await readFile(a["keys-file"], "utf8")).split("\n").filter(Boolean);
}

const rpc = (method, params) => JSON.stringify({ jsonrpc: "2.0", id: 1, method, params });
const mcp = (key, headers, body) => ({
  url: `${base}/mcp`,
  init: { method: "POST", headers: { authorization: `Bearer ${key}`, "content-type": "application/json", accept: "application/json", ...headers }, body },
});

const defs = {
  auth: { expect: [401], request: () => mcp("sa_live_doesnotexist0000000000000000", { "mcp-method": "tools/list" }, rpc("tools/list", {})) },
  pay402: {
    expect: [402],
    request: (i) => mcp(keys[i % keys.length], { "mcp-method": "tools/call", "mcp-name": "memory.ingest" }, rpc("tools/call", { name: "memory.ingest", arguments: { idempotency_key: `load-${i}` } })),
  },
  read: {
    expect: [200],
    request: (i) => mcp(keys[i % keys.length], { "mcp-method": "tools/call", "mcp-name": "memory.search" }, rpc("tools/call", { name: "memory.search", arguments: { query: "load test" } })),
  },
  discover: { expect: [200], request: () => ({ url: `${base}/.well-known/pricing.json` }) },
};

const results = [];
try {
  for (const name of scenarios) {
    const def = defs[name];
    if (!def) throw new Error(`unknown scenario ${name}`);
    results.push(await run({ name: `gateway.${name}`, concurrency, durationSec, ...def }));
    console.log(JSON.stringify(results[results.length - 1]));
  }
  console.log(JSON.stringify({ machine: await machine(), target: a["self-host"] ? "self-host (in-memory store, fake core)" : "url" }));
} finally {
  child?.kill("SIGTERM");
  if (tmp) await rm(tmp, { recursive: true, force: true });
}
const bad = results.filter((r) => r.unexpected_status > 0 || r.network_errors > 0);
process.exit(bad.length > 0 ? 1 : 0);
