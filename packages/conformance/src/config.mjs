// @ts-check
/**
 * Suite options from the environment, so the same suite runs against the local sidecar in CI and
 * against any deployed endpoint later:
 *
 * - `SHOGUN_CONFORMANCE_URL`    the MCP endpoint (`https://…/mcp`). Required.
 * - `SHOGUN_CONFORMANCE_TOKEN`  the bearer (an agent key, or the sidecar's client token).
 * - `SHOGUN_CONFORMANCE_WRITES` `1` to run the write, MRTR and approval checks (they ingest a test
 *                               turn, append a note and queue an L3 approval in that store).
 * - `SHOGUN_CONFORMANCE_APPROVAL_URL` the prefix L3 approval URLs must start with; unset means an
 *                               approval URL is not required (the server may not configure one).
 * - `SHOGUN_CONFORMANCE_L2_TOOL` / `SHOGUN_CONFORMANCE_L2_ARGS` (JSON) an L2 tool to probe for
 *                               MRTR `input_required` (only declines are sent: nothing runs).
 * - `SHOGUN_CONFORMANCE_TOOL_TABLE` the tool table (default `apps/api/fixtures/tool-table.json`).
 * - `SHOGUN_CONFORMANCE_STRICT` `1` to fail on warnings too.
 */
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");
export const DEFAULT_TOOL_TABLE = resolve(REPO_ROOT, "apps/api/fixtures/tool-table.json");

/** @param {string} [path] */
export function loadToolTable(path = DEFAULT_TOOL_TABLE) {
  /** @type {{ tools: Array<{ name: string; level: string }> }} */
  const table = JSON.parse(readFileSync(path, "utf8"));
  if (!Array.isArray(table.tools)) throw new Error(`${path} has no tools array`);
  return table;
}

/** @param {NodeJS.ProcessEnv} [env] */
export function optionsFromEnv(env = process.env) {
  const url = env.SHOGUN_CONFORMANCE_URL?.trim();
  if (!url) throw new Error("Set SHOGUN_CONFORMANCE_URL to the MCP endpoint (e.g. http://127.0.0.1:7464/mcp), or run `pnpm --filter @shogunagent/conformance local` for a throwaway sidecar.");
  const l2Args = env.SHOGUN_CONFORMANCE_L2_ARGS?.trim();
  return {
    url,
    token: env.SHOGUN_CONFORMANCE_TOKEN?.trim() || undefined,
    writes: env.SHOGUN_CONFORMANCE_WRITES === "1",
    approvalUrlPrefix: env.SHOGUN_CONFORMANCE_APPROVAL_URL?.trim() || undefined,
    l2Tool: env.SHOGUN_CONFORMANCE_L2_TOOL?.trim() || undefined,
    l2Args: l2Args ? JSON.parse(l2Args) : undefined,
    toolTable: loadToolTable(env.SHOGUN_CONFORMANCE_TOOL_TABLE?.trim() || DEFAULT_TOOL_TABLE),
    strict: env.SHOGUN_CONFORMANCE_STRICT === "1",
  };
}
