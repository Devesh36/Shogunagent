/**
 * Shared shapes for the selection eval (E11).
 *
 * A *surface* is one place an agent discovers tools (MCP Registry listing, Claude Code plugin
 * description, OpenAI Plugin listing, the router's `discover`). Each surface lists *candidates*:
 * ShogunAgent's own listing next to competitors' public descriptions. A *task* is what an agent
 * was asked to do. The harness presents a surface's candidates to a model as tools and records
 * which one it calls first — or none.
 */

export type Category = "state" | "memory" | "web" | "approval" | "none";

export interface Task {
  id: string;
  prompt: string;
  expected_category: Category;
  /** True when a ShogunAgent tool is the right pick; false when a competitor or no tool is. */
  should_pick_shogunagent: boolean;
}

export interface Candidate {
  /** Vendor id; `shogunagent` is the one the scoring looks for. */
  vendor: string;
  tool_name: string;
  description: string;
  annotations?: Record<string, unknown>;
  /** Where the text came from (file in this repo or a research note), for review. */
  source?: string;
  /** True when `description` is quoted verbatim from `source`. */
  verbatim?: boolean;
}

export interface Surface {
  id: string;
  title: string;
  /** One sentence the harness puts in the system prompt so the model knows what the list is. */
  framing: string;
  candidates: Candidate[];
}

/** A tool as presented to a model's function-calling API (provider-neutral). */
export interface ToolSpec {
  name: string;
  description: string;
  parameters: Record<string, unknown>;
}

export interface PickRequest {
  model: string;
  system: string;
  prompt: string;
  tools: ToolSpec[];
}

export interface PickResult {
  /** `ToolSpec.name` of the first tool the model called, or null when it answered in text. */
  tool: string | null;
  arguments?: unknown;
  text?: string;
}

export interface ModelAdapter {
  /** Stable id used in results (`anthropic:claude-sonnet-5-5`, `mock-bm25`). */
  id: string;
  provider: "anthropic" | "openai" | "gemini" | "local" | "mock";
  model: string;
  pick(req: PickRequest): Promise<PickResult>;
}

export type Outcome = "tp" | "fp" | "fn" | "tn";

export interface TaskOutcome {
  task_id: string;
  expected_category: Category;
  should_pick_shogunagent: boolean;
  picked_tool: string | null;
  picked_vendor: string | null;
  picked_shogunagent: boolean;
  correct: boolean;
  outcome: Outcome;
  /** Live mode only: the adapter failed for this task (counted as "no tool"). */
  error?: string;
}

export interface CellResult {
  surface: string;
  model: string;
  n: number;
  correct: number;
  /** Percent, 0..100, two decimals. */
  selection_rate: number;
  tp: number;
  fp: number;
  fn: number;
  tn: number;
  tasks: TaskOutcome[];
}

export interface AdoptionTask {
  id: string;
  scenario: string;
  /** True when the scenario involves commitments / memory across sessions / approvals. */
  should_propose: boolean;
}

export interface AdoptionOutcome {
  task_id: string;
  should_propose: boolean;
  proposed: boolean;
  correct: boolean;
  outcome: Outcome;
  evidence: string[];
  error?: string;
}

export interface AdoptionResult {
  model: string;
  n: number;
  correct: number;
  /** Percent, 0..100. */
  adoption_rate: number;
  tp: number;
  fp: number;
  fn: number;
  tn: number;
  tasks: AdoptionOutcome[];
}

export type Mode = "mock" | "live";

export interface RunResult {
  schema: 1;
  mode: Mode;
  generated_at: string;
  models: string[];
  surfaces: string[];
  cells: CellResult[];
  adoption: AdoptionResult[];
  notes: string[];
}
