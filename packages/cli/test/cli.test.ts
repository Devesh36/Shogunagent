/** The commands end to end over the fakes: help --json for agents, serve and mcp refuse without
 * trusted binaries, connect writes only with explicit consent and only for present clients, the
 * sidecar gets a generated token and key kept at 0600 and never written anywhere else. */
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

import { ENTRY_NAME } from "../src/clients.js";
import { main, parseArgs } from "../src/cli.js";
import { SKILL_VERSION, VERSION, helpJson } from "../src/help.js";
import { HOME, makeRig, type Rig } from "./fakes.js";

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, "..", "..", "..");
const DATA = `${HOME}/.local/share/shogunagent`;
const TAG = "v0.1.0";

function withBinaries(rig: Rig): Rig {
  rig.fs.seed(`${DATA}/bin/current`, `${TAG}\n`);
  rig.fs.seed(`${DATA}/bin/${TAG}/shogun-api`, "");
  rig.fs.seed(`${DATA}/bin/${TAG}/shogun-mcp`, "");
  rig.fs.seed(`${DATA}/bin/${TAG}/shogun`, "");
  return rig;
}

describe("parseArgs", () => {
  it("defaults to serve, reads flags with values, repeats --client", () => {
    expect(parseArgs([])).toEqual({ command: "serve", flags: {}, positional: [] });
    expect(parseArgs(["--connect", "--port", "8000"])).toEqual({ command: "serve", flags: { connect: true, port: "8000" }, positional: [] });
    expect(parseArgs(["connect", "--yes", "--client", "cursor", "--client=codex"])).toEqual({ command: "connect", flags: { yes: true, client: ["cursor", "codex"] }, positional: [] });
    expect(parseArgs(["help", "--json"])).toEqual({ command: "help", flags: { json: true }, positional: [] });
  });
});

describe("help --json (for agents)", () => {
  it("names the commands, endpoints and skill version; the version equals SKILL.md and package.json", async () => {
    const rig = makeRig();
    expect(await main(["help", "--json"], rig.deps)).toBe(0);
    const doc = JSON.parse(rig.out.join("\n")) as Record<string, unknown>;
    expect(doc).toEqual(helpJson());
    expect(doc.version).toBe(VERSION);
    expect(doc.skill_version).toBe(SKILL_VERSION);
    expect(doc.skill_minor).toBe("0.1");
    const commands = (doc.commands as Array<{ name: string }>).map((c) => c.name);
    expect(commands).toEqual(["serve", "install", "connect", "disconnect", "mcp", "doctor", "status", "help"]);
    expect((doc.hosted as { mcp: string }).mcp).toBe("https://api.shogunagent.dev/mcp");
    expect((doc.local as { entry: unknown; scope: string }).entry).toEqual({ command: "npx", args: ["-y", "shogunagent@0.1", "mcp"] });
    expect((doc.local as { scope: string }).scope).toBe("read");
    const skill = readFileSync(join(repo, "SKILL.md"), "utf8");
    expect(/^version: (\S+)$/m.exec(skill)?.[1]).toBe(SKILL_VERSION);
    const pkg = JSON.parse(readFileSync(join(here, "..", "package.json"), "utf8")) as { version: string; name: string };
    expect(pkg.version).toBe(VERSION);
    expect(pkg.name).toBe("shogunagent");
    const text = makeRig();
    expect(await rigMain(text)(["help"])).toBe(0);
    expect(text.out.join("\n")).toContain("shogunagent 0.1.0");
  });
});

const rigMain = (rig: Rig) => (argv: string[]) => main(argv, rig.deps);

