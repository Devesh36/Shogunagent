import { FetchSpeechUsage } from "./asr-usage.js";
import { asrApp } from "./asr-app.js";
import { SqlAsrStore } from "./asr-store.js";
import { FetchDeepgramGateway } from "./deepgram.js";
/**
 * The relay's bootstrap, shared by both entries:
 *
 * - `index.ts` — Node (development): `process.env`, `@hono/node-server`, the Postgres store or the
 *   in-memory one, a `setInterval` sweep.
 * - `worker.ts` — Cloudflare Workers (production, docs/unit-economics.md §4): `env` bindings, the
 *   Postgres store over Hyperdrive, a Cron Trigger for the sweep.
 *
 * One code path for everything but the server glue: `relayConfigFromEnv` turns an env record into
 * a validated `RelayConfig` (or throws a `BootError` that names a variable and never echoes a
 * value), and `createRelay` wires that config and a store into the Hono app plus the settlement
 * pass. Every check refuses to boot rather than degrade silently: a relay that starts with a
 * mistyped rate limit or without its spend breaker answers /healthz green while the limits it
 * exists to enforce are not there. How "refuse" surfaces is the entry's business — Node exits
 * non-zero, a Worker answers every request 503 (`worker.ts`).
 *
 * Env (secrets via the platform's secret store; non-secrets as plain variables):
 * - ANTHROPIC_API_KEY        — the operator's Anthropic key: every class under
 *                              BATCH_PROVIDER=anthropic (Haiku 5.5, Sonnet 5.5 for the brief).
 *                              Lives ONLY here (server secret); never logged, never in a
 *                              response, never in the desktop binary (§2.1).
 * - OPENAI_API_KEY           — the operator's OpenAI key for every class, the brief included, on
 *                              `gpt-6-luna` (owner decision 2026-10-13, openai-gateway.ts).
 *                              and the hourly review (`/v1/review`). Always required. Same secrecy.
 * - BATCH_PROVIDER           — `openai` (default) or `anthropic`: which rows every class uses
 *                              (models.ts).
 * - LICENSE_PUBKEY_B64       — base64 of the licence API's raw 32-byte Ed25519 public key
 *                              (§4.1; the `SHOGUN_LICENSE_PUBKEY` value that
 *                              `scripts/gen-license-keypair.mjs` prints). Missing, malformed,
 *                              or not the key the shipped app trusts = the relay does not boot.
 * - LICENSE_PUBKEY_NEXT_B64  — optional, key rotation only (audit 2026-10-10 M6): the NEXT
 *                              signing key's public half. Required and checked against
 *                              `ed25519_next` in packages/config/license-public-key.json when
 *                              that file has one; must be unset when it has none.
 * - DAILY_SPEND_CAP_USD      — REQUIRED (audit H1): the global daily spend breaker (FR-SVC-08),
 *                              USD per UTC day across every licence. Missing = the relay does
 *                              not boot, unless RELAY_ALLOW_NO_SPEND_CAP=1 is set explicitly
 *                              (development/tests only; it also closes the trial lane). Size it
 *                              as `active licences × 0.50 × 1.5` (docs/unit-economics.md §3.3).
 *
 * Every numeric setting below is whole decimal digits (USD amounts may carry cents); anything
 * else (a typo, a unit suffix, a negative) refuses to boot with the variable's name. Unset or
 * blank = the default.
 * - DAILY_CHUNK_CAP          — per-license daily chunk cap (default 2000; OPEN-B2).
 * - TRIAL_DAILY_CHUNK_CAP    — per-trial-payer daily chunk cap (the account for an `acc` trial, else the Mac) (default 500; issue #763). Never
 *                              more than DAILY_CHUNK_CAP. Trial tokens (`plan: "trial"`) are
 *                              minted for any device id, so they are served ONLY when
 *                              DAILY_SPEND_CAP_USD is set; without it they get 402.
 * - TRIAL_SPEND_SHARE        — the share (0 to 1, default 0.5) of DAILY_SPEND_CAP_USD that trial
 *                              submissions may reach. Past it only paid submissions continue.
 * - LICENSE_DAILY_SPEND_CAP_USD — per-licence daily spend budget (default 0.50; audit H1 and
 *                              docs/unit-economics.md §3.3). The most one licence may reserve
 *                              against the company key in a UTC day; over it the submission is
 *                              429 `license_spend_cap` and nothing is sent.
 * - TRIAL_DAILY_SPEND_CAP_USD — the same for one trial payer (default 0.25; account or Mac, `meterSubject`). Never more than
 *                              LICENSE_DAILY_SPEND_CAP_USD.
 * - SETTLE_INTERVAL_SECONDS  — period of the periodic sweep (default 900; 0 disables): store
 *                              retention maintenance, then the settlement pass that asks the
 *                              provider about ended batches and books their actual usage (audit
 *                              M9, `sweepOnce`). On Workers the period is the Cron Trigger in
 *                              wrangler.jsonc; this variable only keeps its `0` meaning (the
 *                              sweep's emergency off, without a redeploy).
 * - SETTLE_MAX_PER_TICK      — most batches one settlement pass asks the provider about (default
 *                              500, 1–2000). See `DEFAULT_SETTLE_MAX_PER_TICK` for the ceiling math.
 * - SETTLE_RECHECK_SECONDS   — a batch the pass found still processing is not asked about again
 *                              for this long (default 900 = one cron period; 0 = every pass).
 * - RATE_BURST               — back-to-back request allowance (default 12), per payer +
 *                              device + route (see `bucketKey`), not per payer alone.
 * - RATE_PER_MIN             — sustained requests/minute for that same bucket (default 12).
 * - SPEND_PRICE_TABLE_JSON   — optional rows merged over the Batch price table (`spend.ts`
 *                              `priceTableWithOverride`); rows it does not name keep their
 *                              shipped price, and an unknown model never prices below
 *                              `UNKNOWN_MODEL_FLOOR`.
 * - METRICS_TOKEN            — optional bearer for GET /metrics (FR-SVC-06). Unset = 404.
 * - TYPESAFE_API_KEY         — the operator's TypeSafe key for Jev form decisions
 *                              (`POST /v1/ai/form_decide`, issue #1144 / #1101 D12). A secret,
 *                              same secrecy as the provider keys above. Optional at boot so a
 *                              deploy that predates the secret still serves the Batch lane:
 *                              unset = the form route answers 503 and the Mac fills nothing
 *                              (the boot log says `form_decide_lane closed`).
 * - RELAY_DENY_SUBJECTS      — optional, emergency only (audit 2026-10-20 L3): comma-separated
 *                              licence ids and/or `account:<acc>` subjects answered 401 on every
 *                              route, Batch, review, speech and form decisions, before their
 *                              tokens expire (a relay token cannot otherwise be revoked early: it
 *                              is valid until its `exp`, 26h after issue). Remove an entry once
 *                              26h have passed since the licence was revoked at the licence API.
 *
 * The agent API (docs/implementation-plan.md E3 / E9; apps/api/README-agent-api.md). Off unless
 * AGENT_API_ENABLED=1; then the first two below are required and the rest optional:
 * - AGENT_API_ENABLED        — `1` serves the agent routes (`sa_` keys, wallet, 402, `/mcp`).
 * - AGENT_POW_SECRET         — secret: HMAC key of the registration proof-of-work challenges.
 * - PUBLIC_BASE_URL          — this gateway's public origin (`https://api.shogunaios.com`), for
 *                              the top-up link a 402 carries.
 * - AGENT_POW_BITS           — leading zero bits a registration must present (default 18, 0–32).
 * - AGENT_REGISTER_PER_HOUR  — registrations per client IP per hour (default 5).
 * - AGENT_KEY_PBKDF2_ITERATIONS — PBKDF2 rounds for new keys (default 20000, at most 100000).
 * - AGENT_KEY_CAP_DAY_USD / AGENT_KEY_CAP_MONTH_USD /
 *   AGENT_PRINCIPAL_CAP_DAY_USD / AGENT_PRINCIPAL_CAP_MONTH_USD
 *                            — the hard caps (defaults 1 / 20 / 5 / 100; src/caps.ts).
 * - STRIPE_SECRET_KEY + STRIPE_WEBHOOK_SECRET — secrets, both or neither: card top-ups. Without
 *                              them `POST /v1/wallet/topup` answers 503 and a 402 still names it.
 * - AGENT_CARD_SIGNING_KEY   — secret, optional: the Ed25519 private key the A2A agent card
 *                              (`/.well-known/agent-card.json`, E10-T3) is signed with — PKCS#8
 *                              PEM, or the 32-byte seed as hex / base64. Unset = unsigned card.
 * - AGENT_MCP_URL            — optional: the MCP endpoint the agent-readable documents name when
 *                              it is not `<PUBLIC_BASE_URL>/mcp`.
 * - STRIPE_COMMIT_OVERAGE_PRICE_ID — optional (E19): the metered Stripe price (one unit = one
 *                              cent) commit overage is billed on. Needs STRIPE_SECRET_KEY.
 * - AGENT_OAUTH_SECRET       — secret, optional (E3-T2): at least 32 characters. Set = OAuth 2.1 +
 *                              Client ID Metadata Documents are served (`/oauth/*`, the two
 *                              `.well-known` documents) and `sat_` access tokens are accepted.
 *                              Needs sql/agent-oauth.sql applied.
 *
 * Node-only settings (PORT, RELAY_DATABASE_URL, METRICS_LOG_INTERVAL_SEC) are documented and
 * parsed in `index.ts`; the Worker's store comes from its `HYPERDRIVE` binding.
 */
