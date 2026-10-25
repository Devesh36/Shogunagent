/**
 * The AI gateway app (docs/batch-relay-design.md §4) — a Hono app with injected seams so every
 * route is testable offline.
 *
 * Invariants carried by construction:
 * - **No chunk content persists here** (§3.2 / NFR-PRV-04): the redacting logger is registered
 *   before any route; the UsageStore interface has no content field; results stream through a
 *   line-at-a-time transform, never a buffered body on disk.
 * - **The device never picks the model** (§4.4): the request carries `model_class`; the mapping
 *   lives in `models.ts`. A `model` key in the request body is simply never read.
 * - **Errors follow §4.5**: 401 invalid/expired token, 402 plan without the lane, 429 daily cap
 *   or rate limit, 502 upstream failure.
 * - **Two kinds of 429, told apart by the response (§4.5, issue #29 FR-SVC-02):**
 *   - *transient* (`reason: "rate_limited"`) carries `Retry-After`; the device waits that long and
 *     retries the same request. Every 503 carries `Retry-After` too.
 *   - *cap reached* (`reason: "daily_cap"`, `"spend_cap"` for the global breaker, and
 *     `"license_spend_cap"` for the licence's own daily budget — audit 2026-10-10 H1) carries NO
 *     `Retry-After`. The only refill is the UTC date changing, so there is nothing worth retrying
 *     tonight: the device gives the night to the local lane on the first one. A `Retry-After` here
 *     would invite a client to sleep until midnight UTC and resubmit outside the Dream window.
 */
import { Hono, type Context, type MiddlewareHandler } from "hono";
import { bodyLimit } from "hono/body-limit";
import { createHash, timingSafeEqual, type KeyObject } from "node:crypto";

import { createAgentApp, type AgentDeps } from "./agent-app.js";
import { AuthError, TRIAL_PLAN, deniedByOperator, isExpiredTrialToken, meterSubject, verifyLicense, type License, type TrustedKeys } from "./auth.js";
import type { AnthropicGateway } from "./gateway.js";
import { UpstreamError } from "./gateway.js";
import { redactingLogger, routeTemplate, type LogFn, type LogVars } from "./logging.js";
import { RelayMetrics } from "./metrics.js";
import { EFFORT_BY_CLASS, MAX_TOKENS_BY_CLASS, MODEL_BY_CLASS, systemBlocksFor } from "./models.js";
import { outputFormatFor } from "./schemas.js";
import { InMemoryInFlight, NoRateLimit, WindowLimiter, type InFlightLimiter, type RateLimiter } from "./ratelimit.js";
import {
  FORM_DECIDE_MAX_BODY_BYTES, FORM_DECIDE_MAX_IN_FLIGHT, FORM_DECIDE_PER_HOUR, FORM_DECIDE_WINDOW_MS, JEV_MODEL,
  formDecideReserveTokens, parseFormDecideRequest, type FormDecider, type FormDecideUpstream,
} from "./form-decide.js";
import { REVIEW_MAX_ATTEMPTS, REVIEW_MAX_COMPLETION_TOKENS, ReviewRefused, reviewBillingOf, type ReviewBilling, type Reviewer } from "./review.js";
import { reserveInputTokens, LUNA_MODEL } from "./models.js";
import { randomUUID } from "node:crypto";
import { DEFAULT_BATCH_PRICES, estimateSubmissionMicros, priceFor, usageMicros, type SpendBreaker } from "./spend.js";
import { MAX_CHUNK_BYTES, MAX_CLIENT_REQUEST_ID_BYTES, MAX_CUSTOM_ID_BYTES, MAX_SYSTEM_BYTES, parseRelayBatchRequest, relayPayloadHash, type AnthropicBatchRequestItem, type RelayResult, type RelayUsage } from "./types.js";
import { isBatchPurpose } from "./types.js";
import { LedgerUnavailable, utcDate, type IdempotencyKey, type UsageStore } from "./usage.js";

/** Ceiling on a submission body. MAX_ITEMS × MAX_CHUNK_BYTES is far larger than any real batch;
 * this is the number that actually bounds what one request can make the process buffer. */
export const MAX_BODY_BYTES = 8 * 1024 * 1024;

/** How much of the daily spend cap trial submissions may reach, unless configured. */
export const DEFAULT_TRIAL_SPEND_SHARE = 0.5;

/** Ceiling on a recovery body: a purpose and a request id, nothing else. */
export const MAX_RECOVER_BODY_BYTES = 1024;

/** Context vars of the relay app: the log line's, plus the licence the auth-first middleware
 * verified for the POST routes. */
export interface RelayVars extends LogVars {
  relayLicense?: License;
}

