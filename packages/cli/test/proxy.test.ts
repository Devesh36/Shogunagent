/** The read-only stdio filter behind `shogunagent mcp`: writes never reach the server, the tool
 * list is filtered, everything else is relayed unchanged; the read set equals the tool table's. */
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { PassThrough } from "node:stream";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

import { READ_ONLY_TOOLS, filterRequest, filterResponse, lineSplitter, runReadOnlyProxy, type Pending } from "../src/readonly-proxy.js";

const here = dirname(fileURLToPath(import.meta.url));

describe("filterRequest", () => {
  it("forwards read tools, protocol methods and notifications unchanged", () => {
    const pending: Pending = new Set();
    for (const line of [
      JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize", params: {} }),
      JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }),
      JSON.stringify({ jsonrpc: "2.0", id: 2, method: "tools/call", params: { name: "state.commitments.list", arguments: {} } }),
      JSON.stringify({ jsonrpc: "2.0", id: 3, method: "tools/call", params: { name: "profile.whoami" } }),
      "not json at all",
    ]) {
      expect(filterRequest(line, pending)).toEqual({ forward: line });
    }
    expect(pending.size).toBe(0);
  });

  it("answers a write locally with an MCP tool error and never forwards it", () => {
    const pending: Pending = new Set();
    for (const name of ["memory.ingest", "memory.append_note", "actions.execute", "router.run", "memory.delete_everything"]) {
      const out = filterRequest(JSON.stringify({ jsonrpc: "2.0", id: 7, method: "tools/call", params: { name, arguments: {} } }), pending);
      expect(out.forward).toBeUndefined();
      const reply = JSON.parse(out.reply ?? "") as { id: number; result: { isError: boolean; content: Array<{ text: string }>; structuredContent: { error: { code: string; next_action: string } } } };
      expect(reply.id).toBe(7);
      expect(reply.result.isError).toBe(true);
      expect(reply.result.content[0]?.text).toContain(name);
      expect(reply.result.structuredContent.error).toMatchObject({ code: "source_not_allowed", next_action: "not_allowed" });
    }
    // A call with no tool name, or with no id (nothing to answer to), is dropped.
    expect(filterRequest(JSON.stringify({ jsonrpc: "2.0", id: 8, method: "tools/call", params: {} }), pending).forward).toBeUndefined();
    expect(filterRequest(JSON.stringify({ jsonrpc: "2.0", method: "tools/call", params: { name: "memory.ingest" } }), pending)).toEqual({});
  });

  it("remembers tools/list ids so the answer can be filtered", () => {
    const pending: Pending = new Set();
    filterRequest(JSON.stringify({ jsonrpc: "2.0", id: "abc", method: "tools/list" }), pending);
    filterRequest(JSON.stringify({ jsonrpc: "2.0", id: 5, method: "tools/list" }), pending);
    expect(pending).toEqual(new Set(["string:abc", "number:5"]));
    const answer = JSON.stringify({ jsonrpc: "2.0", id: 5, result: { tools: [{ name: "memory.search" }, { name: "memory.ingest" }, { name: "actions.execute" }], nextCursor: null } });
    const filtered = JSON.parse(filterResponse(answer, pending)) as { result: { tools: Array<{ name: string }>; nextCursor: null } };
    expect(filtered.result.tools.map((t) => t.name)).toEqual(["memory.search"]);
    expect(filtered.result.nextCursor).toBeNull();
    expect(pending).toEqual(new Set(["string:abc"]));
    // Other responses pass through byte for byte, including ones with ids that are not pending.
    const other = JSON.stringify({ jsonrpc: "2.0", id: 9, result: { content: [] } });
    expect(filterResponse(other, pending)).toBe(other);
    expect(filterResponse("garbage", pending)).toBe("garbage");
  });

  it("the read set is the tool table's level: read rows (apps/api/fixtures/tool-table.json)", () => {
    const table = JSON.parse(readFileSync(join(here, "..", "..", "..", "apps", "api", "fixtures", "tool-table.json"), "utf8")) as { tools: Array<{ name: string; level: string }> };
    const reads = table.tools.filter((t) => t.level === "read").map((t) => t.name).sort();
    expect([...READ_ONLY_TOOLS].sort()).toEqual(reads);
    for (const t of table.tools.filter((t) => t.level !== "read")) expect(READ_ONLY_TOOLS.has(t.name), t.name).toBe(false);
  });
});

describe("runReadOnlyProxy over streams", () => {
  it("relays, refuses and filters on a live stream, and ends when the server does", async () => {
    const clientIn = new PassThrough();
    const clientOut = new PassThrough();
    const serverIn = new PassThrough();
    const serverOut = new PassThrough();
    const seenByServer: string[] = [];
    serverIn.on("data", lineSplitter((l) => seenByServer.push(l)));
    const seenByClient: string[] = [];
    clientOut.on("data", lineSplitter((l) => seenByClient.push(l)));
    const done = runReadOnlyProxy({ clientIn, clientOut, serverIn, serverOut });

    // Chunk boundaries do not matter.
    const list = JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list" });
    const write = JSON.stringify({ jsonrpc: "2.0", id: 2, method: "tools/call", params: { name: "memory.ingest" } });
    clientIn.write(list.slice(0, 10));
    clientIn.write(list.slice(10) + "\n" + write + "\n");
    await new Promise((r) => setImmediate(r));
    expect(seenByServer).toEqual([list]);
    expect(seenByClient).toHaveLength(1);
    expect((JSON.parse(seenByClient[0] ?? "") as { id: number }).id).toBe(2);

    serverOut.write(JSON.stringify({ jsonrpc: "2.0", id: 1, result: { tools: [{ name: "memory.recall" }, { name: "memory.ingest" }] } }) + "\n");
    serverOut.end();
    await done;
    const listed = JSON.parse(seenByClient[1] ?? "") as { result: { tools: Array<{ name: string }> } };
    expect(listed.result.tools.map((t) => t.name)).toEqual(["memory.recall"]);
  });
});
