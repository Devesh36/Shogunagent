/**
 * OpenAI Batch API provider (owner decision 2026-10-13, docs/batch-relay-design.md §4.4).
 *
 * Implements the same `AnthropicGateway` seam the app already uses, so the device and `app.ts`
 * never learn which provider ran: `createBatch` takes the Anthropic-shaped requests and translates
 * them to `/v1/chat/completions` lines; `streamResults` yields output lines translated BACK to the
 * Anthropic JSONL shape that `parseResultLine` already understands (text + usage + model).
 * Provider batch ids are prefixed `openai_` so `RoutingGateway` can send recovery/poll to the
 * right provider; Anthropic ids (`msgbatch_…`) stay unprefixed and keep working.
 */
import { NO_RESPONSE, UpstreamError, responseLines, upstreamFetch, upstreamJson } from "./gateway.js";
import type { AnthropicGateway, BatchStatus } from "./gateway.js";
import type { AnthropicBatchRequestItem } from "./types.js";
import { randomUUID } from "node:crypto";

/** Invariant 3: chunk content must not outlive the job at the provider. Our own file-name prefix keys
 * the orphan sweep; the provider-side expiries below are the backstop under the explicit deletes. */
export const FILE_PREFIX = "shogun-relay-";
const INPUT_TTL_SECS = 2 * 86_400; // outlives the 24h completion window
const OUTPUT_TTL_SECS = 86_400; // provider minimum is 3600; a day leaves room to read results
const ORPHAN_AGE_SECS = 86_400;

export const OPENAI_ID_PREFIX = "openai_";
/** `reasoning_effort` when the request names none. The relay's per-class `output_config.effort`
 * (models.ts `EFFORT_BY_CLASS`: `low` / `medium`) is forwarded as is — both are accepted values
 * (verified live 2026-10-13: none|low|medium|high|xhigh, not minimal). */
const REASONING_EFFORT = "low";

const isRec = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null;
const int = (v: unknown): number => (typeof v === "number" && Number.isSafeInteger(v) && v >= 0 ? v : 0);

/** One Anthropic-shaped request → one OpenAI batch input line. Content passes through. */
export function toChatLine(r: AnthropicBatchRequestItem): string {
  const messages: Array<{ role: string; content: string }> = [];
  const system = (r.params.system ?? []).map((b) => b.text).join("\n\n");
  if (system) messages.push({ role: "system", content: system });
  for (const m of r.params.messages) messages.push({ role: m.role, content: m.content });
  const fmt = r.params.output_config?.format;
  return JSON.stringify({
    custom_id: r.custom_id,
    method: "POST",
    url: "/v1/chat/completions",
    body: {
      model: r.params.model,
      reasoning_effort: r.params.output_config?.effort ?? REASONING_EFFORT,
      max_completion_tokens: r.params.max_tokens,
      // D11 zero retention: never let the provider keep the completion (explicit, whatever the
      // account's default).
      store: false,
      messages,
      ...(fmt ? { response_format: { type: "json_schema", json_schema: { name: "out", strict: false, schema: fmt.schema } } } : {}),
    },
  });
}

/** One OpenAI output/error line → one Anthropic-shaped results line. */
export function fromOutputLine(line: string): string {
  let o: unknown;
  try { o = JSON.parse(line); } catch { return line; } // parseResultLine reports it as malformed
  const rec = isRec(o) ? o : {};
  const customId = typeof rec.custom_id === "string" ? rec.custom_id : "";
  const res = isRec(rec.response) ? rec.response : undefined;
  const body = res && isRec(res.body) ? res.body : undefined;
  if (!res || res.status_code !== 200 || !body) {
    return JSON.stringify({ custom_id: customId, result: { type: "errored" } });
  }
  const choice = Array.isArray(body.choices) && isRec(body.choices[0]) ? body.choices[0] : {};
  const msg = isRec(choice.message) ? choice.message : {};
  const text = typeof msg.content === "string" ? msg.content : "";
  const u = isRec(body.usage) ? body.usage : {};
  const cached = int(isRec(u.prompt_tokens_details) ? u.prompt_tokens_details.cached_tokens : 0);
  return JSON.stringify({
    custom_id: customId,
    result: {
      type: "succeeded",
      message: {
        model: typeof body.model === "string" ? body.model : undefined,
        content: [{ type: "text", text }],
        usage: {
          input_tokens: Math.max(0, int(u.prompt_tokens) - cached),
          output_tokens: int(u.completion_tokens),
          cache_read_input_tokens: cached,
          cache_creation_input_tokens: 0,
        },
      },
    },
  });
}

