/**
 * A thin MCP face (`POST /mcp`): JSON-RPC 2.0 over a single JSON response, exposing the three
 * verbs as tools `router.discover`, `router.inspect`, `router.run`. Every response carries
 * `Mcp-Method` and, for a tool call, `Mcp-Name`, which is what apps/api meters on without reading
 * bodies (invariant 10). Full Streamable HTTP conformance (sessions, SSE, MRTR round-trips) is
 * E4's job; this stub keeps the shape so the tool list and results are already the final ones.
 */
import { z } from "zod";
import type { Json, JsonObject } from "../connectors/types.js";
import { callVerb, VERB_DESCRIPTIONS, type RouterDeps } from "./engine.js";
import type { Secret } from "./secret.js";
import { VERB_SCHEMAS, type VerbName } from "./verbs.js";

export const MCP_PROTOCOL_VERSION = "2025-06-18";
export const MCP_SERVER_INFO = { name: "shogunagent-router", version: "0.0.0" } as const;

export interface McpTool {
  name: VerbName;
  description: string;
  inputSchema: JsonObject;
  annotations: { readOnlyHint: boolean; destructiveHint: boolean; idempotentHint: boolean; openWorldHint: boolean };
}

export function mcpTools(): McpTool[] {
  const schema = (name: VerbName): JsonObject => {
    const s = z.toJSONSchema(VERB_SCHEMAS[name], { unrepresentable: "any" }) as JsonObject;
    delete s["$schema"];
    return s;
  };
  return [
    { name: "router.discover", description: VERB_DESCRIPTIONS["router.discover"], inputSchema: schema("router.discover"), annotations: { readOnlyHint: true, destructiveHint: false, idempotentHint: true, openWorldHint: false } },
    { name: "router.inspect", description: VERB_DESCRIPTIONS["router.inspect"], inputSchema: schema("router.inspect"), annotations: { readOnlyHint: true, destructiveHint: false, idempotentHint: true, openWorldHint: false } },
    { name: "router.run", description: VERB_DESCRIPTIONS["router.run"], inputSchema: schema("router.run"), annotations: { readOnlyHint: false, destructiveHint: false, idempotentHint: true, openWorldHint: true } },
  ];
}

export interface McpReply {
  status: number;
  headers: Record<string, string>;
  body: Json;
}

const RpcRequest = z.object({
  jsonrpc: z.literal("2.0"),
  id: z.union([z.string(), z.number(), z.null()]).optional(),
  method: z.string(),
  params: z.record(z.string(), z.unknown()).optional(),
});

function rpcError(id: Json, code: number, message: string, method?: string): McpReply {
  return { status: 200, headers: { "Mcp-Method": method ?? "unknown" }, body: { jsonrpc: "2.0", id, error: { code, message } } };
}

export async function handleMcp(deps: RouterDeps, raw: unknown, bearer?: Secret): Promise<McpReply> {
  const parsed = RpcRequest.safeParse(raw);
  if (!parsed.success) return rpcError(null, -32600, "invalid JSON-RPC 2.0 request");
  const { id = null, method, params = {} } = parsed.data;
  const headers: Record<string, string> = { "Mcp-Method": method };
  const ok = (result: Json): McpReply => ({ status: 200, headers, body: { jsonrpc: "2.0", id, result } });

  switch (method) {
    case "initialize":
      return ok({ protocolVersion: MCP_PROTOCOL_VERSION, capabilities: { tools: { listChanged: false } }, serverInfo: { ...MCP_SERVER_INFO } });
    case "notifications/initialized":
    case "ping":
      return ok({});
    case "tools/list":
      return ok({ tools: mcpTools() as unknown as Json });
    case "tools/call": {
      const name = params["name"];
      if (typeof name !== "string" || !(name in VERB_SCHEMAS)) return rpcError(id, -32602, `unknown tool; one of ${Object.keys(VERB_SCHEMAS).join(", ")}`, method);
      headers["Mcp-Name"] = name;
      const out = await callVerb(deps, name as VerbName, params["arguments"] ?? {}, bearer);
      return ok({
        content: [{ type: "text", text: JSON.stringify(out.body) }],
        structuredContent: out.body,
        isError: out.status >= 400,
      });
    }
    default:
      return rpcError(id, -32601, `method not found: ${method}`, method);
  }
}
