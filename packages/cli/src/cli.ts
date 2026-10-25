/**
 * Argument parsing and the commands (see help.ts for what each does). `main(argv, deps)` returns
 * the exit code; `bin.ts` supplies the real `Deps`.
 */
import { installBinaries, locateBinaries, releaseTarget, InstallError } from "./binaries.js";
import { applyPlan, describePlan, detectClients, entryFor, planFor, PlanRefusal, type FilePlan } from "./clients.js";
import { HOSTED, SKILL_MINOR, VERSION, helpJson, helpText } from "./help.js";
import { ALL_CLIENTS, CLIENT_LABEL, DATA_DIR_ENV, dataPaths, type ClientId } from "./paths.js";
import { runReadOnlyProxy } from "./readonly-proxy.js";
import { ensureMemoryApiEnabled, runningPort, sidecarEnv, sidecarSecrets, startSidecar } from "./sidecar.js";
import type { Deps } from "./types.js";

export interface ParsedArgs {
  command: string;
  flags: Record<string, string | boolean | string[]>;
  positional: string[];
}

const REPEATABLE = new Set(["client"]);
const WITH_VALUE = new Set(["port", "version", "client", "host"]);

export function parseArgs(argv: string[]): ParsedArgs {
  const flags: ParsedArgs["flags"] = {};
  const positional: string[] = [];
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i] ?? "";
    if (a.startsWith("--")) {
      const eq = a.indexOf("=");
      const name = eq >= 0 ? a.slice(2, eq) : a.slice(2);
      let value: string | boolean = eq >= 0 ? a.slice(eq + 1) : true;
      if (value === true && WITH_VALUE.has(name) && i + 1 < argv.length && !(argv[i + 1] ?? "").startsWith("--")) value = argv[++i] ?? "";
      if (REPEATABLE.has(name)) {
        const prev = flags[name];
        flags[name] = [...(Array.isArray(prev) ? prev : []), String(value)];
      } else flags[name] = value;
    } else positional.push(a);
  }
  const command = positional.shift() ?? "serve";
  return { command, flags, positional };
}

function isClientId(s: string): s is ClientId {
  return (ALL_CLIENTS as readonly string[]).includes(s);
}

function clientsFlag(flags: ParsedArgs["flags"]): ClientId[] | undefined {
  const raw = flags.client;
  if (raw === undefined) return undefined;
  const list = Array.isArray(raw) ? raw : [String(raw)];
  const out: ClientId[] = [];
  for (const c of list) {
    if (!isClientId(c)) throw new UsageError(`unknown client ${JSON.stringify(c)}; one of ${ALL_CLIENTS.join(", ")}`);
    out.push(c);
  }
  return out;
}

export class UsageError extends Error {}

const INSTALL_HINT = "No sidecar binaries found. Run `npx shogunagent install` (downloads the release for this platform and verifies its sha256), or point SHOGUNAGENT_BIN_DIR at a directory holding shogun-api and shogun-mcp.";

/**
 * Connect or disconnect: plan per detected client, show, ask (unless `yes`), apply. Returns the
 * exit code: 0 when every planned write happened or nothing needed writing; 2 when consent was
 * not given (nothing written); 1 on a refusal.
 */