import type { KeyObject } from "node:crypto";

import { REGISTER_PER_HOUR, REGISTER_WINDOW_MS } from "./agent-app.js";
import { DEFAULT_PBKDF2_ITERATIONS, MAX_PBKDF2_ITERATIONS } from "./agent-keys.js";
import { parseSigningKey } from "./wellknown/agent-card.js";
import { SqlAgentStore } from "./agent-pg-store.js";
import { httpSidecarClient } from "./approvals/sidecar-client.js";
import { InMemoryWebhookStore, SqlWebhookStore, type WebhookStore } from "./approvals/webhooks.js";
import type { AgentStore } from "./agent-store.js";
import { grantProBundle, reportCommitOverage } from "./billing/jobs.js";
import type { ResolveHost } from "./oauth/cimd.js";
import { SqlOAuthStore, type OAuthStore } from "./oauth/store.js";
import { createApp, parseResultLine, planSpendCapMicros, type RelayDeps, DEFAULT_TRIAL_SPEND_SHARE } from "./app.js";
import { DEFAULT_CAPS, type CapSet } from "./caps.js";
import { DEFAULT_POW_BITS, MAX_POW_BITS } from "./pow.js";
import { releaseTrustedKeysFromEnv, type TrustedKeys } from "./auth.js";
import { FetchAnthropicGateway, UpstreamError, type AnthropicGateway } from "./gateway.js";
import { operational, type LogFn } from "./logging.js";
import { RelayMetrics } from "./metrics.js";
import { batchProviderFromEnv, setBatchProvider, type BatchProvider } from "./models.js";
import { openAiReviewer } from "./review.js";
import { OpenAiBatchGateway, RoutingGateway } from "./openai-gateway.js";
import { SqlInFlightLimiter, SqlRateLimiter, SqlUsageStore, SqlWindowLimiter } from "./pg-store.js";
import type { InFlightLimiter, RateLimiter } from "./ratelimit.js";
import { FORM_DECIDE_MAX_IN_FLIGHT, FORM_DECIDE_PER_HOUR, FORM_DECIDE_TIMEOUT_MS, FORM_DECIDE_WINDOW_MS, typesafeDecider } from "./form-decide.js";
import type { SqlClient } from "./sql.js";
import { DEFAULT_BATCH_PRICES, priceFor, priceTableWithOverride, spendCapMicrosFromEnv, usageMicros, usdCapMicrosFromEnv, type PriceTable, type SpendBreaker } from "./spend.js";
import { SETTLE_WINDOW_DAYS, keepCutoff, utcDate, type UsageStore } from "./usage.js";

/** A malformed boot setting. The message names the variable and never echoes its value. */
export class BootError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "BootError";
  }
}

/** What both entries read their settings from: `process.env`, or a Worker's string bindings. */
export type EnvRecord = Record<string, string | undefined>;

/** Per-licence daily spend budget, USD (docs/unit-economics.md §3.3: ≈ 2× a typical night, ≈ 30%
 * of the plan price over a month). Kept low on purpose: a budget a typical licence never reaches
 * (such as $3.00) never stops anything. */
export const DEFAULT_LICENSE_DAILY_SPEND_CAP_USD = 0.5;

/** Per-trial-device daily spend budget, USD: at most $1.75 of company spend over a 7-day trial. */
export const DEFAULT_TRIAL_DAILY_SPEND_CAP_USD = 0.25;

export const DEFAULT_DAILY_CHUNK_CAP = 2000;
export const DEFAULT_TRIAL_DAILY_CHUNK_CAP = 500;
export const DEFAULT_RATE_BURST = 12;
export const DEFAULT_RATE_PER_MIN = 12;
export const DEFAULT_SETTLE_INTERVAL_SECONDS = 900;

