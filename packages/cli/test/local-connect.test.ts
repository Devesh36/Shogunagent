import { describe, expect, it } from "vitest";
import { main } from "../src/cli.js";
import { applyPlan, localEntryFor, planFor } from "../src/clients.js";
import { ALL_CLIENTS, clientConfigPath } from "../src/paths.js";
import { makeRig, type Rig } from "./fakes.js";

const NODE = "/opt/node runtime/bin/node";
const SCRIPT = "/work/source checkout/packages/cli/bin/shogunagent.mjs";
const COMPILED = "/work/source checkout/packages/cli/dist/bin.js";
const DATA = "/srv/synthetic store";
const BIN = "/work/source checkout/target/debug";
const SECRET = "synthetic-secret-never-in-config";

function prepared(): Rig {
  const rig = makeRig({ platform: {
    localLauncher: { node: NODE, script: SCRIPT },
    env: { SHOGUNAGENT_DATA_DIR: DATA, SHOGUNAGENT_BIN_DIR: BIN, SHOGUN_API_TOKEN: SECRET, SHOGUN_DB_KEY: "ab".repeat(32), PRIVATE_OTHER: SECRET },
  } });
  for (const path of [NODE, SCRIPT, COMPILED, `${BIN}/shogun-api`, `${BIN}/shogun-mcp`]) rig.fs.seed(path, "synthetic file");
  for (const client of ALL_CLIENTS) {
    rig.fs.seed(clientConfigPath(client, rig.deps.platform), client === "codex"
      ? '# keep this comment\nmodel = "unchanged"\n\n[mcp_servers.other]\ncommand = "other-server"\n'
      : JSON.stringify({ theme: "unchanged", mcpServers: { other: { command: "other-server" } } }, null, 2) + "\n");
  }
  return rig;
}

function noSecretsOrProcesses(rig: Rig): void {
  const text = [...rig.out, ...rig.err, ...rig.fs.writes.map((p) => rig.fs.text(p))].join("\n");
  expect(text).not.toContain(SECRET);
  expect(text).not.toContain("ab".repeat(32));
  expect(text).not.toContain("SHOGUN_API_TOKEN");
  expect(text).not.toContain("SHOGUN_DB_KEY");
  expect(rig.requested).toEqual([]);
  expect(rig.spawnCalls).toEqual([]);
}

