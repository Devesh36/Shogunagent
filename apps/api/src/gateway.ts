/**
 * The Anthropic Batch API seam (docs/batch-relay-design.md §1 step ④).
 *
 * The app is written against this interface so every route is testable with an in-memory fake
 * and no socket. The real implementation holds the ONLY Anthropic key in the whole product
 * (server secret; the desktop binary never sees one) and passes chunk content straight through —
 * results are exposed as a line stream precisely so no caller is tempted to buffer a whole
 * results body into memory or, worse, onto disk (§3.2).
 */
import type { AnthropicBatchRequestItem } from "./types.js";

export interface BatchCounts {
  processing: number;
  succeeded: number;
  errored: number;
  canceled: number;
  expired: number;
}

export interface BatchStatus {
  id: string;
  processing_status: "in_progress" | "canceling" | "ended" | string;
  request_counts: BatchCounts;
}

/** `UpstreamError.status` when no HTTP response arrived at all: a DNS/TCP/TLS failure, a reset,
 * or our own request timeout. */
export const NO_RESPONSE = 0;

/** Upstream (Anthropic-side) failure. Mapped to 502 by the app — never echoed with body detail,
 * because upstream error bodies can quote request content. */
export class UpstreamError extends Error {
  constructor(
    public readonly step: string,
    public readonly status: number,
  ) {
    // Status and step only — deliberately no upstream body text, and no transport error text.
    super(status === NO_RESPONSE ? `anthropic ${step} failed without a response` : `anthropic ${step} failed with HTTP ${status}`);
    this.name = "UpstreamError";
  }

  /** A 4xx other than timeout proves this request was rejected before acceptance. A transport
   * failure (`NO_RESPONSE`) proves nothing: the create may have landed before the connection
   * died, so the claim stays unknown. */
  get definitelyRejected(): boolean {
    return this.status >= 400 && this.status < 500 && this.status !== 408;
  }
}

export interface AnthropicGateway {
  /** `POST /v1/messages/batches`. */
  createBatch(requests: AnthropicBatchRequestItem[]): Promise<BatchStatus>;
  /** `GET /v1/messages/batches/{id}`. */
  getBatch(id: string): Promise<BatchStatus>;
  /** `GET /v1/messages/batches/{id}/results`, yielded line by line (JSONL). Holds at most one
   * partial line in memory at a time; never writes anywhere. */
  streamResults(id: string): AsyncIterable<string>;
  /** Called once the DEVICE has fully read the results: drop the provider-side result files now
   * (invariant 3). Never called by the settlement sweep, which only reads usage. */
  releaseResults?(id: string): Promise<void>;
}

export const MAX_RESULT_LINE_BYTES = 1024 * 1024;
const REQUEST_TIMEOUT_MS = 30_000;

const ANTHROPIC_VERSION = "2023-06-01";

function parseCounts(v: unknown): BatchCounts {
  const c = (typeof v === "object" && v !== null ? v : {}) as Record<string, unknown>;
  const n = (k: string): number => (typeof c[k] === "number" ? (c[k] as number) : 0);
  return {
    processing: n("processing"),
    succeeded: n("succeeded"),
    errored: n("errored"),
    canceled: n("canceled"),
    expired: n("expired"),
  };
}

function parseStatus(step: string, status: number, body: unknown): BatchStatus {
  const b = (typeof body === "object" && body !== null ? body : {}) as Record<string, unknown>;
  if (typeof b.id !== "string" || typeof b.processing_status !== "string") {
    throw new UpstreamError(`${step} (malformed response)`, status);
  }
  return {
    id: b.id,
    processing_status: b.processing_status,
    request_counts: parseCounts(b.request_counts),
  };
}

/** Yield a JSONL response body line by line, holding at most one partial line; cancels the
 * upstream stream on any exit. Shared by every provider gateway. */