/**
 * Most batches one settlement pass asks the provider about. The ceiling math: the cron runs 96
 * times a day, so 500 × 96 = 48,000 polls/day, and a batch found still processing is not polled
 * again for `SETTLE_RECHECK_SECONDS` (one period), so each tick's budget goes to batches not yet
 * seen this period. One Dream Cycle is one batch per licence per night; 48,000 polls covers tens
 * of thousands of active licences before a backlog forms, and a backlog only delays the
 * estimate→actual correction (the estimate is an upper bound). Each poll is one small GET; an
 * ended batch then streams its results once (the CPU-bound part, see `limits` in wrangler.jsonc).
 */
export const DEFAULT_SETTLE_MAX_PER_TICK = 500;
export const MAX_SETTLE_MAX_PER_TICK = 2000;
export const DEFAULT_SETTLE_RECHECK_SECONDS = 900;

/**
 * A whole-number env setting in `[min, max]`. Unset or blank is `fallback` (the same convention
 * as `DAILY_SPEND_CAP_USD`); anything else must be plain decimal digits. `Number()` alone is not
 * a validator: `Number("12/min")` is `NaN`, and a `NaN` burst or cap compares false against
 * everything, which silently disables the limit it configures.
 */
export function intFromEnv(
  env: EnvRecord,
  name: string,
  fallback: number,
  min = 1,
  max = Number.MAX_SAFE_INTEGER,
): number {
  const raw = env[name]?.trim();
  if (raw === undefined || raw === "") return fallback;
  const n = /^\d+$/.test(raw) ? Number(raw) : Number.NaN;
  if (!Number.isSafeInteger(n) || n < min || n > max) throw new BootError(`invalid ${name}`);
  return n;
}

/**
 * The global spend breaker is mandatory (audit 2026-10-10 H1). A relay without
 * `DAILY_SPEND_CAP_USD` has no ceiling on what the company key can be made to spend in a day, so
 * it does not boot — unless `RELAY_ALLOW_NO_SPEND_CAP=1` says, explicitly, that this is a
 * development or test relay. Returns the refusal (variable names only), or `undefined` to proceed.
 */
export function spendCapRequirementError(env: EnvRecord): string | undefined {
  if ((env.DAILY_SPEND_CAP_USD?.trim() ?? "") !== "") return undefined;
  if (env.RELAY_ALLOW_NO_SPEND_CAP?.trim() === "1") return undefined;
  return "missing required env: DAILY_SPEND_CAP_USD (the global daily spend breaker; docs/ai-gateway-runbook.md §9-6). Only a development relay may run without it, by setting RELAY_ALLOW_NO_SPEND_CAP=1 explicitly.";
}

/** Everything the relay needs that is not a store or a server: the validated settings. */
export interface RelayConfig {
  deepgramApiKey?: string;
  asrEnabled?: boolean;
  asrAllowedSubjects?: readonly string[];
  /** The operator's Anthropic key. Held for the gateway only; never logged or echoed. */
  anthropicApiKey: string;
  /** The operator's OpenAI key (luna classes and the hourly review). Required at boot. */
  openaiApiKey?: string;
  /** Which provider the four background classes run on (models.ts). */
  batchProvider: BatchProvider;
  licensePublicKey: TrustedKeys;
  dailyChunkCap: number;
  trialDailyChunkCap: number;
  trialSpendShare: number;
  licenseSpendCapMicros: number;
  trialSpendCapMicros: number;
  /** Absent only with `RELAY_ALLOW_NO_SPEND_CAP=1` (development); the trial lane is then closed. */
  spendBreaker?: SpendBreaker;
  rateBurst: number;
  ratePerMin: number;
  /** 0 = the periodic sweep (maintenance and settlement) is off. */
  settleEverySec: number;
  settleMaxPerTick: number;
  settleRecheckSec: number;
  metricsToken?: string;
  /** The operator's TypeSafe key (Jev form decisions). Absent = the form route answers 503. */
  typesafeApiKey?: string;
  /** `RELAY_DENY_SUBJECTS`: licence ids / `account:<acc>` subjects refused before their tokens
   * expire (audit 2026-10-20 L3). */
  denySubjects?: readonly string[];
  /** The agent API (E3 / E9). Absent = `AGENT_API_ENABLED` is not `1`: the routes do not exist. */
  agent?: AgentConfig;
}

/** The validated agent-API settings (the `AGENT_*` / `STRIPE_*` / `PUBLIC_BASE_URL` variables). */
export interface AgentConfig {
  powSecret: string;
  powBits: number;
  publicBaseUrl: string;
  registerPerHour: number;
  keyIterations: number;
  caps: CapSet;
  stripe?: { secretKey: string; webhookSecret: string; overagePriceId?: string };
  /** `AGENT_OAUTH_SECRET` (E3-T2). Absent = OAuth is not served. */
  oauthSecret?: string;
  /** `AGENT_CARD_SIGNING_KEY` (E10-T3), parsed: the Ed25519 key the A2A agent card is signed
   * with. Absent = the card is served unsigned. */
  agentCardSigningKey?: KeyObject;
  /** `AGENT_MCP_URL`: the Streamable HTTP endpoint the documents name when it is not
   * `<PUBLIC_BASE_URL>/mcp`. */
  mcpUrl?: string;
  /** Remote approvals (E15): `SHOGUN_APPROVAL_SIGNING_KEY` (shared with the sidecar),
   * `SHOGUN_SIDECAR_URL`, `WEBHOOK_SIGNING_SECRET`, and the Slack app (`SLACK_BOT_TOKEN`,
   * `SLACK_SIGNING_SECRET`, `SLACK_APPROVAL_CHANNEL`, all three or none). Absent = the approval
   * page, Slack and webhook routes are not served. */
  approvals?: {
    signingKey: string;
    sidecarUrl: string;
    webhookSecret: string;
    slack?: { botToken: string; signingSecret: string; channel: string };
  };
}

/** Parse the remote-approval settings, or `undefined` when `SHOGUN_APPROVAL_SIGNING_KEY` is unset.
 * Throws `BootError` naming the variable, never echoing a value. */