describe("connect --local", () => {
  it.each(ALL_CLIENTS)("round-trips, repeats, reports and removes the local entry for %s", async (client) => {
    const rig = prepared();
    const config = clientConfigPath(client, rig.deps.platform);
    const original = rig.fs.text(config);
    expect(await main(["connect", "--local", "--yes", "--client", client], rig.deps)).toBe(0);
    const expected = { command: NODE, args: [SCRIPT, "mcp"], env: { SHOGUNAGENT_DATA_DIR: DATA, SHOGUNAGENT_BIN_DIR: BIN } };
    expect(await localEntryFor(rig.deps)).toEqual(expected);
    expect((await planFor(rig.deps, client, expected)).change).toBe("unchanged");
    const content = rig.fs.text(config) ?? "";
    expect(content).not.toContain("npx");
    expect(content).toContain("other-server");
    if (client === "codex") expect(content).toContain(original?.trimEnd());
    else expect(JSON.parse(content).mcpServers.shogunagent).toEqual(expected);
    expect(rig.fs.text(`${config}.bak-shogunagent`)).toBe(original);
    const writes = [...rig.fs.writes];
    expect(await main(["connect", "--local", "--yes", "--client", client], rig.deps)).toBe(0);
    expect(rig.fs.writes).toEqual(writes);
    rig.out.length = 0;
    expect(await main(["status", "--json"], rig.deps)).toBe(0);
    expect(JSON.parse(rig.out.join("\n")).clients[client]).toBe("configured (local launcher)");
    // Removal does not depend on the old launcher still existing.
    await rig.fs.remove(SCRIPT);
    expect(await main(["disconnect", "--yes", "--client", client], rig.deps)).toBe(0);
    expect(rig.fs.text(config)).not.toContain("shogunagent");
    expect(rig.fs.text(config)).toContain("other-server");
    if (client === "codex") expect(rig.fs.text(config)).toBe(original);
    else expect(JSON.parse(rig.fs.text(config) ?? "")).toEqual(JSON.parse(original ?? ""));
    noSecretsOrProcesses(rig);
  });

  it("requires existing consent and preserves concurrent changes", async () => {
    const rig = prepared();
    expect(await main(["connect", "--local", "--client", "cursor"], rig.deps)).toBe(2);
    expect(rig.fs.writes).toEqual([]);
    const entry = await localEntryFor(rig.deps);
    const plan = await planFor(rig.deps, "cursor", entry);
    const changed = '{"mcpServers":{"other":{"command":"changed-by-user"}}}';
    rig.fs.seed(plan.path, changed);
    expect(await applyPlan(rig.deps, plan)).toBe("file_changed");
    expect(rig.fs.text(plan.path)).toBe(changed);
    expect(rig.fs.writes).toEqual([]);
    noSecretsOrProcesses(rig);
  });

  it("can select all clients and pin an installed binary directory without an override", async () => {
    const rig = prepared();
    delete rig.deps.platform.env.SHOGUNAGENT_BIN_DIR;
    rig.fs.seed(`${DATA}/bin/current`, "v0.1.0");
    for (const binary of ["shogun-api", "shogun-mcp"]) rig.fs.seed(`${DATA}/bin/v0.1.0/${binary}`, "binary");
    expect(await main(["connect", "--local", "--yes"], rig.deps)).toBe(0);
    const entry = await localEntryFor(rig.deps);
    expect(entry.env?.SHOGUNAGENT_BIN_DIR).toBe(`${DATA}/bin/v0.1.0`);
    for (const client of ALL_CLIENTS) expect((await planFor(rig.deps, client, entry)).change).toBe("unchanged");
    noSecretsOrProcesses(rig);
  });

  it.each([
    ["doctor", "--local"], ["serve", "--connect", "--local"], ["disconnect", "--local"],
    ["connect", "--local=false"], ["connect", "--local", "true"], ["connect", "--local", "--install"],
    ["connect", "--local", "--unknown"], ["connect", "--local", "--client", "unknown"],
  ])("refuses invalid options before writes: %j", async (...args) => {
    const rig = prepared();
    expect(await main(args, rig.deps)).toBe(2);
    expect(rig.fs.writes).toEqual([]);
    noSecretsOrProcesses(rig);
  });

  it.each(["node", "script", "compiled", "binaries", "data_relative", "bin_relative", "path_control", "directory", "no_launcher"])("refuses invalid local prerequisites: %s", async (problem) => {
    const rig = prepared();
    if (problem === "node") rig.deps.platform.localLauncher = { node: "node", script: SCRIPT };
    if (problem === "script") rig.deps.platform.localLauncher = { node: NODE, script: "bin/shogunagent.mjs" };
    if (problem === "compiled") await rig.fs.remove(COMPILED);
    if (problem === "binaries") await rig.fs.remove(`${BIN}/shogun-mcp`);
    if (problem === "data_relative") rig.deps.platform.env.SHOGUNAGENT_DATA_DIR = "./store";
    if (problem === "bin_relative") rig.deps.platform.env.SHOGUNAGENT_BIN_DIR = "./target";
    if (problem === "path_control") rig.deps.platform.env.SHOGUNAGENT_DATA_DIR = "/srv/bad\npath";
    if (problem === "directory") { await rig.fs.remove(NODE); rig.fs.mkdirSync(NODE); }
    if (problem === "no_launcher") delete rig.deps.platform.localLauncher;
    expect(await main(["connect", "--local", "--yes"], rig.deps)).toBe(1);
    expect(rig.fs.writes).toEqual([]);
    noSecretsOrProcesses(rig);
  });
});
