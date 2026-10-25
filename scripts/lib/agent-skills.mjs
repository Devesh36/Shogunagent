// The A2A agent card's `skills` block, derived from SKILL.md (E10-T3). Shared by
// scripts/gen-agent-skills.mjs (writes apps/api/src/wellknown/agent-skills.generated.json) and
// the apps/api test that re-derives it from the current SKILL.md.
import { exclusions, parseFrontmatter, skillsFromWhenToUse } from "./skill-md.mjs";

export function renderAgentSkills(skillText) {
  const fm = parseFrontmatter(skillText);
  return {
    source: "SKILL.md",
    skill_name: fm.name,
    skill_version: fm.version,
    skills: skillsFromWhenToUse(skillText),
    exclusions: exclusions(skillText),
  };
}
