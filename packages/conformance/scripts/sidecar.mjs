// @ts-check
/**
 * A throwaway local sidecar for the conformance suite (E36a): fresh encrypted principal stores in a temp
 * directory, a random store key and client token, the remote
 * approval URL configured (so L3 answers carry one), and `shogun-api --multi` on an ephemeral loopback
 * port. Nothing here contacts the network: the approval base URL is only minted into links, never
 * fetched, and `HOME` points at the temp directory so no user file is read.
 *
 * Binary: `SHOGUN_API_BIN`, else `$CARGO_TARGET_DIR/debug/shogun-api`. Build it with
 * `cargo build -p shogun-core --features daemon-server --bin shogun-api`. Multi-principal mode
 * exercises the server deployment without the desktop hardware-bound trial gate.
 */
import { spawn, execFileSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import { existsSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

export const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");

/** Where the server binary is. */
export function locateBinaries(env = process.env) {
  const target = resolve(REPO_ROOT, env.CARGO_TARGET_DIR || "target");
  return {
    api: env.SHOGUN_API_BIN || join(target, "debug", "shogun-api"),
  };
}

/** Build the server with cargo. */
export function buildBinaries() {
  execFileSync(
    "cargo",
    ["build", "--locked", "-p", "shogun-core", "--features", "daemon-server", "--bin", "shogun-api"],
    { cwd: REPO_ROOT, stdio: "inherit" },
  );
}

/**
 * Start a sidecar. Returns its MCP URL, the client token, whether the approval URL is configured,
 * and `stop()`, which kills the process and deletes the temp directory.
 * @param {{ binaries?: { api: string } }} [options]
 */
export async function startSidecar(options = {}) {
  const binaries = options.binaries ?? locateBinaries();
  for (const [name, path] of Object.entries(binaries)) {
    if (!existsSync(path)) {
      throw new Error(`${name} binary not found at ${path}. Build it with: cargo build -p shogun-core --features daemon-server --bin shogun-api (or pass --build)`);
    }
  }
  const dir = await mkdtemp(join(tmpdir(), "shogun-conformance-"));
  const dbKey = randomBytes(32).toString("hex");
  const token = `shogun_${randomBytes(24).toString("hex")}`;
  const approvalBase = "https://approve.conformance.invalid";
  const env = {
    PATH: process.env.PATH ?? "",
    HOME: dir,
    SHOGUN_DB_KEY: dbKey,
    SHOGUN_PRINCIPAL_TOKENS: `conformance:${token}:conformance`,
    SHOGUN_API_PORT: "0",
    SHOGUN_APPROVAL_BASE_URL: approvalBase,
    SHOGUN_APPROVAL_SIGNING_KEY: randomBytes(32).toString("hex"),
    SHOGUNAGENT_SERVER: "1",
  };
  const child = spawn(binaries.api, ["--multi", join(dir, "principals")], { cwd: dir, env, stdio: ["ignore", "pipe", "pipe"] });
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