describe("serve", () => {
  it("refuses without trusted binaries and tells how to install; never downloads on its own", async () => {
    const rig = makeRig();
    expect(await rigMain(rig)([])).toBe(1);
    expect(rig.err.join("\n")).toContain("npx shogunagent install");
    expect(rig.requested).toEqual([]);
    expect(rig.spawnCalls).toEqual([]);
  });

  it("starts shogun-api with a generated token and store key kept at 0600 under the data dir", async () => {
    const rig = withBinaries(makeRig());
    expect(await rigMain(rig)(["--port", "7777"])).toBe(0);
    expect(rig.spawnCalls).toHaveLength(1);
    const call = rig.spawnCalls[0]!;
    expect(call.command).toBe(`${DATA}/bin/${TAG}/shogun-api`);
    expect(call.stdio).toBe("inherit");
    expect(call.env.SHOGUN_DB_PATH).toBe(`${DATA}/memory.db`);
    expect(call.env.SHOGUN_API_PORT).toBe("7777");
    expect(call.env.SHOGUNAGENT_SERVER).toBe("1");
    expect(call.env.SHOGUN_API_TOKEN).toMatch(/^shogun_[0-9a-f]{48}$/);
    expect(call.env.SHOGUN_DB_KEY).toMatch(/^[0-9a-f]{64}$/);
    expect(rig.fs.text(`${DATA}/api.token`)).toBe(`${call.env.SHOGUN_API_TOKEN}\n`);
    expect(rig.fs.mode(`${DATA}/api.token`)).toBe(0o600);
    expect(rig.fs.text(`${DATA}/db.key`)).toBe(`${call.env.SHOGUN_DB_KEY}\n`);
    expect(rig.fs.mode(`${DATA}/db.key`)).toBe(0o600);
    expect(JSON.parse(rig.fs.text(`${DATA}/memory_api.json`) ?? "")).toEqual({ enabled: true });
    // Nothing printed carries the secrets.
    expect([...rig.out, ...rig.err].join("\n")).not.toContain(call.env.SHOGUN_API_TOKEN ?? "x");
    expect([...rig.out, ...rig.err].join("\n")).not.toContain(call.env.SHOGUN_DB_KEY ?? "x");
    // A second start reuses them.
    const again = withBinaries(makeRig({ fs: rig.fs }));
    await rigMain(again)([]);
    expect(again.spawnCalls[0]?.env.SHOGUN_API_TOKEN).toBe(call.env.SHOGUN_API_TOKEN);
    expect(again.spawnCalls[0]?.env.SHOGUN_DB_KEY).toBe(call.env.SHOGUN_DB_KEY);
  });

  it("environment secrets take precedence and then nothing is written; an existing memory_api.json is left alone", async () => {
    const rig = withBinaries(makeRig({ platform: { env: { HOME, SHOGUN_API_TOKEN: "env-token", SHOGUN_DB_KEY: "ab".repeat(32) } } }));
    rig.fs.seed(`${DATA}/memory_api.json`, JSON.stringify({ enabled: false }));
    expect(await rigMain(rig)([])).toBe(0);
    expect(rig.spawnCalls[0]?.env.SHOGUN_API_TOKEN).toBe("env-token");
    expect(await rig.fs.exists(`${DATA}/api.token`)).toBe(false);
    expect(await rig.fs.exists(`${DATA}/db.key`)).toBe(false);
    expect(rig.fs.text(`${DATA}/memory_api.json`)).toBe(JSON.stringify({ enabled: false }));
  });
});

describe("connect / disconnect", () => {
  it("non-interactive without --yes: shows the plan, writes nothing, exits 2", async () => {
    const rig = makeRig();
    rig.fs.mkdirSync(`${HOME}/.cursor`);
    rig.fs.mkdirSync(`${HOME}/.codex`);
    expect(await rigMain(rig)(["connect"])).toBe(2);
    expect(rig.fs.writes).toEqual([]);
    expect(rig.out.join("\n")).toContain("Nothing was written");
    expect(rig.out.join("\n")).toContain("Cursor: add the shogunagent entry");
    expect(rig.out.join("\n")).toContain("Codex (CLI, ChatGPT desktop): add the shogunagent entry");
  });

  it("interactive no writes nothing; interactive yes writes", async () => {
    const no = makeRig({ platform: { interactive: true }, answers: [false] });
    no.fs.mkdirSync(`${HOME}/.cursor`);
    expect(await rigMain(no)(["connect"])).toBe(2);
    expect(no.fs.writes).toEqual([]);
    const yes = makeRig({ platform: { interactive: true }, answers: [true] });
    yes.fs.mkdirSync(`${HOME}/.cursor`);
    expect(await rigMain(yes)(["connect"])).toBe(0);
    expect(JSON.parse(yes.fs.text(`${HOME}/.cursor/mcp.json`) ?? "")).toEqual({ mcpServers: { [ENTRY_NAME]: { command: "npx", args: ["-y", "shogunagent@0.1", "mcp"] } } });
  });

  it("--yes writes only the present clients, keeps other servers, is idempotent, and disconnect undoes it", async () => {
    const rig = makeRig();
    rig.fs.seed(`${HOME}/.claude.json`, JSON.stringify({ numStartups: 3, mcpServers: { other: { command: "o" } } }, null, 2) + "\n");
    rig.fs.seed(`${HOME}/.codex/config.toml`, `model = "o3"\n\n[mcp_servers.other]\ncommand = "o"\n`);
    expect(await rigMain(rig)(["connect", "--yes"])).toBe(0);
    const claude = JSON.parse(rig.fs.text(`${HOME}/.claude.json`) ?? "") as { numStartups: number; mcpServers: Record<string, unknown> };
    expect(claude.numStartups).toBe(3);
    expect(claude.mcpServers.other).toEqual({ command: "o" });
    expect(claude.mcpServers[ENTRY_NAME]).toEqual({ command: "npx", args: ["-y", "shogunagent@0.1", "mcp"] });
    expect(rig.fs.text(`${HOME}/.codex/config.toml`)).toBe(`model = "o3"\n\n[mcp_servers.other]\ncommand = "o"\n\n[mcp_servers.shogunagent]\ncommand = "npx"\nargs = ["-y", "shogunagent@0.1", "mcp"]\n`);
    expect(await rig.fs.exists(`${HOME}/.claude.json.bak-shogunagent`)).toBe(true);
    // Not present: nothing created.
    expect(await rig.fs.exists(`${HOME}/.cursor/mcp.json`)).toBe(false);
    expect(await rig.fs.exists(`${HOME}/.config/Claude/claude_desktop_config.json`)).toBe(false);
    // Idempotent.
    const writesBefore = rig.fs.writes.length;
    expect(await rigMain(rig)(["connect", "--yes"])).toBe(0);
    expect(rig.fs.writes.length).toBe(writesBefore);
    expect(rig.out.join("\n")).toContain("already connected");
    // Disconnect.
    expect(await rigMain(rig)(["disconnect", "--yes"])).toBe(0);
    expect(JSON.parse(rig.fs.text(`${HOME}/.claude.json`) ?? "")).toEqual({ numStartups: 3, mcpServers: { other: { command: "o" } } });
    expect(rig.fs.text(`${HOME}/.codex/config.toml`)).toBe(`model = "o3"\n\n[mcp_servers.other]\ncommand = "o"\n`);
  });

  it("a non-default data dir rides along in the entry's env; a refused file does not stop the others", async () => {
    const rig = makeRig({ platform: { env: { HOME, SHOGUNAGENT_DATA_DIR: "/srv/shogun" } } });
    rig.fs.mkdirSync(`${HOME}/.cursor`);
    rig.fs.seed(`${HOME}/.claude.json`, "{ broken json");
    expect(await rigMain(rig)(["connect", "--yes"])).toBe(1); // one refusal → exit 1, but Cursor was written
    expect(rig.err.join("\n")).toContain("refused");
    expect(rig.err.join("\n")).toContain("not valid JSON");
    const cursor = JSON.parse(rig.fs.text(`${HOME}/.cursor/mcp.json`) ?? "") as { mcpServers: Record<string, { env?: Record<string, string> }> };
    expect(cursor.mcpServers[ENTRY_NAME]?.env).toEqual({ SHOGUNAGENT_DATA_DIR: "/srv/shogun" });
    expect(rig.fs.text(`${HOME}/.claude.json`)).toBe("{ broken json");
  });

  it("--client narrows to named clients and rejects unknown ones", async () => {
    const rig = makeRig();
    rig.fs.mkdirSync(`${HOME}/.cursor`);
    rig.fs.mkdirSync(`${HOME}/.codex`);
    expect(await rigMain(rig)(["connect", "--yes", "--client", "codex"])).toBe(0);
    expect(await rig.fs.exists(`${HOME}/.cursor/mcp.json`)).toBe(false);
    expect(await rig.fs.exists(`${HOME}/.codex/config.toml`)).toBe(true);
    expect(await rigMain(rig)(["connect", "--yes", "--client", "vscode"])).toBe(2);
    expect(await rigMain(rig)(["connect", "--yes", "--client", "claude-desktop"])).toBe(0);
    expect(rig.out.join("\n")).toContain("is installed here");
  });

  it("serve --connect counts as the explicit consent", async () => {
    const rig = withBinaries(makeRig());
    rig.fs.mkdirSync(`${HOME}/.cursor`);
    expect(await rigMain(rig)(["--connect"])).toBe(0);
    expect(await rig.fs.exists(`${HOME}/.cursor/mcp.json`)).toBe(true);
    expect(rig.spawnCalls.map((c) => c.command)).toEqual([`${DATA}/bin/${TAG}/shogun-api`]);
  });
});

