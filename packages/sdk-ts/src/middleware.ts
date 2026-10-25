/**
 * The behavior every `withShogunAgent()` adapter shares (README §8 "one-line integrations"):
 *
 * - **Before the model call**: `recall(query = the last user message, budget_tokens)` and put the
 *   result in the system prompt as ONE escaped, replaceable
 *   `<shogunagent context="state-of-work" readonly>` block (an earlier block is replaced, never
 *   duplicated). High items as facts, Medium as `possibly:`, Low never.
 * - **After the call**: `ingest` the turn (the user message and the reply) under `threadKey` with
 *   an idempotency key derived from the turn id, so a retried turn replays instead of appending.
 *
 * A ShogunAgent outage never fails the model call: errors go to `onError` and the call proceeds
 * without the block (or without the ingest). A `PaymentRequiredError` from ingest is surfaced the
 * same way, with its payment payload, so the caller can show the top-up URL.
 */
import { ShogunAgent, type ShogunAgentOptions } from "./client.js";
import { contextBlock, replaceContext, stripContext } from "./context.js";
import { PaymentRequiredError } from "./errors.js";
import type { IngestTurn, Namespace, RecallResult } from "./types.js";

export interface WithShogunAgentOptions {
  /** A client to reuse; otherwise one is built from `apiKey` / `baseUrl` / `fetch`. */
  client?: ShogunAgent;
  apiKey?: string;
  baseUrl?: string;
  fetch?: ShogunAgentOptions["fetch"];
  /** Token budget of the recall (100–32,000). Default 2,000. */
  budgetTokens?: number;
  /** The conversation / task id the turns belong to. Adapters fall back to the framework's own
   * thread id when there is one. */
  threadKey?: string;
  /** `false` turns off the after-call ingest (recall only). Default true. */
  ingest?: boolean;
  /** Where ingested turns land: the user's `world` (default) or your `agent_scratch`. */
  namespace?: Namespace;
  /** Also read your own scratch namespace in the recall. */
  includeScratch?: boolean;
  /** `false` turns off the before-call recall (ingest only). Default true. */
  recall?: boolean;
  /** Who writes (`agent:<id>`). */
  actor?: string;
  /** Your harness name. Defaults to `shogunagent-sdk:<adapter>`. */
  source?: string;
  /** Errors from ShogunAgent (never thrown into the model call). Default: ignore. */
  onError?: (error: unknown, phase: "recall" | "ingest") => void;
}

/** One finished turn to remember. */
export interface CompletedTurn {
  /** A stable id for the turn: the model's response id, a message id, … Derived when absent. */
  turnId?: string | undefined;
  threadKey?: string | undefined;
  user?: string | undefined;
  assistant?: string | undefined;
}

const MAX_TURN_CHARS = 16_000;

async function sha256Hex(text: string): Promise<string> {
  const subtle = (globalThis as { crypto?: { subtle?: SubtleCrypto } }).crypto?.subtle;
  if (subtle) {
    const digest = await subtle.digest("SHA-256", new TextEncoder().encode(text));
    return Array.from(new Uint8Array(digest), (b) => b.toString(16).padStart(2, "0")).join("");
  }
  // FNV-1a fallback for runtimes without WebCrypto: stable, not cryptographic (it only keys a retry).
  let h = 0x811c9dc5;
  for (let i = 0; i < text.length; i++) {
    h ^= text.charCodeAt(i);
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return h.toString(16).padStart(8, "0");
}

/** The idempotency key of a turn: `turn:<sha256(thread, turn id)[:40]>`. Deterministic, so the
 * same turn ingested twice replays the first result. */
export async function turnIdempotencyKey(threadKey: string, turnId: string): Promise<string> {
  return `turn:${(await sha256Hex(`${threadKey}\u0000${turnId}`)).slice(0, 40)}`;
}

/** The text of a message's content in any of the shapes the frameworks use. */
export function textOf(content: unknown): string {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content.map((part) => {
      if (typeof part === "string") return part;
      if (part && typeof part === "object") {
        const p = part as Record<string, unknown>;
        if (typeof p.text === "string" && (p.type === undefined || p.type === "text" || p.type === "input_text" || p.type === "output_text")) return p.text;
      }
      return "";
    }).filter(Boolean).join("\n");
  }
  if (content && typeof content === "object") {
    const c = content as Record<string, unknown>;
    if (typeof c.text === "string") return c.text;
    if (Array.isArray(c.parts)) return textOf(c.parts);
    if (typeof c.content === "string") return c.content;
    if (Array.isArray(c.content)) return textOf(c.content);
  }
  return "";
}

