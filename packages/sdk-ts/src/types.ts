/**
 * Wire types of the ShogunAgent agent API (docs/implementation-plan.md §7, docs/api/core-tools.md,
 * docs/api/402.md, docs/api/handoff.md). Field names are the wire's own (snake_case), so a value
 * logged from this SDK reads the same as the API reference.
 */

/** The `fetch` the client calls. Inject one in tests or on runtimes without a global fetch. */
export type FetchLike = (input: string, init?: {
  method?: string;
  headers?: Record<string, string>;
  body?: string;
  signal?: AbortSignal;
}) => Promise<ResponseLike>;

/** The subset of `Response` the client reads. */
export interface ResponseLike {
  readonly status: number;
  readonly ok: boolean;
  readonly headers: { get(name: string): string | null };
  text(): Promise<string>;
}

/** §7.2: every metered response says what it cost. Reads are free (`cost_micro_usd: 0`). */
export interface Usage {
  unit: string;
  quantity: number;
  cost_micro_usd: number;
}

/** A cheaper or better-fitting tool the server suggests. */
export interface Hint {
  tool_id: string;
  reason: string;
  estimated_cost_micro_usd?: number;
}

/** What to do next when something did not simply succeed. */
export type NextAction =
  | "top_up"
  | "approve_at_url"
  | "retry_after"
  | "narrow_query"
  | "not_allowed"
  | "provide_input"
  | "poll_status"
  | "authenticate"
  | "solve_challenge"
  | "discover"
  | "fix_input"
  | "try_alternative"
  | (string & {});

/** The common envelope (§7.2) around a tool's `result`. */
export interface Envelope<T> {
  result: T;
  usage: Usage | null;
  latency_ms: number | null;
  hints: Hint[];
  next_action: NextAction | null;
}

/** Confidence band (shogun-fusion/src/confidence.rs). Low is never returned as a fact. */
export type Band = "high" | "medium" | "low";

export type Namespace = "world" | "agent_scratch";

// --- memory.recall ---

export interface RecallItem {
  kind: "commitment" | "open_loop" | "person" | "project";
  id: number;
  text: string;
  confidence: number;
  band: Band;
  quote: string | null;
  event_id: number | null;
}

export interface RecallEvidence {
  event_id: number;
  ts_ms: number;
  source: string;
  title: string | null;
  excerpt: string;
}

export interface RecallResult {
  items: RecallItem[];
  /** High-confidence facts, as facts. */
  facts: string[];
  /** Medium-confidence facts, each prefixed `possibly: `. */
  possibly: string[];
  evidence: RecallEvidence[];
  truncated: boolean;
  budget_tokens: number;
  used_tokens: number;
  include_scratch: boolean;
}

export interface RecallParams {
  query: string;
  /** 100–32,000; default 2,000 on the server. */
  budgetTokens?: number;
  /** Also read your own `agent_scratch` namespace. */
  includeScratch?: boolean;
}

// --- memory.ingest ---

export interface IngestEvent {
  content: string;
  ts_ms?: number;
  kind?: string;
  title?: string;
}

export interface IngestTurn {
  role: "user" | "assistant" | "system" | "tool";
  content: string;
  ts_ms?: number;
}

export interface IngestParams {
  events?: IngestEvent[];
  turns?: IngestTurn[];
  /** Your harness or agent name. Defaults to the client's `source`. */
  source?: string;
  /** Who writes (`agent:<id>`). Defaults to the client's `actor`. */
  actor?: string;
  threadKey?: string;
  metadata?: Record<string, unknown>;
  /** Generated when absent; pass your own to make a retry replay instead of append. */
  idempotencyKey?: string;
  namespace?: Namespace;
}

/** `handoff.accept`'s receipt (E35): what the core stored in the receiver's scratch. */
export interface AcceptHandoffResult {
  handoff_id: string;
  from_agent: string;
  to_agent: string;
  accepted: number;
  inserted: number;
  deduplicated: number;
  rejected: number;
  /** Rows left out: Low band or without provenance. */
  skipped: number;
  replayed: boolean;
  event_ids: number[];
  thread_key: string | null;
  namespace: "agent_scratch";
  idempotency_key: string;
  [key: string]: unknown;
}

