/**
 * Jev form decisions on the company TypeSafe key (issue #1144, owner decision 2026-10-15 #1101 D12).
 *
 * `POST /v1/ai/form_decide` takes the body the Mac builds for TypeSafe (`forms/jev.rs`) minus
 * `model`, adds the pinned model and the company key, calls TypeSafe once and returns its JSON
 * unchanged. The Mac still parses the answer and applies its own confidence thresholds; the relay
 * never reads, rewrites or keeps the decision. Jev makes every field decision: there is no
 * fallback to a general chat model here or on the Mac.
 *
 * What stays out of every log and metric: the request body (blank-field labels, the facts the
 * user's own AI wrote, routing metadata), TypeSafe's answer and TypeSafe's error bodies. Only
 * the status, the token count and fixed words leave this file.
 */
import { UpstreamError } from "./gateway.js";

/** Pinned, never an alias: the Mac rejects an answer whose `model` differs (`jev.rs` `MODEL`). */
export const JEV_MODEL = "jev-1.13.0";
export const TYPESAFE_ENDPOINT = "https://api.typesafe.ai/v1/systemone";
/** The Mac's own request timeout for a form decision (`jev.rs`). */
export const FORM_DECIDE_TIMEOUT_MS = 45_000;
/** The Mac refuses to send more (`jev.rs` `MAX_BYTES`), so neither does the relay. */
export const FORM_DECIDE_MAX_BODY_BYTES = 128 * 1024;
/** Larger answers than this are not a Jev answer to a 128 KiB question; read no further. */
export const FORM_DECIDE_MAX_RESPONSE_BYTES = 1024 * 1024;
/** Requests one licence may make in an hour (both the Scribe intent check and the decisions). */
export const FORM_DECIDE_PER_HOUR = 30;
/** The window `FORM_DECIDE_PER_HOUR` counts in: a UTC clock hour. */
export const FORM_DECIDE_WINDOW_MS = 3_600_000;
/** Decisions one device may have in flight at once. */
export const FORM_DECIDE_MAX_IN_FLIGHT = 2;
/** TypeSafe's input price, USD per million tokens. Output is free. */
export const JEV_INPUT_USD_PER_MTOK = 0.042;
/** Reservation estimate: one token per this many request bytes (the issue's bytes ÷ 4). */
export const FORM_DECIDE_RESERVE_BYTES_PER_TOKEN = 4;

/** Most questions one request may carry (the Mac sends one per blank field, plus the purpose). */
const MAX_QUESTIONS = 512;
/** Most options one choice may carry (the Mac caps choices at 255 including `none`). */
const MAX_CRITERIA = 256;
const MAX_ID_BYTES = 256;

/** The two `state` shapes the Mac sends, by their exact key sets. */
const DECISION_STATE_KEYS = ["candidates", "fields", "intended_subject", "supporting_evidence"] as const;
const INTENT_STATE_KEYS = ["destination", "user_instruction"] as const;

export interface FormDecideRequest {
  state: Record<string, unknown>;
  questions: Record<string, unknown>;
}

const isObject = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);
const sameKeys = (o: Record<string, unknown>, keys: readonly string[]): boolean => {
  const own = Object.keys(o).sort();
  return own.length === keys.length && own.every((k, i) => k === keys[i]);
};
const idOk = (id: string): boolean => id.length > 0 && Buffer.byteLength(id, "utf8") <= MAX_ID_BYTES;

/**
 * The request contract, checked field by field: exactly `{state, questions}` (a `model` key, or
 * any other, is refused — the relay chooses the model), `state` in one of the two shapes the Mac
 * builds, and every question a `choice` with its instructions and options. Values inside a field
 * or a candidate are the Mac's data and pass through untouched. Returns the error string, never
 * echoing a value.
 */