export interface RelayDeps {
  gateway: AnthropicGateway;
  usage: UsageStore;
  /** The licence API's Ed25519 public key (§4.1 — the SAME key that signs the `v1.` licence
   * tokens; see `licensePublicKeyFromB64`), or the current + next pair during a key rotation
   * (`releaseTrustedKeysFromEnv`). */
  licensePublicKey: KeyObject | TrustedKeys;
  /** Per-license daily chunk cap (OPEN-B2: the concrete number comes from the cost model; the
   * enforcement lives here either way). */
  dailyChunkCap: number;
  /** What one trial payer (`plan: "trial"`, issue #763; its account, or its Mac without `acc` — `meterSubject`) may send in a day. Never more than
   * `dailyChunkCap`. A trial token is minted for any device id, with no proof of an install, so
   * the relay serves it only when BOTH of its ceilings exist: this cap and `spendBreaker`.
   * Without either, every trial token is refused with 402. */
  trialDailyChunkCap?: number;
  /** The share (0 to 1) of `spendBreaker`'s cap that trial submissions may reach. A trial
   * submission is refused (`spend_cap`) once the day's total spend, paid and trial together,
   * would pass it. The rest of the budget is therefore only ever spent by paying users: however
   * many trial tokens are minted, they cannot stop a paid submission. */
  trialSpendShare?: number;
  /** Per-licence request rate limit. Defaults to no limit, for deployments that do this at the
   * edge; `index.ts` wires a token bucket. */
  rateLimit?: RateLimiter;
  log: LogFn;
  /** Injected clock for tests. */
  now?: () => Date;
  /** Global daily spend breaker (FR-SVC-08). Absent = disabled. Production refuses to boot
   * without it (boot.ts `spendCapRequirementError`); only a development relay runs this way. */
  spendBreaker?: SpendBreaker;
  /** Per-licence daily spend budget in micro-USD (audit 2026-10-10 H1): the most one licence may
   * reserve against the company key in a UTC day, whatever the global breaker has left. Absent =
   * no per-licence budget (tests); `index.ts` always sets it. */
  licenseSpendCapMicros?: number;
  /** The daily spend budget of one trial payer (account or Mac, `meterSubject`). Never more than `licenseSpendCapMicros`. */
  trialSpendCapMicros?: number;
  /** In-process metrics (FR-SVC-06). One is created when absent; `index.ts` passes its own so it
   * can also log periodic snapshots. */
  metrics?: RelayMetrics;
  /** Synchronous hourly review (`POST /v1/review`). Absent = the route answers 503. */
  reviewer?: Reviewer;
  /** Bearer token for `GET /metrics`. Absent = the endpoint answers 404 (ops-only, off by default). */
  metricsToken?: string;
  /** Licence ids / metering subjects refused before their token expires (`RELAY_DENY_SUBJECTS`,
   * audit 2026-10-20 L3; `deniedByOperator`). */
  denySubjects?: readonly string[];
  /** Jev form decisions on the company TypeSafe key (`POST /v1/ai/form_decide`, issue #1144).
   * Absent (no `TYPESAFE_API_KEY`) = the route answers 503 and the Mac fills nothing. */
  formDecider?: FormDecider;
  /** The per-payer hourly limit of the form route (default: in-process, 30 per hour). */
  formRateLimit?: RateLimiter;
  /** The per-device in-flight limit of the form route (default: in-process, 2). */
  formInFlight?: InFlightLimiter;
  /** The agent-facing routes (agent-app.ts: `sa_` keys, wallet, 402, caps, quote, receipts, the
   * metered `/mcp` proxy — plan E3 / E9). Absent = those routes do not exist on this relay. */
  agent?: Omit<AgentDeps, "log" | "now">;
}

/** The form route's rate key: the payer alone (`meterSubject`: the licence, or a trial's
 * account — audit 2026-10-20 H1), so every Mac of one payer shares the hour. */
export function formRateKey(license: License): string {
  return `${meterSubject(license)}\u0000form_decide`;
}

/** The form route's in-flight key: one device of one payer. */
export function formInFlightKey(license: License): string {
  return `${meterSubject(license)}\u0000${license.deviceId}`;
}

/** The four token counts of an Anthropic `usage` object, integers only; `undefined` when the
 * provider sent none. Nothing else from the object is copied. */
function usageCounts(v: unknown): RelayUsage | undefined {
  if (typeof v !== "object" || v === null) return undefined;
  const u = v as Record<string, unknown>;
  if (typeof u.input_tokens !== "number" && typeof u.output_tokens !== "number") return undefined;
  const n = (k: string): number => {
    const x = u[k];
    return typeof x === "number" && Number.isSafeInteger(x) && x >= 0 ? x : 0;
  };
  return {
    input_tokens: n("input_tokens"),
    output_tokens: n("output_tokens"),
    cache_read_input_tokens: n("cache_read_input_tokens"),
    cache_creation_input_tokens: n("cache_creation_input_tokens"),
  };
}

/** Map one Anthropic JSONL results line to the relay's result shape (§4.3). Content passes
 * through; nothing is retained. Malformed lines surface as error entries, never silent drops.
 * Token counts ride along (#667) so the device can meter exactly and see prompt-cache hits. */
export function transformResultLine(line: string): RelayResult {
  return parseResultLine(line).result;
}

/** `transformResultLine` plus the model that produced the result, which the spend breaker needs
 * to price the reported usage (FR-SVC-08). The model id is not content. */
export function parseResultLine(line: string): { result: RelayResult; model?: string } {
  let v: unknown;
  try {
    v = JSON.parse(line);
  } catch {
    return { result: { custom_id: "", error: "malformed result line" } };
  }
  const o = (typeof v === "object" && v !== null ? v : {}) as Record<string, unknown>;
  const customId = typeof o.custom_id === "string" ? o.custom_id : "";
  const result = (typeof o.result === "object" && o.result !== null ? o.result : {}) as Record<string, unknown>;
  const type = typeof result.type === "string" ? result.type : "missing result.type";
  if (type !== "succeeded") return { result: { custom_id: customId, error: type } };
  const message = (typeof result.message === "object" && result.message !== null ? result.message : {}) as Record<string, unknown>;
  const usage = usageCounts(message.usage);
  const model = typeof message.model === "string" ? message.model : undefined;
  const withModel = (out: RelayResult) => (model === undefined ? { result: out } : { result: out, model });
  // Haiku 5.5's safety classifiers decline with a normal `succeeded` result whose `stop_reason` is
  // "refusal" and whose content holds no answer; there is no server-side fallback on Batch. It is
  // an error entry for the device (never an empty "" verdict to parse) and keeps its usage, since
  // the prompt was billed. The category is not content, but it is not useful to the device either.
  if (message.stop_reason === "refusal") {
    const out: RelayResult = { custom_id: customId, error: "refusal" };
    return withModel(usage === undefined ? out : { ...out, usage });
  }
  // Read blocks by type, never by position: with adaptive thinking (the 5.5 models) the content
  // can begin with one or more `thinking` blocks, whose text is empty and whose tokens are already
  // in `usage.output_tokens`. Only `text` blocks are the answer.
  const content = Array.isArray(message.content) ? message.content : [];
  const textBlocks = content.flatMap((b: unknown) => {
    const block = (typeof b === "object" && b !== null ? b : {}) as Record<string, unknown>;
    return block.type === "text" && typeof block.text === "string" ? [block.text] : [];
  });
  // Thinking-only truncation: the model spent the whole `max_tokens` (or stopped for another
  // reason) before writing any text. An empty `text` would parse on the device as a malformed
  // verdict with no hint why; the stop reason (`max_tokens`, a category, never content) is the
  // error, and the usage stays because the request was billed in full.
  if (textBlocks.length === 0 && message.stop_reason !== "end_turn") {
    const reason = typeof message.stop_reason === "string" && message.stop_reason !== "" ? message.stop_reason : "no text";
    const out: RelayResult = { custom_id: customId, error: reason };
    return withModel(usage === undefined ? out : { ...out, usage });
  }
  const text = textBlocks.join("");
  return withModel(usage === undefined ? { custom_id: customId, text } : { custom_id: customId, text, usage });
}

