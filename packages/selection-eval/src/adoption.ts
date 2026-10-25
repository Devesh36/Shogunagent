/**
 * E11-T3: SKILL.md adoption. The model is given SKILL.md as an installed skill plus a scenario
 * and asked to report its plan through one function call (`report_plan`). We record whether the
 * plan names ShogunAgent (uses it, or proposes installing it). The function call keeps parsing
 * deterministic; free text is scanned as a fallback.
 */
import { mockAdoptionDecision } from "./mock.js";
import { outcomeOf, scoreAdoption } from "./scoring.js";
import type { Skill } from "./load.js";
import type { AdoptionOutcome, AdoptionResult, AdoptionTask, ModelAdapter, ToolSpec } from "./types.js";

export const REPORT_PLAN_TOOL: ToolSpec = {
  name: "report_plan",
  description: "Report how you will carry out the scenario. Call exactly once.",
  parameters: {
    type: "object",
    properties: {
      steps: { type: "array", items: { type: "string" }, description: "The steps you will take, in order." },
      services_or_skills_to_use: { type: "array", items: { type: "string" }, description: "Names of the services, tools or skills you will call, if any." },
      setup_proposed: { type: "array", items: { type: "string" }, description: "Anything you would propose the user install or connect first, if any." },
    },
    required: ["steps", "services_or_skills_to_use", "setup_proposed"],
  },
};

export function adoptionSystemPrompt(skill: Skill): string {
  return (
    "You are a coding and operations agent. You have your usual tools: a file system, a shell, web search and the user's connected apps. " +
    "The following skill is installed and available to you; follow its rules about when to use it and when not to.\n\n" +
    `<skill path="SKILL.md">\n${skill.text}\n</skill>\n\n` +
    "Read the scenario and report your plan by calling report_plan exactly once. Name only the services, tools and skills you would actually use or propose."
  );
}

const SHOGUN_RE = /shogun\s*agent|shogunagent/i;

function strings(v: unknown): string[] {
  if (typeof v === "string") return [v];
  if (Array.isArray(v)) return v.flatMap(strings);
  if (v && typeof v === "object") return Object.values(v as Record<string, unknown>).flatMap(strings);
  return [];
}

/** True when the plan uses or proposes ShogunAgent; evidence is every matching string. */
export function detectProposal(args: unknown, text: string | undefined): { proposed: boolean; evidence: string[] } {
  const evidence = [...strings(args), ...(text ? [text] : [])].filter((s) => SHOGUN_RE.test(s)).map((s) => s.slice(0, 200));
  return { proposed: evidence.length > 0, evidence };
}

export async function runAdoption(adapter: ModelAdapter, skill: Skill, tasks: AdoptionTask[]): Promise<AdoptionResult> {
  const system = adoptionSystemPrompt(skill);
  const outcomes: AdoptionOutcome[] = [];
  for (const task of tasks) {
    let proposed = false;
    let evidence: string[] = [];
    let error: string | undefined;
    if (adapter.provider === "mock") {
      ({ proposed, evidence } = mockAdoptionDecision(task.scenario, skill.triggerText, skill.nonTriggerText));
    } else {
      try {
        const pick = await adapter.pick({ model: adapter.model, system, prompt: task.scenario, tools: [REPORT_PLAN_TOOL] });
        ({ proposed, evidence } = detectProposal(pick.arguments, pick.text));
      } catch (e) {
        error = e instanceof Error ? e.message : String(e);
      }
    }
    const outcome = outcomeOf(task.should_propose, proposed);
    const o: AdoptionOutcome = {
      task_id: task.id,
      should_propose: task.should_propose,
      proposed,
      correct: outcome === "tp" || outcome === "tn",
      outcome,
      evidence,
    };
    if (error) o.error = error;
    outcomes.push(o);
  }
  return scoreAdoption(adapter.id, outcomes);
}