export interface IngestResult {
  event_ids: number[];
  inserted: number;
  deduplicated: number;
  near_duplicates?: number;
  rejected?: number;
  replayed: boolean;
  thread_key: string | null;
  namespace?: string;
  idempotency_key?: string;
  [key: string]: unknown;
}

// --- state.* ---

export interface StateListParams {
  includeLow?: boolean;
  limit?: number;
  offset?: number;
}

export interface StateRow {
  text: string;
  confidence: number;
  /** Medium (or opted-in Low) rows: say "possibly", never act externally on them. */
  possibly: boolean;
  table?: string;
  id?: number;
  key?: string;
  deferred_until?: number;
  sources?: Array<{ source: string; app_bundle_id?: string; count: number }>;
  [key: string]: unknown;
}

export interface StateListResult {
  tool?: string;
  results: StateRow[];
  [key: string]: unknown;
}

// --- actions.execute ---

export interface ActionArgs {
  kind: string;
  [key: string]: unknown;
}

/** One MRTR input request (MCP 2026-07-28 SEP-2322). */
export interface InputRequest {
  id: string;
  mode: "form" | "url";
  message: string;
  tool?: string;
  arguments?: Record<string, unknown>;
  requestedSchema?: Record<string, unknown>;
  url?: string;
}

/** The answer to one input request, sent back with the retried call. */
export interface InputResponse {
  id: string;
  action: "accept" | "decline" | "cancel";
  content?: Record<string, unknown>;
}

export interface ApprovalInfo {
  approval_id: number;
  level: string;
  url: string | null;
  poll: { tool: string; arguments: { approval_id: number } };
  message: string;
  origin?: string;
}

export type ApprovalState =
  | "pending"
  | "sent"
  | "rejected"
  | "timed_out"
  | "send_failed"
  | "draft_saved"
  | "outcome_unknown"
  | "unknown";

export interface ApprovalStatus {
  approval_id: number;
  status: ApprovalState;
  reason?: string;
}

// --- router ---

export interface DiscoverParams {
  task: string;
  budget_micro_usd?: number;
  limit?: number;
  categories?: string[];
}

export interface DiscoverCandidate {
  tool_id: string;
  provider_id: string;
  first_party: boolean;
  summary: string;
  categories: string[];
  permission_level: string;
  score: number;
  scoreBreakdown: { task_fit: number; price: number; health: number; latency: number; personal: number };
  price: { model: string; unit: string; cost_micro_usd: number; quote_micro_usd: number };
  p50_ms: number;
  p95_ms: number;
}

export interface DiscoverResult {
  candidates: DiscoverCandidate[];
  ranking?: Record<string, unknown>;
  catalog_hash?: string;
  considered?: number;
  [key: string]: unknown;
}

export interface RunParams {
  tool_id: string;
  input: Record<string, unknown>;
  /** Generated when absent. */
  idempotency_key?: string;
  dryRun?: boolean;
  task_hint?: string;
  inputResponses?: Record<string, unknown>;
}

/** A router `run` answer: `resultType` says which branch (README of apps/router). */
export interface RunResult {
  resultType: "result" | "quote" | "input_required" | "approval_required" | "error" | (string & {});
  result?: unknown;
  run_id?: string;
  replayed?: boolean;
  inputRequests?: InputRequest[];
  approval?: { url: string; approval_id?: string | number; expires_at?: string; [key: string]: unknown };
  usage?: Usage;
  latency_ms?: number;
  hints?: Hint[];
  next_action?: NextAction | null;
  [key: string]: unknown;
}

// --- quote ---

export interface QuoteParams {
  method?: string;
  name?: string;
  quantity?: number;
  provider_price_micro_usd?: number;
  tool_id?: string;
}

export interface QuoteResult {
  quote: Usage;
  method: string;
  name: string | null;
  balance_micro_usd: number;
  sufficient: boolean;
  caps?: Record<string, unknown>;
  usage: Usage;
  next_action: NextAction | null;
}

// --- 402 ---

/** `payment` of a 402 (docs/api/402.md). */
export interface PaymentPayload {
  amount_micro_usd: number;
  unit: string;
  accepts: Array<{ rail: string; url: string; method?: string }>;
  retry_after: number;
  balance_micro_usd?: number;
  cap?: { scope: string; period: string; limit_micro_usd: number; spent_micro_usd: number };
}