export function parseFormDecideRequest(body: unknown): FormDecideRequest | string {
  if (!isObject(body)) return "body must be a JSON object";
  if (!sameKeys(body, ["questions", "state"])) return "body must hold exactly state and questions";
  const { state, questions } = body;
  if (!isObject(state)) return "state must be an object";
  if (sameKeys(state, DECISION_STATE_KEYS)) {
    if (!Array.isArray(state.fields) || !state.fields.every(isObject)) return "state.fields must be an array of objects";
    if (!Array.isArray(state.candidates) || !state.candidates.every(isObject)) return "state.candidates must be an array of objects";
    if (typeof state.intended_subject !== "string") return "state.intended_subject must be a string";
    if (!isObject(state.supporting_evidence) || !Object.values(state.supporting_evidence).every((v) => typeof v === "string")) {
      return "state.supporting_evidence must map ids to strings";
    }
  } else if (sameKeys(state, INTENT_STATE_KEYS)) {
    if (typeof state.user_instruction !== "string" || state.user_instruction === "") return "state.user_instruction must be a non-empty string";
    if (!isObject(state.destination)) return "state.destination must be an object";
  } else {
    return "state has an unknown shape";
  }
  if (!isObject(questions)) return "questions must be an object";
  const entries = Object.entries(questions);
  if (entries.length === 0 || entries.length > MAX_QUESTIONS) return `questions must hold 1 to ${MAX_QUESTIONS} entries`;
  for (const [id, q] of entries) {
    if (!idOk(id)) return "a question id is empty or too long";
    if (!isObject(q) || !sameKeys(q, ["criteria", "instructions", "type"])) return "each question holds exactly type, instructions and criteria";
    if (q.type !== "choice") return "each question must be a choice";
    if (typeof q.instructions !== "string" && !isObject(q.instructions)) return "question instructions must be a string or an object";
    if (!isObject(q.criteria)) return "question criteria must be an object";
    const options = Object.entries(q.criteria);
    if (options.length < 2 || options.length > MAX_CRITERIA) return `a choice must hold 2 to ${MAX_CRITERIA} options`;
    for (const [option, label] of options) {
      if (!idOk(option)) return "an option id is empty or too long";
      if (label !== null && typeof label !== "string" && !isObject(label)) return "an option must be a string, an object or null";
    }
  }
  return { state, questions };
}

/** Input tokens reserved for a request of `bytes` bytes. */
export function formDecideReserveTokens(bytes: number): number {
  return Math.ceil(bytes / FORM_DECIDE_RESERVE_BYTES_PER_TOKEN);
}

/** What TypeSafe answered, reduced to what the route needs. Bodies other than a 200's are never
 * read past their status. */
export type FormDecideUpstream =
  | { kind: "ok"; text: string; inputTokens?: number }
  | { kind: "status"; status: number; retryAfterSecs?: number };

/** One TypeSafe call: the request body is already serialized with the pinned model. Throws
 * `UpstreamError` on a transport failure or timeout (status 0). */
export type FormDecider = (body: string) => Promise<FormDecideUpstream>;

/** The usage TypeSafe reports, or undefined when the answer carries none. */
function inputTokensOf(text: string): number | undefined {
  try {
    const usage = (JSON.parse(text) as { usage?: { input_tokens?: unknown } } | null)?.usage;
    const n = usage?.input_tokens;
    return typeof n === "number" && Number.isSafeInteger(n) && n >= 0 ? n : undefined;
  } catch {
    return undefined;
  }
}

/** A `Retry-After` of whole seconds, or undefined (an HTTP date or junk is not followed). */
function retryAfterOf(res: Response): number | undefined {
  const raw = res.headers.get("retry-after")?.trim();
  if (!raw || !/^\d{1,6}$/.test(raw)) return undefined;
  return Number(raw);
}

/** Read at most `max` bytes of a body; `undefined` past it. */
async function boundedText(res: Response, max: number): Promise<string | undefined> {
  const declared = Number(res.headers.get("content-length") ?? "");
  if (Number.isFinite(declared) && declared > max) return undefined;
  const reader = res.body?.getReader();
  if (!reader) return "";
  const chunks: Uint8Array[] = [];
  let total = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > max) {
      await reader.cancel().catch(() => undefined);
      return undefined;
    }
    chunks.push(value);
  }
  return new TextDecoder().decode(Buffer.concat(chunks));
}

/** The company-key TypeSafe client. The key lives only in this closure (the Worker secret
 * `TYPESAFE_API_KEY`); it is never logged or returned. */
export function typesafeDecider(apiKey: string, endpoint = TYPESAFE_ENDPOINT): FormDecider {
  return async (body) => {
    let res: Response;
    try {
      res = await fetch(endpoint, {
        method: "POST",
        headers: { authorization: `Bearer ${apiKey}`, "content-type": "application/json" },
        signal: AbortSignal.timeout(FORM_DECIDE_TIMEOUT_MS),
        body,
      });
    } catch {
      throw new UpstreamError("form_decide", 0);
    }
    if (res.status !== 200) {
      // The error body can quote the request; it is dropped unread.
      await res.body?.cancel().catch(() => undefined);
      const retryAfterSecs = retryAfterOf(res);
      return retryAfterSecs === undefined ? { kind: "status", status: res.status } : { kind: "status", status: res.status, retryAfterSecs };
    }
    let text: string | undefined;
    try {
      text = await boundedText(res, FORM_DECIDE_MAX_RESPONSE_BYTES);
    } catch {
      throw new UpstreamError("form_decide (body)", 0);
    }
    if (text === undefined) throw new UpstreamError("form_decide (oversized answer)", 502);
    const inputTokens = inputTokensOf(text);
    return inputTokens === undefined ? { kind: "ok", text } : { kind: "ok", text, inputTokens };
  };
}
