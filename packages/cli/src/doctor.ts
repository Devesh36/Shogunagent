/** Offline prerequisites for the single-store CLI. Never opens the database or starts a process. */
import { isAbsolute } from "node:path";
import { locateBinaries, releaseTarget } from "./binaries.js";
import { dataPaths } from "./paths.js";
import { DB_KEY_ENV, TOKEN_ENV } from "./sidecar.js";
import type { Deps } from "./types.js";

type Status = "pass" | "warn" | "fail";
export interface DoctorCheck { id: string; status: Status; message: string }
export interface DoctorReport {
  ok: boolean;
  checks: DoctorCheck[];
  limitations: string[];
}

export async function doctor(deps: Deps): Promise<DoctorReport> {
  const paths = dataPaths(deps.platform);
  const checks: DoctorCheck[] = [];
  const add = (id: string, status: Status, message: string): void => { checks.push({ id, status, message }); };
  // Do not include caught exception messages: filesystem/JSON errors can contain secrets.
  const check = async (id: string, work: () => Promise<void>, failure: string): Promise<void> => {
    try { await work(); } catch { add(id, "fail", failure); }
  };
  const absoluteDataDir = isAbsolute(paths.root);
  add("data_dir", absoluteDataDir ? "pass" : "fail", absoluteDataDir
    ? "Data directory is absolute."
    : "Set SHOGUNAGENT_DATA_DIR to an absolute directory. serve changes its working directory, so relative store paths resolve incorrectly.");
  const version = deps.platform.nodeVersion ?? process.versions.node;
  const [major = 0, minor = 0] = version.split(".").map(Number);
  add("node", major > 22 || (major === 22 && minor >= 13) ? "pass" : "fail", "Node.js 22.13.0 or newer is required; use a supported Node.js runtime.");
  const target = releaseTarget(deps.platform);
  add("release_target", target.ok ? "pass" : "warn", target.ok
    ? `Release target: ${target.target}. This does not check whether release assets have been published.`
    : "No release download for this platform. Use trusted source-built binaries; see packages/cli/README.md. On macOS, an existing ShogunAI app connection is another option.");

  await check("binaries", async () => {
    const located = await locateBinaries(deps, paths);
    const files = located && !(await deps.fs.isDirectory(located.api)) && !(await deps.fs.isDirectory(located.mcp));
    add("binaries", files ? "pass" : "fail", files
      ? `shogun-api and shogun-mcp paths found (${located.source}).`
      : "Build shogun-api and shogun-mcp from source and set SHOGUNAGENT_BIN_DIR to their absolute directory; see packages/cli/README.md. Published release assets, when available, can also be installed explicitly.");
  }, "Cannot inspect binaries. Check SHOGUNAGENT_BIN_DIR is absolute and the configured binary files are accessible.");
  await check("store", async () => {
    const exists = await deps.fs.exists(paths.db) && !(await deps.fs.isDirectory(paths.db));
    add("store", exists ? "pass" : "fail", exists
      ? "memory.db path exists in the selected data directory; its contents have not been opened."
      : "Select an existing encrypted memory.db with SHOGUNAGENT_DATA_DIR. Do not create an empty file. For a fresh evaluation, see deploy/README.md section 3.2 for the separate source-built --multi mode.");
  }, "Cannot inspect the store path. Check access to SHOGUNAGENT_DATA_DIR and its memory.db.");

  for (const [id, name, path] of [["db_key", DB_KEY_ENV, paths.dbKey], ["api_token", TOKEN_ENV, paths.token]] as const) {
    await check(id, async () => {
      const env = deps.platform.env[name]?.trim();
      const value = env || (await deps.fs.exists(path) ? (await deps.fs.readFile(path)).trim() : "");
      if (id === "db_key") {
        add(id, /^[0-9a-fA-F]{64}$/.test(value) ? "pass" : "fail", /^[0-9a-fA-F]{64}$/.test(value)
          ? `64-hex-character store key available from ${env ? "environment" : "db.key"}; matching the store is not verified.`
          : "Provide the existing store's matching 64-hex-character key via SHOGUN_DB_KEY or protected db.key. Generating a new key cannot unlock an existing store.");
      } else {
        add(id, value ? "pass" : "warn", value
          ? `API token available from ${env ? "environment" : "api.token"}; its value is not displayed.`
          : "No API token configured. serve will generate api.token; doctor leaves it absent.");
      }
    }, `Cannot read ${id === "db_key" ? "db.key" : "api.token"}. Check file access or supply ${name} through the secure environment.`);
  }
  await check("memory_api", async () => {
    if (!(await deps.fs.exists(paths.memoryApiSettings))) {
      add("memory_api", "warn", "memory_api.json is absent. serve will create it with enabled: true; doctor leaves it absent.");
      return;
    }
    const settings: unknown = JSON.parse(await deps.fs.readFile(paths.memoryApiSettings));
    const enabled = typeof settings === "object" && settings !== null && "enabled" in settings && settings.enabled === true;
    const extraFields = enabled && Object.keys(settings).some((key) => key !== "enabled");
    add("memory_api", enabled ? (extraFields ? "warn" : "pass") : "fail", enabled
      ? (extraFields ? "memory_api.json has enabled: true, but additional settings fields are not validated. The sidecar must validate the complete settings." : "memory_api.json has enabled: true.")
      : "Memory API access is disabled or enabled is not true. Review memory_api.json with the store owner; serve preserves existing settings.");
  }, "Cannot read or parse memory_api.json. Ask the store owner to check its access and JSON syntax; contents are not displayed.");
  return {
    ok: !checks.some((c) => c.status === "fail"), checks,
    limitations: [
      "Store decryption, schema compatibility, complete settings shape, settings seals and plan access are not verified. The sidecar remains authoritative.",
      "Binary executability, database file validity, directory writability and server liveness are not verified.",
      "No network requests, processes, downloads, credential generation or file writes were performed.",
    ],
  };
}
