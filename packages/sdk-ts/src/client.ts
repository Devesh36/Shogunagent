/**
 * `ShogunAgent`: the typed client of the agent API. Core tools go over MCP Streamable HTTP
 * (`POST <baseUrl>/mcp`, one JSON-RPC request per call, `Mcp-Method` / `Mcp-Name` headers the
 * gateway meters on); the gateway's own routes (`/v1/quote`, `/v1/handoff`) and the router verbs
 * (`/v1/router/{discover,inspect,run}`) go over REST. The same client works against the local
 * sidecar (`http://127.0.0.1:7464`, the default) and the hosted gateway
 * (`https://api.shogunagent.dev`, with an `sa_live_…` key).
 */
import {
  ShogunAgentError, ToolError, TransportError, errorFromResponse,
} from "./errors.js";
import {
  checkHandoff, extractHandoff, handoffEvents,
  type Handoff, type HandoffParams, type HandoffResult,
} from "./handoff.js";
import type {
  ActionArgs, ApprovalInfo, ApprovalStatus, ConnectionsStatus, DiscoverParams, DiscoverResult, Envelope, FetchLike,
  AcceptHandoffResult, Hint, IngestParams, IngestResult, InputRequest, InputResponse, MandateListParams, MandateListResult,
  MandateRequestParams, Namespace, PaymentMandate, PaymentSpend, QuoteParams, QuoteResult,
  RecallParams, RecallResult, ResponseLike, RunParams, RunResult, SpendParams, StateListParams, StateListResult, Usage,
} from "./types.js";

export const LOCAL_BASE_URL = "http://127.0.0.1:7464";
export const HOSTED_BASE_URL = "https://api.shogunagent.dev";
export const MCP_PROTOCOL_VERSION = "2026-07-28";
export const SDK_VERSION = "0.2.0";
/** Most items one `memory.ingest` carries (crates/shogun-mcp/src/ingest_api.rs). */
export const MAX_INGEST_ITEMS = 200;

export interface ShogunAgentOptions {
  /** `sa_live_…` for the hosted gateway; the local sidecar's client token locally. */
  apiKey?: string;
  /** Default `http://127.0.0.1:7464`. Use `HOSTED_BASE_URL` for the hosted gateway. */
  baseUrl?: string;
  /** Where the router verbs live; defaults to `baseUrl`. */
  routerBaseUrl?: string;
  /** Who writes (`agent:<id>`): the `actor` of every ingest. Default `agent:shogunagent-sdk`. */
  actor?: string;
  /** Your harness name: the `source` of every ingest. Default `shogunagent-sdk`. */
  source?: string;
  fetch?: FetchLike;
  /** Per-request timeout in ms (default 30,000). */
  timeoutMs?: number;
  /** Clock, for tests. */
  now?: () => number;
}

/** What `actions.execute` (and its retry) answered. */
export type ActionOutcome<T = unknown> =
  | { type: "result"; result: T; envelope: Envelope<T> | null }
  | {
    type: "input_required";
    /** Show these to the human (an L2 one-tap confirmation form). */
    inputRequests: InputRequest[];
    /** Resend the same call with the human's answer. `true` confirms, `false` declines. */
    respond(confirm: boolean | InputResponse[]): Promise<ActionOutcome<T>>;
  }
  | {
    type: "approval_required";
    /** Hand `approval.url` to the human (L3). Never report the action as done before `sent`. */
    approval: ApprovalInfo;
    /** One `actions.status` read. */
    status(): Promise<ApprovalStatus>;
    /** Poll `actions.status` until the approval leaves `pending`. */
    wait(options?: WaitOptions): Promise<ApprovalStatus>;
  }
  | { type: "declined"; result: unknown };

/** A pending L3 the human decides at `approval.url`. The SDK never approves: hand the URL over. */
export interface PendingApproval {
  type: "approval_required";
  approval: ApprovalInfo;
  /** One `actions.status` read. */
  status(): Promise<ApprovalStatus>;
  /** Poll `actions.status` until the approval leaves `pending`. */
  wait(options?: WaitOptions): Promise<ApprovalStatus>;
}

