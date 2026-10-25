/**
 * One dispatcher for the three verbs, used by the REST face (src/app.ts) and the MCP face
 * (src/mcp.ts) so both validate, authorize and answer identically (invariant 4).
 */
import type { Json } from "../connectors/types.js";
import { discover, type PersonalContext } from "./discover.js";
import { inspect } from "./inspect.js";
import { errorBody } from "./responses.js";
import { run, type RunDeps } from "./run.js";
import type { Secret } from "./secret.js";
import { parseVerbInput, type VerbName } from "./verbs.js";

export type RouterDeps = RunDeps;

export interface VerbResult {
  status: number;
  body: Json;
}

export const VERB_DESCRIPTIONS: Record<VerbName, string> = {
  "router.discover":
    "Find tools for a task. Ranks the whole catalog (first-party and third-party alike) by task fit, price, health, latency and (with your agent key) your own past outcomes with each tool, with a published formula and returns the score breakdown, price, p50/p95 and permission level per candidate. Free, no key needed. Not for running a tool: use router.run.",
  "router.inspect":
    "Describe one tool: input/output JSON Schema, price and usage model, live health, what L1/L2/L3 means for it, examples and cheaper alternatives. Free, no key needed. Call before the first run of a tool you have not used.",
  "router.run":
    "Run a tool from the catalog. Validates input, applies the permission tier (L1 runs; L2 asks you to confirm once; L3 returns a URL a human must approve), reserves the quoted cost, executes, settles the measured usage (failures and empty results cost 0) and returns result, usage, latency_ms, hints and next_action. Requires idempotency_key; pass dryRun: true for the quote alone. Metered tools need an agent key as the bearer.",
};

/**
 * The caller's own outcome history for personalised discover (E20). Only a valid agent key has
 * one; anonymous callers and invalid keys get the unpersonalised ranking (discover stays free and
 * keyless). The profile is fetched for this one principal and used for this one response.
 */
async function personalContext(deps: RouterDeps, bearer: Secret | undefined): Promise<PersonalContext | undefined> {
  if (!deps.personal || !bearer) return undefined;
  const identity = await deps.metering.identify(bearer);
  if ("error" in identity || identity.anonymous) return undefined;
  return { profile: await deps.personal.profile(identity.actor) };
}

export async function callVerb(deps: RouterDeps, name: VerbName, rawArgs: unknown, bearer?: Secret): Promise<VerbResult> {
  const start = deps.now?.() ?? Date.now();
  const parsed = parseVerbInput(name, rawArgs);
  if (!parsed.ok) {
    return { status: 400, body: errorBody("invalid_request", `arguments of ${name} are invalid`, "fix_input", (deps.now?.() ?? Date.now()) - start, { details: parsed.errors }) as unknown as Json };
  }
  switch (name) {
    case "router.discover": {
      const out = discover(deps.catalog, deps.health, parsed.value as Parameters<typeof discover>[2], deps.now, await personalContext(deps, bearer));
      return { status: 200, body: out as unknown as Json };
    }
    case "router.inspect": {
      const out = inspect(deps.catalog, deps.health, (parsed.value as { tool_id: string }).tool_id, deps.now);
      return { status: out.status, body: out.body as unknown as Json };
    }
    case "router.run": {
      const out = await run(deps, { input: parsed.value as Parameters<typeof run>[1]["input"], ...(bearer ? { bearer } : {}) });
      return { status: out.status, body: out.body as unknown as Json };
    }
  }
}
