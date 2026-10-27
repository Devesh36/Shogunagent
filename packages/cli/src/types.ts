/**
 * The seams every command goes through, so tests run with an in-memory filesystem, a fake
 * process spawner and a fake fetch — no network, no real files, no real processes.
 */

export interface Fs {
  exists(path: string): Promise<boolean>;
  isDirectory(path: string): Promise<boolean>;
  readFile(path: string): Promise<string>;
  readBytes(path: string): Promise<Uint8Array>;
  /** Write atomically (temp file + rename) with the given mode; creates parent directories. */
  writeFile(path: string, text: string | Uint8Array, mode?: number): Promise<void>;
  mkdirp(path: string): Promise<void>;
  rename(from: string, to: string): Promise<void>;
  chmod(path: string, mode: number): Promise<void>;
  listDir(path: string): Promise<string[]>;
  remove(path: string): Promise<void>;
}

export interface SpawnedProcess {
  pid: number | undefined;
  stdin: NodeJS.WritableStream | null;
  stdout: NodeJS.ReadableStream | null;
  stderr: NodeJS.ReadableStream | null;
  /** Resolves with the exit code when the process ends. */
  exited: Promise<number | null>;
  kill(signal?: NodeJS.Signals): void;
}

export interface SpawnOptions {
  env: Record<string, string>;
  cwd?: string;
  /** `inherit` passes the CLI's own stdio through (the sidecar); `pipe` captures it. */
  stdio: "inherit" | "pipe";
}

export type Spawn = (command: string, args: string[], options: SpawnOptions) => SpawnedProcess;

export type Fetch = (url: string, init?: { headers?: Record<string, string> }) => Promise<{
  ok: boolean;
  status: number;
  json(): Promise<unknown>;
  text(): Promise<string>;
  arrayBuffer(): Promise<ArrayBuffer>;
}>;

export interface Platform {
  /** Running Node.js version; defaults to process.versions.node when omitted. */
  nodeVersion?: string;
  /** `process.platform`. */
  os: "linux" | "darwin" | "win32" | string;
  /** `process.arch`. */
  arch: "x64" | "arm64" | string;
  homedir: string;
  env: Record<string, string | undefined>;
  /** Whether stdin and stdout are a terminal (an interactive yes is only asked then). */
  interactive: boolean;
}

export interface Io {
  out(line: string): void;
  err(line: string): void;
  /** Ask a yes/no question; resolves `false` when not interactive. */
  confirm(question: string): Promise<boolean>;
}

export interface Deps {
  fs: Fs;
  spawn: Spawn;
  fetch: Fetch;
  platform: Platform;
  io: Io;
  now: () => Date;
  randomBytes: (n: number) => Uint8Array;
}
