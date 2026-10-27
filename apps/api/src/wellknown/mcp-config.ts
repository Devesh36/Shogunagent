/**
 * `/.well-known/mcp-config` (E10-T9, Smithery): the JSON Schema of the configuration a client
 * needs to connect to the hosted server. Smithery reads this document for remote servers listed
 * from a URL (research note: "How-to for listing in registries (official registry, Smithery,
 * Glama, PulseMCP)"). The exact vocabulary Smithery expects beyond a JSON Schema object is not
 * published in the note, so the document is a plain draft-07 schema with one required property
 * and the connection facts under an `x-mcp` extension; fields are kept to that minimum.
 */
import { DISPLAY_NAME, SHORT_DESCRIPTION, mcpUrl, origin, type WellknownContext } from "./identity.js";

export function mcpConfigDocument(ctx: WellknownContext): Record<string, unknown> {
  return {
    $schema: "http://json-schema.org/draft-07/schema#",
    $id: `${origin(ctx)}/.well-known/mcp-config`,
    title: `${DISPLAY_NAME} MCP configuration`,
    description: SHORT_DESCRIPTION,
    type: "object",
    properties: {
      apiKey: {
        type: "string",
        title: "Agent key",
        description: `Required for this key-based connection. An sa_live_ key from POST ${origin(ctx)}/v1/agents (self-registration, no email). Hosted /mcp requires authentication, including for free reads.`,
        pattern: "^sa_(live|test)_[A-Za-z0-9_-]+$",
      },
    },
    required: ["apiKey"],
    additionalProperties: false,
    "x-mcp": {
      transport: "streamable-http",
      url: mcpUrl(ctx),
      protocolVersion: "2026-07-28",
      authorization: { header: "Authorization", format: "Bearer {apiKey}", optional: false },
      requiredHeaders: ["Mcp-Method", "Mcp-Name"],
    },
  };
}