export async function connectCommand(deps: Deps, options: { remove: boolean; yes: boolean; clients?: ClientId[] | undefined }): Promise<number> {
  const paths = dataPaths(deps.platform);
  const explicitDataDir = deps.platform.env[DATA_DIR_ENV]?.trim();
  const entry = options.remove ? null : entryFor(SKILL_MINOR, explicitDataDir || undefined);
  const present = await detectClients(deps, options.clients);
  if (present.length === 0) {
    deps.io.out(options.clients ? `None of ${options.clients.map((c) => CLIENT_LABEL[c]).join(", ")} is installed here (no config file or directory found). Nothing written.` : "No agent app found here (Claude Code, Cursor, Codex, Claude Desktop). Nothing written.");
    return 0;
  }
  const plans: FilePlan[] = [];
  let refused = 0;
  for (const client of present) {
    try {
      plans.push(await planFor(deps, client, entry));
    } catch (e) {
      refused += 1;
      deps.io.err(e instanceof PlanRefusal ? `refused — ${e.message}` : `refused — ${CLIENT_LABEL[client]}: ${(e as Error).message}`);
    }
  }
  for (const p of plans) {
    deps.io.out(describePlan(p));
    if (p.before !== undefined && p.change !== "unchanged") deps.io.out(indent(`before:\n${p.before}`));
    if (p.after !== undefined && p.change !== "unchanged") deps.io.out(indent(`after:\n${p.after}`));
  }
  const writes = plans.filter((p) => p.newContents !== undefined);
  if (writes.length === 0) {
    deps.io.out(options.remove ? "Nothing to remove." : "Nothing to write.");
    return refused > 0 ? 1 : 0;
  }
  let go = options.yes;
  if (!go) {
    if (!deps.platform.interactive) {
      deps.io.out(`${writes.length} file${writes.length === 1 ? "" : "s"} would change. Nothing was written: pass --yes (or --connect to serve) to apply, non-interactively.`);
      return 2;
    }
    go = await deps.io.confirm(`Write ${writes.length} file${writes.length === 1 ? "" : "s"} as shown? [y/N] `);
    if (!go) {
      deps.io.out("Nothing written.");
      return 2;
    }
  }
  let failed = refused;
  for (const p of writes) {
    const outcome = await applyPlan(deps, p);
    if (outcome === "written") deps.io.out(`wrote ${p.path} (backup: ${p.path}.bak-shogunagent)`);
    else if (outcome === "file_changed") {
      failed += 1;
      deps.io.err(`${p.path} changed while planning; not written. Retry.`);
    }
  }
  if (!options.remove && failed === 0) deps.io.out(`Connected. Each app now has a read-only "shogunagent" MCP server; restart the app to pick it up. profile.whoami confirms the connection; reads are free.`);
  return failed > 0 ? 1 : 0;
}

function indent(s: string): string {
  return s.split("\n").map((l) => `    ${l}`).join("\n");
}

