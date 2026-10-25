/**
 * Wire types for the batch relay (docs/batch-relay-design.md §4).
 *
 * The device sends a `model_class` *intent*, never a model id (§4.4): a client that can pick the
 * model can pick an expensive one, so the mapping to a concrete model lives server-side
 * (`models.ts`). Chunk content passes through this process and is never written to disk or logs
 * (§3.2 / NFR-PRV-04).
 */
import { createHash } from "node:crypto";

/** The intents a device may send. The relay maps each to a concrete model. */
export const MODEL_CLASSES = [
  "classify",
  "summarize",
  "brief",
  "about_profile",
  "daily_activity_summary",
] as const;
export type ModelClass = (typeof MODEL_CLASSES)[number];

export function isModelClass(v: unknown): v is ModelClass {
  return typeof v === "string" && (MODEL_CLASSES as readonly string[]).includes(v);
}

/** Every Batch purpose has one server-approved output contract and cost class. */
export const MODEL_CLASS_BY_PURPOSE = {
  consolidation: "classify",
  consolidation_window: "classify",
  morning_brief: "brief",
  evening_wrap: "brief",
  meeting_recap: "summarize",
  mail_triage: "classify",
  entity_dedupe: "classify",
  loops_consolidate: "classify",
  entity_page: "classify",
  about_profile: "about_profile",
  daily_activity_summary: "daily_activity_summary",
  dictionary_terms: "classify",
} as const satisfies Record<string, ModelClass>;
export type BatchPurpose = keyof typeof MODEL_CLASS_BY_PURPOSE;

export function isBatchPurpose(v: unknown): v is BatchPurpose {
  return typeof v === "string" && Object.hasOwn(MODEL_CLASS_BY_PURPOSE, v);
}

/** Purposes answered only inline by `POST /v1/review`, never queued as a Batch job. */
export const REVIEW_ONLY_PURPOSES: ReadonlySet<BatchPurpose> = new Set<BatchPurpose>(["dictionary_terms"]);

/** One item of `POST /v1/batch` (§4.2). `chunk` is the text forwarded as the user turn: on the
 * wire it is the item's `chunk` (a self-contained prompt) or, when the request carries a shared
 * `system`, the item's `data` (only the per-item half, #667). */
export interface RelayBatchItem {
  custom_id: string;
  chunk: string;
}

/** The `POST /v1/batch` request body (§4.2). */
export interface RelayBatchRequest {
  purpose: BatchPurpose;
  model_class: ModelClass;
  items: RelayBatchItem[];
  /**
   * Optional shared instruction (#667), sent once and forwarded as every request's `system`
   * block — the prompt-cache prefix. Present ⇔ the items arrived as `{custom_id, data}`.
   * Whether it gets a `cache_control` breakpoint is decided here, not by the device (models.ts).
   */
  system?: string;
  /** Stable job identity (#343), required before any company-funded provider call. */
  clientRequestId: string;
}

/** Ceiling on items per submission. The nightly Dream Cycle submits its whole night in one
 * call, so this is generous — it exists to stop one request from becoming an unbounded fan-out
 * against the operator's key, not to shape normal traffic. */
export const MAX_ITEMS = 1000;

/** Ceiling on one chunk, in UTF-8 bytes. The largest legitimate chunk is a whole meeting
 * transcript folded into a single recap prompt; 192 KiB covers a very long meeting (a three-hour
 * meeting at speaking pace is ~150 KB of English). A chunk this long in CJK (~2 bytes/token) is a
 * ~98K-token prompt, which with the shared `system` lands on Haiku 5.5's long-prompt rate card:
 * that is priced, not prevented (spend.ts `LOW_RATE_CARD_MAX_PROMPT_TOKENS`). */
export const MAX_CHUNK_BYTES = 192 * 1024;

/** Ceiling on a `custom_id`. It is an opaque key the device chooses (an event id, a session
 * id) and is echoed back verbatim. */
export const MAX_CUSTOM_ID_BYTES = 256;

/** What the upstream Batch API accepts as a `custom_id`. A key outside this set (a `:` in a thread
 * key, say) makes the provider reject the whole create; checking it here turns that into a 400
 * before any reservation or spend, with a reason the device can log. */
export const CUSTOM_ID_PATTERN = /^[A-Za-z0-9_-]{1,64}$/;

/** Ceiling on `client_request_id`. Same order of magnitude as a Dream Cycle id; not a blob. */
export const MAX_CLIENT_REQUEST_ID_BYTES = 128;

/** Ceiling on the shared `system` instruction (#667). Today's is ~2.9 KB; this bounds what one
 * request can make the relay repeat into every forwarded item (the upstream body is items ×
 * system), not normal traffic. */
export const MAX_SYSTEM_BYTES = 64 * 1024;

