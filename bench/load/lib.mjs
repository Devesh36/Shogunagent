// A closed-loop HTTP load driver with no dependencies (Node >= 22: global fetch over undici).
//
// `run(scenario)` keeps `concurrency` requests in flight for `durationSec` (after `warmupSec`),
// records each request's wall latency and status, and returns counts by status plus p50/p95/p99
// and throughput. Nothing about a request or response body is kept or printed: the response is
// drained and discarded so connections are reused.

/** The q-quantile (0..1) of a sorted array of numbers, nearest-rank. */
export function quantile(sorted, q) {
  if (sorted.length === 0) return null;
  const i = Math.min(sorted.length - 1, Math.max(0, Math.ceil(q * sorted.length) - 1));
  return sorted[i];
}

const round = (n) => (n === null ? null : Math.round(n * 100) / 100);

/**
 * @param {{ name: string, concurrency: number, durationSec: number, warmupSec?: number,
 *           request: (i: number) => { url: string, init?: RequestInit },
 *           expect?: number[] }} s
 */
export async function run(s) {
  const latencies = [];
  const statuses = {};
  let errors = 0;
  let i = 0;
  const warmupEnd = performance.now() + (s.warmupSec ?? 1) * 1000;
  const end = warmupEnd + s.durationSec * 1000;
  async function worker() {
    while (performance.now() < end) {
      const n = i++;
      const { url, init } = s.request(n);
      const t0 = performance.now();
      let status = 0;
      try {
        const res = await fetch(url, init);
        status = res.status;
        await res.arrayBuffer();
      } catch {
        status = 0;
      }
      const t1 = performance.now();
      if (t0 < warmupEnd) continue;
      if (status === 0) errors += 1;
      statuses[status] = (statuses[status] ?? 0) + 1;
      latencies.push(t1 - t0);
    }
  }
  const started = performance.now();
  await Promise.all(Array.from({ length: s.concurrency }, worker));
  const measuredSec = Math.max(0.001, (performance.now() - Math.max(started, warmupEnd)) / 1000);
  latencies.sort((a, b) => a - b);
  const unexpected = s.expect ? Object.entries(statuses).filter(([k]) => !s.expect.includes(Number(k))).reduce((n, [, v]) => n + v, 0) : 0;
  return {
    scenario: s.name,
    concurrency: s.concurrency,
    duration_s: s.durationSec,
    requests: latencies.length,
    rps: round(latencies.length / measuredSec),
    statuses,
    network_errors: errors,
    unexpected_status: unexpected,
    p50_ms: round(quantile(latencies, 0.5)),
    p95_ms: round(quantile(latencies, 0.95)),
    p99_ms: round(quantile(latencies, 0.99)),
    max_ms: round(latencies[latencies.length - 1] ?? null),
  };
}

/** `--name value` / `--flag` parsing into an object (strings; flags are `true`). */
export function args(argv = process.argv.slice(2)) {
  const out = {};
  for (let k = 0; k < argv.length; k++) {
    const a = argv[k];
    if (!a.startsWith("--")) continue;
    const name = a.slice(2);
    const next = argv[k + 1];
    if (next === undefined || next.startsWith("--")) out[name] = true;
    else {
      out[name] = next;
      k++;
    }
  }
  return out;
}

/** Wait for a child process to print a line matching `re`; resolves with the match. */
export function waitForLine(child, re, timeoutMs = 120_000) {
  return new Promise((resolve, reject) => {
    let buf = "";
    const timer = setTimeout(() => reject(new Error(`timed out waiting for ${re}`)), timeoutMs);
    const onData = (chunk) => {
      buf += chunk.toString();
      const m = re.exec(buf);
      if (m) {
        clearTimeout(timer);
        resolve(m);
      }
      if (buf.length > 65_536) buf = buf.slice(-4096);
    };
    child.stdout?.on("data", onData);
    child.stderr?.on("data", onData);
    child.once("exit", (code) => {
      clearTimeout(timer);
      reject(new Error(`process exited (${code}) before it was ready`));
    });
  });
}

/** One machine-description line for the report (the hardware caveat in docs/slo.md). */
export async function machine() {
  const os = await import("node:os");
  const cpus = os.cpus();
  return { node: process.version, platform: `${os.platform()}-${os.arch()}`, cpus: cpus.length, cpu_model: cpus[0]?.model ?? "unknown", mem_gb: Math.round(os.totalmem() / 2 ** 30) };
}