export function approvalsConfigFromEnv(env: EnvRecord): AgentConfig["approvals"] {
  const signingKey = env.SHOGUN_APPROVAL_SIGNING_KEY?.trim();
  if (!signingKey) return undefined;
  if (signingKey.length < 32) throw new BootError("invalid SHOGUN_APPROVAL_SIGNING_KEY (at least 32 characters, the same value the sidecar has)");
  const sidecarUrl = env.SHOGUN_SIDECAR_URL?.trim();
  if (!sidecarUrl || !/^https?:\/\/[^\s]+$/.test(sidecarUrl)) throw new BootError("missing or invalid SHOGUN_SIDECAR_URL (the sidecar's base URL; SHOGUN_APPROVAL_SIGNING_KEY is set)");
  const webhookSecret = env.WEBHOOK_SIGNING_SECRET?.trim();
  if (!webhookSecret || webhookSecret.length < 32) throw new BootError("missing WEBHOOK_SIGNING_SECRET (at least 32 characters; SHOGUN_APPROVAL_SIGNING_KEY is set)");
  const botToken = env.SLACK_BOT_TOKEN?.trim();
  const slackSecret = env.SLACK_SIGNING_SECRET?.trim();
  const channel = env.SLACK_APPROVAL_CHANNEL?.trim();
  const slackSet = [botToken, slackSecret, channel].filter(Boolean).length;
  if (slackSet !== 0 && slackSet !== 3) throw new BootError("SLACK_BOT_TOKEN, SLACK_SIGNING_SECRET and SLACK_APPROVAL_CHANNEL must be set together");
  return {
    signingKey, sidecarUrl, webhookSecret,
    ...(botToken && slackSecret && channel ? { slack: { botToken, signingSecret: slackSecret, channel } } : {}),
  };
}

/** Parse the agent-API settings, or `undefined` when the API is off. Throws `BootError` naming
 * the variable, never echoing a value. */
export function agentConfigFromEnv(env: EnvRecord): AgentConfig | undefined {
  if (env.AGENT_API_ENABLED?.trim() !== "1") return undefined;
  const powSecret = env.AGENT_POW_SECRET;
  if (!powSecret || powSecret.length < 16) throw new BootError("missing required env: AGENT_POW_SECRET (at least 16 characters; AGENT_API_ENABLED=1)");
  const publicBaseUrl = env.PUBLIC_BASE_URL?.trim();
  if (!publicBaseUrl || !/^https?:\/\/[^\s/]+$/.test(publicBaseUrl)) throw new BootError("invalid PUBLIC_BASE_URL (an origin such as https://api.shogunaios.com; AGENT_API_ENABLED=1)");
  const usd = (name: string, fallbackUsd: number): number => {
    try {
      return usdCapMicrosFromEnv(env[name], name, fallbackUsd);
    } catch {
      throw new BootError(`invalid ${name}`);
    }
  };
  const caps: CapSet = {
    key: { day: usd("AGENT_KEY_CAP_DAY_USD", DEFAULT_CAPS.key.day / 1e6), month: usd("AGENT_KEY_CAP_MONTH_USD", DEFAULT_CAPS.key.month / 1e6) },
    principal: { day: usd("AGENT_PRINCIPAL_CAP_DAY_USD", DEFAULT_CAPS.principal.day / 1e6), month: usd("AGENT_PRINCIPAL_CAP_MONTH_USD", DEFAULT_CAPS.principal.month / 1e6) },
  };
  const stripeKey = env.STRIPE_SECRET_KEY?.trim();
  const webhookSecret = env.STRIPE_WEBHOOK_SECRET?.trim();
  if ((stripeKey ? 1 : 0) + (webhookSecret ? 1 : 0) === 1) throw new BootError("STRIPE_SECRET_KEY and STRIPE_WEBHOOK_SECRET must be set together");
  // The agent card's signing key (E10-T3). A value that is not an Ed25519 key stops boot: an
  // unsigned card by accident is a silent identity downgrade. The error names only the variable.
  let agentCardSigningKey: KeyObject | undefined;
  try {
    agentCardSigningKey = parseSigningKey(env.AGENT_CARD_SIGNING_KEY);
  } catch {
    throw new BootError("invalid AGENT_CARD_SIGNING_KEY (an Ed25519 private key: PKCS#8 PEM, or the 32-byte seed as hex or base64)");
  }
  const mcpUrl = env.AGENT_MCP_URL?.trim();
  if (mcpUrl !== undefined && mcpUrl !== "" && !/^https?:\/\/[^\s]+$/.test(mcpUrl)) throw new BootError("invalid AGENT_MCP_URL (an absolute http(s) URL)");
  const overagePriceId = env.STRIPE_COMMIT_OVERAGE_PRICE_ID?.trim();
  if (overagePriceId && !/^price_[A-Za-z0-9]{1,64}$/.test(overagePriceId)) throw new BootError("invalid STRIPE_COMMIT_OVERAGE_PRICE_ID (a Stripe price id, price_…)");
  if (overagePriceId && !stripeKey) throw new BootError("STRIPE_COMMIT_OVERAGE_PRICE_ID needs STRIPE_SECRET_KEY");
  const oauthSecret = env.AGENT_OAUTH_SECRET;
  if (oauthSecret !== undefined && oauthSecret !== "" && oauthSecret.length < 32) throw new BootError("invalid AGENT_OAUTH_SECRET (at least 32 characters)");
  const approvals = approvalsConfigFromEnv(env);
  return {
    powSecret,
    powBits: intFromEnv(env, "AGENT_POW_BITS", DEFAULT_POW_BITS, 0, MAX_POW_BITS),
    publicBaseUrl,
    registerPerHour: intFromEnv(env, "AGENT_REGISTER_PER_HOUR", REGISTER_PER_HOUR, 1),
    keyIterations: intFromEnv(env, "AGENT_KEY_PBKDF2_ITERATIONS", DEFAULT_PBKDF2_ITERATIONS, 1_000, MAX_PBKDF2_ITERATIONS),
    caps,
    ...(stripeKey && webhookSecret ? { stripe: { secretKey: stripeKey, webhookSecret, ...(overagePriceId ? { overagePriceId } : {}) } } : {}),
    ...(oauthSecret ? { oauthSecret } : {}),
    ...(agentCardSigningKey ? { agentCardSigningKey } : {}),
    ...(mcpUrl ? { mcpUrl } : {}),
    ...(approvals ? { approvals } : {}),
  };
}

/** The periodic billing jobs (E19 / E14-T3, billing/jobs.ts) and OAuth retention, run from the
 * sweep. Each is idempotent; failures are one fixed warning line and never stop the sweep. */
export async function billingJobs(config: Pick<RelayConfig, "agent">, store: Pick<RelayStore, "agent" | "oauth">, nowMs: number): Promise<void> {
  if (!config.agent || !store.agent) return;
  const counts = await grantProBundle(store.agent, nowMs).catch(() => undefined);
  if (!counts) operational.warn("pro bundle grant unavailable");
  else if (counts.granted > 0) operational.info(`pro_bundle_granted ${counts.granted}`);
  if (config.agent.stripe?.overagePriceId) {
    const overage = await reportCommitOverage(store.agent, config.agent.stripe, nowMs).catch(() => undefined);
    if (!overage) operational.warn("commit overage report unavailable");
    else if (overage.claimed > 0) operational.info(`commit_overage_reports sent=${overage.sent} failed=${overage.failed}`);
  }
  if (config.agent.oauthSecret && store.oauth) await store.oauth.maintenance(nowMs).catch(() => operational.warn("oauth maintenance unavailable"));
}

