/**
 * The one identity every agent-readable surface repeats (README §11 principle 3): the same name,
 * version and description appear in the MCP Registry `server.json`, the server card, the A2A
 * agent card, the Claude Code plugin manifest and `llms.txt`. Tests pin the static files at the
 * repository root (`server.json`, `.claude-plugin/*.json`, `smithery.yaml`) to these values.
 */

/** Reverse-DNS name in the MCP Registry (E10-T2). */
export const REGISTRY_NAME = "io.github.torutesu/shogunagent";
export const DISPLAY_NAME = "ShogunAgent";
/** Matches SKILL.md `version` and the `shogunagent` npm package: one minor, one skill. */
export const VERSION = "0.1.0";
/** ≤ 100 characters: the registry's limit on `description`. */
export const SHORT_DESCRIPTION = "The state layer for AI agents: people, projects, commitments and open loops, with provenance.";
export const LONG_DESCRIPTION =
  "The state layer for AI agents. Read what is true about the user's work right now (people, projects, commitments, open loops) with a quote and a confidence score on every fact; write what you learned so the next agent starts from it; run tools through a router that enforces permission tiers. Not a transcript store and not a general web-search tool.";
export const PROTOCOL_VERSION = "2026-07-28";
export const REPOSITORY_URL = "https://github.com/Torutesu/Shogunagent";
export const WEBSITE_URL = "https://shogunagent.dev";
export const DOCS_URL = "https://shogunagent.dev/docs";
export const SKILL_URL = "https://shogunagent.dev/SKILL.md";
export const HOSTED_MCP_URL = "https://api.shogunagent.dev/mcp";
export const NPM_PACKAGE = "shogunagent";
export const PROVIDER = { organization: "Select KK", url: WEBSITE_URL } as const;

/** What a surface needs to know about the deployment that serves it. */
export interface WellknownContext {
  /** The public origin of this gateway (`PUBLIC_BASE_URL`), e.g. `https://api.shogunagent.dev`. */
  publicBaseUrl: string;
  /** The Streamable HTTP endpoint agents connect to. Defaults to `<publicBaseUrl>/mcp`. */
  mcpUrl?: string;
}

export function origin(ctx: WellknownContext): string {
  return ctx.publicBaseUrl.replace(/\/+$/, "");
}

export function mcpUrl(ctx: WellknownContext): string {
  return ctx.mcpUrl ?? `${origin(ctx)}/mcp`;
}

/** Every agent-readable URL this gateway serves, absolute. */
export function wellknownLinks(ctx: WellknownContext): Record<string, string> {
  const o = origin(ctx);
  return {
    mcp: mcpUrl(ctx),
    server_card: `${o}/.well-known/mcp/server-card.json`,
    agent_card: `${o}/.well-known/agent-card.json`,
    pricing: `${o}/.well-known/pricing.json`,
    status: `${o}/.well-known/status.json`,
    mcp_config: `${o}/.well-known/mcp-config`,
    health: `${o}/health`,
    openapi: `${o}/openapi.json`,
    llms: `${o}/llms.txt`,
    llms_full: `${o}/llms-full.txt`,
    register: `${o}/v1/agents`,
    skill: SKILL_URL,
    docs: DOCS_URL,
    repository: REPOSITORY_URL,
    registry: `https://registry.modelcontextprotocol.io/v0/servers/${encodeURIComponent(REGISTRY_NAME)}`,
  };
}