/** What `payments.mandates.request` answered. */
export type MandateRequestOutcome = { mandateId: string; idempotencyKey: string; raw: unknown } & (
  | PendingApproval
  /** Narrower terms on an existing mandate applied at once; or a retried request whose mandate a
   * human already granted (`replayed`). */
  | { type: "narrowed" | "replayed"; mandate: PaymentMandate }
);

/** What `payments.spend` answered. Never report a payment as made unless `type` is `settled`. */
export type SpendOutcome = { idempotencyKey: string; raw: unknown } & (
  | { type: "settled"; spend: PaymentSpend; replayed: boolean }
  /** Outside every mandate: a per-payment approval for a human. `reason` says why. */
  | (PendingApproval & { spendRef: string; reason: string })
  /** A replay of a spend that did not settle: still awaiting a human, rejected, expired or failed.
   * `rejected` / `expired` paid nothing; ask the human before trying again under a new key. */
  | { type: "not_settled"; spend: PaymentSpend; replayed: boolean }
);

export interface WaitOptions {
  intervalMs?: number;
  timeoutMs?: number;
  signal?: AbortSignal;
}

interface ToolCallOptions {
  inputResponses?: InputResponse[];
  quantity?: number;
}

/** The raw `tools/call` result after the transport, before the tool-specific typing. */
type RawToolResult =
  | { kind: "payload"; payload: unknown }
  | { kind: "input_required"; inputRequests: InputRequest[] }
  | { kind: "approval_required"; approval: ApprovalInfo; payload: unknown };

