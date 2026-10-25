/**
 * `run(tool_id, input, idempotency_key, dryRun?)` (E8-T5; implementation-plan §7.4). The engine:
 *
 *   lookup → scope (L1/L2/L3, deny unknown) → validate input → quote (dryRun stops here)
 *   → identify actor → idempotency replay → L2: input_required / L3: approval URL
 *   → reserve → execute through the transport → settle (failures and empty results cost 0)
 *   → map output → hints → run record → store for idempotency → respond.
 *
 * The engine never sees a credential: the bearer is a `Secret` it forwards opaquely to metering
 * and (for `caller_bearer` providers only) to the transport; provider keys never enter this file.
 * The logger receives tool ids, outcomes and numbers; the run record has no body field.
 */
import type { EndpointDefinition, Json, RunRequest } from "../connectors/types.js";
import type { CompiledCatalog } from "./catalog.js";
import { contentHash } from "./hash.js";
import type { HealthStore } from "./health.js";
import { betterFitAlternatives, cheaperAlternatives, type Hint } from "./hints.js";
import type { IdempotencyStore } from "./idempotency.js";
import type { Logger } from "./logging.js";
import type { MeteringClient } from "./metering.js";
import { ERROR_STATUS, errorBody, type ErrorBody, type NextAction } from "./responses.js";
import { clampTaskHint, type RunLogSink, type RunOutcome } from "./runlog.js";
import { authorize, type Level } from "./scope.js";
import type { Secret } from "./secret.js";
import { TransportError, type Transport } from "./transport.js";
import { settleUsage, type Usage, ZERO_USAGE } from "./usage.js";
import type { RunVerbInput } from "./verbs.js";

export interface RunDeps {
  catalog: CompiledCatalog;
  transport: Transport;
  metering: MeteringClient;
  runLog: RunLogSink;
  idempotency: IdempotencyStore;
  health: HealthStore;
  logger: Logger;
  /** Where L3 approvals are opened. The E8 skeleton only mints the URL; the approval queue is E4/E9. */
  approvalBaseUrl: string;
  now?: () => number;
  randomId?: () => string;
}

export interface RunCall {
  input: RunVerbInput;
  bearer?: Secret;
}

interface Common {
  tool_id: string;
  permission_level: Level;
  usage: Usage;
  latency_ms: number;
  hints: Hint[];
  next_action: NextAction;
}

export interface RunOkBody extends Common {
  resultType: "result";
  result: Json;
  run_id: string;
  replayed: boolean;
}

export interface DryRunBody extends Common {
  resultType: "quote";
  dryRun: true;
  quote: { estimate_micro_usd: number; unit: string; model: string; failures_cost: 0; empty_results_cost: 0 };
}

/** MCP "input required" shape (E4-T2 MRTR): the client answers with `inputResponses`. */
export interface InputRequiredBody extends Common {
  resultType: "input_required";
  inputRequests: { id: string; message: string; schema: Json }[];
  retry: { with: "inputResponses"; example: { confirm: true } };
}

export interface ApprovalRequiredBody extends Common {
  resultType: "approval_required";
  approval: { approval_id: string; url: string; mode: "url"; expires_at: number };
}

export type RunBody = RunOkBody | DryRunBody | InputRequiredBody | ApprovalRequiredBody | ErrorBody;

export interface RunResult {
  status: number;
  body: RunBody;
}

function defaultRequest(endpoint: EndpointDefinition, input: Json): RunRequest {
  const base: RunRequest = { method: endpoint.request.method, path: endpoint.request.path };
  if (endpoint.request.timeout_ms !== undefined) base.timeout_ms = endpoint.request.timeout_ms;
  if (endpoint.request.method === "GET") {
    const query: Record<string, string> = {};
    if (input && typeof input === "object" && !Array.isArray(input)) {
      for (const [k, v] of Object.entries(input)) if (v !== undefined && v !== null) query[k] = typeof v === "string" ? v : JSON.stringify(v);
    }
    return { ...base, query };
  }
  return { ...base, body: input };
}

function randomIdDefault(): string {
  return crypto.randomUUID();
}