/** Whether the agent API is served; one boot-log line, so a 404 on `/v1/agents` is not a mystery. */
export function agentLaneLine(config: Pick<RelayConfig, "agent">): string {
  if (!config.agent) return "agent_api closed (AGENT_API_ENABLED unset)";
  return `agent_api open (card top-ups ${config.agent.stripe ? "on" : "off: STRIPE_SECRET_KEY unset"}; oauth ${config.agent.oauthSecret ? "on" : "off: AGENT_OAUTH_SECRET unset"}; commit overage billing ${config.agent.stripe?.overagePriceId ? "on" : "off"})`;
}

/** The Batch price table in force: the override, or the shipped defaults. */
export function pricesFor(config: Pick<RelayConfig, "spendBreaker">): PriceTable {
  return config.spendBreaker?.prices ?? DEFAULT_BATCH_PRICES;
}

/**
 * Parse and validate the relay's settings. Throws `BootError`. The checks are the same on both
 * runtimes, in the same order.
 */
export function relayConfigFromEnv(env: EnvRecord): RelayConfig {
  const required = (name: string): string => {
    const v = env[name];
    if (!v) throw new BootError(`missing required env: ${name}`);
    return v;
  };
  const anthropicApiKey = required("ANTHROPIC_API_KEY");
  let batchProvider: BatchProvider;
  try {
    batchProvider = batchProviderFromEnv(env.BATCH_PROVIDER);
  } catch {
    throw new BootError("invalid BATCH_PROVIDER (openai | anthropic)");
  }
  const openaiApiKey = env.OPENAI_API_KEY || undefined;
  // The key in force must exist: a relay that accepted submissions and answered every create with
  // 502 would look healthy to the device and spend nothing — a silent outage. The hourly review
  // (`/v1/review`) runs on OpenAI under either BATCH_PROVIDER, so the key is always required.
  if (!openaiApiKey) {
    throw new BootError("missing required env: OPENAI_API_KEY (the background classes and the hourly review run on OpenAI)");
  }
  const publicKeyB64 = required("LICENSE_PUBKEY_B64");

  // 0 is valid: it is the runbook's emergency stop for the Batch lane (§6-1).
  const dailyChunkCap = intFromEnv(env, "DAILY_CHUNK_CAP", DEFAULT_DAILY_CHUNK_CAP, 0);
  const trialDailyChunkCap = Math.min(dailyChunkCap, intFromEnv(env, "TRIAL_DAILY_CHUNK_CAP", DEFAULT_TRIAL_DAILY_CHUNK_CAP, 0));
  const trialSpendShare = Number(env.TRIAL_SPEND_SHARE?.trim() || String(DEFAULT_TRIAL_SPEND_SHARE));
  if (!Number.isFinite(trialSpendShare) || trialSpendShare < 0 || trialSpendShare > 1) throw new BootError("invalid TRIAL_SPEND_SHARE");
  const rateBurst = intFromEnv(env, "RATE_BURST", DEFAULT_RATE_BURST);
  const ratePerMin = intFromEnv(env, "RATE_PER_MIN", DEFAULT_RATE_PER_MIN);
  // Bounded to a day: a period past 2^31-1 ms would overflow Node's timer and fire every 1 ms.
  const settleEverySec = intFromEnv(env, "SETTLE_INTERVAL_SECONDS", DEFAULT_SETTLE_INTERVAL_SECONDS, 0, 86_400);
  const settleMaxPerTick = intFromEnv(env, "SETTLE_MAX_PER_TICK", DEFAULT_SETTLE_MAX_PER_TICK, 1, MAX_SETTLE_MAX_PER_TICK);
  const settleRecheckSec = intFromEnv(env, "SETTLE_RECHECK_SECONDS", DEFAULT_SETTLE_RECHECK_SECONDS, 0, 86_400);

  // Per-licence daily spend budgets (audit 2026-10-10 H1). Always on: a licence is cents a day,
  // whatever the global breaker still has.
  const usdMicros = (name: string, fallbackUsd: number): number => {
    try {
      return usdCapMicrosFromEnv(env[name], name, fallbackUsd);
    } catch {
      throw new BootError(`invalid ${name}`);
    }
  };
  const licenseSpendCapMicros = usdMicros("LICENSE_DAILY_SPEND_CAP_USD", DEFAULT_LICENSE_DAILY_SPEND_CAP_USD);
  const trialSpendCapMicros = Math.min(licenseSpendCapMicros, usdMicros("TRIAL_DAILY_SPEND_CAP_USD", DEFAULT_TRIAL_DAILY_SPEND_CAP_USD));

  // The current release key, plus the next one during a rotation (audit M6). Any mismatch with
  // the shipped trust file refuses to boot; the message names variables, never key material.
  let licensePublicKey: TrustedKeys;
  try {
    licensePublicKey = releaseTrustedKeysFromEnv(publicKeyB64, env.LICENSE_PUBKEY_NEXT_B64);
  } catch (e) {
    throw new BootError(e instanceof Error ? e.message : "invalid LICENSE_PUBKEY_B64 or LICENSE_PUBKEY_NEXT_B64");
  }

  // The agent API (E3 / E9), off unless asked for.
  const agent = agentConfigFromEnv(env);

  // The global breaker is not optional in production (audit H1).
  const spendCapRefusal = spendCapRequirementError(env);
  if (spendCapRefusal) throw new BootError(spendCapRefusal);
  let spendBreaker: SpendBreaker | undefined;
  try {
    const capMicros = spendCapMicrosFromEnv(env.DAILY_SPEND_CAP_USD);
    if (capMicros !== undefined) {
      const raw = env.SPEND_PRICE_TABLE_JSON;
      spendBreaker = { capMicros, prices: priceTableWithOverride(raw) };
    }
  } catch {
    // A malformed value is refused at boot, never silently priced at zero.
    throw new BootError("invalid DAILY_SPEND_CAP_USD or SPEND_PRICE_TABLE_JSON");
  }

  return {
    anthropicApiKey,
    ...(openaiApiKey ? { openaiApiKey } : {}),
    batchProvider,
    licensePublicKey,
    dailyChunkCap,
    trialDailyChunkCap,
    trialSpendShare,
    licenseSpendCapMicros,
    trialSpendCapMicros,
    ...(spendBreaker ? { spendBreaker } : {}),
    rateBurst,
    ratePerMin,
    settleEverySec,
    settleMaxPerTick,
    settleRecheckSec,
    ...(env.DEEPGRAM_API_KEY ? { deepgramApiKey: env.DEEPGRAM_API_KEY } : {}),
    asrEnabled: env.ASR_ENABLED === "1",
    ...(env.ASR_ALLOWED_SUBJECTS !== undefined ? { asrAllowedSubjects: env.ASR_ALLOWED_SUBJECTS.split(",").map(value => value.trim()).filter(Boolean) } : {}),
    ...(env.METRICS_TOKEN ? { metricsToken: env.METRICS_TOKEN } : {}),
    ...(env.TYPESAFE_API_KEY?.trim() ? { typesafeApiKey: env.TYPESAFE_API_KEY.trim() } : {}),
    ...(env.RELAY_DENY_SUBJECTS?.trim() ? { denySubjects: env.RELAY_DENY_SUBJECTS.split(",").map(value => value.trim()).filter(Boolean) } : {}),
    ...(agent ? { agent } : {}),
  };
}