export class OpenAiBatchGateway implements AnthropicGateway {
  constructor(
    private readonly apiKey: string,
    private readonly baseUrl: string = "https://api.openai.com",
    private readonly log: (line: string) => void = () => undefined,
  ) {}

  /** Best-effort delete. A missing file (404) is already gone. The log line carries the status
   * alone, never a file or batch id (worker.ts: Workers Logs sees no id); the orphan sweep and the
   * provider-side expiry retry whatever this misses. */
  private async deleteFile(fileId: unknown): Promise<void> {
    if (typeof fileId !== "string") return;
    try {
      const res = await upstreamFetch("cleanup", `${this.baseUrl}/v1/files/${encodeURIComponent(fileId)}`, { method: "DELETE", headers: this.auth() });
      if (!res.ok && res.status !== 404) this.log(`openai_file_delete_failed status=${res.status}`);
    } catch {
      this.log("openai_file_delete_failed status=0");
    }
  }

  /** Delete leftover files of terminal batches older than 24h whose input carries our prefix
   * (cancelled/expired/failed jobs nobody polled again). One page per call. */
  async sweepOrphans(nowSecs = Math.floor(Date.now() / 1000)): Promise<number> {
    const res = await upstreamFetch("sweep", `${this.baseUrl}/v1/batches?limit=100`, { headers: this.auth() });
    if (!res.ok) throw new UpstreamError("sweep", res.status);
    const list = await upstreamJson("sweep", res);
    const data = isRec(list) && Array.isArray(list.data) ? list.data : [];
    let swept = 0;
    for (const b of data) {
      if (!isRec(b) || !["completed", "failed", "expired", "cancelled"].includes(String(b.status))) continue;
      if (typeof b.created_at !== "number" || nowSecs - b.created_at < ORPHAN_AGE_SECS || typeof b.input_file_id !== "string") continue;
      const meta = await upstreamFetch("sweep", `${this.baseUrl}/v1/files/${encodeURIComponent(b.input_file_id)}`, { headers: this.auth() });
      if (meta.status === 404) { // input already gone: still drop any result files
        await this.deleteFile(b.output_file_id); await this.deleteFile(b.error_file_id); continue;
      }
      const f = meta.ok ? await upstreamJson("sweep", meta) : undefined;
      if (!isRec(f) || typeof f.filename !== "string" || !f.filename.startsWith(FILE_PREFIX)) continue;
      for (const id of [b.input_file_id, b.output_file_id, b.error_file_id]) await this.deleteFile(id);
      // Counted, not logged per batch: boot.ts logs the total (`openai_orphans_swept count=`).
      swept++;
    }
    return swept;
  }

  private auth(): Record<string, string> {
    return { authorization: `Bearer ${this.apiKey}` };
  }

  private async batch(step: string, rawId: string): Promise<Record<string, unknown>> {
    const id = rawId.startsWith(OPENAI_ID_PREFIX) ? rawId.slice(OPENAI_ID_PREFIX.length) : rawId;
    const res = await upstreamFetch(step, `${this.baseUrl}/v1/batches/${encodeURIComponent(id)}`, { headers: this.auth() });
    if (!res.ok) throw new UpstreamError(step, res.status);
    const b = await upstreamJson(step, res);
    if (!isRec(b) || typeof b.id !== "string" || typeof b.status !== "string") throw new UpstreamError(`${step} (malformed response)`, res.status);
    return b;
  }