/** The rate-limit bucket a request draws from: payer + device + route.
 *
 * All three matter. **Payer** is the metering subject (`meterSubject`): the licence, or for a
 * trial issued through an account the account (audit 2026-10-20 H1), so the bucket and the
 * budgets it paces name the same payer. **Device** keeps one Mac's pace from starving another's
 * under the same payer, and bounds a copied token to the pace of the Mac it was issued to (see
 * `License.deviceId`); the budgets, not the pace, are what bound the money. **Route** keeps
 * submission and polling in separate buckets — sharing one means a Dream Cycle polling its own
 * batch every few seconds eats the allowance the next submission needs, and the lane starves
 * itself with no abuse involved. */
export function bucketKey(license: License, route: string): string {
  return `${meterSubject(license)}\u0000${license.deviceId}\u0000${route}`;
}

/** The global spend ceiling a licence reserves against: the breaker's whole cap for a paid
 * licence, only the trial share of it for a trial token. Anything that is not a share reads as 0,
 * never as "all of it". Shared by the Batch lane, the review and the speech grants (audit
 * 2026-10-20 H3), so no lane lets a trial reach the paid users' part of the day. */
export function planSpendCapMicros(license: License, capMicros: number, trialSpendShare: number = DEFAULT_TRIAL_SPEND_SHARE): number {
  if (license.plan !== TRIAL_PLAN) return capMicros;
  return Number.isFinite(trialSpendShare) ? Math.floor(capMicros * Math.min(1, Math.max(0, trialSpendShare))) : 0;
}

/** Seconds until the UTC day rolls over — when a daily cap refills, and nowhere sooner. */
export function secsUntilUtcMidnight(now: Date): number {
  const midnight = Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate() + 1);
  return Math.max(1, Math.ceil((midnight - now.getTime()) / 1000));
}

/** The `reason` code of a 429 (§4.5). Clients branch on the ABSENCE of `Retry-After`
 * first — any 429 without one is "cap reached" — and read `reason` for the ledger/indicator. */
export type TooManyReason = "rate_limited" | "daily_cap" | "spend_cap" | "license_spend_cap";

/** Transient 429: back off for `retryAfterSecs`, then the same request may succeed. */
export function rateLimitedResponse(retryAfterSecs: number): Response {
  return Response.json({ error: "too many requests", reason: "rate_limited" satisfies TooManyReason }, {
    status: 429,
    // At least one second: `Retry-After: 0` reads as "retry now", the loop this exists to stop.
    headers: { "retry-after": String(Math.max(1, Math.ceil(retryAfterSecs))) },
  });
}

/** Cap-reached 429: deliberately no `Retry-After` (see the module header). `resets_at` is
 * informational only — the UTC instant the cap refills. */
export function capReachedResponse(reason: Exclude<TooManyReason, "rate_limited">, now: Date): Response {
  const resetsAt = new Date(now.getTime() + secsUntilUtcMidnight(now) * 1000).toISOString();
  const error = reason === "daily_cap" ? "daily chunk cap reached"
    : reason === "spend_cap" ? "daily spend cap reached" : "daily licence spend cap reached";
  return Response.json({ error, reason, resets_at: resetsAt }, { status: 429 });
}

/** The request's Workers execution context, or `undefined` where there is none (the Node entry,
 * `app.request` in tests). Hono's getter throws in that case rather than returning undefined. */
export function executionContextOf(c: Context): { waitUntil(promise: Promise<unknown>): void } | undefined {
  try {
    return c.executionCtx;
  } catch {
    return undefined;
  }
}

