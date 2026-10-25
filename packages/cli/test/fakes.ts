/** Fakes behind `Deps`: an in-memory filesystem, a recording spawn, a fetch that answers from a
 * table (and throws for anything else, so no test can reach the network), a scripted terminal. */
import { PassThrough } from "node:stream";
import { dirname } from "node:path";

import type { Deps, Fetch, Fs, Platform, Spawn, SpawnedProcess } from "../src/types.js";

export class MemFs implements Fs {
  files = new Map<string, { data: Uint8Array; mode: number }>();
  dirs = new Set<string>();
  writes: string[] = [];

  seed(path: string, text: string, mode = 0o644): void {
    this.files.set(path, { data: Buffer.from(text), mode });
    this.mkdirSync(dirname(path));
  }
  mkdirSync(path: string): void {
    let p = path;
    while (p && p !== "/" && !this.dirs.has(p)) {
      this.dirs.add(p);
      p = dirname(p);
    }
  }
  text(path: string): string | undefined {
    const f = this.files.get(path);
    return f ? Buffer.from(f.data).toString("utf8") : undefined;
  }
  mode(path: string): number | undefined {
    return this.files.get(path)?.mode;
  }
  exists(path: string): Promise<boolean> {
    return Promise.resolve(this.files.has(path) || this.dirs.has(path));
  }
  isDirectory(path: string): Promise<boolean> {
    return Promise.resolve(this.dirs.has(path));
  }
  readFile(path: string): Promise<string> {
    const t = this.text(path);
    if (t === undefined) return Promise.reject(new Error(`ENOENT ${path}`));
    return Promise.resolve(t);
  }
  readBytes(path: string): Promise<Uint8Array> {
    const f = this.files.get(path);
    if (!f) return Promise.reject(new Error(`ENOENT ${path}`));
    return Promise.resolve(f.data);
  }
  writeFile(path: string, data: string | Uint8Array, mode = 0o644): Promise<void> {
    this.mkdirSync(dirname(path));
    this.files.set(path, { data: typeof data === "string" ? Buffer.from(data) : data, mode });
    this.writes.push(path);
    return Promise.resolve();
  }
  mkdirp(path: string): Promise<void> {
    this.mkdirSync(path);
    return Promise.resolve();
  }
  rename(from: string, to: string): Promise<void> {
    const f = this.files.get(from);
    if (!f) return Promise.reject(new Error(`ENOENT ${from}`));
    this.files.delete(from);
    this.files.set(to, f);
    return Promise.resolve();
  }
  chmod(path: string, mode: number): Promise<void> {
    const f = this.files.get(path);
    if (f) f.mode = mode;
    return Promise.resolve();
  }
  listDir(path: string): Promise<string[]> {
    const prefix = path.endsWith("/") ? path : `${path}/`;
    const names = new Set<string>();
    for (const k of [...this.files.keys(), ...this.dirs]) if (k.startsWith(prefix)) names.add(k.slice(prefix.length).split("/")[0] ?? "");
    return Promise.resolve([...names].filter(Boolean).sort());
  }
  remove(path: string): Promise<void> {
    this.files.delete(path);
    return Promise.resolve();
  }
}

export interface SpawnCall {
  command: string;
  args: string[];
  env: Record<string, string>;
  cwd?: string;
  stdio: "inherit" | "pipe";
}

export interface FakeChild extends SpawnedProcess {
  stdinStream: PassThrough;
  stdoutStream: PassThrough;
  stderrStream: PassThrough;
  finish(code: number): void;
}

/** Records every spawn; `script` decides each child's exit code (default 0, immediately) and may
 * drive its streams. */
export function fakeSpawn(script: (call: SpawnCall, child: FakeChild) => void = (_c, child) => child.finish(0)): { spawn: Spawn; calls: SpawnCall[] } {
  const calls: SpawnCall[] = [];
  const spawn: Spawn = (command, args, options) => {
    const call: SpawnCall = { command, args, env: options.env, stdio: options.stdio, ...(options.cwd ? { cwd: options.cwd } : {}) };
    calls.push(call);
    const stdinStream = new PassThrough();
    const stdoutStream = new PassThrough();
    const stderrStream = new PassThrough();
    let resolveExit: (code: number | null) => void = () => undefined;
    const exited = new Promise<number | null>((resolve) => { resolveExit = resolve; });
    const child: FakeChild = {
      pid: 4242,
      stdin: stdinStream,
      stdout: stdoutStream,
      stderr: stderrStream,
      stdinStream, stdoutStream, stderrStream,
      exited,
      kill: () => resolveExit(null),
      finish: (code) => { stdoutStream.end(); resolveExit(code); },
    };
    queueMicrotask(() => script(call, child));
    return child;
  };
  return { spawn, calls };
}

export type FetchTable = Record<string, { status?: number; body: string | Uint8Array | Record<string, unknown> }>;

/** Answers from the table; any other URL throws (no network in tests). */
export function fakeFetch(table: FetchTable): { fetch: Fetch; requested: string[] } {
  const requested: string[] = [];
  const fetch: Fetch = (url) => {
    requested.push(url);
    const hit = table[url];
    if (!hit) throw new Error(`unexpected network request: ${url}`);
    const status = hit.status ?? 200;
    const bytes = typeof hit.body === "string" ? Buffer.from(hit.body) : hit.body instanceof Uint8Array ? hit.body : Buffer.from(JSON.stringify(hit.body));
    return Promise.resolve({
      ok: status >= 200 && status < 300,
      status,
      json: () => Promise.resolve(JSON.parse(Buffer.from(bytes).toString("utf8")) as unknown),
      text: () => Promise.resolve(Buffer.from(bytes).toString("utf8")),
      arrayBuffer: () => Promise.resolve(bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer),
    });
  };
  return { fetch, requested };
}

export const HOME = "/home/alice";

export function linuxPlatform(overrides: Partial<Platform> = {}): Platform {
  return { os: "linux", arch: "x64", homedir: HOME, env: { PATH: "/usr/bin", HOME }, interactive: false, ...overrides };
}

export interface Rig {
  deps: Deps;
  fs: MemFs;
  out: string[];
  err: string[];
  spawnCalls: SpawnCall[];
  requested: string[];
  answers: boolean[];
}

export function makeRig(options: { platform?: Partial<Platform>; fetch?: FetchTable; spawn?: Parameters<typeof fakeSpawn>[0]; answers?: boolean[]; fs?: MemFs } = {}): Rig {
  const fs = options.fs ?? new MemFs();
  const out: string[] = [];
  const err: string[] = [];
  const { spawn, calls } = fakeSpawn(options.spawn);
  const { fetch, requested } = fakeFetch(options.fetch ?? {});
  const answers = [...(options.answers ?? [])];
  let seed = 1;
  const deps: Deps = {
    fs, spawn, fetch,
    platform: linuxPlatform(options.platform),
    io: {
      out: (l) => out.push(l),
      err: (l) => err.push(l),
      confirm: () => Promise.resolve(answers.shift() ?? false),
    },
    now: () => new Date("2026-10-25T12:00:00Z"),
    // Deterministic "random" bytes so secrets are inspectable in assertions.
    randomBytes: (n) => new Uint8Array(Array.from({ length: n }, (_, i) => (i + seed++) % 256)),
  };
  return { deps, fs, out, err, spawnCalls: calls, requested, answers };
}