/** Whether Jev form decisions are served; a closed lane is a boot-log line, not a mystery 503. */
export function formLaneLine(config: Pick<RelayConfig, "typesafeApiKey">): string {
  return config.typesafeApiKey ? "form_decide_lane open" : "form_decide_lane closed (TYPESAFE_API_KEY unset)";
}

/** How long a form route in-flight slot is held before it counts as free again: longer than the
 * TypeSafe timeout, so a live call is never double-counted out of its slot. */
export const FORM_IN_FLIGHT_LEASE_MS = FORM_DECIDE_TIMEOUT_MS + 15_000;

/** Trial tokens need the global breaker as their backstop (issue #763); say which way this
 * deployment went, so a closed trial lane is a line in the boot log and not a mystery 402. */
export function trialLaneLine(config: Pick<RelayConfig, "spendBreaker">): string {
  return config.spendBreaker ? "trial_lane open" : "trial_lane closed (DAILY_SPEND_CAP_USD unset)";
}

/** The ledger and the rate buckets an entry chose, plus how to release them. */
export interface RelayStore {
  asr?: SqlAsrStore;
  usage: UsageStore;
  rateLimit: RateLimiter;
  /** The form route's per-licence hourly limit and per-device in-flight limit. Absent = the
   * app's in-process defaults (the Node development entry). */
  formRateLimit?: RateLimiter;
  formInFlight?: InFlightLimiter;
  /** The agent store (keys, wallet, receipts — agent-store.ts). Absent = the agent routes are
   * not served even when configured. */
  agent?: AgentStore;
  /** The per-IP registration limiter. Absent = the app's in-process default. */
  registerLimiter?: RateLimiter;
  /** The OAuth store (E3-T2). Absent = OAuth is not served even when configured. */
  oauth?: OAuthStore;
  /** Resolves a CIMD host before it is fetched (the Node entry: `node:dns`). */
  resolveHost?: ResolveHost;
  /** Approval webhooks (E15). Absent = an in-process store (development). */
  webhooks?: WebhookStore;
  close(): Promise<void>;
}

/**
 * The shared Postgres store (FR-SVC-04): every instance — or every Worker request — reserves
 * against the same rows, so adding one never doubles a cap. The rate limiter fails OPEN on a
 * store error (pacing is not the spend control; the ledger right behind it is), and says so with
 * one fixed warning line. `checkSchema` is the caller's: Node runs it before listening, the
 * Worker on each sweep (a missing table is otherwise a 503 per request, fail-closed).
 */
export function postgresStore(db: SqlClient, config: Pick<RelayConfig, "rateBurst" | "ratePerMin" | "spendBreaker" | "agent">, warn: LogFn = operational.warn): RelayStore & { usage: SqlUsageStore; agent: SqlAgentStore; oauth: SqlOAuthStore } {
  return {
    oauth: new SqlOAuthStore(db),
    usage: new SqlUsageStore(db),
    asr: new SqlAsrStore(db, Date.now, config.spendBreaker?.capMicros ?? 0),
    rateLimit: new SqlRateLimiter(db, config.rateBurst, config.ratePerMin, () => warn("ratelimit_unavailable (failing open)")),
    formRateLimit: new SqlWindowLimiter(db, FORM_DECIDE_PER_HOUR, FORM_DECIDE_WINDOW_MS, () => warn("ratelimit_unavailable (failing open)")),
    formInFlight: new SqlInFlightLimiter(db, FORM_DECIDE_MAX_IN_FLIGHT, FORM_IN_FLIGHT_LEASE_MS, () => warn("inflight_unavailable (failing open)")),
    agent: new SqlAgentStore(db),
    registerLimiter: new SqlWindowLimiter(db, config.agent?.registerPerHour ?? REGISTER_PER_HOUR, REGISTER_WINDOW_MS, () => warn("ratelimit_unavailable (failing open)")),
    webhooks: new SqlWebhookStore(db),
    close: () => db.close(),
  };
}

/** What the entries may share across `createRelay` calls (the Worker wires one relay per request
 * but keeps one metrics window and one gateway per isolate). */
export interface RelayShared {
  metrics?: RelayMetrics;
  gateway?: AnthropicGateway;
  /** The provider-side cleanup that rides the sweep (`providerGateways`). */
  orphanSweep?: () => Promise<number>;
  /** The per-request log line sink. Default: the operational console writer. */
  log?: LogFn;
  /** Injected clock (tests). */
  now?: () => Date;
}

export interface Relay {
  app: ReturnType<typeof createApp>;
  /** One periodic tick (`sweepOnce`): store maintenance, then the bounded, idempotent settlement
   * pass. The Node entry runs it on a timer, the Worker from its Cron Trigger. */
  sweep: () => Promise<SettlementCounts>;
  /** The settlement pass alone (`settleOnce`). */
  settle: () => Promise<SettlementCounts>;
  settlementDeps: SettlementDeps;
  metrics: RelayMetrics;
  gateway: AnthropicGateway;
}

/** The in-process metrics window for a config (one per Node process, one per Worker isolate). */
export function newMetrics(config: Pick<RelayConfig, "dailyChunkCap" | "trialDailyChunkCap" | "spendBreaker">): RelayMetrics {
  return new RelayMetrics(config.dailyChunkCap, config.spendBreaker ? { trial: config.trialDailyChunkCap } : {});
}

/** The provider gateways for a config: Anthropic always (the fallback), OpenAI when its key is set,
 * behind one `RoutingGateway` that creates by model and reads by id prefix (openai-gateway.ts).
 * `orphanSweep` is the OpenAI file cleanup (invariant 3) the periodic sweep runs; absent without
 * that provider. One per Node process / Worker isolate: the gateways hold no state but the keys. */
export function providerGateways(config: RelayConfig, log: LogFn = operational.info): { gateway: AnthropicGateway; orphanSweep?: () => Promise<number> } {
  const anthropic = new FetchAnthropicGateway(config.anthropicApiKey);
  if (!config.openaiApiKey) return { gateway: new RoutingGateway(anthropic, undefined) };
  const openai = new OpenAiBatchGateway(config.openaiApiKey, undefined, log);
  return { gateway: new RoutingGateway(anthropic, openai), orphanSweep: () => openai.sweepOrphans() };
}

