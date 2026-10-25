/**
 * Synchronous hourly review: one plain OpenAI chat completion, answered inline. No job, no id, no
 * poll, no stored items — the only thing persisted is the spend booking (app.ts `/v1/review`).
 *
 * **Billing (audit 2026-10-20 C1).** The provider bills a call whether or not its answer is any
 * use to us: an answer cut off at `max_completion_tokens`, a refusal, a content-filter stop and an
 * invalid JSON answer are all billed in full. So the reviewer reads `usage` from every response
 * before it decides anything about the answer, sums it over every attempt, and reports it with
 * the outcome — success or failure — as a `ReviewBilling`. An attempt whose bill cannot be read (a
 * timeout, a dropped connection, a 5xx, a 2xx without `usage`) makes the billing incomplete, and
 * the route then keeps its whole reservation booked: an unknown bill never reads as zero.
 */
import { UpstreamError } from "./gateway.js";
import { LUNA_MODEL } from "./models.js";
import type { RelayUsage } from "./types.js";

export const REVIEW_TIMEOUT_MS = 90_000;
/** Big enough that low-effort reasoning cannot eat the answer. */
export const REVIEW_MAX_COMPLETION_TOKENS = 16_000;
/** At most this many provider calls per review: the first, plus one retry on an empty or invalid
 * JSON answer. The route reserves spend for every one of them. */
export const REVIEW_MAX_ATTEMPTS = 2;

/** The model refused this content (a content filter or an explicit refusal) or answered it
 * unusably twice. Resending the same request fails the same way, so the route answers 422 with
 * this reason — never the 502 of an outage, which the device retries. */
export class ReviewRefused extends Error {
  constructor(public readonly reason: "upstream_refused" | "upstream_unusable") {
    super(reason);
    this.name = "ReviewRefused";
  }
}

/** What the provider billed for one review, across every attempt.
 *
 * - `sent: false` — no attempt reached the provider, so nothing can have been billed.
 * - `sent: true, complete: true` — every attempt either reported its `usage` or was definitely
 *   rejected (a 4xx other than 408: refused before any work, not billed). `usage` is the sum, so it
 *   is exactly what was billed.
 * - `sent: true, complete: false` — at least one attempt's bill is unknown. `usage` is only the
 *   known part; the caller keeps its whole reservation booked. */
export type ReviewBilling =
  | { sent: false }
  | { sent: true; complete: boolean; usage: RelayUsage };

export interface ReviewResult {
  json: unknown;
  /** The usage of every attempt, summed. */
  usage: RelayUsage;
  /** Absent = `{ sent: true, complete: true, usage }` (a reviewer that made one call). */
  billing?: ReviewBilling;
}
export type Reviewer = (system: string, user: string, schema: object) => Promise<ReviewResult>;

export const zeroUsage = (): RelayUsage => ({ input_tokens: 0, output_tokens: 0, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 });

export function addUsage(a: RelayUsage, b: RelayUsage): RelayUsage {
  return {
    input_tokens: a.input_tokens + b.input_tokens,
    output_tokens: a.output_tokens + b.output_tokens,
    cache_read_input_tokens: a.cache_read_input_tokens + b.cache_read_input_tokens,
    cache_creation_input_tokens: a.cache_creation_input_tokens + b.cache_creation_input_tokens,
  };
}

/** The billing a reviewer's failure carries. An error without one (a reviewer that did not say)
 * reads as "sent, bill unknown", so the route keeps the whole reservation. */
export function reviewBillingOf(e: unknown): ReviewBilling {
  const b = typeof e === "object" && e !== null ? (e as { reviewBilling?: ReviewBilling }).reviewBilling : undefined;
  return b ?? { sent: true, complete: false, usage: zeroUsage() };
}

/** Attach `billing` to an error the reviewer throws. Non-enumerable: never serialised or logged. */
export function withReviewBilling<E extends Error>(e: E, billing: ReviewBilling): E {
  Object.defineProperty(e, "reviewBilling", { value: billing, enumerable: false });
  return e;
}

const int = (v: unknown): number => (typeof v === "number" && Number.isSafeInteger(v) && v >= 0 ? v : 0);

/** One attempt. `usage` undefined = this attempt reached the provider and its bill is unknown. */
type Attempt =
  | { kind: "answer"; json: unknown; usage: RelayUsage | undefined }
  /** `error` is thrown if this is the last attempt; `retry` = an empty / invalid answer worth one
   * more call. */
  | { kind: "failed"; usage: RelayUsage | undefined; error: Error; retry: boolean };