export function createApp(deps: RelayDeps): Hono<{ Variables: RelayVars }> {
  const app = new Hono<{ Variables: RelayVars }>();
  const now = deps.now ?? (() => new Date());

  // FIRST middleware: the only logger. Registered before any route exists so no handler can be
  // reached without it, and it cannot see bodies at all (logging.ts).
  app.use("*", redactingLogger(deps.log));

  // Per-route RPS / p95 / 429 / 5xx (FR-SVC-06). Route templates only, never the raw path.
  // The trial lane (issue #763) is open only with both ceilings configured; see `RelayDeps`.
  const trialChunkCap =
    deps.spendBreaker !== undefined && deps.trialDailyChunkCap !== undefined
      ? Math.min(deps.trialDailyChunkCap, deps.dailyChunkCap)
      : undefined;
  const metrics =
    deps.metrics ??
    new RelayMetrics(deps.dailyChunkCap, trialChunkCap === undefined ? {} : { [TRIAL_PLAN]: trialChunkCap });
  app.use("*", async (c, next) => {
    const start = performance.now();
    try {
      await next();
    } finally {
      metrics.observe(routeTemplate(new URL(c.req.url).pathname), c.res.status, performance.now() - start, now().getTime());
    }
  });

  app.onError((err, c) => {
    if (err instanceof AuthError) {
      return c.json({ error: err.message }, err.status);
    }
    if (err instanceof ReviewRefused) {
      // The model refused or could not answer this content: 422, so the device stops resending
      // it instead of retrying it like an outage. The reason is a fixed word.
      deps.log(`review_refused reason=${err.reason}`);
      return c.json({ error: err.reason }, 422);
    }
    if (err instanceof UpstreamError) {
      // Status only — upstream bodies can quote request content and never reach a log or client.
      // Step and status are fixed words and numbers: the cause (status 0 = no response, e.g. the
      // network is down right after a wake) so a 502 is diagnosable from the relay log.
      deps.log(`upstream_failure step=${err.step} status=${err.status}`);
      return c.json({ error: "upstream failure" }, 502);
    }
    if (err instanceof LedgerUnavailable) {
      // Never overwrite a lifecycle event the handler already recorded: a bind that fails after
      // the provider accepted the batch must log `provider_accepted_unbound` (the operator's cue
      // that a provider job exists without a durable owner), not a generic metering failure.
      if (c.get("relayLogEvent") === undefined) c.set("relayLogEvent", "metering_unavailable");
      return c.json({ error: "metering unavailable" }, 503, { "retry-after": "1" });
    }
    // Unknown failure: a fixed string. err.message could contain interpolated request data.
    return c.json({ error: "internal error" }, 500);
  });

  const nowSecs = (): number => Math.floor(now().getTime() / 1000);
  const limiter = deps.rateLimit ?? new NoRateLimit();

  // The agent routes, behind the same logger and metrics middleware as everything else. Their
  // log line is the same shape (route template, status, counters, `key=<hash>`).
  if (deps.agent) app.route("/", createAgentApp({ ...deps.agent, log: deps.log, now }));

  /** Verify the bearer, for every authenticated route. A valid trial token is still refused
   * (402, before any metering or provider call) on a relay whose trial lane is closed.
   *
   * A trial whose trial is over (`isExpiredTrialToken`: `exp` and the signed `period_end` both
   * past) is 402 `plan_required`, not the 401 of a stale token (#940): the Mac maps 402 to "the
   * plan does not cover this lane" and stops, where a 401 sends it to re-verify first (one more
   * licence-API round trip that can only fail for an ended trial). Every shipped relay client maps
   * 402 that way (`relay_status_error`, since the lane shipped), and both answers end the night on
   * the local lane, so no client loops on either. A trial token that only ran out mid-trial stays
   * 401, so the Mac still refreshes it (issue #777). */
  const authorize = (authorization: string | undefined): License => {
    let license: License;
    try {
      license = verifyLicense(authorization, deps.licensePublicKey, nowSecs());
    } catch (e) {
      if (e instanceof AuthError && e.status === 401 && isExpiredTrialToken(authorization, deps.licensePublicKey, nowSecs())) {
        throw new AuthError("plan_required", 402);
      }
      throw e;
    }
    // The operator's emergency denylist (L3): the same 401 as an expired token, so the device
    // re-verifies its licence, which the licence API then refuses.
    if (deniedByOperator(license, deps.denySubjects)) throw new AuthError("invalid or expired license token", 401);
    if (license.plan === TRIAL_PLAN && trialChunkCap === undefined) {
      throw new AuthError("trial tokens are not enabled on this relay", 402);
    }
    return license;
  };
  /** Authenticate and take a rate-limit slot BEFORE anything reads the body. Registered ahead of
   * `bodyLimit` on the POST routes, so an unauthenticated (or rate-limited) client is answered
   * 401/402/429 without the relay buffering a byte of its body — otherwise anyone could make
   * the process hold up to `MAX_BODY_BYTES` per connection with no licence at all. */
  const authenticateFirst = (route: "batch_submit" | "batch_poll" | "review" | "form_decide"): MiddlewareHandler<{ Variables: RelayVars }> =>
    async (c, next) => {
      const license = route === "form_decide" ? authorizeForm(c.req.header("authorization")) : authorize(c.req.header("authorization"));
      c.set("relayLogLicense", license.licenseId);
      const key = bucketKey(license, route);
      const nowMs = now().getTime();
      if (!(await limiter.take(key, nowMs))) {
        return rateLimitedResponse(await limiter.retryAfterSecs(key, nowMs));
      }
      if (route === "form_decide") {
        // The route's own hourly allowance per payer (`formRateKey`), also before a byte of the
        // body is read.
        const formKey = formRateKey(license);
        if (!(await formLimiter.take(formKey, nowMs))) {
          return rateLimitedResponse(await formLimiter.retryAfterSecs(formKey, nowMs));
        }
      }
      c.set("relayLicense", license);
      await next();
    };
  /** The form route's auth: the same licence check, but the device acts on one word. A plan the
   * route does not cover, and a trial token whose trial is over (`authorize`), answer 402
   * `plan_required` (the Mac's "Get a plan to continue", #940) — never "add your key". */
  const authorizeForm = (authorization: string | undefined): License => {
    try {
      return authorize(authorization);
    } catch (e) {
      if (e instanceof AuthError && e.status === 402) throw new AuthError("plan_required", 402);
      throw e;
    }
  };
  const formLimiter = deps.formRateLimit ?? new WindowLimiter(FORM_DECIDE_PER_HOUR, FORM_DECIDE_WINDOW_MS);
  const formInFlight = deps.formInFlight ?? new InMemoryInFlight(FORM_DECIDE_MAX_IN_FLIGHT);
  /** The licence `authenticateFirst` verified. A route registered without it fails closed. */
  const authenticated = (c: Context<{ Variables: RelayVars }>): License => {
    const license = c.get("relayLicense");
    if (!license) throw new AuthError("missing bearer license token", 401);
    return license;
  };
  const bodyTooLarge = (c: Context): Response => c.json({ error: "body too large" }, 413);

  /** The daily chunk cap this licence reserves against. */
  const chunkCapFor = (license: License): number =>
    license.plan === TRIAL_PLAN ? (trialChunkCap ?? 0) : deps.dailyChunkCap;
  /** The spend ceiling this licence reserves against: the breaker's cap, or for a trial token
   * only its share of it. Anything that is not a share reads as 0, never as "all of it". */
  const spendCapFor = (license: License, breaker: SpendBreaker): number =>
    planSpendCapMicros(license, breaker.capMicros, deps.trialSpendShare);
  /** The licence's own daily spend budget (H1), or none when the deployment set none. A trial
   * token gets the trial budget, never more than the licence budget. */
  const licenseSpendCapFor = (license: License): number | undefined => {
    const cap = deps.licenseSpendCapMicros;
    if (cap === undefined) return undefined;
    if (license.plan !== TRIAL_PLAN) return cap;
    return Math.min(cap, deps.trialSpendCapMicros ?? cap);
  };
  /** Whether any spend budget exists; when one does, every submission books an estimate that the
   * results read and the settlement sweep (boot.ts) later replace with the provider's usage. */
  const spendBooked = deps.spendBreaker !== undefined || deps.licenseSpendCapMicros !== undefined;
  const prices = deps.spendBreaker?.prices ?? DEFAULT_BATCH_PRICES;

  // Liveness / readiness. Deliberately the dullest endpoint in the process:
  //
  // - **No auth**, because a health check that needs a licence cannot be used by the thing that
  //   restarts the container.
  // - **No provider call.** A health check that pings Anthropic spends the operator's key on
  //   every probe, and turns a provider outage into a restart loop that makes it worse.
  // - **No configuration in the body.** Not the key, not its length, not the model table, not the
  //   caps. An unauthenticated endpoint that reports how the gateway is configured is a
  //   reconnaissance tool; there is nothing here an attacker can learn from a probe — the body
  //   is a liveness answer and nothing else.
  app.get("/healthz", (c) => c.json({ ok: true }));

  // Ops-only metrics (FR-SVC-06). Off (404) unless METRICS_TOKEN is configured, and then only for
  // that bearer — unlike /healthz, this body does describe the deployment (caps, traffic), which
  // is reconnaissance for anyone else. Numbers only; no ids, no content.
  const metricsDigest = deps.metricsToken ? createHash("sha256").update(deps.metricsToken).digest() : undefined;
  app.get("/metrics", async (c) => {
    if (!metricsDigest) return c.json({ error: "not found" }, 404);
    const presented = (c.req.header("authorization") ?? "").replace(/^Bearer\s+/i, "");
    const digest = createHash("sha256").update(presented).digest();
    if (!timingSafeEqual(digest, metricsDigest)) return c.json({ error: "unauthorized" }, 401);
    const date = utcDate(now());
    let spend: { date: string; spentMicros: number; capMicros: number } | undefined;
    if (deps.spendBreaker) {
      const spentMicros = await deps.usage.spentOn(date).catch(() => -1);
      spend = { date, spentMicros, capMicros: deps.spendBreaker.capMicros };
    }
    return c.json(metrics.snapshot(now().getTime(), spend), 200, { "cache-control": "no-store" });
  });

  // Order matters: authenticate (and rate-limit) first, then bound the body, then read it. An
  // unbounded `c.req.json()` is a one-request OOM; a bounded one behind no auth is still 8 MiB
  // per anonymous connection.
  app.post("/v1/batch", authenticateFirst("batch_submit"), bodyLimit({ maxSize: MAX_BODY_BYTES, onError: bodyTooLarge }), async (c) => {
    const license = authenticated(c);

    let raw: unknown;
    try {
      raw = await c.req.json();
    } catch {
      return c.json({ error: "body must be JSON" }, 400);
    }
    const parsed = parseRelayBatchRequest(raw);
    if (typeof parsed === "string") {
      return c.json({ error: parsed }, 400);
    }

    const date = utcDate(now());
    const idemKey: IdempotencyKey = {
      licenseId: license.licenseId, deviceId: license.deviceId,
      purpose: parsed.purpose, clientRequestId: parsed.clientRequestId,
    };
    const claim = await deps.usage.claimIdempotency({ ...idemKey, payloadHash: relayPayloadHash(parsed), date });
    if (claim.kind === "unavailable") {
      c.set("relayLogEvent", "metering_unavailable");
      return c.json({ error: "metering unavailable" }, 503, { "retry-after": "1" });
    }
    if (claim.kind === "fresh") idemKey.claimToken = claim.claimToken;
    if (claim.kind === "replay") {
      c.set("relayLogEvent", "duplicate_prevented");
      c.set("relayLogChunks", parsed.items.length);
      return c.json({ batch_id: claim.batchId, accepted: parsed.items.length, state: "running" }, 202);
    }
    if (claim.kind === "conflict") {
      c.set("relayLogEvent", "payload_conflict");
      return c.json({ error: "client_request_id conflict" }, 409);
    }
    if (claim.kind === "unknown") {
      c.set("relayLogEvent", "submission_unknown");
      return c.json({ error: "submission unknown", state: "submission_unknown", batch_id: claim.batchId }, 503, { "retry-after": "1" });
    }
    // Forward with the server-chosen model (§4.4) and the custom_ids untouched. A shared
    // instruction becomes the same `system` blocks on every request — built once, so the
    // prefix (tools → system) is byte-identical across the batch and the per-item text follows
    // it in the user turn (#667). Built before the reservation: the spend breaker prices it.
    // `output_config` carries the relay's effort for the class (how much the model thinks; a
    // cost lever, so server-side like the model) and the purpose's forced shape. Nothing else
    // from the device reaches `params`: no sampling parameter, no `thinking`, no prefill
    // (`AnthropicBatchRequestItem`).
    const system = parsed.system === undefined ? undefined : systemBlocksFor(parsed.model_class, parsed.system);
    const format = outputFormatFor(parsed.purpose);
    const outputConfig = { effort: EFFORT_BY_CLASS[parsed.model_class], ...(format === undefined ? {} : { format }) };
    const requests: AnthropicBatchRequestItem[] = parsed.items.map((it) => ({
      custom_id: it.custom_id,
      params: {
        model: MODEL_BY_CLASS[parsed.model_class],
        max_tokens: MAX_TOKENS_BY_CLASS[parsed.model_class],
        ...(system === undefined ? {} : { system }),
        messages: [{ role: "user", content: it.chunk }],
        output_config: outputConfig,
      },
    }));

    // Daily cap (§2.2 — the reason the relay exists: the limit is enforced where the key lives).
    // Reserved BEFORE the upstream call and released if that call fails, so N simultaneous
    // submissions cannot each read the same "used" and all pass. The global spend breaker
    // (FR-SVC-08) and the licence's own spend budget (H1) are reserved in the same transaction.
    // Without a breaker (development only) the global ceiling is simply unbounded.
    // Budgets are metered on the payer (`meterSubject`, audit 2026-10-20 H1): one account's trial
    // devices share one chunk cap and one spend budget. Ownership (`idemKey`) stays licence + device.
    const meter = meterSubject(license);
    const licenseCap = licenseSpendCapFor(license);
    const spend = !spendBooked ? undefined : {
      batchId: claim.batchId,
      micros: estimateSubmissionMicros(parsed.model_class, requests, prices),
      capMicros: deps.spendBreaker === undefined ? Number.MAX_SAFE_INTEGER : spendCapFor(license, deps.spendBreaker),
      ...(licenseCap === undefined ? {} : { licenseCapMicros: licenseCap }),
    };
    const reservation = await deps.usage.tryReserve(
      meter,
      date,
      parsed.items.length,
      chunkCapFor(license),
      idemKey,
      spend,
    );
    metrics.reservation(date, license.plan, meter, reservation, parsed.items.length);
    if (reservation === "unknown") {
      // The claim was lost between claim and reserve. Log what the device is told, so an alert on
      // recurring unknown submissions sees it (docs/batch-relay-design.md).
      c.set("relayLogEvent", "submission_unknown");
      return c.json({ error: "submission unknown", state: "submission_unknown", batch_id: claim.batchId }, 503, { "retry-after": "1" });
    }
    if (reservation === "capped" || reservation === "spend_capped" || reservation === "license_spend_capped") {
      await deps.usage.releaseIdempotency(idemKey);
      // The cap refills at the UTC date boundary and nowhere else: nothing is retryable tonight,
      // so no Retry-After — that absence is what tells the device "cap reached" (§4.5).
      const capped = {
        capped: { event: "cap_reached", reason: "daily_cap" },
        spend_capped: { event: "spend_cap_reached", reason: "spend_cap" },
        license_spend_capped: { event: "license_spend_cap_reached", reason: "license_spend_cap" },
      } as const;
      c.set("relayLogEvent", capped[reservation].event);
      return capReachedResponse(capped[reservation].reason, now());
    }
    if (reservation === "unavailable") {
      c.set("relayLogEvent", "metering_unavailable");
      // No provider call has happened. Refund/reopen only this token's unbound
      // reservation in one transaction; never use this after entering createBatch.
      await deps.usage.cancelReservation(idemKey, parsed.items.length, meter);
      return c.json({ error: "metering unavailable" }, 503, { "retry-after": "1" });
    }

    c.set("relayLogReservedChunks", parsed.items.length);
    let created;
    try {
      created = await deps.gateway.createBatch(requests);
    } catch (e) {
      c.set("relayLogEvent", "submission_unknown");
      // Only a non-timeout 4xx is a definite rejection. A transport, 5xx, or malformed-2xx
      // failure can happen after Anthropic accepted the create, so retain reservations fail closed.
      if (e instanceof UpstreamError && e.definitelyRejected) {
        c.set("relayLogEvent", "provider_rejected");
        await deps.usage.release(meter, date, parsed.items.length);
        await deps.usage.releaseIdempotency(idemKey);
      }
      throw e;
    }

    // Provider identity and owner become visible in one durable transaction. Never expose a
    // handle whose ownership was not saved; a failed write leaves the original claim unknown.
    c.set("relayLogEvent", "provider_accepted_unbound");
    const batchId = await deps.usage.bindIdempotency(idemKey, created.id);
    c.set("relayLogEvent", "running");
    c.set("relayLogChunks", parsed.items.length);
    c.set("relayLogModel", MODEL_BY_CLASS[parsed.model_class]);
    return c.json({ batch_id: batchId, accepted: parsed.items.length, state: "running" }, 202);
  });

  // Hourly review: one synchronous model call, answered inline. Same licence auth and spend
  // reservation as /v1/batch, but its own rate bucket: a backfill's many small calls must not eat
  // the allowance batch submissions need (and the reverse). Nothing is stored but the spend booking.
  app.post("/v1/review", authenticateFirst("review"), bodyLimit({ maxSize: MAX_BODY_BYTES, onError: bodyTooLarge }), async (c) => {
    const license = authenticated(c);
    if (!deps.reviewer) return c.json({ error: "review unavailable" }, 503);
    let raw: Record<string, unknown>;
    try { raw = (await c.req.json()) as Record<string, unknown>; } catch { return c.json({ error: "body must be JSON" }, 400); }
    const { purpose, system, user } = raw ?? {};
    const format = isBatchPurpose(purpose) ? outputFormatFor(purpose) : undefined;
    if (!format || typeof system !== "string" || typeof user !== "string" || user === "") {
      return c.json({ error: "purpose, system and user are required" }, 400);
    }
    // The Batch lane's per-field ceilings (types.ts): a review is one Batch item sent inline, so
    // its shared instruction is a Batch `system` and its text a Batch chunk. Checked before any
    // reservation, so an oversized request costs nothing. The Mac's largest review (the hourly
    // window: ≤ 150,000 bytes of sessions per call, a ≤ 24,000-byte roster in `system`) fits both.
    if (Buffer.byteLength(system, "utf8") > MAX_SYSTEM_BYTES) return c.json({ error: "system is too large" }, 400);
    if (Buffer.byteLength(user, "utf8") > MAX_CHUNK_BYTES) return c.json({ error: "user is too large" }, 400);
    // Sync list price is twice the batch card; each attempt may use the whole completion budget,
    // and the reviewer may make REVIEW_MAX_ATTEMPTS of them (audit 2026-10-20 C1).
    const price = priceFor(prices, LUNA_MODEL);
    const perAttempt = Math.ceil(2 * (
      (reserveInputTokens(system) + reserveInputTokens(user) + reserveInputTokens(JSON.stringify(format.schema))) * price.input +
      REVIEW_MAX_COMPLETION_TOKENS * price.output));
    const reserved = perAttempt * REVIEW_MAX_ATTEMPTS;
    const date = utcDate(now());
    // The payer (audit 2026-10-20 H1): one account's trial devices share one budget.
    const meter = meterSubject(license);
    const licenseCap = licenseSpendCapFor(license);
    const batchId = `sync_${randomUUID()}`;
    const reservation = await deps.usage.tryReserve(meter, date, 1, chunkCapFor(license), undefined,
      !spendBooked ? undefined : {
        batchId, micros: reserved,
        capMicros: deps.spendBreaker === undefined ? Number.MAX_SAFE_INTEGER : spendCapFor(license, deps.spendBreaker),
        ...(licenseCap === undefined ? {} : { licenseCapMicros: licenseCap }),
      });
    if (reservation === "capped" || reservation === "spend_capped" || reservation === "license_spend_capped") {
      const reason = { capped: "daily_cap", spend_capped: "spend_cap", license_spend_capped: "license_spend_cap" } as const;
      c.set("relayLogEvent", "cap_reached");
      return capReachedResponse(reason[reservation], now());
    }
    if (reservation !== "ok") return c.json({ error: "metering unavailable" }, 503, { "retry-after": "1" });
    /** Close the reservation on what the provider billed, never below it (audit 2026-10-20 C1):
     * - never sent: nothing was billed; the chunk and the whole estimate come back;
     * - every attempt's bill known: book their summed usage (a refused, cut-off or invalid answer
     *   included) and keep the chunk;
     * - any bill unknown (a timeout, a dropped connection, a 5xx): keep the whole reservation as
     *   the final figure (or the known part, if that is larger) and keep the chunk. */
    const close = async (billing: ReviewBilling): Promise<void> => {
      if (!billing.sent) {
        await deps.usage.release(meter, date, 1);
        if (spendBooked) await deps.usage.settleSpend(batchId, 0).catch(() => deps.log("spend_settle_failure"));
        return;
      }
      if (!spendBooked) return;
      const known = Math.ceil(2 * usageMicros(billing.usage, price));
      if (!billing.complete) deps.log("review_bill_unknown (reservation kept)");
      await deps.usage.settleSpend(batchId, billing.complete ? known : Math.max(reserved, known)).catch(() => deps.log("spend_settle_failure"));
    };
    let out;
    try {
      out = await deps.reviewer(system, user, format.schema);
    } catch (e) {
      await close(reviewBillingOf(e));
      throw e;
    }
    await close(out.billing ?? { sent: true, complete: true, usage: out.usage });
    c.set("relayLogModel", LUNA_MODEL);
    return c.json({ json: out.json, usage: out.usage });
  });

  // Jev form decisions on the company TypeSafe key (issue #1144, #1101 D12). The Mac sends what
  // it would send TypeSafe minus `model`; the relay adds the pinned model and the key, calls once
  // and returns TypeSafe's JSON unchanged. Nothing is stored but the spend booking, and no body —
  // the request's or TypeSafe's — reaches a log line or a metric.
  app.post("/v1/ai/form_decide", authenticateFirst("form_decide"), bodyLimit({ maxSize: FORM_DECIDE_MAX_BODY_BYTES, onError: bodyTooLarge }), async (c) => {
    const license = authenticated(c);
    if (!deps.formDecider) {
      c.set("relayLogEvent", "form_decide_unavailable");
      return c.json({ error: "form_decide_unavailable" }, 503, { "retry-after": "60" });
    }
    let raw: unknown;
    try { raw = await c.req.json(); } catch { return c.json({ error: "body must be JSON" }, 400); }
    const parsed = parseFormDecideRequest(raw);
    if (typeof parsed === "string") return c.json({ error: parsed }, 400);
    // The relay picks the model, pinned. The device's body never carried one (a `model` key is
    // refused above), so this is the only place it is set.
    const body = JSON.stringify({ model: JEV_MODEL, state: parsed.state, questions: parsed.questions });

    const nowMs = now().getTime();
    const releaseSlot = await formInFlight.acquire(formInFlightKey(license), nowMs);
    if (!releaseSlot) {
      c.set("relayLogEvent", "in_flight_capped");
      return rateLimitedResponse(2);
    }
    try {
      const price = priceFor(prices, JEV_MODEL);
      const reserved = Math.ceil(formDecideReserveTokens(Buffer.byteLength(body, "utf8")) * price.input);
      const date = utcDate(now());
      // The payer (audit 2026-10-20 H1): one account's trial devices share one budget, as on
      // `/v1/review` and the Batch lane.
      const meter = meterSubject(license);
      const licenseCap = licenseSpendCapFor(license);
      const batchId = `form_${randomUUID()}`;
      // The chunk counter is the Batch lane's budget: a form decision holds one only for the
      // length of the call (released below) and is never stopped by it. Spend is what it books.
      const reservation = await deps.usage.tryReserve(meter, date, 1, Number.MAX_SAFE_INTEGER, undefined,
        !spendBooked ? undefined : {
          batchId, micros: reserved,
          capMicros: deps.spendBreaker === undefined ? Number.MAX_SAFE_INTEGER : spendCapFor(license, deps.spendBreaker),
          ...(licenseCap === undefined ? {} : { licenseCapMicros: licenseCap }),
        });
      if (reservation === "capped" || reservation === "spend_capped" || reservation === "license_spend_capped") {
        const reason = { capped: "daily_cap", spend_capped: "spend_cap", license_spend_capped: "license_spend_cap" } as const;
        c.set("relayLogEvent", "cap_reached");
        return capReachedResponse(reason[reservation], now());
      }
      if (reservation !== "ok") {
        c.set("relayLogEvent", "metering_unavailable");
        return c.json({ error: "metering unavailable" }, 503, { "retry-after": "1" });
      }
      /** Close the reservation on what TypeSafe billed, never below it (as `/v1/review` does
       * after audit 2026-10-20 C1): the reported usage when the answer carries it; zero for a
       * definite rejection (a 4xx other than 408 — no decision was made); the whole estimate
       * whenever the bill is unknown (no answer, a timeout, a 5xx, an answer without usage). */
      const settle = async (micros: number): Promise<void> => {
        if (spendBooked) await deps.usage.settleSpend(batchId, micros).catch(() => deps.log("spend_settle_failure"));
      };
      let out: FormDecideUpstream;
      try {
        out = await deps.formDecider(body);
      } catch (e) {
        // No answer at all (a transport failure or the 45 s timeout) throws UpstreamError → 502,
        // never an empty decision. TypeSafe may still have billed it: the estimate is the bill.
        await settle(reserved);
        throw e;
      } finally {
        await deps.usage.release(meter, date, 1).catch(() => undefined);
      }
      c.set("relayLogModel", JEV_MODEL);
      if (out.kind === "ok") {
        await settle(out.inputTokens === undefined ? reserved : Math.ceil(out.inputTokens * price.input));
        return new Response(out.text, { status: 200, headers: { "content-type": "application/json", "cache-control": "no-store" } });
      }
      // A status answer was not a decision. Its body was never read.
      const definitelyRejected = out.status >= 400 && out.status < 500 && out.status !== 408;
      if (!definitelyRejected) deps.log("form_bill_unknown (reservation kept)");
      await settle(definitelyRejected ? 0 : reserved);
      switch (out.status) {
        case 401:
        case 403:
          // The company key was refused. The Mac must not be told to add a key of its own: the
          // operator rotates `TYPESAFE_API_KEY` (docs/ai-gateway-runbook.md). Alert line.
          deps.log(`ALERT typesafe_key_rejected status=${out.status}`);
          c.set("relayLogEvent", "typesafe_key_rejected");
          return c.json({ error: "form_decide_unavailable" }, 503, { "retry-after": "60" });
        case 429:
          c.set("relayLogEvent", "upstream_rate_limited");
          return rateLimitedResponse(out.retryAfterSecs ?? 30);
        case 529:
          c.set("relayLogEvent", "upstream_overloaded");
          return c.json({ error: "overloaded", reason: "overloaded" }, 503, { "retry-after": String(Math.max(1, out.retryAfterSecs ?? 30)) });
        case 422:
          return c.json({ error: "form_request_invalid" }, 422);
        default:
          throw new UpstreamError("form_decide", out.status);
      }
    } finally {
      await releaseSlot();
    }
  });

  // Recovery carries only a request id and purpose, never deleted input or a rebuilt prompt.
  app.post("/v1/batch/recover", authenticateFirst("batch_poll"), bodyLimit({ maxSize: MAX_RECOVER_BODY_BYTES, onError: bodyTooLarge }), async (c) => {
    const license = authenticated(c);
    let raw: unknown;
    try { raw = await c.req.json(); } catch { return c.json({ error: "body must be JSON" }, 400); }
    if (!raw || typeof raw !== "object") return c.json({ error: "invalid recovery request" }, 400);
    const { purpose, client_request_id: clientRequestId, retire_if_missing: retireIfMissing } = raw as Record<string, unknown>;
    if ((retireIfMissing !== undefined && typeof retireIfMissing !== "boolean") || typeof purpose !== "string" || !purpose || purpose.length > MAX_CUSTOM_ID_BYTES ||
        typeof clientRequestId !== "string" || !clientRequestId ||
        Buffer.byteLength(clientRequestId, "utf8") > MAX_CLIENT_REQUEST_ID_BYTES) {
      return c.json({ error: "invalid recovery request" }, 400);
    }
    const recovered = await deps.usage.recoverIdempotency({
      licenseId: license.licenseId, deviceId: license.deviceId, purpose, clientRequestId,
    }, retireIfMissing === true, utcDate(now()));
    if (recovered.kind === "retired") return c.json({ found: false, retired: true });
    if (recovered.kind === "missing") return c.json({ found: false });
    if (recovered.kind === "unknown") {
      c.set("relayLogEvent", "submission_unknown");
      return c.json({ error: "submission unknown", state: "submission_unknown", batch_id: recovered.batchId }, 503, { "retry-after": "1" });
    }
    if (await deps.usage.batchOwner(recovered.batchId, license.deviceId) !== license.licenseId) {
      return c.json({ error: "submission unavailable" }, 503);
    }
    return c.json({ found: true, batch_id: recovered.batchId });
  });

  app.get("/v1/batch/:id", async (c) => {
    const license = authorize(c.req.header("authorization"));
    c.set("relayLogLicense", license.licenseId);
    const pollKey = bucketKey(license, "batch_poll");
    const pollNowMs = now().getTime();
    if (!(await limiter.take(pollKey, pollNowMs))) {
      return rateLimitedResponse(await limiter.retryAfterSecs(pollKey, pollNowMs));
    }
    const id = c.req.param("id");

    // Ownership check (§4.1): a licence may only read back batches IT created. Anthropic batch
    // ids are guessable, and without this any licensee could stream any other user's Dream-Cycle
    // results. "Unknown" and "someone else's" answer identically — 404, no oracle.
    const owner = await deps.usage.batchOwner(id, license.deviceId);
    if (owner !== license.licenseId) {
      return c.json({ error: "not found" }, 404);
    }

    const providerId = await deps.usage.providerBatchId(id);
    if (!providerId) {
      c.set("relayLogEvent", "submission_unknown");
      return c.json({ status: "submission_unknown", state: "submission_unknown" });
    }
    const status = await deps.gateway.getBatch(providerId);
    if (status.processing_status !== "ended") {
      const counts = status.request_counts;
      const total =
        counts.processing + counts.succeeded + counts.errored + counts.canceled + counts.expired;
      return c.json({
        status: status.processing_status,
        state: "running",
        completed: counts.succeeded + counts.errored,
        total,
      });
    }

    // Ended: stream the results through, one transformed line at a time (§4.3). Pull-based so
    // the relay holds at most one result line in memory — the response is built as the client
    // reads it, and nothing is retained afterwards (§3.2).
    const lines = deps.gateway.streamResults(providerId)[Symbol.asyncIterator]();
    // Pull the first line BEFORE answering 200: an unreadable result must be a retryable 503, not a
    // 200 that is aborted mid-body (the device saw that as a transport decode error).
    let primed: IteratorResult<string> | undefined;
    try { primed = await lines.next(); }
    catch (e) {
      deps.log("batch_result_stream_failure");
      await lines.return?.().catch(() => undefined);
      const gone = e instanceof UpstreamError && (e.status === 404 || e.status === 410);
      // Nothing succeeded and the provider has no result file to give (an all-errored OpenAI
      // batch): that is a finished, failed batch, not a missing one. Say so, so the device marks
      // it failed instead of re-reading a 410 forever.
      if (gone && status.request_counts.succeeded === 0) {
        c.set("relayLogEvent", "batch_failed_no_results");
        return c.json({ status: "ended", state: "failed", results: [] });
      }
      return c.json({ error: "results unavailable" }, gone ? 410 : 503, { "retry-after": "60" });
    }
    const encoder = new TextEncoder();
    let actualMicros = 0;
    let first = true;
    let opened = false;
    const body = new ReadableStream<Uint8Array>({
      async pull(controller) {
        if (!opened) {
          opened = true;
          controller.enqueue(encoder.encode(JSON.stringify({ status: "ended", state: status.request_counts.succeeded > 0 ? "succeeded" : "failed" }).slice(0, -1) + ',"results":['));
          return;
        }
        let next;
        try { next = primed ?? await lines.next(); primed = undefined; }
        catch {
          deps.log("batch_result_stream_failure");
          controller.error(new Error("batch results unavailable"));
          return;
        }
        if (next.done) {
          // Complete stream only: replace this batch's reserved estimate with what the provider
          // reports, once (settleSpend is idempotent, so a re-read books nothing twice, and the
          // settlement sweep in boot.ts settling first leaves this a no-op). A cancelled read
          // leaves the estimate — an upper bound — in place for the sweep.
          if (spendBooked) {
            await deps.usage.settleSpend(id, Math.ceil(actualMicros)).catch(() => deps.log("spend_settle_failure"));
          }
          // Every result line has left the provider: drop its files (D11, best effort). Started
          // BEFORE the stream closes, because the Workers runtime may cancel whatever a request
          // still has pending once its response has completed. On Workers it is handed to
          // `waitUntil`, which keeps it alive past the close without holding the last bytes
          // back; the Node entry has no execution context, so there the stream waits for it.
          const cleanup = deps.gateway.releaseResults?.(providerId).catch(() => deps.log("batch_cleanup_failure"));
          if (cleanup) {
            const ctx = executionContextOf(c);
            if (ctx) ctx.waitUntil(cleanup);
            else await cleanup;
          }
          controller.enqueue(encoder.encode("]}"));
          controller.close();
          return;
        }
        const { result: item, model } = parseResultLine(next.value);
        if (spendBooked && "usage" in item && item.usage) actualMicros += usageMicros(item.usage, priceFor(prices, model));
        controller.enqueue(encoder.encode((first ? "" : ",") + JSON.stringify(item)));
        first = false;
      },
      async cancel() {
        await lines.return?.().catch(() => deps.log("batch_cleanup_failure"));
      },
    });
    return new Response(body, {
      status: 200,
      headers: { "content-type": "application/json" },
    });
  });

  return app;
}
