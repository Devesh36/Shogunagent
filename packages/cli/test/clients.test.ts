/** The config-file editor (a port of client_install.rs): only our entry changes, every other byte
 * stays, idempotent, refusals instead of repairs, detection of present clients only. */
import { describe, expect, it } from "vitest";

import { ENTRY_NAME, MASK, PlanRefusal, applyPlan, backupPath, describePlan, detectClients, entryFor, looksLikeToken, planFor, planJson, planToml, renderTomlEntry } from "../src/clients.js";
import { clientConfigPath } from "../src/paths.js";
import { HOME, makeRig } from "./fakes.js";

const entry = entryFor("0.1");
const CURSOR = `${HOME}/.cursor/mcp.json`;
const CODEX = `${HOME}/.codex/config.toml`;

describe("the entry", () => {
  it("is npx shogunagent@<minor> mcp, with the data dir only when it is not the default, and never a secret", () => {
    expect(entry).toEqual({ command: "npx", args: ["-y", "shogunagent@0.1", "mcp"] });
    expect(entryFor("0.1", "/srv/shogun")).toEqual({ command: "npx", args: ["-y", "shogunagent@0.1", "mcp"], env: { SHOGUNAGENT_DATA_DIR: "/srv/shogun" } });
    expect(JSON.stringify(entry)).not.toMatch(/token|key/i);
  });

  it("masks what looks like a credential in previews, not paths or package specs", () => {
    expect(looksLikeToken("shogun_0123456789abcdef0123456789abcdef")).toBe(true);
    expect(looksLikeToken("/home/alice/.local/share/shogunagent")).toBe(false);
    expect(looksLikeToken("shogunagent@0.1")).toBe(false);
    expect(looksLikeToken("core")).toBe(false);
  });
});

describe("JSON files (Claude Code, Cursor, Claude Desktop)", () => {
  it("creates the file when absent", () => {
    const p = planJson("cursor", CURSOR, undefined, entry);
    expect(p.change).toBe("add");
    expect(p.baseDigest).toBe("absent");
    expect(JSON.parse(p.newContents ?? "")).toEqual({ mcpServers: { [ENTRY_NAME]: entry } });
    expect(p.newContents?.endsWith("\n")).toBe(true);
  });

  it("adds next to other servers, keeps them and the rest of the file, keeps the indentation", () => {
    const existing = `{\n    "theme": "dark",\n    "mcpServers": {\n        "other": {"command": "x", "env": {"KEY": "abcdefghijklmnopqrstuvwxyz0123"}}\n    }\n}\n`;
    const p = planJson("claude-code", `${HOME}/.claude.json`, existing, entry);
    expect(p.change).toBe("add");
    expect(p.otherEntries).toBe(1);
    const next = JSON.parse(p.newContents ?? "") as { theme: string; mcpServers: Record<string, unknown> };
    expect(next.theme).toBe("dark");
    expect(next.mcpServers.other).toEqual({ command: "x", env: { KEY: "abcdefghijklmnopqrstuvwxyz0123" } });
    expect(next.mcpServers[ENTRY_NAME]).toEqual(entry);
    expect(p.newContents?.includes("\n    \"theme\"")).toBe(true); // 4-space indentation detected
    expect(p.before).toBeUndefined();
    // The preview never shows the other server or its key.
    expect(p.after).not.toContain("abcdefghijklmnopqrstuvwxyz0123");
  });

  it("is idempotent: the same entry is unchanged and nothing is written", async () => {
    const rig = makeRig();
    rig.fs.seed(CURSOR, JSON.stringify({ mcpServers: { [ENTRY_NAME]: entry } }, null, 2) + "\n");
    rig.fs.seed(`${HOME}/.cursor/.keep`, "");
    const p = await planFor(rig.deps, "cursor", entry);
    expect(p.change).toBe("unchanged");
    expect(p.newContents).toBeUndefined();
    expect(await applyPlan(rig.deps, p)).toBe("nothing_to_do");
    expect(rig.fs.writes).toEqual([]);
  });

  it("replaces a differing entry and masks the old one's token in the preview", () => {
    const old = { command: "/old/shogun-mcp", env: { SHOGUN_API_TOKEN: "shogun_0123456789abcdef0123456789" } };
    const p = planJson("cursor", CURSOR, JSON.stringify({ mcpServers: { [ENTRY_NAME]: old, b: {} } }), entry);
    expect(p.change).toBe("replace");
    expect(p.before).toContain(MASK);
    expect(p.before).not.toContain("0123456789abcdef");
    expect(JSON.parse(p.newContents ?? "")).toEqual({ mcpServers: { [ENTRY_NAME]: entry, b: {} } });
  });

  it("removes only our entry; absent when there is none", () => {
    const p = planJson("cursor", CURSOR, JSON.stringify({ mcpServers: { [ENTRY_NAME]: entry, b: { command: "y" } } }), null);
    expect(p.change).toBe("remove");
    expect(JSON.parse(p.newContents ?? "")).toEqual({ mcpServers: { b: { command: "y" } } });
    expect(planJson("cursor", CURSOR, JSON.stringify({ mcpServers: { b: {} } }), null).change).toBe("absent");
    expect(planJson("cursor", CURSOR, undefined, null).change).toBe("absent");
  });

  it("refuses a file it cannot parse or whose shape is not an mcpServers object — never repairs", () => {
    expect(() => planJson("cursor", CURSOR, "{ not json", entry)).toThrow(PlanRefusal);
    try { planJson("cursor", CURSOR, "{ not json", entry); } catch (e) { expect((e as PlanRefusal).refusal).toBe("unparseable"); }
    expect(() => planJson("cursor", CURSOR, "[]", entry)).toThrow(/not an object/);
    expect(() => planJson("cursor", CURSOR, JSON.stringify({ mcpServers: ["x"] }), entry)).toThrow(/not an object/);
  });
});