export function openAiReviewer(apiKey: string, baseUrl = "https://api.openai.com"): Reviewer {
  const once = async (system: string, user: string, schema: object): Promise<Attempt> => {
    let res: Response;
    try {
      res = await fetch(`${baseUrl}/v1/chat/completions`, {
        method: "POST",
        headers: { authorization: `Bearer ${apiKey}`, "content-type": "application/json" },
        signal: AbortSignal.timeout(REVIEW_TIMEOUT_MS),
        body: JSON.stringify({
          model: LUNA_MODEL,
          reasoning_effort: "low",
          max_completion_tokens: REVIEW_MAX_COMPLETION_TOKENS,
          // D11 zero retention: never let the provider keep the completion (explicit, whatever
          // the account's default).
          store: false,
          messages: [{ role: "system", content: system }, { role: "user", content: user }],
          // ponytail: strict:false like the batch lane — the shared schemas are not strict-mode clean.
          response_format: { type: "json_schema", json_schema: { name: "out", strict: false, schema } },
        }),
      });
    } catch {
      // A timeout or a dropped connection: the request may have been sent and the model may have
      // run to its full budget. The bill is unknown.
      return { kind: "failed", usage: undefined, error: new UpstreamError("review", 0), retry: false };
    }
    if (!res.ok) {
      // Only a content-filter 400 is about the content; every other status is an outage. The body
      // is read for its code alone and never logged or echoed.
      const code = res.status === 400 ? ((await res.json().catch(() => undefined)) as any)?.error?.code : undefined;
      const upstream = new UpstreamError("review", res.status);
      const error = code === "content_filter" || code === "content_policy_violation" ? new ReviewRefused("upstream_refused") : upstream;
      // A 4xx other than 408 is refused before any work and is not billed; a 5xx or 408 can follow
      // work the provider did, so its bill is unknown.
      return { kind: "failed", usage: upstream.definitelyRejected ? zeroUsage() : undefined, error, retry: false };
    }
    let body: Record<string, any>;
    try { body = (await res.json()) as Record<string, any>; } catch {
      return { kind: "failed", usage: undefined, error: new UpstreamError("review (malformed response)", res.status), retry: false };
    }
    // The bill first, before anything decides the answer is unusable: a refused, cut-off or
    // invalid answer is billed all the same.
    const u = typeof body?.usage === "object" && body.usage !== null ? body.usage : undefined;
    let usage: RelayUsage | undefined;
    if (u) {
      const cached = int(u.prompt_tokens_details?.cached_tokens);
      usage = {
        input_tokens: Math.max(0, int(u.prompt_tokens) - cached),
        output_tokens: int(u.completion_tokens),
        cache_read_input_tokens: cached,
        cache_creation_input_tokens: 0,
      };
    }
    const choice = body?.choices?.[0];
    if ((typeof choice?.message?.refusal === "string" && choice.message.refusal !== "") || choice?.finish_reason === "content_filter") {
      return { kind: "failed", usage, error: new ReviewRefused("upstream_refused"), retry: false };
    }
    // Cut off at the answer budget: the window was too big for one answer, not bad content.
    // An outage keeps its events for a retry; a refusal would abandon the hour.
    if (choice?.finish_reason === "length") {
      return { kind: "failed", usage, error: new UpstreamError("review (answer cut off)", 502), retry: false };
    }
    const text = choice?.message?.content;
    const unusable: Attempt = { kind: "failed", usage, error: new ReviewRefused("upstream_unusable"), retry: true };
    if (typeof text !== "string" || text.trim() === "") return unusable;
    try {
      return { kind: "answer", json: JSON.parse(text), usage };
    } catch {
      return unusable;
    }
  };
  return async (system, user, schema) => {
    let known = zeroUsage();
    let complete = true;
    for (let attempt = 1; ; attempt++) {
      const a = await once(system, user, schema);
      if (a.usage) known = addUsage(known, a.usage);
      else complete = false;
      if (a.kind === "answer") return { json: a.json, usage: known, billing: { sent: true, complete, usage: known } };
      if (!a.retry || attempt >= REVIEW_MAX_ATTEMPTS) throw withReviewBilling(a.error, { sent: true, complete, usage: known });
    }
  };
}
