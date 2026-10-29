/** Isolated protocol smoke: real CLI processes, a synthetic MCP executable, temporary files only. */
import { spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import * as ts from "typescript";
import { expect, it } from "vitest";
import type { Entry } from "../src/clients.js";

// The synthetic executable uses a POSIX shebang, so Windows needs a separate executable fixture.
it.skipIf(process.platform === "win32" || /\s/u.test(process.execPath))("launches the generated local entry without npx and preserves protocol/read-only behavior", () => {
  const root = mkdtempSync(join(tmpdir(), "shogun-local-connect-"));
  try {
    const checkout = join(root, "checkout with spaces");
    const home = join(root, "isolated home");
    const data = join(root, "synthetic store");
    const binaries = join(root, "fake binaries");
    const pkg = join(dirname(fileURLToPath(import.meta.url)), "..");
    for (const dir of [join(checkout, "bin"), join(checkout, "dist"), join(home, ".cursor"), data, binaries]) mkdirSync(dir, { recursive: true });
    writeFileSync(join(checkout, "package.json"), '{"type":"module"}');
    copyFileSync(join(pkg, "bin/shogunagent.mjs"), join(checkout, "bin/shogunagent.mjs"));
    // Make the smoke independent of a prior build/dist directory. Typecheck runs separately.
    for (const name of readdirSync(join(pkg, "src")).filter((name) => name.endsWith(".ts"))) {
      const compiled = ts.transpileModule(readFileSync(join(pkg, "src", name), "utf8"), {
        compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext },
      });
      writeFileSync(join(checkout, "dist", name.replace(/\.ts$/u, ".js")), compiled.outputText);
    }
    const key = "ab".repeat(32);
    const token = "synthetic-smoke-token";
    writeFileSync(join(data, "db.key"), key, { mode: 0o600 });
    writeFileSync(join(data, "api.token"), token, { mode: 0o600 });
    writeFileSync(join(data, "memory.db"), "synthetic placeholder; the fixture never opens a store");
    writeFileSync(join(data, "memory_api.json"), '{"enabled":true}');
    const fixture = `#!${process.execPath}
const readline = require("node:readline");
readline.createInterface({input:process.stdin}).on("line", (line) => {
  const request = JSON.parse(line);
  if (request.id === undefined) return;
  let result;
  if (request.method === "initialize") result = {protocolVersion:"2025-03-26",capabilities:{tools:{}},serverInfo:{name:"synthetic",version:"1"}};
  else if (request.method === "tools/list") result = {tools:[{name:"profile.whoami"},{name:"memory.ingest"}]};
  else if (request.params?.name === "profile.whoami") result = {content:[{type:"text",text:"synthetic-profile"}],isError:false};
  else result = {unexpected_write_reached_server:true};
  process.stdout.write(JSON.stringify({jsonrpc:"2.0",id:request.id,result})+"\\n");
});
`;
    for (const name of ["shogun-api", "shogun-mcp"]) writeFileSync(join(binaries, name), fixture, { mode: 0o700 });
    // Empty PATH proves the generated launcher does not need npx, npm or a PATH-resolved Node.
    const env = { HOME: home, PATH: "", SHOGUNAGENT_DATA_DIR: data, SHOGUNAGENT_BIN_DIR: binaries };
    const configured = spawnSync(process.execPath, [join(checkout, "bin/shogunagent.mjs"), "connect", "--local", "--yes", "--client", "cursor"], {
      env, cwd: home, encoding: "utf8", timeout: 10_000,
    });
    expect(configured.error).toBeUndefined();
    expect(configured.status, configured.stderr).toBe(0);
    const configText = readFileSync(join(home, ".cursor/mcp.json"), "utf8");
    const entry = (JSON.parse(configText) as { mcpServers: { shogunagent: Entry } }).mcpServers.shogunagent;
    expect(entry.command).toBe(process.execPath);
    expect(entry.env).toEqual({ SHOGUNAGENT_DATA_DIR: data, SHOGUNAGENT_BIN_DIR: binaries });
    expect(configText).not.toContain("npx");
    for (const secret of [key, token]) expect(configText + configured.stdout + configured.stderr).not.toContain(secret);
    const messages = [
      { jsonrpc: "2.0", id: 1, method: "initialize", params: {} },
      { jsonrpc: "2.0", method: "notifications/initialized" },
      { jsonrpc: "2.0", id: 2, method: "tools/list", params: {} },
      { jsonrpc: "2.0", id: 3, method: "tools/call", params: { name: "profile.whoami", arguments: {} } },
      { jsonrpc: "2.0", id: 4, method: "tools/call", params: { name: "memory.ingest", arguments: {} } },
    ];
    const connected = spawnSync(entry.command, entry.args, {
      env: { HOME: home, PATH: "", ...entry.env }, cwd: home, encoding: "utf8", timeout: 10_000,
      input: messages.map((message) => JSON.stringify(message)).join("\n") + "\n",
    });
    expect(connected.error).toBeUndefined();
    expect(connected.status, connected.stderr).toBe(0);
    const replies = connected.stdout.trim().split("\n").map((line) => JSON.parse(line) as { id: number; result: Record<string, unknown> });
    expect(replies.find((reply) => reply.id === 1)?.result.serverInfo).toEqual({ name: "synthetic", version: "1" });
    expect(replies.find((reply) => reply.id === 2)?.result.tools).toEqual([{ name: "profile.whoami" }]);
    expect(replies.find((reply) => reply.id === 3)?.result.content).toEqual([{ type: "text", text: "synthetic-profile" }]);
    expect(replies.find((reply) => reply.id === 4)?.result.isError).toBe(true);
    expect(connected.stdout).not.toContain("unexpected_write_reached_server");
    for (const secret of [key, token]) expect(connected.stdout + connected.stderr).not.toContain(secret);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}, 20_000);