  async createBatch(requests: AnthropicBatchRequestItem[]): Promise<BatchStatus> {
    const form = new FormData();
    form.set("purpose", "batch");
    form.set("expires_after[anchor]", "created_at");
    form.set("expires_after[seconds]", String(INPUT_TTL_SECS));
    form.set("file", new Blob([requests.map(toChatLine).join("\n") + "\n"], { type: "application/jsonl" }), `${FILE_PREFIX}${randomUUID()}.jsonl`);
    const up = await upstreamFetch("create", `${this.baseUrl}/v1/files`, { method: "POST", headers: this.auth(), body: form });
    if (!up.ok) throw new UpstreamError("create", up.status);
    const file = await upstreamJson("create", up);
    if (!isRec(file) || typeof file.id !== "string") throw new UpstreamError("create (malformed response)", up.status);
    const res = await upstreamFetch("create", `${this.baseUrl}/v1/batches`, {
      method: "POST",
      headers: { ...this.auth(), "content-type": "application/json" },
      body: JSON.stringify({ input_file_id: file.id, endpoint: "/v1/chat/completions", completion_window: "24h", output_expires_after: { anchor: "created_at", seconds: OUTPUT_TTL_SECS } }),
    });
    if (!res.ok) throw new UpstreamError("create", res.status);
    const b = await upstreamJson("create", res);
    if (!isRec(b) || typeof b.id !== "string" || typeof b.status !== "string") throw new UpstreamError("create (malformed response)", res.status);
    return toStatus(b);
  }

  async getBatch(id: string): Promise<BatchStatus> {
    return toStatus(await this.batch("poll", id));
  }

  async *streamResults(id: string): AsyncIterable<string> {
    const b = await this.batch("results", id);
    for (const key of ["output_file_id", "error_file_id"]) {
      const fileId = b[key];
      if (typeof fileId !== "string") continue;
      const res = await upstreamFetch("results", `${this.baseUrl}/v1/files/${encodeURIComponent(fileId)}/content`, { headers: this.auth() });
      if (!res.ok || res.body === null) throw new UpstreamError("results", res.status);
      for await (const line of responseLines(res.body)) yield fromOutputLine(line);
    }
    // Reached only when every line was read. Only the INPUT goes now: this generator also runs for
    // the settlement sweep (boot.ts), which reads before the device does, so deleting the result
    // files here would make the device's own read 404. They go in releaseResults once the device has
    // read them; provider expiry (OUTPUT_TTL_SECS) and sweepOrphans are the backstop.
    await this.deleteFile(b.input_file_id);
  }

  async releaseResults(id: string): Promise<void> {
    const b = await this.batch("results", id);
    for (const k of ["output_file_id", "error_file_id"]) await this.deleteFile(b[k]);
  }
}

function toStatus(b: Record<string, unknown>): BatchStatus {
  const status = String(b.status);
  const ended = ["completed", "failed", "expired", "cancelled"].includes(status);
  const c = isRec(b.request_counts) ? b.request_counts : {};
  const total = int(c.total), succeeded = int(c.completed), errored = int(c.failed);
  return {
    id: OPENAI_ID_PREFIX + String(b.id),
    processing_status: ended ? "ended" : status === "cancelling" ? "canceling" : "in_progress",
    request_counts: { processing: ended ? 0 : Math.max(0, total - succeeded - errored), succeeded, errored, canceled: 0, expired: 0 },
  };
}

/** Sends each call to the provider that owns it: creates by the request model, reads by id prefix. */
export class RoutingGateway implements AnthropicGateway {
  constructor(
    private readonly anthropic: AnthropicGateway | undefined,
    private readonly openai: AnthropicGateway | undefined,
  ) {}

  private pick(g: AnthropicGateway | undefined, step: string): AnthropicGateway {
    if (!g) throw new UpstreamError(step, NO_RESPONSE); // provider not configured on this relay
    return g;
  }
  private byId(id: string, step: string): AnthropicGateway {
    return this.pick(id.startsWith(OPENAI_ID_PREFIX) ? this.openai : this.anthropic, step);
  }

  async createBatch(requests: AnthropicBatchRequestItem[]): Promise<BatchStatus> {
    const oai = requests[0]?.params.model.startsWith("gpt-") === true;
    return await this.pick(oai ? this.openai : this.anthropic, "create").createBatch(requests);
  }
  getBatch(id: string): Promise<BatchStatus> {
    return this.byId(id, "poll").getBatch(id);
  }
  streamResults(id: string): AsyncIterable<string> {
    return this.byId(id, "results").streamResults(id);
  }
  releaseResults(id: string): Promise<void> {
    return this.byId(id, "results").releaseResults?.(id) ?? Promise.resolve();
  }
}
