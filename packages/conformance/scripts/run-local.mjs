#!/usr/bin/env node
// @ts-check
/**
 * Stand up a throwaway sidecar (scripts/sidecar.mjs: temp store, random keys, ephemeral loopback
 * port, approval URL configured) and run the whole suite against it, writes included. This is what
 * CI runs. `--build` builds the two binaries first; `--json` prints JSON; `--strict` fails on
 * warnings. The L2 probe runs only when SHOGUN_CONFORMANCE_L2_TOOL is set.
 */
import { loadToolTable } from "../src/config.mjs";
import { formatReport, passed, runSuite } from "../src/suite.mjs";
import { buildBinaries, startSidecar } from "./sidecar.mjs";

const args = new Set(process.argv.slice(2));
if (args.has("--build")) buildBinaries();
const sidecar = await startSidecar();
let ok = false;
try {
  const l2Args = process.env.SHOGUN_CONFORMANCE_L2_ARGS?.trim();
  const results = await runSuite({
    url: sidecar.url,
    token: sidecar.token,
    writes: true,
    approvalUrlPrefix: `${sidecar.approvalBase}/approve/`,
    l2Tool: process.env.SHOGUN_CONFORMANCE_L2_TOOL?.trim() || undefined,
    l2Args: l2Args ? JSON.parse(l2Args) : undefined,
    toolTable: loadToolTable(),
  });
  if (args.has("--json")) console.log(JSON.stringify({ url: sidecar.url, results }, null, 2));
  else console.log(formatReport(results, sidecar.url));
  ok = passed(results, args.has("--strict") || process.env.SHOGUN_CONFORMANCE_STRICT === "1");
} finally {
  await sidecar.stop();
}
process.exit(ok ? 0 : 1);