describe("TOML file (Codex)", () => {
  it("renders the documented table form and creates the file when absent", () => {
    expect(renderTomlEntry(entry)).toBe(`[mcp_servers.shogunagent]\ncommand = "npx"\nargs = ["-y", "shogunagent@0.1", "mcp"]\n`);
    expect(renderTomlEntry(entryFor("0.1", "/srv/s"))).toContain(`[mcp_servers.shogunagent.env]\nSHOGUNAGENT_DATA_DIR = "/srv/s"\n`);
    const p = planToml("codex", CODEX, undefined, entry);
    expect(p.change).toBe("add");
    expect(p.newContents).toBe(renderTomlEntry(entry));
  });

  it("appends after the existing content, byte for byte, and counts other servers", () => {
    const existing = `model = "o3"\n\n[mcp_servers.other]\ncommand = "x"\nargs = ["--flag"]\n\n[mcp_servers.other.env]\nKEY = "abcdefghijklmnopqrstuvwxyz0123"\n`;
    const p = planToml("codex", CODEX, existing, entry);
    expect(p.change).toBe("add");
    expect(p.otherEntries).toBe(1);
    expect(p.newContents?.startsWith(existing.trimEnd())).toBe(true);
    expect(p.newContents?.endsWith(renderTomlEntry(entry))).toBe(true);
  });

  it("is idempotent, replaces an old form, removes cleanly", () => {
    const withOurs = `model = "o3"\n\n${renderTomlEntry(entry)}\n[mcp_servers.other]\ncommand = "x"\n`;
    expect(planToml("codex", CODEX, withOurs, entry).change).toBe("unchanged");
    const old = `model = "o3"\n\n[mcp_servers.shogunagent]\ncommand = "/old/shogun-mcp"\nargs = ["--client", "codex-cli"]\n\n[mcp_servers.shogunagent.env]\nSHOGUN_API_TOKEN = "shogun_0123456789abcdef0123456789"\n\n[mcp_servers.other]\ncommand = "x"\n`;
    const rep = planToml("codex", CODEX, old, entry);
    expect(rep.change).toBe("replace");
    expect(rep.before).toContain(MASK);
    expect(rep.before).not.toContain("0123456789abcdef");
    expect(rep.newContents).toBe(`model = "o3"\n\n[mcp_servers.other]\ncommand = "x"\n\n${renderTomlEntry(entry)}`);
    const rem = planToml("codex", CODEX, old, null);
    expect(rem.change).toBe("remove");
    expect(rem.newContents).toBe(`model = "o3"\n\n[mcp_servers.other]\ncommand = "x"\n`);
    expect(planToml("codex", CODEX, `model = "o3"\n`, null).change).toBe("absent");
  });

  it("refuses dotted keys / inline tables for our entry, duplicates, and non-TOML", () => {
    const refusal = (text: string) => { try { planToml("codex", CODEX, text, entry); return "none"; } catch (e) { return (e as PlanRefusal).refusal; } };
    expect(refusal(`[mcp_servers]\nshogunagent = { command = "x" }\n`)).toBe("unsupported_entry_form");
    expect(refusal(`mcp_servers.shogunagent.command = "x"\n`)).toBe("unsupported_entry_form");
    expect(refusal(`[mcp_servers.shogunagent]\ncommand = "a"\n[mcp_servers.shogunagent]\ncommand = "b"\n`)).toBe("ambiguous_entry");
    expect(refusal(`this is not toml at all\n`)).toBe("unparseable");
    // An inline table for ANOTHER server is fine and counted.
    expect(planToml("codex", CODEX, `[mcp_servers]\nother = { command = "x" }\n`, entry).otherEntries).toBe(1);
  });
});