/** Wire a validated config and a store into the app and the settlement pass. Pure composition:
 * nothing here touches the network or the store. */
export function createRelay(config: RelayConfig, store: RelayStore, shared: RelayShared = {}): Relay {
  // The model tables read the provider at access time (models.ts); the Worker's vars are not in
  // process.env while modules evaluate, so the validated config sets it here, before any request.
  setBatchProvider(config.batchProvider);
  const log = shared.log ?? operational.info;
  const provided = shared.gateway ? { gateway: shared.gateway, orphanSweep: shared.orphanSweep } : providerGateways(config, log);
  const gateway = provided.gateway;
  const metrics = shared.metrics ?? newMetrics(config);
  const deps: RelayDeps = {
    gateway,
    usage: store.usage,
    licensePublicKey: config.licensePublicKey,
    dailyChunkCap: config.dailyChunkCap,
    trialDailyChunkCap: config.trialDailyChunkCap,
    trialSpendShare: config.trialSpendShare,
    licenseSpendCapMicros: config.licenseSpendCapMicros,
    trialSpendCapMicros: config.trialSpendCapMicros,
    rateLimit: store.rateLimit,
    ...(config.openaiApiKey ? { reviewer: openAiReviewer(config.openaiApiKey) } : {}),
    ...(config.typesafeApiKey ? { formDecider: typesafeDecider(config.typesafeApiKey) } : {}),
    ...(store.formRateLimit ? { formRateLimit: store.formRateLimit } : {}),
    ...(store.formInFlight ? { formInFlight: store.formInFlight } : {}),
    // The line carries method/route/status/counters only (logging.ts) — never a body.
    log,
    metrics,
    ...(shared.now ? { now: shared.now } : {}),
    ...(config.spendBreaker ? { spendBreaker: config.spendBreaker } : {}),
    ...(config.metricsToken ? { metricsToken: config.metricsToken } : {}),
    ...(config.denySubjects ? { denySubjects: config.denySubjects } : {}),
    // The agent routes (E3 / E9): the agent store, the PoW and cap settings, Stripe when
    // configured, and the licence ledger + breaker so agent spend shares the company's day. The
    // core proxy (`mcpUpstream`) is E4's: until it is wired, `/mcp` answers 503 before reserving.
    ...(config.agent && store.agent ? {
      agent: {
        store: store.agent,
        publicBaseUrl: config.agent.publicBaseUrl,
        powSecret: config.agent.powSecret,
        powBits: config.agent.powBits,
        keyIterations: config.agent.keyIterations,
        caps: config.agent.caps,
        usage: store.usage,
        ...(store.registerLimiter ? { registerLimiter: store.registerLimiter } : {}),
        ...(config.agent.stripe ? { stripe: config.agent.stripe } : {}),
        // E14-T3: linking a ShogunAI licence verifies its token against the same trust roots.
        licensePublicKey: config.licensePublicKey,
        // E3-T2: OAuth when the secret is set and the store exists.
        ...(config.agent.oauthSecret && store.oauth ? { oauth: { store: store.oauth, secret: config.agent.oauthSecret, ...(store.resolveHost ? { resolveHost: store.resolveHost } : {}) } } : {}),
        ...(config.spendBreaker ? { spendBreaker: config.spendBreaker } : {}),
        // The agent-readable documents (E10): the card's key and the MCP URL, when configured.
        wellknown: {
          ...(config.agent.agentCardSigningKey ? { agentCardSigningKey: config.agent.agentCardSigningKey } : {}),
          ...(config.agent.mcpUrl ? { mcpUrl: config.agent.mcpUrl } : {}),
        },
        // Remote approvals (E15): the page, Slack and webhooks reach the sidecar through one client.
        ...(config.agent.approvals ? {
          approvals: {
            signingKey: config.agent.approvals.signingKey,
            sidecar: httpSidecarClient(config.agent.approvals.sidecarUrl),
            webhooks: store.webhooks ?? new InMemoryWebhookStore(),
            webhookSecret: config.agent.approvals.webhookSecret,
            ...(config.agent.approvals.slack ? { slack: config.agent.approvals.slack } : {}),
          },
        } : {}),
      },
    } : {}),
  };
  const settlementDeps: SettlementDeps = {
    usage: store.usage,
    gateway,
    prices: pricesFor(config),
    log,
    maxPerTick: config.settleMaxPerTick,
    recheckSeconds: config.settleRecheckSec,
    ...(shared.now ? { now: shared.now } : {}),
    ...(provided.orphanSweep ? { orphanSweep: provided.orphanSweep } : {}),
  };
  const app = createApp(deps);
  if (config.asrEnabled && config.deepgramApiKey && store.asr) {
    const speechUsage = new FetchSpeechUsage(config.deepgramApiKey);
    const speechStore = store.asr;
    app.route("/v1/asr", asrApp(config.licensePublicKey, store.asr, new FetchDeepgramGateway(config.deepgramApiKey), config.deepgramApiKey, config.asrAllowedSubjects, async (subject, tag) => {
      await speechStore.checkLedger();
      await speechStore.reconcile(subject, await speechUsage.read(tag, new Date()));
    }, {
      // Speech books against the same global day as the Batch lane, and a trial only up to its
      // share of it (audit 2026-10-20 H3). No breaker (development) = the store's cap of 0: no grants.
      spendCapFor: (license) => planSpendCapMicros(license, config.spendBreaker?.capMicros ?? 0, config.trialSpendShare),
      ...(config.denySubjects ? { denySubjects: config.denySubjects } : {}),
    }));
  } else {
    app.post("/v1/asr/grant", c => c.json({ error: "speech_unavailable" }, 503, { "cache-control": "no-store" }));
    app.post("/v1/asr/finish", c => c.json({ error: "speech_unavailable" }, 503, { "cache-control": "no-store" }));
    app.post("/v1/asr/refresh", c => c.json({ error: "speech_unavailable" }, 503, { "cache-control": "no-store" }));
  }
  return {
    app,
    sweep: async () => {
      if (config.asrEnabled && store.asr) await store.asr.maintenance().catch(() => operational.warn("speech maintenance unavailable"));
      // Expired PoW rows and settled receipts past their window (agent-store.ts). Counts nothing.
      if (config.agent && store.agent) await store.agent.maintenance((shared.now?.() ?? new Date()).getTime()).catch(() => operational.warn("agent maintenance unavailable"));
      if (config.agent && store.agent) await billingJobs(config, store, (shared.now?.() ?? new Date()).getTime());
      return sweepOnce(settlementDeps);
    },
    settle: () => settleOnce(settlementDeps),
    settlementDeps,
    metrics,
    gateway,
  };
}

