/**
 * `/.well-known/mcp/server-card.json` (E10-T1): the MCP Server Card — what an agent needs to
 * discover and assess this server without connecting (the MCP roadmap's "progressive discovery";
 * research note: the Server Card Working Group is defining the `.well-known` metadata).
 *
 * Rendered from the tool table (`tool-table.ts`) and the identity module, never written by hand
 * (invariant 12). The Working Group's schema is a draft at the time of writing, so the top-level
 * keys follow the MCP vocabulary that already exists (`serverInfo`, `capabilities`, `tools` with
 * `inputSchema` and `annotations`, a `transport` object) and stay minimal; `schemaVersion` names
 * the draft this file tracks so a client can tell when it changes.
 */
import { DISPLAY_NAME, LONG_DESCRIPTION, PROTOCOL_VERSION, REGISTRY_NAME, VERSION, mcpUrl, origin, wellknownLinks, type WellknownContext } from "./identity.js";
import { annotationsFor, type ToolTable } from "./tool-table.js";

export const SERVER_CARD_SCHEMA_VERSION = "mcp-server-card/draft-2026-10";

export function serverCard(table: ToolTable, ctx: WellknownContext): Record<string, unknown> {
  const links = wellknownLinks(ctx);
  return {
    schemaVersion: SERVER_CARD_SCHEMA_VERSION,
    protocolVersion: PROTOCOL_VERSION,
    serverInfo: { name: REGISTRY_NAME, title: DISPLAY_NAME, version: VERSION },
    description: LONG_DESCRIPTION,
    transport: { type: "streamable-http", url: mcpUrl(ctx), headers: ["Mcp-Method", "Mcp-Name", "Mcp-Quantity", "Mcp-Dry-Run"] },
    authentication: {
      // Hosted /mcp authenticates before forwarding any request, including free reads.
      // Public discovery documents and the separate router verbs do not make /mcp anonymous.
      anonymous: { allowed: false },
      schemes: [
        {
          type: "http", scheme: "bearer", bearerFormat: "sa_live_<key>",
          scopes: ["read", "write", "run"],
          registration: { url: `${origin(ctx)}/v1/agents`, method: "POST", description: "Self-registration: a proof-of-work challenge, no email; returns a scoped key and the free allowance." },
        },
        {
          // E3-T2: MCP clients connect over OAuth 2.1 with a Client ID Metadata Document as their
          // client_id; there is no dynamic registration endpoint.
          type: "oauth2",
          scopes: ["read", "write", "run"],
          protectedResourceMetadata: `${origin(ctx)}/.well-known/oauth-protected-resource/mcp`,
          authorizationServerMetadata: `${origin(ctx)}/.well-known/oauth-authorization-server`,
          clientRegistration: "client-id-metadata-document",
          pkce: ["S256"],
        },
      ],
    },
    capabilities: { tools: { listChanged: false } },
    toolset: table.toolset,
    tools: table.tools.map((t) => ({
      name: t.name,
      title: t.title,
      description: t.description,
      inputSchema: t.input_schema,
      ...(t.output_schema ? { outputSchema: t.output_schema } : {}),
      annotations: annotationsFor(t.level),
      // ShogunAgent extensions, namespaced so they cannot collide with the spec's keys.
      "x-shogunagent": { level: t.level, cost_unit: t.cost_unit, ...(t.deprecated ? { deprecated: t.deprecated } : {}) },
    })),
    toolCount: table.tools.length,
    toolTable: { source: table.source, schema: table.schema },
    pricing: links.pricing,
    status: links.status,
    openapi: links.openapi,
    agentCard: links.agent_card,
    documentation: links.docs,
    llms: links.llms,
    skill: links.skill,
    repository: links.repository,
    registry: links.registry,
  };
}
