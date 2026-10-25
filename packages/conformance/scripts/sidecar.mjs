// @ts-check
/**
 * A throwaway local sidecar for the conformance suite (E36a): a fresh encrypted store in a temp
 * directory, a random store key and client token, a running trial, the Memory API on, the remote
 * approval URL configured (so L3 answers carry one), and `shogun-api` on an ephemeral loopback
 * port. Nothing here contacts the network: the approval base URL is only minted into links, never
 * fetched, and `HOME` points at the temp directory so no user file is read.
 *
 * Binaries: `SHOGUN_API_BIN` and `SHOGUN_CONFORMANCE_STORE_BIN`, else
 * `$CARGO_TARGET_DIR/debug/{shogun-api,examples/conformance_store}` (CARGO_TARGET_DIR defaults to
 * `<repo>/target`). Build them with
 * `cargo build -p shogun-core --features daemon-server --bin shogun-api --example conformance_store`
 * (debug: the `SHOGUN_*_JSON` plan overrides exist only in debug builds).
 */
import { spawn, execFileSync } from "node:child_process";
import { generateKeyPairSync, randomBytes, sign } from "node:crypto";
import { existsSync } from "node:fs";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

export const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");

/** Where the two binaries are. */
export function locateBinaries(env = process.env) {
  const target = resolve(REPO_ROOT, env.CARGO_TARGET_DIR || "target");
  return {
    api: env.SHOGUN_API_BIN || join(target, "debug", "shogun-api"),
    store: env.SHOGUN_CONFORMANCE_STORE_BIN || join(target, "debug", "examples", "conformance_store"),
  };
}

/** Build both binaries with cargo (debug profile). */
export function buildBinaries() {
  execFileSync(
    "cargo",
    ["build", "--locked", "-p", "shogun-core", "--features", "daemon-server", "--bin", "shogun-api", "--example", "conformance_store"],
    { cwd: REPO_ROOT, stdio: "inherit" },
  );
}

/**
 * Start a sidecar. Returns its MCP URL, the client token, whether the approval URL is configured,
 * and `stop()`, which kills the process and deletes the temp directory.
 * @param {{ binaries?: { api: string; store: string } }} [options]
 */
export async function startSidecar(options = {}) {
  const binaries = options.binaries ?? locateBinaries();
  for (const [name, path] of Object.entries(binaries)) {
    if (!existsSync(path)) {
      throw new Error(`${name} binary not found at ${path}. Build it with: cargo build -p shogun-core --features daemon-server --bin shogun-api --example conformance_store (or pass --build)`);
    }
  }
  const dir = await mkdtemp(join(tmpdir(), "shogun-conformance-"));
  const dbKey = randomBytes(32).toString("hex");
  const token = `shogun_${randomBytes(24).toString("hex")}`;
  const db = join(dir, "memory.db");
  const approvalBase = "https://approve.conformance.invalid";
  execFileSync(binaries.store, [db], { env: { PATH: process.env.PATH ?? "", SHOGUN_DB_KEY: dbKey }, stdio: ["ignore", "ignore", "inherit"] });
  await writeFile(join(dir, "memory_api.json"), `${JSON.stringify({ enabled: true })}\n`, { mode: 0o600 });
  // A trial that started a minute ago: the plan gate answers like a running trial.
  const startedAt = Math.floor(Date.now() / 1000) - 60;
  await writeFile(join(dir, "onboarding.json"), `${JSON.stringify({ version: 1, state: { completed: true, step: "ready", trial_started_at: startedAt } })}\n`);
  // The licence API's answer as the desktop records it (#1031): a stamp alone reads as "not
  // started"; only a signed trial token makes it run. This run signs one with a keypair made for
  // it and hands the public half to the debug build through SHOGUN_LICENSE_PUBKEY (honoured in
  // debug builds only; a release binary keeps its embedded key).
  const licence = generateKeyPairSync("ed25519");
  const publicRaw = licence.publicKey.export({ format: "der", type: "spki" }).subarray(-32);
  const endsAt = startedAt + 7 * 86_400;
  const payload = { v: 1, device: "conformance", plan: "trial", status: "trialing", iat: startedAt, exp: startedAt + 24 * 3_600, period_end: endsAt };
  const body = Buffer.from(JSON.stringify(payload));
  const trialToken = `v1.${body.toString("base64url")}.${sign(null, body, licence.privateKey).toString("base64url")}`;
  await writeFile(join(dir, "trial_relay.json"), `${JSON.stringify({ version: 1, device_id: "conformance", token: trialToken, expires_at: payload.exp, checked_at: startedAt, trial_ends_at: endsAt })}\n`);
  const env = {
    PATH: process.env.PATH ?? "",
    HOME: dir,
    SHOGUN_DB_PATH: db,
    SHOGUN_DB_KEY: dbKey,
    SHOGUN_API_TOKEN: token,
    SHOGUN_API_PORT: "0",
    SHOGUN_MEMORY_API_SETTINGS: join(dir, "memory_api.json"),
    SHOGUN_ONBOARDING_JSON: join(dir, "onboarding.json"),
    SHOGUN_BILLING_JSON: join(dir, "billing.json"),
    SHOGUN_TRIAL_RELAY_JSON: join(dir, "trial_relay.json"),
    SHOGUN_ANALYTICS_JSON: join(dir, "analytics.json"),
    SHOGUN_APPROVAL_BASE_URL: approvalBase,
    SHOGUN_APPROVAL_SIGNING_KEY: randomBytes(32).toString("hex"),
    SHOGUN_LICENSE_PUBKEY: publicRaw.toString("base64"),
    SHOGUNAGENT_SERVER: "1",
  };
  const child = spawn(binaries.api, [], { cwd: dir, env, stdio: ["ignore", "pipe", "pipe"] });
  /** @type {string[]} */
  const stderr = [];
  child.stderr?.setEncoding("utf8");
  child.stderr?.on("data", (chunk) => stderr.push(String(chunk)));
  const stop = async () => {
    if (child.exitCode === null && child.signalCode === null) {
      const exited = new Promise((done) => child.once("exit", done));
      child.kill("SIGTERM");
      await Promise.race([exited, new Promise((done) => setTimeout(done, 3_000))]);
      if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
    }
    await rm(dir, { recursive: true, force: true });
  };
  try {
    const port = await new Promise((resolvePort, reject) => {
      const timer = setTimeout(() => reject(new Error(`shogun-api did not report a port within 30 s. stderr:\n${stderr.join("")}`)), 30_000);
      child.once("exit", (code) => {
        clearTimeout(timer);
        reject(new Error(`shogun-api exited (${code}) before listening. stderr:\n${stderr.join("")}`));
      });
      const lines = createInterface({ input: /** @type {import("node:stream").Readable} */ (child.stdout) });
      lines.on("line", (line) => {
        const match = /listening on http:\/\/127\.0\.0\.1:(\d+)/.exec(line);
        if (match) {
          clearTimeout(timer);
          resolvePort(Number(match[1]));
        }
      });
    });
    return { url: `http://127.0.0.1:${port}/mcp`, token, approvalBase, stderr, stop };
  } catch (e) {
    await stop();
    throw e;
  }
}
