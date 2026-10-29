/**
 * `shogunagent help [--json]`: the help an agent reads (machine-readable) and the one a human
 * reads. The JSON names the commands, the hosted endpoints and the skill version so an agent can
 * check the version lock SKILL.md asks for ("if `npx shogunagent help --json` reports a different
 * minor, re-read the skill").
 */
import { READ_ONLY_TOOLS } from "./readonly-proxy.js";

/** One minor, one skill: equals SKILL.md `version` and package.json `version` (tests pin both). */
export const VERSION = "0.1.0";
export const SKILL_VERSION = VERSION;
export const SKILL_MINOR = VERSION.split(".").slice(0, 2).join(".");

export const HOSTED = {
  mcp: "https://api.shogunagent.dev/mcp",
  register: "https://api.shogunagent.dev/v1/agents",
  server_card: "https://api.shogunagent.dev/.well-known/mcp/server-card.json",
  agent_card: "https://api.shogunagent.dev/.well-known/agent-card.json",
  pricing: "https://api.shogunagent.dev/.well-known/pricing.json",
  status: "https://api.shogunagent.dev/.well-known/status.json",
  openapi: "https://api.shogunagent.dev/openapi.json",
  llms: "https://api.shogunagent.dev/llms.txt",
  skill: "https://shogunagent.dev/SKILL.md",
} as const;

export interface CommandHelp {
  name: string;
  usage: string;
  summary: string;
  flags: Array<{ flag: string; meaning: string }>;
  writes: string;
  network: string;
}

export const COMMANDS: readonly CommandHelp[] = [
  {
    name: "serve", usage: "shogunagent [serve] [--connect] [--install] [--port N] [--yes]",
    summary: "Start the local sidecar (shogun-api on 127.0.0.1) with a generated token and the store under the OS data directory. The default command.",
    flags: [
      { flag: "--connect", meaning: "Also write the read-only MCP entry into the agent apps found here (same consent rule as `connect`)." },
      { flag: "--install", meaning: "Download the release binaries for this platform first (sha256-verified). Never happens without this flag." },
      { flag: "--port N", meaning: "Listen port (default 7464)." },
      { flag: "--yes", meaning: "Answer yes to the connect question (non-interactive use)." },
    ],
    writes: "data dir: memory.db, api.token (0600), db.key (0600), memory_api.json; with --connect: the agent config files",
    network: "none, unless --install (GitHub release assets)",
  },
  {
    name: "install", usage: "shogunagent install [--version vX.Y.Z]",
    summary: "Download shogun-api / shogun-mcp / shogun from the GitHub release for this platform, verify the .sha256, extract under the data directory and select it.",
    flags: [{ flag: "--version TAG", meaning: "A release tag; default: the latest release." }],
    writes: "data dir: bin/<tag>/, bin/current",
    network: "api.github.com (latest tag), github.com release assets",
  },
  {
    name: "connect", usage: "shogunagent connect [--local] [--yes] [--client claude-code|cursor|codex|claude-desktop]",
    summary: "Write a read-only stdio MCP entry into each agent app found here. Default: npx -y shogunagent@0.1 mcp; --local uses this built CLI and Node directly. Shows the plan; writes only after an interactive yes or --yes. Idempotent; other entries are left byte for byte.",
    flags: [{ flag: "--local", meaning: "Use the absolute running Node and local CLI paths, with the selected data/binary directories. Requires a built CLI and existing binaries; copies no credentials. connect only." }, { flag: "--yes", meaning: "Skip the question." }, { flag: "--client ID", meaning: "Only this client (repeatable)." }],
    writes: "~/.claude.json, ~/.cursor/mcp.json, ~/.codex/config.toml, Claude Desktop's claude_desktop_config.json (each backed up to <file>.bak-shogunagent first)",
    network: "none",
  },
  {
    name: "disconnect", usage: "shogunagent disconnect [--yes] [--client ID]",
    summary: "Remove the shogunagent entry from the agent apps' config files. Same consent rule as connect.",
    flags: [{ flag: "--yes", meaning: "Skip the question." }, { flag: "--client ID", meaning: "Only this client (repeatable)." }],
    writes: "the same files as connect",
    network: "none",
  },
  {
    name: "mcp", usage: "shogunagent mcp",
    summary: "The stdio MCP face agent apps spawn: runs shogun-mcp on the local store behind a read-only filter (tools/call outside the read set is refused with next_action not_allowed; tools/list is filtered). Not for humans.",
    flags: [],
    writes: "none",
    network: "none",
  },
  {
    name: "doctor", usage: "shogunagent doctor [--json]",
    summary: "Check local single-store prerequisites without starting anything: runtime, binary paths, store/key presence and API settings. Does not verify decryption, schema or server health. Exit 1 means a prerequisite failed.",
    flags: [{ flag: "--json", meaning: "Machine-readable checks, warnings and limitations." }],
    writes: "none",
    network: "none",
  },
  {
    name: "status", usage: "shogunagent status [--json]",
    summary: "Whether binaries are installed, whether the sidecar is listening, which agent apps are connected.",
    flags: [{ flag: "--json", meaning: "Machine-readable." }],
    writes: "none",
    network: "none",
  },
  {
    name: "help", usage: "shogunagent help [--json]",
    summary: "This help. --json is for agents.",
    flags: [{ flag: "--json", meaning: "Machine-readable: commands, endpoints, skill version." }],
    writes: "none",
    network: "none",
  },
];

export function helpJson(): Record<string, unknown> {
  return {
    name: "shogunagent",
    version: VERSION,
    skill_version: SKILL_VERSION,
    skill_minor: SKILL_MINOR,
    skill_url: HOSTED.skill,
    version_lock: "The skill minor must equal this CLI's minor; if SKILL.md says otherwise, re-read it from skill_url before continuing.",
    commands: COMMANDS,
    local: {
      transport: "stdio",
      entry: { command: "npx", args: ["-y", `shogunagent@${SKILL_MINOR}`, "mcp"] },
      scope: "read",
      read_only_tools: [...READ_ONLY_TOOLS],
      sidecar: { binary: "shogun-api", default_port: 7464, host: "127.0.0.1" },
    },
    hosted: HOSTED,
    env: {
      SHOGUNAGENT_DATA_DIR: "Where the store, token and binaries live (default: the OS data directory).",
      SHOGUNAGENT_BIN_DIR: "An absolute directory holding shogun-api and shogun-mcp, instead of an installed release.",
      SHOGUN_API_TOKEN: "The sidecar's client token, instead of the 0600 file.",
      SHOGUN_DB_KEY: "The store key (64 hex), instead of the 0600 file.",
    },
    telemetry: "none by default: the server sends nothing unless analytics.json in the data directory says opt_out: false.",
  };
}

export function helpText(): string {
  const lines = [`shogunagent ${VERSION} — the state layer for AI agents (local sidecar + agent-app connector)`, ""];
  for (const c of COMMANDS) {
    lines.push(`  ${c.usage}`);
    lines.push(`      ${c.summary}`);
    for (const f of c.flags) lines.push(`        ${f.flag.padEnd(16)} ${f.meaning}`);
    lines.push("");
  }
  lines.push(`Hosted (no install): ${HOSTED.mcp} — register at ${HOSTED.register}; prices at ${HOSTED.pricing}.`);
  lines.push(`Skill: ${HOSTED.skill} (version ${SKILL_VERSION}). Agents: \`shogunagent help --json\`.`);
  return lines.join("\n") + "\n";
}
