/**
 * Where things live: the data directory under the OS data directory (or
 * `SHOGUNAGENT_DATA_DIR`), the files inside it, and each AI app's MCP config file.
 */
import { join } from "node:path";

import type { Platform } from "./types.js";

export const DATA_DIR_ENV = "SHOGUNAGENT_DATA_DIR";
export const BIN_DIR_ENV = "SHOGUNAGENT_BIN_DIR";

/** The OS data directory for this user: XDG on Linux, Application Support on macOS, APPDATA on Windows. */
export function dataDir(p: Platform): string {
  const explicit = p.env[DATA_DIR_ENV]?.trim();
  if (explicit) return explicit;
  if (p.os === "darwin") return join(p.homedir, "Library", "Application Support", "shogunagent");
  if (p.os === "win32") return join(p.env.APPDATA?.trim() || join(p.homedir, "AppData", "Roaming"), "shogunagent");
  return join(p.env.XDG_DATA_HOME?.trim() || join(p.homedir, ".local", "share"), "shogunagent");
}

export interface DataPaths {
  root: string;
  /** The encrypted store the sidecar opens (`SHOGUN_DB_PATH`). */
  db: string;
  /** The client token the sidecar accepts (`SHOGUN_API_TOKEN`), mode 0600. */
  token: string;
  /** The store's key (`SHOGUN_DB_KEY`, hex), mode 0600. */
  dbKey: string;
  /** `memory_api.json` next to the store: the sidecar refuses to start without `enabled: true`. */
  memoryApiSettings: string;
  /** Where the sidecar writes the port it bound (`api.port` next to the store). */
  port: string;
  /** Installed release binaries: `<bin>/<tag>/shogun-api`, `shogun-mcp`, `shogun`. */
  bin: string;
  /** The tag of the release currently selected (`current` file under bin/). */
  binCurrent: string;
}

export function dataPaths(p: Platform): DataPaths {
  const root = dataDir(p);
  return {
    root,
    db: join(root, "memory.db"),
    token: join(root, "api.token"),
    dbKey: join(root, "db.key"),
    memoryApiSettings: join(root, "memory_api.json"),
    port: join(root, "api.port"),
    bin: join(root, "bin"),
    binCurrent: join(root, "bin", "current"),
  };
}

export type ClientId = "claude-code" | "cursor" | "codex" | "claude-desktop";

export const ALL_CLIENTS: readonly ClientId[] = ["claude-code", "cursor", "codex", "claude-desktop"];

export const CLIENT_LABEL: Readonly<Record<ClientId, string>> = {
  "claude-code": "Claude Code",
  cursor: "Cursor",
  codex: "Codex (CLI, ChatGPT desktop)",
  "claude-desktop": "Claude Desktop",
};

/** The documented MCP config file of each client. Claude Code's user config is `~/.claude.json`
 * (top-level `mcpServers`); Cursor `~/.cursor/mcp.json`; Codex CLI and the ChatGPT desktop app
 * share `~/.codex/config.toml`; Claude Desktop's file is per OS. */
export function clientConfigPath(client: ClientId, p: Platform): string {
  switch (client) {
    case "claude-code":
      return join(p.homedir, ".claude.json");
    case "cursor":
      return join(p.homedir, ".cursor", "mcp.json");
    case "codex":
      return join(p.homedir, ".codex", "config.toml");
    case "claude-desktop":
      if (p.os === "darwin") return join(p.homedir, "Library", "Application Support", "Claude", "claude_desktop_config.json");
      if (p.os === "win32") return join(p.env.APPDATA?.trim() || join(p.homedir, "AppData", "Roaming"), "Claude", "claude_desktop_config.json");
      return join(p.env.XDG_CONFIG_HOME?.trim() || join(p.homedir, ".config"), "Claude", "claude_desktop_config.json");
  }
}

/** What counts as "this client is installed": its config directory or file exists. Nothing is
 * ever written for a client that is not there. */
export function clientPresenceMarkers(client: ClientId, p: Platform): string[] {
  switch (client) {
    case "claude-code":
      return [join(p.homedir, ".claude.json"), join(p.homedir, ".claude")];
    case "cursor":
      return [join(p.homedir, ".cursor")];
    case "codex":
      return [join(p.homedir, ".codex")];
    case "claude-desktop": {
      const file = clientConfigPath(client, p);
      return [file, file.slice(0, file.lastIndexOf("claude_desktop_config.json") - 1)];
    }
  }
}