export interface SettlementDeps {
  usage: UsageStore;
  gateway: AnthropicGateway;
  prices: PriceTable;
  /** Counts only — never ids, never content. */
  log: (line: string) => void;
  now?: () => Date;
  /** Default `DEFAULT_SETTLE_MAX_PER_TICK`. */
  maxPerTick?: number;
  /** Default `DEFAULT_SETTLE_RECHECK_SECONDS`. */
  recheckSeconds?: number;
  /** Provider-side file cleanup (OpenAI orphan sweep, invariant 3), run by `sweepOnce` before the
   * settlement pass. Counts only in the log. */
  orphanSweep?: () => Promise<number>;
}

/** What one sweep did: numbers for the log line, nothing identifying. */
export interface SettlementCounts {
  checked: number;
  settled: number;
  abandoned: number;
  pending: number;
  failed: number;
}

/**
 * One settlement pass (audit 2026-10-10 M9). A batch's reserved estimate is replaced by the
 * provider's actual usage when the client reads its results through the relay; a client that never
 * reads would leave the (upper-bound) estimate standing and the ledger's picture of the day stale.
 * The sweep asks the provider about every unsettled, bound
 * batch of the last `SETTLE_WINDOW_DAYS` days — constant base URL, fixed headers, never a
 * client-supplied id (the provider id comes from the ledger's own owner row) — and when the batch
 * has ended, streams its results line by line (held one at a time, retained nowhere) to sum the
 * usage and books it through the same `settleSpend` the results route uses. That method settles a
 * batch once, so whichever path runs first wins and the other is a no-op.
 *
 * A batch found still processing is stamped (`markChecked`) and not asked about again for
 * `recheckSeconds`, so a pass's `maxPerTick` budget goes to batches not yet seen this period and
 * a backlog cannot pin the same in-progress batches at the front of every tick.
 *
 * A definite 4xx from the provider (the batch or its results no longer exist) closes the
 * reservation with its estimate as the final figure (`markSettled`): over-counting is the safe
 * direction, and polling it forever would crowd out live batches. Transport and 5xx failures leave
 * it for the next tick. Nothing here throws: every failure is a count in the log line.
 */
export async function settleOnce(deps: SettlementDeps): Promise<SettlementCounts> {
  const counts: SettlementCounts = { checked: 0, settled: 0, abandoned: 0, pending: 0, failed: 0 };
  const now = deps.now?.() ?? new Date();
  const nowMs = now.getTime();
  let batches;
  try {
    batches = await deps.usage.unsettledBatches(
      deps.maxPerTick ?? DEFAULT_SETTLE_MAX_PER_TICK,
      keepCutoff(utcDate(now), SETTLE_WINDOW_DAYS),
      nowMs - (deps.recheckSeconds ?? DEFAULT_SETTLE_RECHECK_SECONDS) * 1000,
    );
  } catch {
    counts.failed += 1;
    return counts;
  }
  for (const batch of batches) {
    counts.checked += 1;
    try {
      const status = await deps.gateway.getBatch(batch.providerId);
      if (status.processing_status !== "ended") {
        counts.pending += 1;
        // Best effort: a failed stamp only means the batch is asked about again next tick.
        await deps.usage.markChecked(batch.batchId, nowMs).catch(() => undefined);
        continue;
      }
      let actualMicros = 0;
      for await (const line of deps.gateway.streamResults(batch.providerId)) {
        const { result, model } = parseResultLine(line);
        if ("usage" in result && result.usage) actualMicros += usageMicros(result.usage, priceFor(deps.prices, model));
      }
      await deps.usage.settleSpend(batch.batchId, Math.ceil(actualMicros));
      counts.settled += 1;
    } catch (e) {
      // Only a batch the provider no longer has (404/410) is abandoned with its estimate as the
      // final figure. A 429/401/403 on the poll says nothing about the batch, so it stays pending
      // and is retried next tick instead of freezing the over-estimate.
      if (e instanceof UpstreamError && (e.status === 404 || e.status === 410)) {
        await deps.usage.markSettled(batch.batchId).then(() => { counts.abandoned += 1; }, () => { counts.failed += 1; });
      } else {
        counts.failed += 1;
      }
    }
  }
  return counts;
}

/**
 * One periodic tick: the store's retention maintenance (expired counters, old legacy owner rows,
 * idle rate buckets — `UsageStore.maintenance`), then the settlement pass. Maintenance runs here
 * and nowhere else, so no request transaction ever carries a retention DELETE. A maintenance
 * failure is one fixed log line and never stops the settlement pass.
 */
export async function sweepOnce(deps: SettlementDeps): Promise<SettlementCounts> {
  const now = deps.now?.() ?? new Date();
  try {
    await deps.usage.maintenance(utcDate(now), now.getTime());
  } catch {
    deps.log("store_maintenance_failure");
  }
  if (deps.orphanSweep) {
    try {
      const swept = await deps.orphanSweep();
      if (swept > 0) deps.log(`openai_orphans_swept count=${swept}`);
    } catch (e) {
      deps.log(e instanceof UpstreamError ? `openai_sweep_failure step=${e.step} status=${e.status}` : "openai_sweep_failure");
    }
  }
  return settleOnce(deps);
}

/**
 * Run `sweepOnce` every `intervalSeconds` (0 = disabled), one pass at a time, logging one
 * `settlement_sweep {counts}` line per pass. Returns the stop function. The timer is unref'd so it
 * never keeps a draining process alive, and nothing it does can throw out of the timer. Node
 * entry only: the Worker runs the same tick from a Cron Trigger (`worker.ts` `scheduled`).
 */
export function startPeriodicSweep(deps: SettlementDeps, intervalSeconds: number): () => void {
  if (!(intervalSeconds > 0)) return () => undefined;
  let running = false;
  const timer = setInterval(() => {
    if (running) return;
    running = true;
    void sweepOnce(deps)
      .then((counts) => deps.log(`settlement_sweep ${JSON.stringify(counts)}`))
      .catch(() => deps.log("settlement_sweep_failure"))
      .finally(() => { running = false; });
  }, intervalSeconds * 1000);
  timer.unref();
  return () => clearInterval(timer);
}

/**
 * Node HTTP server timeouts. Node's defaults (300s per request, 60s for headers) let a client
 * that trickles a body hold a connection, and its buffer, for five minutes. 120s still moves the
 * full 8 MiB body ceiling at ~70 KB/s; real Dream Cycle submissions are far smaller. Response
 * streaming (GET results) is not covered by `requestTimeout`, which ends once the request is in.
 * On Workers the platform owns these limits.
 */
export const SERVER_TIMEOUTS = {
  requestTimeout: 120_000,
  headersTimeout: 15_000,
} as const;