describe("mcp and status", () => {
  it("mcp refuses without binaries; with them it spawns shogun-mcp on the core toolset with the store env", async () => {
    const none = makeRig();
    expect(await rigMain(none)(["mcp"])).toBe(1);
    expect(none.spawnCalls).toEqual([]);
    const rig = withBinaries(makeRig({ spawn: (_c, child) => { child.stdinStream.on("data", () => undefined); child.finish(0); } }));
    expect(await rigMain(rig)(["mcp"])).toBe(0);
    expect(rig.spawnCalls[0]).toMatchObject({ command: `${DATA}/bin/${TAG}/shogun-mcp`, args: ["--toolset", "core"], stdio: "pipe" });
    expect(rig.spawnCalls[0]?.env.SHOGUN_MCP_TOOLSETS).toBe("core");
    expect(rig.spawnCalls[0]?.env.SHOGUN_DB_PATH).toBe(`${DATA}/memory.db`);
    expect(rig.spawnCalls[0]?.env.SHOGUN_API_TOKEN).toMatch(/^shogun_/);
  });

  it("status reports binaries, the sidecar port file and each present client's connection", async () => {
    const rig = withBinaries(makeRig());
    rig.fs.seed(`${DATA}/api.port`, "7464\n");
    rig.fs.seed(`${HOME}/.cursor/mcp.json`, JSON.stringify({ mcpServers: { [ENTRY_NAME]: { command: "npx", args: ["-y", "shogunagent@0.1", "mcp"] } } }));
    rig.fs.mkdirSync(`${HOME}/.codex`);
    expect(await rigMain(rig)(["status", "--json"])).toBe(0);
    const s = JSON.parse(rig.out.join("\n")) as { binaries: { source: string }; sidecar: { listening: boolean; port: number }; clients: Record<string, string> };
    expect(s.binaries.source).toBe("installed");
    expect(s.sidecar).toEqual({ listening: true, port: 7464, url: "http://127.0.0.1:7464" });
    expect(s.clients).toEqual({ cursor: "connected", codex: "not connected" });
  });
});
