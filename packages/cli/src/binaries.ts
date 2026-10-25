/**
 * Finding and installing the `shogun-api` / `shogun-mcp` / `shogun` binaries.
 *
 * Trusted locations only: `SHOGUNAGENT_BIN_DIR` (the user named it), or a release this CLI
 * installed under the data directory after verifying its SHA-256 against the `.sha256` asset the
 * release workflow publishes (.github/workflows/release.yml: `shogunagent-<tag>-<target>.tar.gz`
 * and `.tar.gz.sha256`). Never the current directory, never a bare name resolved through PATH.
 *
 * Downloads happen only on an explicit `--install` (or `shogunagent install`), never as a side
 * effect of `serve` or `connect`.
 */
import { createHash } from "node:crypto";
import { join } from "node:path";

import type { DataPaths } from "./paths.js";
import { BIN_DIR_ENV } from "./paths.js";
import type { Deps, Platform } from "./types.js";

export const RELEASE_REPO = "Torutesu/Shogunagent";
export const BINARIES = ["shogun-api", "shogun-mcp", "shogun"] as const;
export type BinaryName = (typeof BINARIES)[number];

/** The Rust target triple the release workflow builds for this machine, or why there is none. */
export function releaseTarget(p: Platform): { ok: true; target: string } | { ok: false; reason: string } {
  if (p.os === "linux" && p.arch === "x64") return { ok: true, target: "x86_64-unknown-linux-gnu" };
  if (p.os === "linux" && p.arch === "arm64") return { ok: true, target: "aarch64-unknown-linux-gnu" };
  if (p.os === "darwin") return { ok: false, reason: "macOS binaries are not published yet (release.yml); on a Mac the ShogunAI app provides the sidecar, or build from source: cargo build --release -p shogun-core --features daemon-server --bin shogun-api --bin shogun-mcp" };
  return { ok: false, reason: `no release binaries for ${p.os}/${p.arch}; build from source (see README)` };
}

export function assetName(tag: string, target: string): string {
  return `shogunagent-${tag}-${target}.tar.gz`;
}

export function assetUrl(tag: string, target: string): string {
  return `https://github.com/${RELEASE_REPO}/releases/download/${encodeURIComponent(tag)}/${assetName(tag, target)}`;
}

export interface Located {
  dir: string;
  source: "env" | "installed";
  api: string;
  mcp: string;
  cli: string;
}

const exe = (p: Platform, name: string): string => (p.os === "win32" ? `${name}.exe` : name);

/** Where the binaries are, from the trusted locations only, or `undefined`. */
export async function locateBinaries(deps: Deps, paths: DataPaths): Promise<Located | undefined> {
  const p = deps.platform;
  const explicit = p.env[BIN_DIR_ENV]?.trim();
  const candidates: Array<{ dir: string; source: Located["source"] }> = [];
  if (explicit) {
    if (!explicit.startsWith("/") && !/^[A-Za-z]:[\\/]/.test(explicit)) {
      throw new Error(`${BIN_DIR_ENV} must be an absolute path (got a relative one); refusing to run binaries from a relative location`);
    }
    candidates.push({ dir: explicit, source: "env" });
  }
  if (await deps.fs.exists(paths.binCurrent)) {
    const tag = (await deps.fs.readFile(paths.binCurrent)).trim();
    if (/^[A-Za-z0-9._-]+$/.test(tag)) candidates.push({ dir: join(paths.bin, tag), source: "installed" });
  }
  for (const c of candidates) {
    const api = join(c.dir, exe(p, "shogun-api"));
    const mcp = join(c.dir, exe(p, "shogun-mcp"));
    const cli = join(c.dir, exe(p, "shogun"));
    if ((await deps.fs.exists(api)) && (await deps.fs.exists(mcp))) return { dir: c.dir, source: c.source, api, mcp, cli };
  }
  return undefined;
}

export interface InstallOptions {
  /** A release tag such as `v0.1.0`; default: the latest release. */
  tag?: string;
}

export interface InstallResult {
  tag: string;
  dir: string;
  sha256: string;
}

export class InstallError extends Error {}

