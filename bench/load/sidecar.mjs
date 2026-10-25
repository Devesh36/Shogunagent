#!/usr/bin/env node
// Load test for the sidecar (the Rust Memory API server, `shogun-api --multi`, plan E37). Manual,
// never in CI.
//
// Scenarios (closed loop, --concurrency for --duration seconds), over the REST face:
//   ingest   POST /v1/memory/ingest — 5 events per call, a fresh idempotency_key each time → 200
//   recall   POST /v1/memory/recall — budget_tokens 800 over what ingest wrote          → 200
//   status   GET  /v1/status        — the liveness read, no store work                  → 200
//
// Targets:
//   --self-host --bin <path/to/shogun-api>
//                         start the sidecar on 127.0.0.1 with a throwaway principal root under the
//                         system temp dir, the `env` key provider and a random key and token. Build
//                         it with: cargo build -p shogun-core --features daemon-server --bin shogun-api
//                         (add --profile fast or --release for numbers worth recording).
//   --url <base> --token <bearer>
//                         an existing sidecar and one principal token. It WRITES events: never
//                         point it at a principal whose memory matters.
//
// The synthetic events are generated text ("load event <n> about project <k>"); nothing is read
// back beyond the status. Output: one JSON line per scenario and the machine line.
//
//   node bench/load/sidecar.mjs --self-host --bin target/fast/shogun-api
import { spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { args as parseArgs, machine, run, waitForLine } from "./lib.mjs";

const a = parseArgs();
const concurrency = Number(a.concurrency ?? 8);
const durationSec = Number(a.duration ?? 10);
const scenarios = String(a.scenarios ?? "ingest,recall,status").split(",");

let base = a.url;
let token = a.token;
let child;
let tmp;
if (a["self-host"]) {
  if (!a.bin) {
    console.error("--self-host needs --bin <path to the shogun-api binary>");
    process.exit(2);
  }
  tmp = await mkdtemp(join(tmpdir(), "shogun-sidecar-load-"));
  token = randomBytes(24).toString("hex");
  child = spawn(String(a.bin), ["--multi", join(tmp, "principals"), "--key-provider", "env"], {
    stdio: ["ignore", "pipe", "pipe"],
    env: {
      PATH: process.env.PATH ?? "",
      HOME: tmp,
      SHOGUN_DB_KEY: randomBytes(32).toString("hex"),
      SHOGUN_PRINCIPAL_TOKENS: `load:${token}`,
      SHOGUN_API_PORT: String(a.port ?? 17464),
      SHOGUN_REGION: "local",
      SQLITE_TMPDIR: tmp,
    },
  });
  const m = await waitForLine(child, /listening on http:\/\/([0-9.:]+)/);
  base = `http://${m[1]}`;
} else if (!base || !token) {
  console.error("pass --self-host --bin <shogun-api>, or --url <base> --token <bearer>");
  process.exit(2);
}

const headers = { authorization: `Bearer ${token}`, "content-type": "application/json" };
const post = (path, body) => ({ url: `${base}${path}`, init: { method: "POST", headers, body: JSON.stringify(body) } });
const run_id = randomBytes(4).toString("hex");
const ingestBody = (i) => ({
  idempotency_key: `load-${run_id}-${i}`,
  actor: "load-test",
  source: "bench",
  thread_key: `t-${i % 50}`,
  events: Array.from({ length: 5 }, (_, j) => ({
    ts_ms: Date.now() - j * 1000,
    kind: "note",
    title: `load ${i}.${j}`,
    content: `load event ${i}.${j} about project ${(i + j) % 20}: follow up with the team on the forecast draft`,
  })),
});

const defs = {
  ingest: { expect: [200], request: (i) => post("/v1/memory/ingest", ingestBody(i)) },
  recall: { expect: [200], request: (i) => post("/v1/memory/recall", { query: `forecast project ${i % 20}`, budget_tokens: 800 }) },
  status: { expect: [200], request: () => ({ url: `${base}/v1/status` }) },
};

const results = [];
try {
  for (const name of scenarios) {
    const def = defs[name];
    if (!def) throw new Error(`unknown scenario ${name}`);
    results.push(await run({ name: `sidecar.${name}`, concurrency, durationSec, ...def }));
    console.log(JSON.stringify(results[results.length - 1]));
  }
  console.log(JSON.stringify({ machine: await machine(), target: a["self-host"] ? "self-host (env key provider, one principal)" : "url" }));
} finally {
  child?.kill("SIGTERM");
  if (tmp) await rm(tmp, { recursive: true, force: true });
}
const bad = results.filter((r) => r.unexpected_status > 0 || r.network_errors > 0);
process.exit(bad.length > 0 ? 1 : 0);
