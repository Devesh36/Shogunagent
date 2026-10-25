#!/usr/bin/env node
// E10-T3: derive the A2A agent card's `skills` from SKILL.md's "When to use this skill" list
// (invariant 12: the card is generated, SKILL.md is the source). Writes
// apps/api/src/wellknown/agent-skills.generated.json; `--check` fails when the committed file
// differs from what SKILL.md yields today (CI runs it; apps/api tests re-derive it too).
//
//   node scripts/gen-agent-skills.mjs --write
//   node scripts/gen-agent-skills.mjs --check
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { renderAgentSkills } from "./lib/agent-skills.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const skillPath = join(root, "SKILL.md");
const outPath = join(root, "apps/api/src/wellknown/agent-skills.generated.json");

const mode = process.argv[2];
if (mode !== "--write" && mode !== "--check") {
  console.error("usage: gen-agent-skills.mjs --write | --check");
  process.exit(2);
}
const next = JSON.stringify(renderAgentSkills(readFileSync(skillPath, "utf8")), null, 2) + "\n";
if (mode === "--write") {
  writeFileSync(outPath, next);
  console.log(`wrote ${outPath}`);
} else {
  let current = "";
  try { current = readFileSync(outPath, "utf8"); } catch { /* absent */ }
  if (current !== next) {
    console.error(`${outPath} is stale: run \`pnpm agent-skills:gen\` and commit the result.`);
    process.exit(1);
  }
  console.log("agent-skills.generated.json matches SKILL.md");
}