export async function main(argv: string[], deps: Deps): Promise<number> {
  let parsed: ParsedArgs;
  try {
    parsed = parseArgs(argv);
  } catch (e) {
    deps.io.err((e as Error).message);
    return 2;
  }
  const { command, flags } = parsed;
  if (flags.version === true && command === "serve" && parsed.positional.length === 0 && Object.keys(flags).length === 1) {
    deps.io.out(VERSION);
    return 0;
  }
  const paths = dataPaths(deps.platform);
  try {
    switch (command) {
      case "help":
      case "--help": {
        deps.io.out(flags.json ? JSON.stringify(helpJson(), null, 2) : helpText());
        return 0;
      }
      case "install": {
        const tag = typeof flags.version === "string" ? flags.version : undefined;
        const r = await installBinaries(deps, paths, tag ? { tag } : {});
        deps.io.out(`installed ${r.tag} into ${r.dir} (sha256 ${r.sha256.slice(0, 12)}…)`);
        return 0;
      }
      case "connect":
        return connectCommand(deps, { remove: false, yes: flags.yes === true, clients: clientsFlag(flags) });
      case "disconnect":
        return connectCommand(deps, { remove: true, yes: flags.yes === true, clients: clientsFlag(flags) });
      case "status": {
        const located = await locateBinaries(deps, paths);
        const port = await runningPort(deps, paths);
        const present = await detectClients(deps);
        const connected: Record<string, string> = {};
        for (const c of present) {
          try {
            const p = await planFor(deps, c, entryFor(SKILL_MINOR, deps.platform.env[DATA_DIR_ENV]?.trim() || undefined));
            connected[c] = p.change === "unchanged" ? "connected" : p.before !== undefined ? "entry differs" : "not connected";
          } catch (e) {
            connected[c] = e instanceof PlanRefusal ? `refused: ${e.refusal}` : "unreadable";
          }
        }
        const status = {
          version: VERSION, data_dir: paths.root,
          binaries: located ? { dir: located.dir, source: located.source } : null,
          sidecar: port ? { listening: true, port, url: `http://127.0.0.1:${port}` } : { listening: false },
          clients: connected, hosted: HOSTED.mcp,
        };
        if (flags.json) deps.io.out(JSON.stringify(status, null, 2));
        else {
          deps.io.out(`data dir: ${paths.root}`);
          deps.io.out(`binaries: ${located ? `${located.dir} (${located.source})` : "none — " + INSTALL_HINT}`);
          deps.io.out(`sidecar: ${port ? `listening on 127.0.0.1:${port}` : "not running"}`);
          for (const [c, s] of Object.entries(connected)) deps.io.out(`${CLIENT_LABEL[c as ClientId]}: ${s}`);
        }
        return 0;
      }
      case "mcp": {
        const located = await locateBinaries(deps, paths);
        if (!located) {
          deps.io.err(INSTALL_HINT);
          return 1;
        }
        const secrets = await sidecarSecrets(deps, paths);
        await ensureMemoryApiEnabled(deps, paths);
        const child = deps.spawn(located.mcp, ["--toolset", "core"], { env: sidecarEnv(deps, paths, secrets, { SHOGUN_MCP_TOOLSETS: "core" }), cwd: paths.root, stdio: "pipe" });
        if (!child.stdin || !child.stdout) {
          deps.io.err("could not attach to shogun-mcp's stdio");
          return 1;
        }
        child.stderr?.on("data", (chunk: Buffer | string) => deps.io.err(String(chunk).trimEnd()));
        await runReadOnlyProxy({ clientIn: process.stdin, clientOut: process.stdout, serverIn: child.stdin as NodeJS.WritableStream & import("node:stream").Writable, serverOut: child.stdout as NodeJS.ReadableStream & import("node:stream").Readable });
        const code = await child.exited;
        return code ?? 0;
      }
      case "serve": {
        if (flags.install === true) {
          const r = await installBinaries(deps, paths);
          deps.io.out(`installed ${r.tag} into ${r.dir}`);
        }
        const located = await locateBinaries(deps, paths);
        if (!located) {
          const t = releaseTarget(deps.platform);
          deps.io.err(INSTALL_HINT);
          if (!t.ok) deps.io.err(t.reason);
          return 1;
        }
        if (flags.connect === true) {
          const code = await connectCommand(deps, { remove: false, yes: flags.yes === true || flags.connect === true, clients: clientsFlag(flags) });
          if (code === 1) return 1;
        }
        const port = typeof flags.port === "string" ? Number(flags.port) : undefined;
        if (port !== undefined && (!Number.isInteger(port) || port < 1 || port > 65_535)) throw new UsageError("--port must be 1..65535");
        const host = typeof flags.host === "string" ? flags.host : undefined;
        const started = await startSidecar(deps, paths, located, { ...(port !== undefined ? { port } : {}), ...(host ? { host } : {}) });
        deps.io.out(`shogunagent ${VERSION}: sidecar starting on http://${started.host}:${started.port} (store: ${paths.db}). Agent apps use the "shogunagent" MCP entry (read-only). Hosted: ${HOSTED.mcp}. Ctrl-C to stop.`);
        const code = await started.child.exited;
        return code ?? 0;
      }
      default:
        deps.io.err(`unknown command ${JSON.stringify(command)}\n`);
        deps.io.err(helpText());
        return 2;
    }
  } catch (e) {
    if (e instanceof UsageError) {
      deps.io.err(e.message);
      return 2;
    }
    if (e instanceof InstallError || e instanceof PlanRefusal) {
      deps.io.err(e.message);
      return 1;
    }
    deps.io.err(e instanceof Error ? e.message : String(e));
    return 1;
  }
}