/** The shared core the adapters drive. */
export class ShogunAgentMemory {
  readonly client: ShogunAgent;
  readonly options: WithShogunAgentOptions;
  private readonly adapter: string;

  constructor(options: WithShogunAgentOptions, adapter: string) {
    this.options = options;
    this.adapter = adapter;
    this.client = options.client ?? new ShogunAgent({
      ...(options.apiKey !== undefined ? { apiKey: options.apiKey } : {}),
      ...(options.baseUrl !== undefined ? { baseUrl: options.baseUrl } : {}),
      ...(options.fetch !== undefined ? { fetch: options.fetch } : {}),
      ...(options.actor !== undefined ? { actor: options.actor } : {}),
      source: options.source ?? `shogunagent-sdk:${adapter}`,
    });
  }

  private warnedPayment = false;

  private report(error: unknown, phase: "recall" | "ingest"): void {
    try {
      if (this.options.onError) {
        this.options.onError(error, phase);
      } else if (error instanceof PaymentRequiredError && !this.warnedPayment) {
        // Without a hook, a 402 is still surfaced once (the code and where to top up; no content):
        // a silent wallet stop would look like memory that quietly stopped working.
        this.warnedPayment = true;
        console.warn(`shogunagent: ${phase} answered 402 ${error.code}; next_action=${error.nextAction ?? ""}${error.topUpUrl ? ` top_up=${error.topUpUrl}` : ""}`);
      }
    } catch {
      /* the hook must not break the call either */
    }
  }

  /** The block for `query`, or `""` (nothing found, recall off, empty query, or an error). */
  async block(query: string | undefined): Promise<string> {
    if (this.options.recall === false) return "";
    const q = (query ?? "").trim();
    if (!q) return "";
    try {
      const recall: RecallResult = await this.client.recall({
        query: q.slice(0, 2_000),
        budgetTokens: this.options.budgetTokens ?? 2_000,
        ...(this.options.includeScratch !== undefined ? { includeScratch: this.options.includeScratch } : {}),
      });
      return contextBlock(recall);
    } catch (e) {
      this.report(e, "recall");
      return "";
    }
  }

  /** `system` with the fresh block for `query` (any earlier block replaced). */
  async inject(system: string | undefined | null, query: string | undefined): Promise<string> {
    const block = await this.block(query);
    return replaceContext(system ?? "", block);
  }

  /** Remember one finished turn. Never throws; returns whether an ingest was sent and accepted. */
  async remember(turn: CompletedTurn): Promise<boolean> {
    if (this.options.ingest === false) return false;
    const turns: IngestTurn[] = [];
    const user = stripContext(turn.user ?? "").trim();
    const assistant = stripContext(turn.assistant ?? "").trim();
    if (user) turns.push({ role: "user", content: user.slice(0, MAX_TURN_CHARS) });
    if (assistant) turns.push({ role: "assistant", content: assistant.slice(0, MAX_TURN_CHARS) });
    if (turns.length === 0) return false;
    const threadKey = turn.threadKey ?? this.options.threadKey ?? `${this.adapter}:default`;
    const turnId = turn.turnId ?? `content:${await sha256Hex(turns.map((t) => `${t.role}\u0000${t.content}`).join("\u0001"))}`;
    try {
      await this.client.ingest({
        turns,
        threadKey,
        idempotencyKey: await turnIdempotencyKey(threadKey, turnId),
        namespace: this.options.namespace ?? "world",
        metadata: { adapter: this.adapter, turn_id: turnId.slice(0, 200) },
      });
      return true;
    } catch (e) {
      this.report(e, "ingest");
      return false;
    }
  }
}