/** Resolve the tag, download the asset and its `.sha256`, verify, extract with `tar`, select. */
export async function installBinaries(deps: Deps, paths: DataPaths, options: InstallOptions = {}): Promise<InstallResult> {
  const target = releaseTarget(deps.platform);
  if (!target.ok) throw new InstallError(target.reason);
  const tag = options.tag ?? (await latestTag(deps));
  if (!/^v?[0-9][A-Za-z0-9._-]*$/.test(tag)) throw new InstallError(`unexpected release tag ${JSON.stringify(tag)}`);
  const name = assetName(tag, target.target);
  const url = assetUrl(tag, target.target);
  const shaRes = await deps.fetch(`${url}.sha256`);
  if (!shaRes.ok) throw new InstallError(`no ${name}.sha256 for ${tag} (${shaRes.status}); the release may not have finished publishing`);
  const expected = parseSha256Line(await shaRes.text(), name);
  const res = await deps.fetch(url);
  if (!res.ok) throw new InstallError(`download of ${name} failed (${res.status})`);
  const bytes = new Uint8Array(await res.arrayBuffer());
  const actual = createHash("sha256").update(bytes).digest("hex");
  if (actual !== expected) throw new InstallError(`checksum mismatch for ${name}: expected ${expected}, got ${actual}; nothing was installed`);
  // Everything lands under bin/<tag>/ only after the checksum passed.
  const dest = join(paths.bin, tag);
  const archive = join(paths.bin, `${name}.download`);
  await deps.fs.mkdirp(dest);
  await deps.fs.writeFile(archive, bytes, 0o600);
  const tar = deps.spawn("tar", ["-xzf", archive, "-C", dest, "--strip-components=1"], { env: minimalEnv(deps.platform), stdio: "pipe" });
  const code = await tar.exited;
  await deps.fs.remove(archive).catch(() => undefined);
  if (code !== 0) throw new InstallError(`tar exited with ${code ?? "a signal"} while extracting ${name}`);
  for (const b of BINARIES) {
    const path = join(dest, exe(deps.platform, b));
    if (!(await deps.fs.exists(path))) throw new InstallError(`${name} did not contain ${b}`);
    if (deps.platform.os !== "win32") await deps.fs.chmod(path, 0o755);
  }
  await deps.fs.writeFile(paths.binCurrent, `${tag}\n`, 0o644);
  return { tag, dir: dest, sha256: actual };
}

/** `sha256sum` output: `<hex>  <name>`. The name must match: a checksum for another file is no checksum. */
export function parseSha256Line(text: string, name: string): string {
  const line = text.split(/\r?\n/).map((l) => l.trim()).find((l) => l.length > 0);
  const m = line ? /^([0-9a-fA-F]{64})\s+\*?(\S+)$/.exec(line) : null;
  if (!m) throw new InstallError(`unreadable .sha256 for ${name}`);
  const listed = m[2] ?? "";
  if (listed !== name && listed !== `./${name}`) throw new InstallError(`.sha256 names ${listed}, not ${name}`);
  return (m[1] ?? "").toLowerCase();
}

async function latestTag(deps: Deps): Promise<string> {
  const res = await deps.fetch(`https://api.github.com/repos/${RELEASE_REPO}/releases/latest`, { headers: { accept: "application/vnd.github+json", "user-agent": "shogunagent-cli" } });
  if (!res.ok) throw new InstallError(`could not read the latest release (${res.status}); pass --version <tag>`);
  const body = (await res.json()) as { tag_name?: unknown };
  if (typeof body.tag_name !== "string") throw new InstallError("the latest release has no tag");
  return body.tag_name;
}

/** The environment a child gets: PATH and the locale, nothing else of the CLI's own. */
export function minimalEnv(p: Platform): Record<string, string> {
  const out: Record<string, string> = {};
  for (const k of ["PATH", "HOME", "LANG", "LC_ALL", "TMPDIR", "SystemRoot", "APPDATA", "USERPROFILE"]) {
    const v = p.env[k];
    if (v !== undefined) out[k] = v;
  }
  return out;
}
