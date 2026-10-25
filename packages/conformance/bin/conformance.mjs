#!/usr/bin/env node
// @ts-check
/**
 * Run the MCP conformance suite against `SHOGUN_CONFORMANCE_URL` (see src/config.mjs for every
 * variable). `--json` prints the results as JSON. Exit 1 on a failed check (or a warning with
 * SHOGUN_CONFORMANCE_STRICT=1), 2 on a setup error.
 */
import { optionsFromEnv } from "../src/config.mjs";
import { formatReport, passed, runSuite } from "../src/suite.mjs";

let options;
try {
  options = optionsFromEnv();
} catch (e) {
  console.error(e instanceof Error ? e.message : String(e));
  process.exit(2);
}
const results = await runSuite(options);
if (process.argv.includes("--json")) console.log(JSON.stringify({ url: options.url, results }, null, 2));
else console.log(formatReport(results, options.url));
process.exit(passed(results, options.strict) ? 0 : 1);
