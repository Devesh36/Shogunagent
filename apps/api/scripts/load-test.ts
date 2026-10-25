/**
 * Load test for the shared services (issue #29 FR-SVC-07). Manual, never in CI — see
 * docs/ai-gateway-runbook.md §9-7 for the procedure and the pass criteria.
 *
 * Three scenarios from the spec (§4-7), with scaled-down defaults so a laptop run is cheap:
 *
 *   1. submit  — N devices each POST /v1/batch at once (10 items × ~1 KB).    spec: 3,000
 *                --submit-over-sec S spreads them evenly over S seconds instead (what the device
 *                jitter of FR-SVC-01 does to the nightly herd).
 *   2. poll    — GET /v1/batch/{id} at a fixed open-loop rate for a while.     spec: 30,000/h
 *   3. verify  — POST /api/license/verify at a fixed rate.                     spec: 1,000 rps × 60 s
 *
 * Targets:
 *   --self-host            start a local relay process (scripts/local-relay.ts) on 127.0.0.1 with a FAKE upstream (never
 *                          calls Anthropic) and its own throwaway licence key pair. Store:
 *                          --store memory (default; relay-only numbers) | postgres (the production
 *                          store; needs --database-url with sql/shared-store.sql applied).
 *   --url <relay base>     an existing relay; --tokens-file has one licence token per line
 *                          (one per simulated device — the rate limit is per licence+device).
 *                          Point it at a staging relay whose upstream is NOT the production key.
 *   --verify-url <site>    enables scenario 3 against <site>/api/license/verify with
 *                          --verify-keys-file lines "<license_key> <device_id>". Staging only.
 *
 * Submissions follow the device contract (docs/batch-relay-design.md §4.5): a 429/503 WITH
 * `Retry-After` is retried with the same body and `client_request_id` after that many seconds
 * (+ up to --retry-jitter-ms), at most --retries times; a 429 without it (cap reached) is final.
 * The report shows both the first attempt and the final outcome.
 *
 * Output: one JSON object per scenario (counts by status, p50/p95/p99 ms) and a pass/fail line
 * per criterion. Nothing about request bodies or tokens is printed.
 *
 *   pnpm --filter @shogun-ai/api load-test -- --self-host
 *   pnpm --filter @shogun-ai/api load-test -- --self-host --devices 3000 --poll-per-hour 30000 --poll-seconds 120
 */
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { mkdtemp, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

const { values: args } = parseArgs({
  options: {
    "self-host": { type: "boolean", default: false },
    store: { type: "string", default: "memory" },
    "database-url": { type: "string" },
    url: { type: "string" },
    "tokens-file": { type: "string" },
    devices: { type: "string", default: "300" },
    items: { type: "string", default: "10" },
    "item-bytes": { type: "string", default: "1024" },
    "poll-per-hour": { type: "string", default: "3000" },
    "poll-seconds": { type: "string", default: "30" },
    "verify-url": { type: "string" },
    "verify-keys-file": { type: "string" },
    "verify-rps": { type: "string", default: "100" },
    "verify-seconds": { type: "string", default: "10" },
    "upstream-ms": { type: "string", default: "50" },
    retries: { type: "string", default: "3" },
    "submit-over-sec": { type: "string", default: "0" },
    "retry-jitter-ms": { type: "string", default: "1000" },
  },
  allowPositionals: true,
});

const n = (k: keyof typeof args): number => Number(args[k]);

// ---------------------------------------------------------------------------------------------
// Self-hosted relay: a separate process (scripts/local-relay.ts), so the load generator and the
// relay do not share one event loop and the numbers describe the relay, not this script.

async function selfHost(devices: number): Promise<{ base: string; tokens: string[]; stop: () => Promise<void> }> {
  const dir = await mkdtemp(join(tmpdir(), "relay-load-"));
  const tokensFile = join(dir, "tokens.txt");
  const childArgs = ["--import", "tsx", fileURLToPath(new URL("./local-relay.ts", import.meta.url)),
    "--devices", String(devices), "--tokens-out", tokensFile, "--store", String(args.store),
    "--upstream-ms", String(args["upstream-ms"]),
    ...(args["database-url"] ? ["--database-url", args["database-url"]] : [])];
  const child = spawn(process.execPath, childArgs, { stdio: ["ignore", "pipe", "inherit"] });
  const port = await new Promise<number>((resolve, reject) => {
    let out = "";
    child.stdout.on("data", (d: Buffer) => {
      out += d.toString();
      const m = /LISTENING (\d+)/.exec(out);
      if (m) resolve(Number(m[1]));
    });
    child.once("exit", (code) => reject(new Error(`local relay exited (${String(code)})`)));
  });
  const tokens = (await readFile(tokensFile, "utf8")).split("\n").filter(Boolean);
  return {
    base: `http://127.0.0.1:${port}`,
    tokens,
    stop: async () => {
      child.kill("SIGTERM");
      await new Promise((r) => child.once("exit", r));
    },
  };
}

// ---------------------------------------------------------------------------------------------
// Measurement.

interface Sample { status: number; ms: number }

function summarize(name: string, samples: Sample[], wallMs: number): Record<string, unknown> {
  const sorted = samples.map((s) => s.ms).sort((a, b) => a - b);
  const q = (p: number): number | null => sorted.length ? Math.round(sorted[Math.min(sorted.length - 1, Math.ceil(p * sorted.length) - 1)]! * 10) / 10 : null;
  const byStatus: Record<string, number> = {};
  for (const s of samples) byStatus[s.status] = (byStatus[s.status] ?? 0) + 1;
  const five = samples.filter((s) => s.status >= 500 || s.status === 0).length;
  return {
    scenario: name,
    requests: samples.length,
    wall_s: Math.round(wallMs / 100) / 10,
    achieved_rps: Math.round((samples.length / (wallMs / 1000)) * 10) / 10,
    by_status: byStatus,
    rate_5xx_or_network: samples.length ? Math.round((five / samples.length) * 1e4) / 1e4 : 0,
    p50_ms: q(0.5),
    p95_ms: q(0.95),
    p99_ms: q(0.99),
    max_ms: sorted.length ? Math.round(sorted[sorted.length - 1]! * 10) / 10 : null,
  };
}

async function timed(url: string, init: RequestInit): Promise<{ sample: Sample; body?: unknown; retryAfter?: number }> {
  const t = performance.now();
  try {
    const res = await fetch(url, init);
    const text = await res.text();
    const sample = { status: res.status, ms: performance.now() - t };
    const header = res.headers.get("retry-after");
    const retryAfter = header === null ? undefined : Number(header);
    let body: unknown;
    try { body = JSON.parse(text); } catch { body = undefined; }
    return { sample, body, ...(retryAfter !== undefined && Number.isFinite(retryAfter) ? { retryAfter } : {}) };
  } catch {
    return { sample: { status: 0, ms: performance.now() - t } };
  }
}

/** One device's submission under the §4.5 contract: retry only what says it is transient. */
async function submitLikeADevice(url: string, init: RequestInit): Promise<{ first: Sample; final: Sample; totalMs: number; attempts: number; body?: unknown }> {
  const start = performance.now();
  let r = await timed(url, init);
  const first = r.sample;
  let attempts = 1;
  while ((r.sample.status === 429 || r.sample.status === 503) && r.retryAfter !== undefined && attempts <= n("retries")) {
    await new Promise((res) => setTimeout(res, r.retryAfter! * 1000 + Math.random() * n("retry-jitter-ms")));
    r = await timed(url, init);
    attempts += 1;
  }
  return { first, final: r.sample, totalMs: performance.now() - start, attempts, body: r.body };
}

/** Fire `total` requests at `rps`, open-loop: the schedule does not wait for answers, which is
 * what a herd of independent devices does. */
async function openLoop(rps: number, seconds: number, fire: (i: number) => Promise<Sample>): Promise<{ samples: Sample[]; wallMs: number }> {
  const total = Math.max(1, Math.round(rps * seconds));
  const start = performance.now();
  const pending: Array<Promise<Sample>> = [];
  for (let i = 0; i < total; i++) {
    const due = start + (i / rps) * 1000;
    const delay = due - performance.now();
    if (delay > 1) await new Promise((r) => setTimeout(r, delay));
    pending.push(fire(i));
  }
  const samples = await Promise.all(pending);
  return { samples, wallMs: performance.now() - start };
}

function batchBody(items: number, itemBytes: number): string {
  const chunk = "x".repeat(itemBytes);
  return JSON.stringify({
    purpose: "consolidation",
    model_class: "classify",
    client_request_id: randomUUID(),
    items: Array.from({ length: items }, (_, i) => ({ custom_id: String(i), chunk })),
  });
}

// ---------------------------------------------------------------------------------------------

async function main(): Promise<void> {
  let base: string;
  let tokens: string[];
  let stop = async (): Promise<void> => undefined;
  if (args["self-host"]) {
    ({ base, tokens, stop } = await selfHost(n("devices")));
  } else {
    if (!args.url || !args["tokens-file"]) throw new Error("use --self-host, or --url with --tokens-file");
    base = args.url.replace(/\/$/, "");
    tokens = (await readFile(args["tokens-file"], "utf8")).split("\n").map((l) => l.trim()).filter(Boolean).slice(0, n("devices"));
  }
  const results: Array<Record<string, unknown>> = [];
  const verdicts: string[] = [];

  // 1. submit herd.
  const t1 = performance.now();
  const spreadMs = n("submit-over-sec") * 1000;
  const submits = await Promise.all(tokens.map(async (token, i) => {
    if (spreadMs > 0) await new Promise((r) => setTimeout(r, (i / tokens.length) * spreadMs));
    return submitLikeADevice(`${base}/v1/batch`, {
    method: "POST",
    headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      body: batchBody(n("items"), n("item-bytes")),
    }).then((r) => ({ ...r, token }));
  }));
  const wall1 = performance.now() - t1;
  const label = `submit x${tokens.length} (${n("items")} items x ${n("item-bytes")} B)${spreadMs > 0 ? ` spread over ${n("submit-over-sec")} s` : " at once"}`;
  const submitFirst = summarize(`${label}, first attempt`, submits.map((s) => s.first), wall1);
  const submitFinal = summarize(`${label}, final after <=${n("retries")} Retry-After retries (latency = first try to final answer)`,
    submits.map((s) => ({ status: s.final.status, ms: s.totalMs })), wall1);
  results.push(submitFirst, submitFinal);
  const acceptedFinal = submits.filter((s) => s.final.status === 202);
  const acceptedP95 = summarize("accepted", acceptedFinal.map((s) => s.first.status === 202 ? s.first : s.final), wall1).p95_ms;
  verdicts.push(`submit p95 to acceptance (per accepted attempt) ${String(acceptedP95)} ms <= 1000 ms: ${Number(acceptedP95) <= 1000 ? "PASS" : "FAIL"}`);
  verdicts.push(`submit accepted ${acceptedFinal.length}/${submits.length}; the rest fell back to the local lane (UX-neutral per spec §3)`);

  // 2. polling at a fixed hourly rate, round-robin over accepted batches (each by its owner).
  const accepted = submits.flatMap((s) => {
    const id = (s.body as { batch_id?: unknown } | undefined)?.batch_id;
    return s.final.status === 202 && typeof id === "string" ? [{ id, token: s.token }] : [];
  });
  if (accepted.length > 0) {
    const rps = n("poll-per-hour") / 3600;
    const run = await openLoop(rps, n("poll-seconds"), async (i) => {
      const a = accepted[i % accepted.length]!;
      return (await timed(`${base}/v1/batch/${encodeURIComponent(a.id)}`, { headers: { authorization: `Bearer ${a.token}` } })).sample;
    });
    const poll = summarize(`poll ${n("poll-per-hour")}/h for ${n("poll-seconds")} s`, run.samples, run.wallMs);
    results.push(poll);
    verdicts.push(`poll 5xx rate ${String(poll.rate_5xx_or_network)} < 0.01: ${Number(poll.rate_5xx_or_network) < 0.01 ? "PASS" : "FAIL"}`);
  } else {
    verdicts.push("poll: skipped (no submission accepted)");
  }

  // 3. license/verify, only against an explicitly named (staging) site.
  if (args["verify-url"] && args["verify-keys-file"]) {
    const pairs = (await readFile(args["verify-keys-file"], "utf8")).split("\n").map((l) => l.trim().split(/\s+/)).filter((p) => p.length === 2);
    if (pairs.length === 0) throw new Error("--verify-keys-file has no '<license_key> <device_id>' lines");
    const url = `${args["verify-url"].replace(/\/$/, "")}/api/license/verify`;
    const run = await openLoop(n("verify-rps"), n("verify-seconds"), async (i) => {
      const [key, device] = pairs[i % pairs.length]!;
      return (await timed(url, { method: "POST", headers: { "content-type": "application/json" },
        body: JSON.stringify({ license_key: key, device_id: device, app_version: "load-test" }) })).sample;
    });
    const verify = summarize(`verify ${n("verify-rps")} rps x ${n("verify-seconds")} s`, run.samples, run.wallMs);
    results.push(verify);
    verdicts.push(`verify p95 ${String(verify.p95_ms)} ms <= 300 ms: ${Number(verify.p95_ms) <= 300 ? "PASS" : "FAIL"}`);
  } else {
    verdicts.push("verify: skipped (no --verify-url / --verify-keys-file)");
  }

  await stop();
  for (const r of results) console.log(JSON.stringify(r));
  for (const v of verdicts) console.log(v);
}

main().catch((e: unknown) => {
  console.error(e instanceof Error ? e.message : "load test failed");
  process.exit(1);
});