export async function* responseLines(stream: ReadableStream<Uint8Array>): AsyncIterable<string> {
  const decoder = new TextDecoder();
  const reader = stream.getReader();
  let carry = "";
  try {
    for (;;) {
      let read: Awaited<ReturnType<typeof reader.read>>;
      try {
        read = await reader.read();
      } catch {
        // The connection died mid-body: same shape as a fetch that never got a response.
        throw new UpstreamError("results", NO_RESPONSE);
      }
      const { done, value } = read;
      if (done) break;
      carry += decoder.decode(value, { stream: true });
      let nl: number;
      while ((nl = carry.indexOf("\n")) >= 0) {
        const line = carry.slice(0, nl);
        carry = carry.slice(nl + 1);
        if (Buffer.byteLength(line, "utf8") > MAX_RESULT_LINE_BYTES) throw new UpstreamError("results (oversized line)", 502);
        if (line.trim().length > 0) yield line;
      }
      if (Buffer.byteLength(carry, "utf8") > MAX_RESULT_LINE_BYTES) throw new UpstreamError("results (oversized line)", 502);
    }
    carry += decoder.decode();
    if (Buffer.byteLength(carry, "utf8") > MAX_RESULT_LINE_BYTES) throw new UpstreamError("results (oversized line)", 502);
    if (carry.trim().length > 0) yield carry;
  } finally {
    await reader.cancel().catch(() => undefined);
    reader.releaseLock();
  }
}

/** `fetch` that fails only as an `UpstreamError`. A rejected fetch (network failure, abort,
 * timeout) becomes `NO_RESPONSE`, so the app answers 502 'upstream failure' rather than a 500,
 * and the cause's text — which can carry the request URL — goes nowhere. */
export async function upstreamFetch(step: string, url: string, init: RequestInit): Promise<Response> {
  try {
    return await fetch(url, { ...init, signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS) });
  } catch {
    throw new UpstreamError(step, NO_RESPONSE);
  }
}

/** The JSON body of a 2xx, or an `UpstreamError` for a body that is cut off or is not JSON. The
 * status is the 2xx itself, so the failure is never read as a definite rejection. */
export async function upstreamJson(step: string, res: Response): Promise<unknown> {
  try {
    return await res.json();
  } catch {
    throw new UpstreamError(`${step} (malformed response)`, res.status);
  }
}

/** The real, fetch-based gateway. Constructed once at boot with the server-side key. */
export class FetchAnthropicGateway implements AnthropicGateway {
  constructor(
    private readonly apiKey: string,
    private readonly baseUrl: string = "https://api.anthropic.com",
  ) {}

  private headers(): Record<string, string> {
    return {
      "x-api-key": this.apiKey,
      "anthropic-version": ANTHROPIC_VERSION,
      "content-type": "application/json",
    };
  }

  async createBatch(requests: AnthropicBatchRequestItem[]): Promise<BatchStatus> {
    const res = await upstreamFetch("create", `${this.baseUrl}/v1/messages/batches`, {
      method: "POST",
      headers: this.headers(),
      body: JSON.stringify({ requests }),
    });
    if (!res.ok) throw new UpstreamError("create", res.status);
    return parseStatus("create", res.status, await upstreamJson("create", res));
  }

  async getBatch(id: string): Promise<BatchStatus> {
    const res = await upstreamFetch("poll", `${this.baseUrl}/v1/messages/batches/${encodeURIComponent(id)}`, {
      headers: this.headers(),
    });
    if (!res.ok) throw new UpstreamError("poll", res.status);
    return parseStatus("poll", res.status, await upstreamJson("poll", res));
  }

  async *streamResults(id: string): AsyncIterable<string> {
    const res = await upstreamFetch(
      "results",
      `${this.baseUrl}/v1/messages/batches/${encodeURIComponent(id)}/results`,
      { headers: this.headers() },
    );
    if (!res.ok || res.body === null) throw new UpstreamError("results", res.status);
    yield* responseLines(res.body);
  }

  /** `DELETE /v1/messages/batches/{id}`: removes the batch and its stored inputs and results at
   * the provider (otherwise kept 29 days). Called only after the device has read every result,
   * so a later poll or re-read answers 404/410 and the settlement sweep abandons it with its
   * estimate. A batch that is already gone (404) is the goal state. #1101 D11. */
  async releaseResults(id: string): Promise<void> {
    const res = await upstreamFetch("cleanup", `${this.baseUrl}/v1/messages/batches/${encodeURIComponent(id)}`, {
      method: "DELETE",
      headers: this.headers(),
    });
    if (!res.ok && res.status !== 404) throw new UpstreamError("cleanup", res.status);
  }
}