/** Parse + validate an unknown JSON body into a RelayBatchRequest, or return an error string. */
export function parseRelayBatchRequest(body: unknown): RelayBatchRequest | string {
  if (typeof body !== "object" || body === null) return "body must be a JSON object";
  const b = body as Record<string, unknown>;
  if (typeof b.purpose !== "string" || b.purpose.length === 0) return "purpose must be a non-empty string";
  if (!isBatchPurpose(b.purpose)) return "purpose is not allowlisted";
  if (REVIEW_ONLY_PURPOSES.has(b.purpose)) return "purpose is review-only";
  if (!isModelClass(b.model_class)) return "model_class must be one of classify|summarize|brief|about_profile|daily_activity_summary";
  if (b.model_class !== MODEL_CLASS_BY_PURPOSE[b.purpose]) return "model_class does not match purpose";
  if (!Array.isArray(b.items) || b.items.length === 0) return "items must be a non-empty array";
  if (b.items.length > MAX_ITEMS) return `items must hold at most ${MAX_ITEMS} entries`;
  // #667: a shared `system` pairs with `{custom_id, data}` items, never with `chunk`. The two
  // shapes are exclusive so neither can be misread as the other: a `data` fragment forwarded
  // without its instruction would bill a batch of unparseable results, and an older relay that
  // does not know `system` rejects a `data` item ("each item needs a chunk") before any spend.
  let system: string | undefined;
  if (Object.prototype.hasOwnProperty.call(b, "system")) {
    if (typeof b.system !== "string" || b.system.length === 0) return "system must be a non-empty string";
    if (Buffer.byteLength(b.system, "utf8") > MAX_SYSTEM_BYTES) return "system is too large";
    system = b.system;
  }
  const textKey = system === undefined ? "chunk" : "data";
  const otherKey = system === undefined ? "data" : "chunk";
  const items: RelayBatchItem[] = [];
  const customIds = new Set<string>();
  for (const raw of b.items) {
    if (typeof raw !== "object" || raw === null) return "each item must be an object";
    const it = raw as Record<string, unknown>;
    if (typeof it.custom_id !== "string" || it.custom_id.length === 0) return "each item needs a custom_id";
    if (Buffer.byteLength(it.custom_id, "utf8") > MAX_CUSTOM_ID_BYTES) return "a custom_id is too long";
    if (!CUSTOM_ID_PATTERN.test(it.custom_id)) return "a custom_id must match [A-Za-z0-9_-]{1,64}";
    if (customIds.has(it.custom_id)) return "custom_ids must be unique";
    customIds.add(it.custom_id);
    if (Object.prototype.hasOwnProperty.call(it, otherKey)) {
      return system === undefined ? "an item with data needs a system" : "items with a system carry data, not chunk";
    }
    const text = it[textKey];
    if (typeof text !== "string" || text.length === 0) return `each item needs a ${textKey}`;
    if (Buffer.byteLength(text, "utf8") > MAX_CHUNK_BYTES) return `a ${textKey} is too large`;
    items.push({ custom_id: it.custom_id, chunk: text });
  }
  let clientRequestId: string;
  {
    if (typeof b.client_request_id !== "string" || b.client_request_id.length === 0) {
      return "client_request_id must be a non-empty string";
    }
    if (Buffer.byteLength(b.client_request_id, "utf8") > MAX_CLIENT_REQUEST_ID_BYTES) {
      return "client_request_id is too long";
    }
    clientRequestId = b.client_request_id;
  }
  const parsed: RelayBatchRequest = { purpose: b.purpose, model_class: b.model_class, items, clientRequestId };
  if (system !== undefined) parsed.system = system;
  return parsed;
}

/** Content-free fingerprint of a submit body. Stored for idempotency (#343); never logged. */
export function relayPayloadHash(req: RelayBatchRequest): string {
  // A body without `system` hashes exactly as before #667, so ids already bound in the ledger
  // keep replaying; with one, the instruction is part of the payload identity.
  const canonical = JSON.stringify({
    purpose: req.purpose,
    model_class: req.model_class,
    ...(req.system === undefined ? {} : { system: req.system }),
    items: req.items.map((it) => [it.custom_id, it.chunk]),
  });
  return createHash("sha256").update(canonical).digest("hex");
}

/** A `system` text block as Anthropic takes it. `cache_control` only ever comes from
 * `models.ts` (`systemBlocksFor`) — never from the device. */
export interface SystemTextBlock {
  type: "text";
  text: string;
  cache_control?: { type: "ephemeral"; ttl: "1h" };
}

/** One request line forwarded to Anthropic `POST /v1/messages/batches`. This is the whole
 * forwarded shape: no `temperature` / `top_p` / `top_k` / `thinking` / assistant prefill field
 * exists on it, so none can reach the provider (Haiku 5.5 returns 400 for any non-default value
 * of the first three and for a prefill). `parseRelayBatchRequest` copies named fields only, so a
 * device-sent sampling parameter is never read, let alone forwarded. */
export interface AnthropicBatchRequestItem {
  custom_id: string;
  params: {
    model: string;
    max_tokens: number;
    system?: SystemTextBlock[];
    messages: Array<{ role: "user"; content: string }>;
    output_config: {
      /** The relay's per-class effort (`EFFORT_BY_CLASS`), never the device's. */
      effort: "low" | "medium";
      /** Absent for a purpose sent without a forced shape (`SCHEMA_BY_PURPOSE`). */
      format?: { type: "json_schema"; schema: object };
    };
  };
}

/** Provider-reported token counts for one result, passed through so the device's meter is exact
 * (and a prompt-cache hit is visible at all, #667). Integers only — never content. */
export interface RelayUsage {
  input_tokens: number;
  output_tokens: number;
  cache_read_input_tokens: number;
  cache_creation_input_tokens: number;
}

/** A relay result entry (§4.3): text on success, error type otherwise. An error entry carries
 * `usage` when the provider billed the request anyway — a `succeeded` result that ended with
 * `stop_reason: "refusal"` (Haiku 5.5 safety classifiers), or one that ran out of `max_tokens`
 * inside its thinking and never wrote text — so settlement still books what was spent. The device
 * reads `error` and ignores the rest. */
export type RelayResult =
  | { custom_id: string; text: string; usage?: RelayUsage }
  | { custom_id: string; error: string; usage?: RelayUsage };