export async function run(deps: RunDeps, call: RunCall): Promise<RunResult> {
  const now = deps.now ?? Date.now;
  const randomId = deps.randomId ?? randomIdDefault;
  const start = now();
  const { input: verb } = call;
  const toolId = verb.tool_id;
  const taskHint = clampTaskHint(verb.task_hint);
  const hintFollowed = verb.hint_followed === true;
  let actor = "anonymous";

  const finish = async (outcome: RunOutcome, cost: number, status: number, body: RunBody): Promise<RunResult> => {
    const latency = now() - start;
    body.latency_ms = latency;
    await deps.runLog.record({ ts: start, actor, tool_id: toolId, task_hint: taskHint, outcome, cost_micro_usd: cost, latency_ms: latency, hint_followed: hintFollowed });
    deps.logger.log({ kind: "run", tool_id: toolId, provider_id: deps.catalog.get(toolId)?.provider_id ?? "unknown", outcome, status, latency_ms: latency });
    return { status, body };
  };
  const fail = (code: ErrorBody["error"]["code"], message: string, next: NextAction, outcome: RunOutcome, extra?: Parameters<typeof errorBody>[4]) =>
    finish(outcome, 0, ERROR_STATUS[code], errorBody(code, message, next, now() - start, extra));

  // 1. lookup + scope. An operation not in the table is denied, never defaulted.
  const entry = deps.catalog.get(toolId);
  const auth = authorize(deps.catalog.scope, toolId);
  if (!entry || auth.kind !== "allowed") {
    return entry
      ? fail("operation_denied", `"${toolId}" is not in the permission table`, "discover", "denied")
      : fail("tool_not_found", `no tool "${toolId}" in the catalog; call discover with the task`, "discover", "denied");
  }
  const def = deps.catalog.lookup(toolId);
  if (!def) return fail("tool_not_found", `no definition behind "${toolId}"`, "discover", "denied");
  const level = auth.level;
  const hints = (): Hint[] => [...cheaperAlternatives(deps.catalog, entry), ...(taskHint ? betterFitAlternatives(deps.catalog, entry, taskHint) : [])];

  // 2. validate input.
  const validated = def.endpoint.input.schema.validate(verb.input);
  if (!validated.ok) {
    return fail("invalid_input", `input does not match the schema of "${toolId}"; call inspect for the schema`, "fix_input", "invalid_input", { details: validated.errors });
  }
  const input = validated.value;
  const quote = { tool_id: toolId, estimate_micro_usd: entry.quote_micro_usd, unit: entry.usage.unit };

  // 3. dryRun: the quote and the permission level, nothing else.
  if (verb.dryRun) {
    const body: DryRunBody = {
      resultType: "quote",
      dryRun: true,
      tool_id: toolId,
      permission_level: level,
      quote: { estimate_micro_usd: quote.estimate_micro_usd, unit: quote.unit, model: entry.usage.model, failures_cost: 0, empty_results_cost: 0 },
      usage: ZERO_USAGE,
      latency_ms: 0,
      hints: hints(),
      next_action: level === "L3" ? "approve_at_url" : level === "L2" ? "provide_input" : null,
    };
    return { status: 200, body: { ...body, latency_ms: now() - start } };
  }

  // 4. identity.
  const identity = await deps.metering.identify(call.bearer);
  if ("error" in identity) return fail("authentication_required", "the bearer is not a valid agent key", "authenticate", "authentication_required");
  actor = identity.actor;

  // 5. idempotency replay.
  const inputHash = contentHash(input);
  const stored = await deps.idempotency.get(actor, verb.idempotency_key);
  if (stored) {
    if (stored.tool_id !== toolId || stored.input_hash !== inputHash) {
      return fail("idempotency_conflict", "this idempotency_key was used with a different tool or input; generate a new key", "fix_input", "denied");
    }
    const body = { ...(stored.response as unknown as RunOkBody), replayed: true, usage: ZERO_USAGE };
    return finish("replayed", 0, 200, body);
  }

  // 6. permission tier. L2 asks for confirmation once; L3 always goes to a human URL.
  if (level === "L2" && verb.inputResponses?.["confirm"] !== true) {
    const body: InputRequiredBody = {
      resultType: "input_required",
      tool_id: toolId,
      permission_level: level,
      inputRequests: [
        {
          id: "confirm",
          message: `"${toolId}" writes the user's data (${entry.summary}). Confirm to proceed.`,
          schema: { type: "object", properties: { confirm: { type: "boolean", const: true } }, required: ["confirm"] },
        },
      ],
      retry: { with: "inputResponses", example: { confirm: true } },
      usage: ZERO_USAGE,
      latency_ms: 0,
      hints: [],
      next_action: "provide_input",
    };
    return finish("input_required", 0, 200, body);
  }
  if (level === "L3") {
    const approvalId = randomId();
    const body: ApprovalRequiredBody = {
      resultType: "approval_required",
      tool_id: toolId,
      permission_level: level,
      approval: { approval_id: approvalId, url: `${deps.approvalBaseUrl.replace(/\/$/, "")}/${approvalId}`, mode: "url", expires_at: start + 15 * 60 * 1000 },
      usage: ZERO_USAGE,
      latency_ms: 0,
      hints: [],
      next_action: "approve_at_url",
    };
    return finish("approval_required", 0, 200, body);
  }

  // 7. a provider that reads the user's own state needs the agent's own bearer; without one there
  // is nothing to forward, so stop before reserving or calling out.
  if (def.provider.auth.kind === "caller_bearer" && !call.bearer) {
    return fail("authentication_required", "this tool reads the user's own state; send your agent key as the bearer", "authenticate", "authentication_required");
  }
  const reservation = await deps.metering.reserve(identity, quote);
  if (!reservation.ok) {
    if (reservation.status === 401) return fail("authentication_required", "this tool is metered; send an agent key as the bearer", "authenticate", "authentication_required");
    return fail("payment_required", "insufficient balance for this run", "top_up", "payment_required", { payment: reservation.payment, hints: hints() });
  }

  // 8. execute. The transport injects the provider credential; the engine never holds it.
  const request = def.endpoint.input.toRequest ? def.endpoint.input.toRequest({ input, request: { method: def.endpoint.request.method, path: def.endpoint.request.path } }) : defaultRequest(def.endpoint, input);
  const providerRef = { id: def.provider.id, auth: def.provider.auth, baseUrl: def.provider.request.baseUrl, ...(def.provider.request.baseUrlEnv ? { baseUrlEnv: def.provider.request.baseUrlEnv } : {}) };
  const callerForwarding = def.provider.auth.kind === "caller_bearer" && call.bearer ? { caller: { bearer: call.bearer } } : {};
  let response;
  const execStart = now();
  try {
    response = await deps.transport.execute({ provider: providerRef, request, ...callerForwarding });
  } catch (e) {
    await deps.metering.settle(reservation.reservation_id, 0);
    deps.health.observe(toolId, { latency_ms: now() - execStart, ok: false });
    if (e instanceof TransportError) {
      if (e.code === "caller_bearer_missing") return fail("provider_unavailable", "this tool reads the user's own state; send your agent key as the bearer", "authenticate", "transport_error");
      if (e.code === "credential_missing") return fail("provider_unavailable", "the provider is not configured on this router", "try_alternative", "transport_error", { hints: hints() });
      return fail("upstream_error", e.code === "timeout" ? "the provider did not answer in time" : "the provider is unreachable", "retry_after", "transport_error", { hints: hints() });
    }
    return fail("upstream_error", "the provider call failed", "retry_after", "transport_error", { hints: hints() });
  }
  const execLatency = now() - execStart;
  const failed = def.endpoint.output.isFailure ? def.endpoint.output.isFailure(response) : response.status >= 400;
  if (failed) {
    await deps.metering.settle(reservation.reservation_id, 0);
    deps.health.observe(toolId, { latency_ms: execLatency, ok: false });
    const h = hints();
    return fail("upstream_error", `the provider answered with status ${response.status}; nothing was charged`, h.length ? "try_alternative" : "retry_after", "provider_error", { hints: h });
  }

  // 9. map, settle, record.
  const output: Json = def.endpoint.output.fromResponse ? def.endpoint.output.fromResponse({ input, response }) : response.body;
  const usage = settleUsage(def.endpoint, { input, response, output });
  await deps.metering.settle(reservation.reservation_id, usage.cost_micro_usd);
  deps.health.observe(toolId, { latency_ms: execLatency, ok: true });
  const body: RunOkBody = {
    resultType: "result",
    result: output,
    tool_id: toolId,
    run_id: randomId(),
    permission_level: level,
    replayed: false,
    usage,
    latency_ms: 0,
    hints: hints(),
    next_action: null,
  };
  await deps.idempotency.put(actor, verb.idempotency_key, { tool_id: toolId, input_hash: inputHash, response: body as unknown as Json, stored_at: start });
  return finish(usage.quantity === 0 ? "empty" : "ok", usage.cost_micro_usd, 200, body);
}