describe("detection and applying", () => {
  it("detects only the clients whose config file or directory exists", async () => {
    const rig = makeRig();
    expect(await detectClients(rig.deps)).toEqual([]);
    rig.fs.mkdirSync(`${HOME}/.cursor`);
    rig.fs.seed(`${HOME}/.claude.json`, "{}");
    expect(await detectClients(rig.deps)).toEqual(["claude-code", "cursor"]);
    rig.fs.mkdirSync(`${HOME}/.codex`);
    rig.fs.mkdirSync(`${HOME}/.config/Claude`);
    expect(await detectClients(rig.deps)).toEqual(["claude-code", "cursor", "codex", "claude-desktop"]);
    expect(await detectClients(rig.deps, ["codex"])).toEqual(["codex"]);
  });

  it("names each client's documented file per OS", () => {
    const linux = makeRig().deps.platform;
    expect(clientConfigPath("claude-desktop", linux)).toBe(`${HOME}/.config/Claude/claude_desktop_config.json`);
    expect(clientConfigPath("claude-desktop", { ...linux, os: "darwin" })).toBe(`${HOME}/Library/Application Support/Claude/claude_desktop_config.json`);
    expect(clientConfigPath("claude-desktop", { ...linux, os: "win32", env: { APPDATA: "C:\\Users\\alice\\AppData\\Roaming" } })).toContain("claude_desktop_config.json");
    expect(clientConfigPath("codex", linux)).toBe(CODEX);
  });

  it("applies with a backup and refuses when the file changed since the plan", async () => {
    const rig = makeRig();
    rig.fs.seed(CURSOR, JSON.stringify({ mcpServers: { other: {} } }) + "\n");
    const plan = await planFor(rig.deps, "cursor", entry);
    // Someone edits the file between plan and apply.
    rig.fs.seed(CURSOR, JSON.stringify({ mcpServers: { other: {}, third: {} } }) + "\n");
    expect(await applyPlan(rig.deps, plan)).toBe("file_changed");
    const fresh = await planFor(rig.deps, "cursor", entry);
    expect(await applyPlan(rig.deps, fresh)).toBe("written");
    expect(rig.fs.text(backupPath(CURSOR))).toBe(JSON.stringify({ mcpServers: { other: {}, third: {} } }) + "\n");
    expect(JSON.parse(rig.fs.text(CURSOR) ?? "")).toEqual({ mcpServers: { other: {}, third: {}, [ENTRY_NAME]: entry } });
    expect(rig.fs.mode(CURSOR)).toBe(0o600);
    expect(describePlan(fresh)).toContain("add the shogunagent entry");
  });
});
