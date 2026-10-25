/**
 * The local sidecar: `shogun-api` on loopback with a generated client token and the store under
 * the data directory, plus what `shogun-mcp` (the stdio face the agent apps spawn) needs to open
 * the same store.
 *
 * Secrets: the client token and the store key are generated once and kept in the data directory
 * with mode 0600 (`api.token`, `db.key`). There is no Keychain on a Linux server box, and the
 * repository's invariant 7 names the environment as the server's key source: both files are read
 * only to hand their value to the child process's environment and are never written into any
 * agent's config file or printed. `SHOGUN_API_TOKEN` / `SHOGUN_DB_KEY` in the CLI's own
 * environment take precedence and then nothing is written.
 */
import { join } from "node:path";

import { minimalEnv, type Located } from "./binaries.js";
import type { DataPaths } from "./paths.js";
import type { Deps, SpawnedProcess } from "./types.js";

export const DEFAULT_PORT = 7464;
export const TOKEN_ENV = "SHOGUN_API_TOKEN";
export const DB_KEY_ENV = "SHOGUN_DB_KEY";

const hex = (bytes: Uint8Array): string => Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");

async function secretFromEnvOrFile(deps: Deps, envName: string, path: string, generate: () => string): Promise<string> {
  const fromEnv = deps.platform.env[envName]?.trim();
  if (fromEnv) return fromEnv;
  if (await deps.fs.exists(path)) {
    const text = (await deps.fs.readFile(path)).trim();
    if (text) return text;
  }
  const value = generate();
  await deps.fs.writeFile(path, `${value}\n`, 0o600);
  return value;
}

export interface SidecarSecrets {
  token: string;
  dbKeyHex: string;
}

/** The token and the store key: from the environment, else the 0600 files, else generated. */
export async function sidecarSecrets(deps: Deps, paths: DataPaths): Promise<SidecarSecrets> {
  await deps.fs.mkdirp(paths.root);
  const token = await secretFromEnvOrFile(deps, TOKEN_ENV, paths.token, () => `shogun_${hex(deps.randomBytes(24))}`);
  const dbKeyHex = await secretFromEnvOrFile(deps, DB_KEY_ENV, paths.dbKey, () => hex(deps.randomBytes(32)));
  if (!/^[0-9a-fA-F]{64}$/.test(dbKeyHex)) throw new Error(`${DB_KEY_ENV} must be 64 hex characters (32 bytes)`);
  return { token, dbKeyHex };
}

/** The server refuses to start unless `memory_api.json` next to the store says `enabled: true`.
 * Written only when absent: an existing `false` is the user's choice and is left alone. */
export async function ensureMemoryApiEnabled(deps: Deps, paths: DataPaths): Promise<"written" | "kept"> {
  if (await deps.fs.exists(paths.memoryApiSettings)) return "kept";
  await deps.fs.writeFile(paths.memoryApiSettings, JSON.stringify({ enabled: true }, null, 2) + "\n", 0o600);
  return "written";
}

/** The environment both faces (`shogun-api`, `shogun-mcp`) read. */
export function sidecarEnv(deps: Deps, paths: DataPaths, secrets: SidecarSecrets, extra: Record<string, string> = {}): Record<string, string> {
  return {
    ...minimalEnv(deps.platform),
    SHOGUN_DB_PATH: paths.db,
    [TOKEN_ENV]: secrets.token,
    [DB_KEY_ENV]: secrets.dbKeyHex,
    SHOGUN_MEMORY_API_SETTINGS: paths.memoryApiSettings,
    // E13-T4: the server posture — nothing is sent unless analytics.json opts in.
    SHOGUNAGENT_SERVER: "1",
    ...extra,
  };
}

export interface ServeOptions {
  port?: number;
  host?: string;
}

/** Start `shogun-api` in the foreground (stdio inherited). Returns the child. */
export async function startSidecar(deps: Deps, paths: DataPaths, located: Located, options: ServeOptions = {}): Promise<{ child: SpawnedProcess; port: number; host: string }> {
  const secrets = await sidecarSecrets(deps, paths);
  await ensureMemoryApiEnabled(deps, paths);
  const port = options.port ?? DEFAULT_PORT;
  const host = options.host ?? "127.0.0.1";
  const env = sidecarEnv(deps, paths, secrets, { SHOGUN_API_PORT: String(port), SHOGUN_API_HOST: host });
  const child = deps.spawn(located.api, [], { env, cwd: paths.root, stdio: "inherit" });
  return { child, port, host };
}

/** The port the running sidecar bound (it writes `api.port` next to the store), or `undefined`. */
export async function runningPort(deps: Deps, paths: DataPaths): Promise<number | undefined> {
  if (!(await deps.fs.exists(paths.port))) return undefined;
  const n = Number((await deps.fs.readFile(paths.port)).trim());
  return Number.isInteger(n) && n > 0 && n < 65_536 ? n : undefined;
}

/** The absolute path of a binary inside the located directory. */
export function binaryPath(located: Located, name: "api" | "mcp" | "cli"): string {
  return name === "api" ? located.api : name === "mcp" ? located.mcp : located.cli;
}

export { join };
