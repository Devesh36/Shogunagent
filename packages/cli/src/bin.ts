/**
 * The real `Deps` (filesystem, child processes, fetch, the terminal) behind `bin/shogunagent.mjs`.
 * Nothing here is imported by the tests; they build `Deps` from fakes.
 */
import { spawn as nodeSpawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { chmod, mkdir, readdir, readFile, rename, rm, stat, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { createInterface } from "node:readline/promises";
import { fileURLToPath } from "node:url";

import { main as run } from "./cli.js";
import type { Deps, Fs, Spawn, SpawnedProcess } from "./types.js";

const fs: Fs = {
  exists: (p) => stat(p).then(() => true, () => false),
  isDirectory: (p) => stat(p).then((s) => s.isDirectory(), () => false),
  readFile: (p) => readFile(p, "utf8"),
  readBytes: (p) => readFile(p).then((b) => new Uint8Array(b)),
  async writeFile(p, data, mode = 0o644) {
    await mkdir(dirname(p), { recursive: true });
    const tmp = join(dirname(p), `.${randomBytes(6).toString("hex")}.tmp`);
    await writeFile(tmp, data, { mode });
    await rename(tmp, p);
    // rename keeps the temp file's mode; an existing file's mode is replaced, which is the point
    // for the 0600 secrets.
    await chmod(p, mode).catch(() => undefined);
  },
  mkdirp: (p) => mkdir(p, { recursive: true }).then(() => undefined),
  rename,
  chmod,
  listDir: (p) => readdir(p),
  remove: (p) => rm(p, { force: true }),
};

const spawn: Spawn = (command, args, options) => {
  const child = nodeSpawn(command, args, { env: options.env, ...(options.cwd ? { cwd: options.cwd } : {}), stdio: options.stdio === "inherit" ? "inherit" : ["pipe", "pipe", "pipe"] });
  const exited = new Promise<number | null>((resolve) => {
    child.once("exit", (code) => resolve(code));
    child.once("error", () => resolve(null));
  });
  const proc: SpawnedProcess = {
    pid: child.pid,
    stdin: child.stdin,
    stdout: child.stdout,
    stderr: child.stderr,
    exited,
    kill: (signal) => { child.kill(signal); },
  };
  return proc;
};

const interactive = Boolean(process.stdin.isTTY && process.stdout.isTTY);

const deps: Deps = {
  fs,
  spawn,
  fetch: (url, init) => fetch(url, init ? { headers: init.headers ?? {} } : undefined),
  platform: {
    nodeVersion: process.versions.node, os: process.platform, arch: process.arch, homedir: homedir(), env: process.env, interactive,
    localLauncher: { node: process.execPath, script: fileURLToPath(new URL("../bin/shogunagent.mjs", import.meta.url)) },
  },
  io: {
    out: (line) => console.log(line),
    err: (line) => console.error(line),
    async confirm(question) {
      if (!interactive) return false;
      const rl = createInterface({ input: process.stdin, output: process.stdout });
      try {
        const answer = (await rl.question(question)).trim().toLowerCase();
        return answer === "y" || answer === "yes";
      } finally {
        rl.close();
      }
    },
  },
  now: () => new Date(),
  randomBytes: (n) => new Uint8Array(randomBytes(n)),
};

export function main(argv: string[]): Promise<number> {
  // The sidecar inherits the terminal; forward the stop signals so Ctrl-C stops it, not just us.
  return run(argv, deps);
}