function isObj(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function parseJson(text: string): unknown {
  if (!text) return null;
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

/** The last `data:` payload of an SSE body (the transport sends one message per request). */
function parseSse(text: string): unknown {
  let last: string | null = null;
  for (const block of text.split(/\r?\n\r?\n/)) {
    const data = block.split(/\r?\n/).filter((l) => l.startsWith("data:")).map((l) => l.slice(5).replace(/^ /, "")).join("\n");
    if (data) last = data;
  }
  return last === null ? null : parseJson(last);
}

function randomId(): string {
  const c = (globalThis as { crypto?: { randomUUID?: () => string } }).crypto;
  if (c?.randomUUID) return c.randomUUID();
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}-${Math.random().toString(36).slice(2)}`;
}

/** Generate an idempotency key (`sdk_<uuid>`) when the caller passed none. */
export function newIdempotencyKey(prefix = "sdk"): string {
  return `${prefix}_${randomId()}`;
}

function envelopeOf<T>(payload: unknown): Envelope<T> {
  const p = isObj(payload) ? payload : {};
  return {
    result: ("result" in p ? p.result : payload) as T,
    usage: isObj(p.usage) ? (p.usage as unknown as Usage) : null,
    latency_ms: typeof p.latency_ms === "number" ? p.latency_ms : null,
    hints: Array.isArray(p.hints) ? (p.hints as Hint[]) : [],
    next_action: typeof p.next_action === "string" ? p.next_action : null,
  };
}

const sleep = (ms: number, signal?: AbortSignal) => new Promise<void>((resolve, reject) => {
  if (signal?.aborted) return reject(new TransportError({ status: 0, code: "aborted", message: "Aborted." }));
  const t = setTimeout(resolve, ms);
  signal?.addEventListener("abort", () => { clearTimeout(t); reject(new TransportError({ status: 0, code: "aborted", message: "Aborted." })); }, { once: true });
});

export class ShogunAgent {
  readonly baseUrl: string;
  readonly routerBaseUrl: string;
  readonly actor: string;
  readonly source: string;
  private readonly apiKey: string | undefined;
  private readonly fetchImpl: FetchLike;
  private readonly timeoutMs: number;
  private readonly now: () => number;
  private rpcId = 0;

  constructor(options: ShogunAgentOptions = {}) {
    this.baseUrl = (options.baseUrl ?? LOCAL_BASE_URL).replace(/\/+$/, "");
    this.routerBaseUrl = (options.routerBaseUrl ?? this.baseUrl).replace(/\/+$/, "");
    this.apiKey = options.apiKey;
    this.actor = options.actor ?? "agent:shogunagent-sdk";
    this.source = options.source ?? "shogunagent-sdk";
    this.timeoutMs = options.timeoutMs ?? 30_000;
    this.now = options.now ?? (() => Date.now());
    const f = options.fetch ?? (globalThis as { fetch?: FetchLike }).fetch;
    if (!f) throw new ShogunAgentError({ status: 0, code: "fetch_unavailable", message: "No global fetch on this runtime; pass options.fetch." });
    this.fetchImpl = f;
  }

  // --- transport ---

  private headers(extra: Record<string, string>): Record<string, string> {
    return {
      "content-type": "application/json",
      accept: "application/json, text/event-stream",
      "user-agent": `shogunagent-sdk-ts/${SDK_VERSION}`,
      ...(this.apiKey ? { authorization: `Bearer ${this.apiKey}` } : {}),
      ...extra,
    };
  }

  private async send(url: string, method: string, headers: Record<string, string>, body?: unknown): Promise<{ res: ResponseLike; parsed: unknown }> {
    const controller = typeof AbortController === "function" ? new AbortController() : undefined;
    const timer = controller ? setTimeout(() => controller.abort(), this.timeoutMs) : undefined;
    let res: ResponseLike;
    try {
      res = await this.fetchImpl(url, {
        method,
        headers,
        ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
        ...(controller ? { signal: controller.signal } : {}),
      });
    } catch (e) {
      throw new TransportError({ status: 0, code: "transport_error", message: `Could not reach ${url}: ${e instanceof Error ? e.message : String(e)}`, nextAction: "retry_after" });
    } finally {
      if (timer) clearTimeout(timer);
    }
    const text = await res.text();
    const contentType = res.headers.get("content-type") ?? "";
    const parsed = contentType.includes("text/event-stream") ? parseSse(text) : parseJson(text);
    if (!res.ok) throw errorFromResponse(res.status, parsed, res.headers);
    return { res, parsed };
  }

  /** `POST <baseUrl><path>` with a JSON body; the parsed JSON answer. */
  async rest<T = unknown>(path: string, body?: unknown, options: { method?: string; base?: "api" | "router"; headers?: Record<string, string> } = {}): Promise<T> {
    const base = options.base === "router" ? this.routerBaseUrl : this.baseUrl;
    const { parsed } = await this.send(`${base}${path}`, options.method ?? (body === undefined ? "GET" : "POST"), this.headers(options.headers ?? {}), body);
    return parsed as T;
  }

  /**
   * One MCP `tools/call` over Streamable HTTP. Returns the tool's payload (the JSON in
   * `content[0].text`, or `structuredContent`), or the MRTR `input_required` /
   * `approval_required` branch. Throws `ToolError` on `isError` and on a JSON-RPC error.
   */
  private async toolCall(name: string, args: Record<string, unknown>, options: ToolCallOptions = {}): Promise<RawToolResult> {
    const id = ++this.rpcId;
    const params: Record<string, unknown> = { name, arguments: args };
    if (options.inputResponses) params.inputResponses = options.inputResponses;
    const headers = this.headers({
      "mcp-method": "tools/call",
      "mcp-name": name,
      "mcp-protocol-version": MCP_PROTOCOL_VERSION,
      ...(options.quantity !== undefined ? { "mcp-quantity": String(options.quantity) } : {}),
    });
    const { parsed } = await this.send(`${this.baseUrl}/mcp`, "POST", headers, { jsonrpc: "2.0", id, method: "tools/call", params });
    if (!isObj(parsed)) throw new ToolError({ status: 200, code: "invalid_response", message: `${name}: the server answered something that is not JSON-RPC.`, body: parsed });
    if (isObj(parsed.error)) {
      const message = typeof parsed.error.message === "string" ? parsed.error.message : "JSON-RPC error";
      const code = /^([a-z_]+):/.exec(message)?.[1] ?? `rpc_${String(parsed.error.code ?? "error")}`;
      throw new ToolError({ status: 200, code, message, nextAction: code === "memory_update_pending" ? "retry_after" : "fix_input", body: parsed.error });
    }
    const result = isObj(parsed.result) ? parsed.result : {};
    if (result.resultType === "input_required") {
      return { kind: "input_required", inputRequests: Array.isArray(result.inputRequests) ? (result.inputRequests as InputRequest[]) : [] };
    }
    if (result.resultType === "approval_required" && isObj(result.approval)) {
      const first = Array.isArray(result.content) ? (result.content as unknown[])[0] : undefined;
      const payload = isObj(first) && typeof first.text === "string" ? parseJson(first.text) : null;
      return { kind: "approval_required", approval: result.approval as unknown as ApprovalInfo, payload };
    }
    let payload: unknown = result.structuredContent;
    if (payload === undefined) {
      const first = Array.isArray(result.content) ? (result.content as unknown[])[0] : undefined;
      payload = isObj(first) && typeof first.text === "string" ? parseJson(first.text) : null;
    }
    if (result.isError === true) {
      const p = isObj(payload) ? payload : {};
      const err = p.error;
      const code = typeof err === "string" ? err : isObj(err) && typeof err.code === "string" ? err.code : "tool_error";
      const message = typeof p.message === "string" ? p.message : isObj(err) && typeof err.message === "string" ? err.message : `${name} refused the call (${code}).`;
      const next = typeof p.next_action === "string" ? p.next_action : isObj(err) && typeof err.next_action === "string" ? err.next_action : null;
      throw new ToolError({ status: 200, code, message, nextAction: next, body: payload });
    }
    return { kind: "payload", payload };
  }

  /** A tool call that must answer a payload (reads, ingest). */
  async callTool<T = unknown>(name: string, args: Record<string, unknown> = {}, options: ToolCallOptions = {}): Promise<T> {
    const raw = await this.toolCall(name, args, options);
    if (raw.kind !== "payload") {
      throw new ToolError({ status: 200, code: raw.kind, message: `${name} answered ${raw.kind}; call it through actions.execute to handle confirmation and approval.`, nextAction: raw.kind === "input_required" ? "provide_input" : "approve_at_url", body: raw });
    }
    return raw.payload as T;
  }

  // --- memory ---

  /** `memory.recall`: what memory holds on a question, cut to the budget. Free. */
  async recall(params: RecallParams | string): Promise<RecallResult & { envelope: Envelope<RecallResult> }> {
    const p = typeof params === "string" ? { query: params } : params;
    const args: Record<string, unknown> = { query: p.query };
    if (p.budgetTokens !== undefined) args.budget_tokens = p.budgetTokens;
    if (p.includeScratch !== undefined) args.include_scratch = p.includeScratch;
    const env = envelopeOf<RecallResult>(await this.callTool("memory.recall", args));
    const r = (isObj(env.result) ? env.result : {}) as Partial<RecallResult>;
    return {
      items: r.items ?? [], facts: r.facts ?? [], possibly: r.possibly ?? [], evidence: r.evidence ?? [],
      truncated: r.truncated ?? false, budget_tokens: r.budget_tokens ?? p.budgetTokens ?? 0,
      used_tokens: r.used_tokens ?? 0, include_scratch: r.include_scratch ?? Boolean(p.includeScratch),
      envelope: env,
    };
  }

  /**
   * `memory.ingest`: hand over what you learned. An idempotency key is generated when absent;
   * pass your own (derived from a turn or task id) so a retry replays instead of appending.
   * Metered per event on the hosted gateway.
   */
  async ingest(params: IngestParams): Promise<IngestResult & { idempotency_key: string; envelope: Envelope<IngestResult> }> {
    const items = params.events ?? params.turns ?? [];
    if (items.length === 0) throw new ShogunAgentError({ status: 0, code: "invalid_events", message: "Pass at least one event or turn.", nextAction: "fix_input" });
    if (items.length > MAX_INGEST_ITEMS) throw new ShogunAgentError({ status: 0, code: "invalid_events", message: `At most ${MAX_INGEST_ITEMS} items per ingest; split the batch.`, nextAction: "fix_input" });
    const key = params.idempotencyKey ?? newIdempotencyKey();
    const args: Record<string, unknown> = {
      source: params.source ?? this.source,
      actor: params.actor ?? this.actor,
      idempotency_key: key,
      namespace: params.namespace ?? "world",
    };
    if (params.events) args.events = params.events;
    else args.turns = params.turns;
    if (params.threadKey) args.thread_key = params.threadKey;
    if (params.metadata) args.metadata = params.metadata;
    const env = envelopeOf<IngestResult>(await this.callTool("memory.ingest", args, { quantity: items.length }));
    const r = (isObj(env.result) ? env.result : {}) as IngestResult;
    return { ...r, idempotency_key: key, envelope: env };
  }

  /** `memory.get_context_pack`: one grounded pack for a person, project or task. Free. */
  async contextPack(query: string): Promise<Envelope<unknown> & { raw: unknown }> {
    const raw = await this.callTool("memory.get_context_pack", { query });
    return { ...envelopeOf<unknown>(raw), raw };
  }

  // --- state.* (free reads) ---

  readonly state = {
    commitments: { list: (p: StateListParams = {}) => this.stateList("state.commitments.list", p) },
    openLoops: { list: (p: StateListParams = {}) => this.stateList("state.open_loops.list", p) },
    people: {
      list: (p: StateListParams = {}) => this.stateList("state.people.list", p),
      get: (id: number) => this.callTool<StateListResult>("state.people.get", { id }),
    },
    projects: {
      list: (p: StateListParams = {}) => this.stateList("state.projects.list", p),
      get: (id: number) => this.callTool<StateListResult>("state.projects.get", { id }),
    },
  };

  private async stateList(tool: string, p: StateListParams): Promise<StateListResult> {
    const args: Record<string, unknown> = {};
    if (p.includeLow !== undefined) args.include_low = p.includeLow;
    if (p.limit !== undefined) args.limit = p.limit;
    if (p.offset !== undefined) args.offset = p.offset;
    const raw = await this.callTool<unknown>(tool, args);
    const obj = isObj(raw) ? raw : {};
    const results = Array.isArray(obj.results) ? obj.results : isObj(obj.result) && Array.isArray(obj.result.results) ? obj.result.results : [];
    return { ...obj, results: results as StateListResult["results"] };
  }

  // --- actions ---

  readonly actions = {
    /**
     * `actions.execute`. L1 kinds run and answer `type: "result"`; L2 answers `input_required`
     * (show the form, then `respond(true)`); L3 answers `approval_required` (hand `approval.url` to
     * the human, then `wait()`). Never report a send before `wait()` resolves with `sent`.
     */
    execute: <T = unknown>(args: ActionArgs, options: { inputResponses?: InputResponse[] } = {}): Promise<ActionOutcome<T>> => this.execute<T>(args, options),
    /** `actions.status`: one read of an L3 approval. Free. */
    status: (approvalId: number): Promise<ApprovalStatus> => this.approvalStatus(approvalId),
    /** Poll `actions.status` until the approval leaves `pending`. */
    waitForApproval: (approvalId: number, options: WaitOptions = {}): Promise<ApprovalStatus> => this.waitForApproval(approvalId, options),
  };

  private async execute<T>(args: ActionArgs, options: { inputResponses?: InputResponse[] }): Promise<ActionOutcome<T>> {
    const raw = await this.toolCall("actions.execute", args as Record<string, unknown>, options.inputResponses ? { inputResponses: options.inputResponses } : {});
    if (raw.kind === "input_required") {
      const requests = raw.inputRequests;
      return {
        type: "input_required",
        inputRequests: requests,
        respond: (answer) => {
          const responses: InputResponse[] = Array.isArray(answer) ? answer : [{
            id: requests[0]?.id ?? "l2_confirm",
            action: answer ? "accept" : "decline",
            content: { confirm: answer },
          }];
          return this.execute<T>(args, { inputResponses: responses });
        },
      };
    }
    if (raw.kind === "approval_required") {
      const approval = raw.approval;
      return {
        type: "approval_required",
        approval,
        status: () => this.approvalStatus(approval.approval_id),
        wait: (o = {}) => this.waitForApproval(approval.approval_id, o),
      };
    }
    const payload = raw.payload;
    if (isObj(payload) && payload.declined === true && payload.ran === false) return { type: "declined", result: payload };
    // A pending envelope that the server did not reshape (an older core): still an approval.
    if (isObj(payload) && payload.pending === true && typeof payload.approval_id === "number") {
      const id = payload.approval_id;
      const approval: ApprovalInfo = {
        approval_id: id, level: typeof payload.level === "string" ? payload.level : "L3",
        url: typeof payload.url === "string" ? payload.url : null,
        poll: { tool: "actions.status", arguments: { approval_id: id } },
        message: "Hand the URL to the human, or wait for approval in the ShogunAI app; poll actions.status.",
      };
      return { type: "approval_required", approval, status: () => this.approvalStatus(id), wait: (o = {}) => this.waitForApproval(id, o) };
    }
    const env = envelopeOf<T>(payload);
    return { type: "result", result: env.result, envelope: isObj(payload) && "result" in payload ? env : null };
  }

  private async approvalStatus(approvalId: number): Promise<ApprovalStatus> {
    const raw = await this.callTool<unknown>("actions.status", { approval_id: approvalId });
    const obj = isObj(raw) ? (isObj(raw.result) ? raw.result : raw) : {};
    return { approval_id: approvalId, status: "unknown", ...(obj as Partial<ApprovalStatus>) } as ApprovalStatus;
  }

  private async waitForApproval(approvalId: number, options: WaitOptions): Promise<ApprovalStatus> {
    const interval = Math.max(1, options.intervalMs ?? 2_000);
    const deadline = this.now() + (options.timeoutMs ?? 15 * 60_000);
    for (;;) {
      const s = await this.approvalStatus(approvalId);
      if (s.status !== "pending") return s;
      if (this.now() >= deadline) return s;
      await sleep(interval, options.signal);
    }
  }

  // --- payments (E46 / E51, docs/api/payments.md) ---

  readonly payments = {
    mandates: {
      /** `payments.mandates.list`: the mandates a human granted, with spent and remaining; with
       * `mandateId`, that mandate and its recent spends. Free. */
      list: (params: MandateListParams = {}): Promise<MandateListResult> => this.listMandates(params),
      /**
       * `payments.mandates.request`: ask the human for a mandate (L3). Answers `approval_required`
       * with the URL to hand to the human; nothing is granted until they approve, and this SDK never
       * approves. Terms that only narrow an existing mandate (`mandateId`) apply at once.
       */
      request: (params: MandateRequestParams): Promise<MandateRequestOutcome> => this.requestMandate(params),
      /** `payments.mandates.revoke`: revoked at once (L1); twice is harmless. */
      revoke: (mandateId: string, options: { idempotencyKey?: string } = {}): Promise<{ revoked: boolean; already_revoked: boolean; mandate: PaymentMandate }> => {
        const args: Record<string, unknown> = { mandate_id: mandateId };
        if (options.idempotencyKey !== undefined) args.idempotency_key = options.idempotencyKey;
        return this.callTool("payments.mandates.revoke", args);
      },
    },
    /**
     * `payments.spend` (L3 by type). Inside an active mandate granted to you it settles through the
     * payment rail; otherwise it answers `approval_required` with the URL for a human. An
     * idempotency key is generated when absent: reuse the returned `idempotencyKey` to retry, and a
     * retry never pays twice.
     */
    spend: (params: SpendParams): Promise<SpendOutcome> => this.spend(params),
  };

  private pendingOf(approval: ApprovalInfo): PendingApproval {
    return { type: "approval_required", approval, status: () => this.approvalStatus(approval.approval_id), wait: (o = {}) => this.waitForApproval(approval.approval_id, o) };
  }

  /** The approval of a payload that is still the core's plain pending envelope (no MRTR shaping). */
  private plainPending(payload: unknown): ApprovalInfo | undefined {
    if (!isObj(payload) || payload.pending !== true || typeof payload.approval_id !== "number") return undefined;
    const id = payload.approval_id;
    return {
      approval_id: id, level: typeof payload.level === "string" ? payload.level : "L3",
      url: typeof payload.url === "string" ? payload.url : null,
      poll: { tool: "actions.status", arguments: { approval_id: id } },
      message: "Hand the URL to the human, or wait for approval in the ShogunAI app; poll actions.status.",
    };
  }

  private async listMandates(params: MandateListParams): Promise<MandateListResult> {
    const args: Record<string, unknown> = {};
    if (params.mandateId !== undefined) args.mandate_id = params.mandateId;
    if (params.limit !== undefined) args.limit = params.limit;
    const raw = await this.callTool<Record<string, unknown>>("payments.mandates.list", args);
    const obj = isObj(raw) ? raw : {};
    const mandate = isObj(obj.mandate) ? (obj.mandate as unknown as PaymentMandate) : undefined;
    return {
      mandates: Array.isArray(obj.mandates) ? (obj.mandates as PaymentMandate[]) : mandate ? [mandate] : [],
      ...(mandate ? { mandate } : {}),
      ...(Array.isArray(obj.spends) ? { spends: obj.spends as PaymentSpend[] } : {}),
    };
  }

  private async requestMandate(params: MandateRequestParams): Promise<MandateRequestOutcome> {
    if ((params.payee === undefined) === (params.payeeClass === undefined)) {
      throw new ShogunAgentError({ status: 0, code: "invalid_payee", message: "Give exactly one of payee (an exact payee id) or payeeClass.", nextAction: "fix_input" });
    }
    const key = params.idempotencyKey ?? newIdempotencyKey("mandate");
    const args: Record<string, unknown> = {
      currency: params.currency, max_per_payment_minor: params.maxPerPaymentMinor, total_cap_minor: params.totalCapMinor,
      purpose: params.purpose, idempotency_key: key,
    };
    if (params.payee !== undefined) args.payee = params.payee;
    if (params.payeeClass !== undefined) args.payee_class = params.payeeClass;
    if (params.expiresInDays !== undefined) args.expires_in_days = params.expiresInDays;
    if (params.expiresAtMs !== undefined) args.expires_at_ms = params.expiresAtMs;
    if (params.singleUse !== undefined) args.single_use = params.singleUse;
    if (params.mandateId !== undefined) args.mandate_id = params.mandateId;
    const raw = await this.toolCall("payments.mandates.request", args);
    const payload = raw.kind === "input_required" ? null : raw.payload;
    const p = isObj(payload) ? payload : {};
    const mandateId = typeof p.mandate_id === "string" ? p.mandate_id : isObj(p.mandate) && typeof p.mandate.mandate_id === "string" ? p.mandate.mandate_id : params.mandateId ?? "";
    const approval = raw.kind === "approval_required" ? raw.approval : this.plainPending(payload);
    if (approval) return { ...this.pendingOf(approval), mandateId, idempotencyKey: key, raw: payload };
    if (raw.kind === "input_required" || !isObj(p.mandate)) {
      throw new ToolError({ status: 200, code: "invalid_response", message: "payments.mandates.request answered neither an approval nor a mandate.", body: payload });
    }
    return { type: p.narrowed === true ? "narrowed" : "replayed", mandate: p.mandate as unknown as PaymentMandate, mandateId, idempotencyKey: key, raw: payload };
  }

  private async spend(params: SpendParams): Promise<SpendOutcome> {
    const key = params.idempotencyKey ?? newIdempotencyKey("pay");
    const args: Record<string, unknown> = { payee: params.payee, currency: params.currency, amount_minor: params.amountMinor, idempotency_key: key };
    if (params.memo !== undefined) args.memo = params.memo;
    if (params.mandateId !== undefined) args.mandate_id = params.mandateId;
    const raw = await this.toolCall("payments.spend", args);
    const payload = raw.kind === "input_required" ? null : raw.payload;
    const p = isObj(payload) ? payload : {};
    const approval = raw.kind === "approval_required" ? raw.approval : this.plainPending(payload);
    if (approval) {
      return { ...this.pendingOf(approval), spendRef: typeof p.spend_ref === "string" ? p.spend_ref : "", reason: typeof p.reason === "string" ? p.reason : "no_mandate", idempotencyKey: key, raw: payload };
    }
    if (raw.kind === "input_required" || !isObj(p.spend)) {
      throw new ToolError({ status: 200, code: "invalid_response", message: "payments.spend answered neither a spend nor an approval.", body: payload });
    }
    const spend = p.spend as unknown as PaymentSpend;
    const replayed = p.replayed === true;
    return spend.status === "settled"
      ? { type: "settled", spend, replayed, idempotencyKey: key, raw: payload }
      : { type: "not_settled", spend, replayed, idempotencyKey: key, raw: payload };
  }

  // --- connections (docs/api/connector-tokens.md) ---

  readonly connections = {
    /** `connections.list`: which accounts the principal has connected and each one's state. Free. */
    status: (): Promise<ConnectionsStatus> => this.callTool<ConnectionsStatus>("connections.list", {}),
    /** `connections.disconnect`: remove one connected account (`gcal`, `gmail`, `slack`, …). Only
     * removes access; reconnecting is always the human's. */
    disconnect: (service: string): Promise<Record<string, unknown>> => this.callTool("connections.disconnect", { service }),
  };

  // --- router ---

  readonly router = {
    /** Find a tool for a task. Free, no key needed. */
    discover: (params: DiscoverParams): Promise<Envelope<DiscoverResult>> =>
      this.rest<unknown>("/v1/router/discover", params, { base: "router" }).then((p) => envelopeOf<DiscoverResult>(p)),
    /** One tool's schema, price, examples and cheaper alternatives. Free, no key needed. */
    inspect: (toolId: string): Promise<Envelope<Record<string, unknown>>> =>
      this.rest<unknown>("/v1/router/inspect", { tool_id: toolId }, { base: "router" }).then((p) => envelopeOf<Record<string, unknown>>(p)),
    /**
     * Run a tool with the permission tiers applied. `resultType` says what happened:
     * `result`, `quote` (dry run), `input_required` (L2), `approval_required` (L3). An idempotency
     * key is generated when absent.
     */
    run: (params: RunParams): Promise<RunResult & { idempotency_key: string }> => {
      const key = params.idempotency_key ?? newIdempotencyKey("run");
      return this.rest<RunResult>("/v1/router/run", { ...params, idempotency_key: key }, { base: "router" }).then((r) => ({ ...r, idempotency_key: key }));
    },
  };

  // --- gateway ---

  /** `POST /v1/quote`: the exact cost of a call, nothing reserved (hosted gateway). */
  quote(params: QuoteParams): Promise<QuoteResult> {
    return this.rest<QuoteResult>("/v1/quote", { method: "tools/call", ...params });
  }

  /**
   * `POST /v1/handoff`: the state of work for a task, as a typed handoff object (a strict subset
   * of export v1) plus the A2A data part to put in your message to the receiving agent. Free.
   */
  async handoff(params: HandoffParams): Promise<HandoffResult & { envelope: Envelope<HandoffResult> }> {
    const body: Record<string, unknown> = { task: params.task, to_agent: params.toAgent, from_agent: params.fromAgent ?? this.actor };
    if (params.query !== undefined) body.query = params.query;
    if (params.budgetTokens !== undefined) body.budget_tokens = params.budgetTokens;
    if (params.ttlSeconds !== undefined) body.ttl_seconds = params.ttlSeconds;
    if (params.threadKey !== undefined) body.thread_key = params.threadKey;
    const env = envelopeOf<HandoffResult>(await this.rest<unknown>("/v1/handoff", body));
    return { ...env.result, envelope: env };
  }

  /**
   * Accept a handoff another agent sent: checked here first (format, addressee, expiry) so a bad
   * one costs no call, then `handoff.accept`, which the core validates again and stores in YOUR
   * `agent_scratch` namespace — never the user's world — under `thread_key handoff:<id>`.
   * Idempotent: accepting the same handoff twice replays (key `handoff:<id>:<receiver>`). Accepts
   * the object, its A2A data part, the A2A message, or `handoff()`'s result. Metered per event
   * stored (ingest).
   */
  async acceptHandoff(input: unknown, options: { as?: string; checkAddressee?: boolean } = {}): Promise<{ handoff: Handoff; accepted: AcceptHandoffResult; envelope: Envelope<AcceptHandoffResult> }> {
    const h = extractHandoff(input);
    const receiver = options.as ?? this.actor;
    const check = options.checkAddressee !== false;
    checkHandoff(h, check ? receiver : undefined, this.now());
    const args: Record<string, unknown> = { handoff: h, actor: receiver };
    if (!check) args.check_addressee = false;
    const env = envelopeOf<AcceptHandoffResult>(await this.callTool("handoff.accept", args, { quantity: handoffEvents(h).length }));
    const accepted = (isObj(env.result) ? env.result : {}) as AcceptHandoffResult;
    return { handoff: h, accepted, envelope: env };
  }
}
