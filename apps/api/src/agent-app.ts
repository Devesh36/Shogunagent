/**
 * The agent-facing routes (docs/implementation-plan.md E3 + E9), as one Hono app that
 * `createApp` mounts next to the licence routes (`app.ts`). Every route is documented with
 * examples in apps/api/README-agent-api.md.
 *
 *   GET  /.well-known/pricing.json       the rate table (pricing.ts), no auth
 *   GET  /v1/agents/challenge            a proof-of-work challenge, no auth
 *   POST /v1/agents                      self-registration (PoW + per-IP limit) → key + $5 wallet
 *   GET  /v1/agents/me                   the key's principal, scopes, caps        [read]
 *   POST /v1/keys/:id/revoke             revoke a key (own key, or admin)         [any]
 *   PATCH /v1/keys/:id                   set a key's hard caps                     [any; raising needs admin]
 *   GET  /v1/wallet                      balance, reserved, today's and this month's spend [read]
 *   POST /v1/wallet/topup                a Stripe Checkout session                 [write]
 *   POST /v1/stripe/webhook              Stripe → wallet credit (signed, idempotent), no key
 *   POST /v1/quote                       the exact cost of a call, nothing reserved [read]
 *   GET  /v1/receipts?since=             per-call receipts                         [read]
 *   POST /v1/receipts/:id/dispute        open a dispute                            [write]
 *   POST /v1/internal/router/connector-token  a caller's connector token for one router run (connector-tokens.ts) [router token + caller key]
 *   POST /v1/connectors/:provider/connect   a link a human opens to connect their account (connector-connect.ts) [run]
 *   GET|POST /v1/connectors/connect/:link, GET /v1/connectors/callback  the human's connect pages, no key
 *   POST /v1/handoff                     an A2A state-of-work handoff, relayed to the core (handoff.ts) [read]
 *   /v1/ingest-hooks…                    provider webhooks into memory (ingest-hooks/app.ts, E44) [write / signed]
 *   ALL  /mcp                            the metered MCP proxy (metering.ts)       [by unit]
 *
 * Bodies are read only where the route's contract is JSON the gateway itself defines
 * (registration, caps, top-up, quote, dispute, the Stripe event); `/mcp` and `/v1/handoff` bodies
 * are relayed untouched and never parsed.
 */
import { Hono, type Context } from "hono";
import { bodyLimit } from "hono/body-limit";
import { randomUUID } from "node:crypto";

import {
  DEFAULT_PBKDF2_ITERATIONS, agentError, agentPrincipalOf, generateAgentKey, hashAgentKey, requireAgentKey,
  type AgentKeyLookup, type AgentKeyRecord, type AgentPrincipal, type AgentScope, type AgentVars,
} from "./agent-keys.js";
import type { KeyObject } from "node:crypto";

import { capSubjectKey, capSubjectPrincipal, type AgentStore } from "./agent-store.js";
import type { TrustedKeys } from "./auth.js";
import { InMemoryNotifier } from "./billing/notify.js";
import { registerBillingRoutes } from "./billing/routes.js";
import {
  MAX_TOPUP_MICROS, MIN_TOPUP_MICROS, StripeError, chargeRefundedOf, checkoutSessionEventOf, createCheckoutSession, creditFromEvent, verifyStripeSignature,
  type StripeFetch,
} from "./billing/stripe.js";
import { createOAuthApp, oauthKeyLookup, type OAuthDeps } from "./oauth/app.js";
import { DEFAULT_CAPS, capsFor, parseCapMicros, periodsOf, type CapSet } from "./caps.js";
import type { LogFn } from "./logging.js";
import { classifyCall, fixedRunPriceMicros, pricingDocument, quoteCost, usageOf, FREE_ALLOWANCE_MICROS, type MeterUnit } from "./pricing.js";
import { DEFAULT_POW_BITS, MAX_POW_BITS, challengeHash, mintChallenge, verifyPow } from "./pow.js";
import { WindowLimiter, type RateLimiter } from "./ratelimit.js";
import { metered, priceCall, type MeteringDeps } from "./metering.js";
import { HANDOFF_PATH, handleHandoff } from "./handoff.js";
import { createApprovalsApp, type ApprovalsDeps } from "./approvals/app.js";
import { createWellknown, type WellknownDeps } from "./wellknown/app.js";
import { registerPayoutRoutes, type PayoutDeps } from "./payouts-app.js";
import { ROUTER_RESERVE_PATH, ROUTER_SETTLEMENTS_PATH, registerRouterWalletRoutes } from "./router-wallet.js";
import { registerConnectorConnectRoutes, registerConnectorStatusRoutes } from "./connector-connect.js";
import { CONNECTOR_TOKEN_PATH, registerConnectorTokenRoute, type ConnectorVault } from "./connector-tokens.js";
import { createEnterpriseApp, ssoConsentPolicy, type EnterpriseDeps } from "./enterprise/app.js";
import { registerIngestHookRoutes, type IngestHookDeps } from "./ingest-hooks/app.js";
import { regionAdmission } from "./enterprise/region.js";

/** Self-registration: at most this many per IP per hour (in-process default; production wires
 * `SqlWindowLimiter`). */
export const REGISTER_PER_HOUR = 5;
export const REGISTER_WINDOW_MS = 3_600_000;
/** A self-registered key lives a year; the agent rotates by registering again. */
export const DEFAULT_KEY_TTL_MS = 365 * 86_400_000;
export const SELF_REGISTERED_SCOPES: readonly AgentScope[] = Object.freeze(["read", "write", "run"]);
const MAX_SMALL_BODY_BYTES = 4 * 1024;
const MAX_WEBHOOK_BODY_BYTES = 64 * 1024;
const MAX_RECEIPTS = 500;
const MAX_REASON_LENGTH = 1_000;
const MAX_REF_LENGTH = 128;
const REF_RE = /^[A-Za-z0-9_.:-]{1,128}$/;

export interface StripeDeps {
  secretKey: string;
  webhookSecret: string;
  /** Default: the global `fetch`. */
  fetch?: StripeFetch;
  /** Where Checkout sends the human afterwards. Defaults derive from `publicBaseUrl`. */
  successUrl?: string;
  cancelUrl?: string;
  /** `STRIPE_COMMIT_OVERAGE_PRICE_ID` (E19): the metered price commit overage is billed on. */
  overagePriceId?: string;
}

export interface AgentDeps extends MeteringDeps {
  /** HMAC secret of the PoW challenges (`AGENT_POW_SECRET`). */
  powSecret: string;
  /** Leading zero bits a registration must present (default `DEFAULT_POW_BITS`). */
  powBits?: number;
  /** Per-IP registration limiter (default: in-process window, `REGISTER_PER_HOUR`). */
  registerLimiter?: RateLimiter;
  stripe?: StripeDeps;
  /** PBKDF2 rounds for new keys (tests lower it). */
  keyIterations?: number;
  keyTtlMs?: number;
  /** The agent-readable documents (plan E10, `wellknown/app.ts`): the agent card's signing key,
   * the MCP URL when it is not `<publicBaseUrl>/mcp`, and test overrides. */
  wellknown?: Omit<WellknownDeps, "publicBaseUrl" | "now">;
  /** The ShogunAI licence trust roots (E14-T3: `POST /v1/agents/me/licence`). */
  licensePublicKey?: KeyObject | TrustedKeys;
  /** OAuth 2.1 + Client ID Metadata Documents (E3-T2). Absent = OAuth is not served and `sat_`
   * tokens are not accepted. */
  oauth?: OAuthDeps;
  /** Remote approvals (E15, approvals/app.ts): the approval page, the Slack app and the
   * webhooks. Absent = those routes do not exist on this gateway. */
  approvals?: Omit<ApprovalsDeps, "log" | "now" | "publicBaseUrl">;
  /** Provider payouts (E29, payouts-app.ts): the settlement seam, Connect onboarding, provider
   * statements and the payout run. Absent = those routes do not exist on this gateway. */
  payouts?: PayoutDeps;
  /** The router's wallet (E32, router-wallet.ts): `ROUTER_SETTLEMENT_TOKEN`. Set = the router
   * reserves and settles its runs here (`/v1/internal/router/{reserve,settlements}`). Absent, a
   * `payouts.settlementToken` serves the same routes. */
  routerWallet?: { token: string };
  /** E48 / E50: where the router's per-caller connector tokens come from (connector-tokens.ts):
   * the principal's sidecar (`sidecarConnectorVault`). The token route is served with the router's
   * wallet (same `ROUTER_SETTLEMENT_TOKEN`); without a vault it answers every caller
   * `404 connector_not_connected`. With `connectSecret` (`CONNECTOR_CONNECT_SECRET`) and a vault,
   * the hosted connect flow is served too (connector-connect.ts). */
  connectorTokens?: { vault?: ConnectorVault; connectSecret?: string };
  /** The enterprise lane (E26, enterprise/app.ts): tenants, SSO for tenant admins, SCIM, region
   * separation, the tenant audit export. Absent = none of those routes exist and every principal
   * is served here. */
  enterprise?: EnterpriseDeps;
  /** Provider webhooks into memory (E44, ingest-hooks/app.ts): `INGEST_HOOKS_SECRET`. Absent =
   * the `/v1/ingest-hooks` routes do not exist on this gateway. */
  ingestHooks?: IngestHookDeps;
}

/** The client IP as the edge reports it. Workers set `CF-Connecting-IP`; behind another proxy the
 * first `X-Forwarded-For` entry; a direct Node connection has neither and shares one bucket. */
export function clientIp(c: Context): string {
  const cf = c.req.header("cf-connecting-ip")?.trim();
  if (cf) return cf;
  const xff = c.req.header("x-forwarded-for")?.split(",")[0]?.trim();
  if (xff) return xff;
  return c.req.header("x-real-ip")?.trim() || "unknown";
}

function isoOrNull(ms: number | undefined): string | null {
  return ms === undefined ? null : new Date(ms).toISOString();
}

function capsBody(caps: CapSet): Record<string, unknown> {
  return {
    key: { day_micro_usd: caps.key.day, month_micro_usd: caps.key.month },
    principal: { day_micro_usd: caps.principal.day, month_micro_usd: caps.principal.month },
  };
}

async function readJson(c: Context): Promise<Record<string, unknown> | undefined> {
  try {
    const raw: unknown = await c.req.json();
    return typeof raw === "object" && raw !== null && !Array.isArray(raw) ? (raw as Record<string, unknown>) : {};
  } catch {
    return undefined;
  }
}

export function createAgentApp(deps: AgentDeps): Hono<{ Variables: AgentVars }> {
  const app = new Hono<{ Variables: AgentVars }>();
  const now = deps.now ?? (() => new Date());
  const store: AgentStore = deps.store;
  const caps = deps.caps ?? DEFAULT_CAPS;
  const powBits = Math.min(MAX_POW_BITS, Math.max(0, deps.powBits ?? DEFAULT_POW_BITS));
  const registerLimiter = deps.registerLimiter ?? new WindowLimiter(REGISTER_PER_HOUR, REGISTER_WINDOW_MS);
  const bodyTooLarge = (c: Context): Response => agentError(c, 413, "body_too_large", "The body is too large for this route.");
  // E3-T2: with OAuth on, every key-guarded route (and /mcp) also accepts `sat_` access tokens.
  const baseLookup: AgentKeyLookup = deps.oauth ? oauthKeyLookup(store, deps.oauth.store) : store;
  // E26: with the enterprise lane on, a principal homed in another region is refused (421) on
  // every key-guarded route and on /mcp, before anything is read or booked.
  const lookup: AgentKeyLookup = deps.enterprise
    ? { keysByPrefix: (prefix) => baseLookup.keysByPrefix(prefix), ...(baseLookup.accessToken ? { accessToken: baseLookup.accessToken.bind(baseLookup) } : {}), admit: regionAdmission(deps.enterprise.store, deps.enterprise.region) }
    : baseLookup;
  const base = deps.publicBaseUrl.replace(/\/+$/, "");
  // E19: the pluggable notifier; the default keeps events in memory and never logs them.
  const notifier = deps.notifier ?? new InMemoryNotifier();
  const meteringDeps = {
    ...deps, notifier, keyLookup: deps.keyLookup ?? lookup,
    ...(deps.oauth ? { resourceMetadataUrl: `${base}/.well-known/oauth-protected-resource/mcp` } : {}),
  };
  const requireKey = (scopes: readonly AgentScope[]) => requireAgentKey(lookup, scopes, now);
  const principalOf = (c: Context<{ Variables: AgentVars }>): AgentPrincipal => {
    const p = agentPrincipalOf(c);
    if (!p) throw new Error("route registered without requireAgentKey");
    return p;
  };
  const effectiveCaps = async (p: AgentPrincipal): Promise<CapSet> => capsFor(caps, p, (await store.principal(p.principalId).catch(() => undefined)) ?? {});
  const storeFailure = (c: Context): Response => {
    c.set("relayLogEvent", "wallet_unavailable");
    return agentError(c, 503, "store_unavailable", "The agent store is unavailable. Retry shortly.", "retry_after", { "retry-after": "1" });
  };

  // --- pricing (E10-T7 reads this; one source with the metering) ---
  app.get("/.well-known/pricing.json", (c) => c.json(pricingDocument({ caps }), 200, { "cache-control": "public, max-age=300" }));

  // --- the other agent-readable documents (E10) and the status window over the agent routes ---
  const wellknown = createWellknown({ ...(deps.wellknown ?? {}), publicBaseUrl: deps.publicBaseUrl, now });
  for (const path of ["/mcp", "/v1/agents", "/v1/agents/*", "/v1/keys/*", "/v1/wallet", "/v1/wallet/*", "/v1/quote", "/v1/receipts", "/v1/receipts/*",
    HANDOFF_PATH, "/v1/statements", "/v1/commit", "/oauth/*"]) {
    app.use(path, wellknown.observe);
  }
  app.route("/", wellknown.app);

  // --- OAuth 2.1 + CIMD (E3-T2) ---
  if (deps.oauth) {
    app.route("/", createOAuthApp({
      ...deps.oauth, agentStore: store, publicBaseUrl: deps.publicBaseUrl, ...(deps.wellknown?.mcpUrl ? { mcpUrl: deps.wellknown.mcpUrl } : {}), now,
      // E26: an SSO-enforced tenant's principals cannot consent by pasting a key.
      ...(deps.enterprise ? { consentPolicy: ssoConsentPolicy(deps.enterprise.store) } : {}),
    }));
  }

  // --- enterprise (E26): tenants, SSO, SCIM, the tenant audit export ---
  if (deps.enterprise) {
    for (const path of ["/v1/admin/tenants", "/v1/admin/tenants/*", "/v1/tenant/*", "/sso/*", "/scim/*"]) app.use(path, wellknown.observe);
    app.route("/", createEnterpriseApp({
      ...deps.enterprise, agentStore: store, ...(deps.oauth ? { oauthStore: deps.oauth.store } : {}),
      publicBaseUrl: deps.publicBaseUrl, now, log: deps.log, ...(deps.keyIterations !== undefined ? { keyIterations: deps.keyIterations } : {}), requireKey,
    }));
  }

  // --- registration (E3-T3) ---
  app.get("/v1/agents/challenge", (c) => c.json(mintChallenge(deps.powSecret, powBits, now().getTime()), 200, { "cache-control": "no-store" }));

  app.post("/v1/agents", bodyLimit({ maxSize: MAX_SMALL_BODY_BYTES, onError: bodyTooLarge }), async (c) => {
    const nowMs = now().getTime();
    // Per-IP pacing first: a refused registration costs the relay nothing, not even a PoW check.
    const ip = clientIp(c);
    if (!(await registerLimiter.take(ip, nowMs))) {
      c.set("relayLogEvent", "register_rate_limited");
      const retry = Math.max(1, Math.ceil(await registerLimiter.retryAfterSecs(ip, nowMs)));
      return agentError(c, 429, "rate_limited", `Too many registrations from this address. Retry after ${retry} seconds.`, "retry_after", { "retry-after": String(retry) });
    }
    const pow = verifyPow(c.req.header("x-agent-pow"), deps.powSecret, powBits, nowMs);
    if (!pow.ok) {
      c.set("relayLogEvent", "pow_rejected");
      return agentError(c, 401, pow.code, `Solve GET /v1/agents/challenge (sha256(challenge + nonce) with ${powBits} leading zero bits) and send it as X-Agent-Pow: <challenge>:<nonce>.`, "solve_challenge");
    }
    const body = c.req.header("content-length") === "0" || c.req.header("content-type") === undefined ? {} : await readJson(c);
    if (body === undefined) return agentError(c, 400, "invalid_json", "The body must be JSON (or empty).");
    const ref = body.ref;
    if (ref !== undefined && (typeof ref !== "string" || !REF_RE.test(ref) || ref.length > MAX_REF_LENGTH)) {
      return agentError(c, 400, "invalid_ref", "ref must be a key id or developer id: letters, digits, `_ . : -`, at most 128 characters.");
    }
    const requested = (typeof body.caps === "object" && body.caps !== null ? body.caps : {}) as Record<string, unknown>;
    const capDay = parseCapMicros(requested.day_micro_usd);
    const capMonth = parseCapMicros(requested.month_micro_usd);
    if (capDay === null || capMonth === null) return agentError(c, 400, "invalid_caps", "caps.day_micro_usd and caps.month_micro_usd must be non-negative integers of micro-USD.");
    if ((capDay !== undefined && capDay > caps.key.day) || (capMonth !== undefined && capMonth > caps.key.month)) {
      return agentError(c, 400, "caps_above_default", `A self-registered key may only lower its caps (defaults: ${caps.key.day} per day, ${caps.key.month} per month, micro-USD).`);
    }
    let spent: boolean;
    try {
      spent = await store.spendChallenge(challengeHash(pow.challenge), pow.expiresAt, nowMs);
    } catch {
      return storeFailure(c);
    }
    if (!spent) {
      c.set("relayLogEvent", "pow_rejected");
      return agentError(c, 401, "pow_reused", "This challenge was already used. Fetch a new one from GET /v1/agents/challenge.", "solve_challenge");
    }
    const principalId = `prn_${randomUUID()}`;
    const keyId = `ak_${randomUUID()}`;
    const { key, prefix } = generateAgentKey("live");
    const record: AgentKeyRecord = {
      keyId, prefix, hash: await hashAgentKey(key, deps.keyIterations ?? DEFAULT_PBKDF2_ITERATIONS), mode: "live",
      principalId, tenantId: principalId, scopes: [...SELF_REGISTERED_SCOPES], createdAt: nowMs,
      expiresAt: nowMs + (deps.keyTtlMs ?? DEFAULT_KEY_TTL_MS),
      ...(typeof ref === "string" ? { ref } : {}),
      ...(capDay !== undefined ? { capDayMicros: capDay } : {}),
      ...(capMonth !== undefined ? { capMonthMicros: capMonth } : {}),
    };
    try {
      await store.createPrincipal({ principalId, tenantId: principalId, createdAt: nowMs, ...(typeof ref === "string" ? { ref } : {}) });
      await store.insertKey(record);
      // The free allowance, once per principal (the fixed topup id makes a retried insert a no-op).
      await store.credit(principalId, `grant:signup:${principalId}`, FREE_ALLOWANCE_MICROS, "signup_grant", nowMs);
    } catch {
      return storeFailure(c);
    }
    c.set("relayLogAgentKey", keyId);
    c.set("relayLogEvent", "agent_registered");
    const effective = capsFor(caps, record, {});
    return c.json({
      key, key_id: keyId, prefix, principal_id: principalId, tenant_id: principalId, scopes: record.scopes,
      created_at: new Date(nowMs).toISOString(), expires_at: isoOrNull(record.expiresAt), ref: record.ref ?? null,
      wallet: { balance_micro_usd: FREE_ALLOWANCE_MICROS, reserved_micro_usd: 0 },
      caps: capsBody(effective),
      next_action: null,
    }, 201, { "cache-control": "no-store" });
  });

  app.get("/v1/agents/me", requireKey(["read"]), async (c) => {
    const p = principalOf(c);
    let effective: CapSet;
    try { effective = await effectiveCaps(p); } catch { return storeFailure(c); }
    return c.json({
      key_id: p.keyId, mode: p.mode, principal_id: p.principalId, tenant_id: p.tenantId, scopes: p.scopes,
      expires_at: isoOrNull(p.expiresAt), ref: p.ref ?? null, caps: capsBody(effective),
    }, 200, { "cache-control": "no-store" });
  });

  // --- keys ---
  // Key management takes the key itself: an OAuth access token is a delegate of a key and may not
  // revoke or re-cap it.
  const keyOnly = (c: Context<{ Variables: AgentVars }>): Response | undefined =>
    principalOf(c).via === "oauth" ? agentError(c, 403, "oauth_not_allowed", "Manage keys with an agent key, not an OAuth access token.") : undefined;

  app.post("/v1/keys/:id/revoke", requireKey([]), async (c) => {
    const refused = keyOnly(c);
    if (refused) return refused;
    const p = principalOf(c);
    const id = c.req.param("id");
    try {
      const target = await store.keyById(id);
      // Unknown and someone else's answer the same: no oracle on key ids.
      if (!target || (target.principalId !== p.principalId && !p.scopes.includes("admin"))) return agentError(c, 404, "key_not_found", "No such key.");
      await store.revokeKey(id, now().getTime());
    } catch {
      return storeFailure(c);
    }
    return c.json({ key_id: id, revoked: true }, 200, { "cache-control": "no-store" });
  });

  app.patch("/v1/keys/:id", requireKey([]), bodyLimit({ maxSize: MAX_SMALL_BODY_BYTES, onError: bodyTooLarge }), async (c) => {
    const refused = keyOnly(c);
    if (refused) return refused;
    const p = principalOf(c);
    const id = c.req.param("id");
    const body = await readJson(c);
    if (body === undefined) return agentError(c, 400, "invalid_json", "The body must be JSON.");
    const capsIn = (typeof body.caps === "object" && body.caps !== null ? body.caps : body) as Record<string, unknown>;
    const day = parseCapMicros(capsIn.day_micro_usd);
    const month = parseCapMicros(capsIn.month_micro_usd);
    if (day === null || month === null) return agentError(c, 400, "invalid_caps", "day_micro_usd and month_micro_usd must be non-negative integers of micro-USD (omit one to reset it to the default).");
    const admin = p.scopes.includes("admin");
    if (!admin && ((day !== undefined && day > caps.key.day) || (month !== undefined && month > caps.key.month))) {
      return agentError(c, 403, "caps_above_default", `Raising a key's caps above the defaults (${caps.key.day} per day, ${caps.key.month} per month) needs the admin scope.`);
    }
    try {
      const target = await store.keyById(id);
      if (!target || (target.principalId !== p.principalId && !admin)) return agentError(c, 404, "key_not_found", "No such key.");
      await store.setKeyCaps(id, { ...(day !== undefined ? { dayMicros: day } : {}), ...(month !== undefined ? { monthMicros: month } : {}) });
      const updated = await store.keyById(id);
      const principalRow = await store.principal(target.principalId);
      return c.json({ key_id: id, caps: capsBody(capsFor(caps, updated ?? target, principalRow ?? {})) }, 200, { "cache-control": "no-store" });
    } catch {
      return storeFailure(c);
    }
  });

  // --- wallet (E9-T1, E9-T4) ---
  app.get("/v1/wallet", requireKey(["read"]), async (c) => {
    const p = principalOf(c);
    const { day, month } = periodsOf(now().getTime());
    try {
      const [wallet, effective, keyDay, keyMonth, prnDay, prnMonth] = await Promise.all([
        store.wallet(p.principalId), effectiveCaps(p),
        store.periodSpend(capSubjectKey(p.keyId), day), store.periodSpend(capSubjectKey(p.keyId), month),
        store.periodSpend(capSubjectPrincipal(p.principalId), day), store.periodSpend(capSubjectPrincipal(p.principalId), month),
      ]);
      return c.json({
        principal_id: p.principalId,
        balance_micro_usd: wallet.balanceMicros, reserved_micro_usd: wallet.reservedMicros,
        available_micro_usd: Math.max(0, wallet.balanceMicros - wallet.reservedMicros),
        caps: capsBody(effective),
        spend: { key: { day_micro_usd: keyDay, month_micro_usd: keyMonth }, principal: { day_micro_usd: prnDay, month_micro_usd: prnMonth } },
        topup: { rail: "card", url: `${deps.publicBaseUrl.replace(/\/+$/, "")}/v1/wallet/topup`, method: "POST", min_micro_usd: MIN_TOPUP_MICROS, max_micro_usd: MAX_TOPUP_MICROS },
      }, 200, { "cache-control": "no-store" });
    } catch {
      return storeFailure(c);
    }
  });

  app.post("/v1/wallet/topup", requireKey(["write"]), bodyLimit({ maxSize: MAX_SMALL_BODY_BYTES, onError: bodyTooLarge }), async (c) => {
    const p = principalOf(c);
    if (!deps.stripe) {
      c.set("relayLogEvent", "stripe_unavailable");
      return agentError(c, 503, "topup_unavailable", "Card top-ups are not configured on this gateway.", "retry_after", { "retry-after": "3600" });
    }
    const body = await readJson(c);
    if (body === undefined) return agentError(c, 400, "invalid_json", "The body must be JSON: {\"amount_micro_usd\": 10000000}.");
    const amount = body.amount_micro_usd;
    if (typeof amount !== "number" || !Number.isSafeInteger(amount) || amount < MIN_TOPUP_MICROS || amount > MAX_TOPUP_MICROS) {
      return agentError(c, 400, "invalid_amount", `amount_micro_usd must be an integer between ${MIN_TOPUP_MICROS} and ${MAX_TOPUP_MICROS}.`);
    }
    const idem = c.req.header("idempotency-key")?.trim();
    if (idem !== undefined && (idem.length === 0 || idem.length > 255)) return agentError(c, 400, "invalid_idempotency_key", "Idempotency-Key must be 1 to 255 characters.");
    const base = deps.publicBaseUrl.replace(/\/+$/, "");
    try {
      const session = await createCheckoutSession({
        secretKey: deps.stripe.secretKey, fetch: deps.stripe.fetch ?? (globalThis.fetch as unknown as StripeFetch),
        principalId: p.principalId, amountMicros: amount,
        successUrl: deps.stripe.successUrl ?? `${base}/v1/wallet/topup/done?status=success`,
        cancelUrl: deps.stripe.cancelUrl ?? `${base}/v1/wallet/topup/done?status=cancelled`,
        // Scoped to the principal so one agent's key cannot replay another's session.
        ...(idem ? { idempotencyKey: `${p.principalId}:${idem}` } : {}),
      });
      // E19: the session is tracked so `checkout.session.expired` can close it. Best effort: the
      // credit does not depend on this row (the webhook carries the principal).
      await store.recordCheckoutSession(session.sessionId, p.principalId, amount, now().getTime()).catch(() => deps.log("checkout_session_record_failure"));
      c.set("relayLogEvent", "topup_session_created");
      return c.json({
        rail: "card", url: session.url, session_id: session.sessionId, amount_micro_usd: amount,
        next_action: "approve_at_url",
        instructions: "Open url in a browser and pay. The wallet is credited when Stripe confirms; then retry the call that answered 402.",
      }, 201, { "cache-control": "no-store" });
    } catch (e) {
      c.set("relayLogEvent", "stripe_failure");
      deps.log(`stripe_failure status=${e instanceof StripeError ? e.status : 0}`);
      return agentError(c, 503, "topup_unavailable", "The card processor did not answer. Retry shortly.", "retry_after", { "retry-after": "30" });
    }
  });

  // A plain landing for the human after Checkout; the agent never calls it.
  app.get("/v1/wallet/topup/done", (c) => c.text(c.req.query("status") === "success" ? "Payment received. The wallet will be credited within a minute; the agent can retry now." : "Payment cancelled. Nothing was charged.", 200, { "cache-control": "no-store" }));

  app.post("/v1/stripe/webhook", bodyLimit({ maxSize: MAX_WEBHOOK_BODY_BYTES, onError: bodyTooLarge }), async (c) => {
    if (!deps.stripe) return agentError(c, 503, "topup_unavailable", "Card top-ups are not configured on this gateway.");
    // The raw body is needed for the signature; it is parsed once and never kept or logged.
    const raw = await c.req.text();
    if (!verifyStripeSignature(c.req.header("stripe-signature"), raw, deps.stripe.webhookSecret, Math.floor(now().getTime() / 1000))) {
      c.set("relayLogEvent", "webhook_rejected");
      return agentError(c, 401, "invalid_signature", "The Stripe signature did not verify.");
    }
    let event: unknown;
    try { event = JSON.parse(raw); } catch { return agentError(c, 400, "invalid_json", "The event must be JSON."); }
    const nowMs = now().getTime();
    const ack = (extra: Record<string, unknown>): Response => c.json({ received: true, credited: false, ...extra }, 200, { "cache-control": "no-store" });
    try {
      // E19: a refund (from the dashboard, the admin route or a dispute) debits the wallet by the
      // increase of the charge's refunded total, once per charge high-water mark.
      const refunded = chargeRefundedOf(event);
      if (refunded) {
        const principalId = (refunded.paymentIntentId ? await store.paymentPrincipal(refunded.paymentIntentId) : undefined) ?? refunded.principalIdHint;
        if (!principalId) return ack({ refunded: false });
        const out = await store.recordChargeRefund(principalId, refunded.chargeId, refunded.cumulativeMicros, nowMs);
        if (out.debitedMicros > 0) c.set("relayLogEvent", "refund_debited");
        return ack({ refunded: true, debited_micro_usd: out.debitedMicros });
      }
      const session = checkoutSessionEventOf(event);
      if (session?.kind === "expired") {
        const changed = await store.setCheckoutStatus(session.sessionId, "expired", nowMs);
        if (changed) c.set("relayLogEvent", "topup_expired");
        return ack({ expired: changed });
      }
      const credit = creditFromEvent(event);
      if (!credit) return ack({});
      const outcome = await store.credit(credit.principalId, credit.eventId, credit.micros, "stripe_checkout", nowMs);
      if (session) {
        await store.setCheckoutStatus(session.sessionId, "completed", nowMs);
        if (session.paymentIntentId) await store.recordPayment(session.paymentIntentId, credit.principalId, session.sessionId, credit.micros, nowMs);
      }
      c.set("relayLogEvent", outcome === "credited" ? "topup_credited" : "topup_duplicate");
      return c.json({ received: true, credited: outcome === "credited" }, 200, { "cache-control": "no-store" });
    } catch {
      // 503 makes Stripe retry the delivery; the event and charge ids keep the retry idempotent.
      return storeFailure(c);
    }
  });

  // --- quote (E9-T6) ---
  app.post("/v1/quote", requireKey(["read"]), bodyLimit({ maxSize: MAX_SMALL_BODY_BYTES, onError: bodyTooLarge }), async (c) => {
    const p = principalOf(c);
    const body = await readJson(c);
    if (body === undefined) return agentError(c, 400, "invalid_json", "The body must be JSON: {\"method\": \"tools/call\", \"name\": \"memory.ingest\", \"quantity\": 12}.");
    const method = typeof body.method === "string" ? body.method : "tools/call";
    const name = typeof body.name === "string" ? body.name : undefined;
    const quantity = body.quantity === undefined ? 1 : body.quantity;
    if (typeof quantity !== "number" || !Number.isSafeInteger(quantity) || quantity < 1) return agentError(c, 400, "invalid_quantity", "quantity must be a positive integer.");
    const unit: MeterUnit = classifyCall(method, name);
    let providerPrice: number | undefined;
    if (unit === "run") {
      const given = body.provider_price_micro_usd;
      if (given !== undefined && (typeof given !== "number" || !Number.isSafeInteger(given) || given < 0)) return agentError(c, 400, "invalid_price", "provider_price_micro_usd must be a non-negative integer.");
      providerPrice = typeof given === "number" ? given : (fixedRunPriceMicros(name) ?? (deps.runPricer ? await deps.runPricer(typeof body.tool_id === "string" ? body.tool_id : undefined, p) : undefined));
      if (providerPrice === undefined) return agentError(c, 400, "run_price_unavailable", "Pass provider_price_micro_usd (from router.inspect) to quote a run.");
    }
    const quote = quoteCost(unit, quantity, providerPrice);
    try {
      const wallet = await store.wallet(p.principalId);
      const available = Math.max(0, wallet.balanceMicros - wallet.reservedMicros);
      const effective = await effectiveCaps(p);
      // E19: an active commit discounts the quote; the list price rides alongside.
      const price = await priceCall(deps, p, quote, now().getTime());
      const sufficient = price.terms?.funding === "commit" || available >= price.charged;
      return c.json({
        quote: { ...usageOf(quote), cost_micro_usd: price.charged, ...(price.terms ? { list_cost_micro_usd: quote.cost_micro_usd, commit_id: price.terms.commitId, discount_bps: price.terms.discountBps } : {}) },
        method, name: name ?? null,
        balance_micro_usd: available, sufficient, caps: capsBody(effective),
        usage: { unit, quantity: 0, cost_micro_usd: 0 }, next_action: sufficient ? null : "top_up",
      }, 200, { "cache-control": "no-store" });
    } catch {
      return storeFailure(c);
    }
  });

  // --- receipts (E9-T7) ---
  app.get("/v1/receipts", requireKey(["read"]), async (c) => {
    const p = principalOf(c);
    const sinceRaw = c.req.query("since")?.trim();
    let sinceMs = 0;
    if (sinceRaw) {
      sinceMs = /^\d+$/.test(sinceRaw) ? Number(sinceRaw) : Date.parse(sinceRaw);
      if (!Number.isFinite(sinceMs)) return agentError(c, 400, "invalid_since", "since must be an ISO-8601 instant or milliseconds since the epoch.");
    }
    const limitRaw = c.req.query("limit")?.trim();
    const limit = limitRaw ? (/^\d+$/.test(limitRaw) ? Math.min(MAX_RECEIPTS, Number(limitRaw)) : Number.NaN) : 100;
    if (!Number.isSafeInteger(limit) || limit < 1) return agentError(c, 400, "invalid_limit", `limit must be between 1 and ${MAX_RECEIPTS}.`);
    try {
      const rows = await store.receipts(p.principalId, sinceMs, limit);
      const last = rows[rows.length - 1];
      return c.json({
        receipts: rows.map((r) => ({
          id: r.receiptId, key_id: r.keyId, method: r.method, name: r.name ?? null, unit: r.unit, quantity: r.quantity,
          // E48: what was reserved and what the call settled at (null while in flight, and on a
          // receipt settled before the gateway recorded it).
          reserved_quantity: r.quantity, settled_quantity: r.settled ? (r.settledQuantity ?? null) : null,
          cost_micro_usd: r.costMicros, reserved_micro_usd: r.settled ? null : r.estimateMicros, settled: r.settled, funding: r.funding, ts: new Date(r.ts).toISOString(),
        })),
        next_since: last ? new Date(last.ts + 1).toISOString() : null,
      }, 200, { "cache-control": "no-store" });
    } catch {
      return storeFailure(c);
    }
  });

  app.post("/v1/receipts/:id/dispute", requireKey(["write"]), bodyLimit({ maxSize: MAX_SMALL_BODY_BYTES, onError: bodyTooLarge }), async (c) => {
    const p = principalOf(c);
    const id = c.req.param("id");
    const body = await readJson(c);
    if (body === undefined) return agentError(c, 400, "invalid_json", "The body must be JSON: {\"reason\": \"…\"}.");
    const reason = body.reason;
    if (typeof reason !== "string" || reason.trim().length === 0 || reason.length > MAX_REASON_LENGTH) return agentError(c, 400, "invalid_reason", `reason is required, at most ${MAX_REASON_LENGTH} characters.`);
    try {
      const out = await store.dispute(id, p.principalId, reason.trim(), `dsp_${randomUUID()}`, now().getTime());
      if (!out) return agentError(c, 404, "receipt_not_found", "No such receipt.");
      return c.json({ dispute_id: out.dispute.disputeId, receipt_id: id, status: out.dispute.status, ts: new Date(out.dispute.ts).toISOString() }, out.created ? 201 : 200, { "cache-control": "no-store" });
    } catch {
      return storeFailure(c);
    }
  });

  // --- A2A handoff (E16, E35): the core assembles it; this route relays the body to the sidecar ---
  app.post(HANDOFF_PATH, requireKey(["read"]), bodyLimit({ maxSize: MAX_SMALL_BODY_BYTES, onError: bodyTooLarge }), (c) =>
    handleHandoff(c, principalOf(c), deps.mcpUpstream ? { mcpUpstream: deps.mcpUpstream } : {}));

  // --- E19 billing: statements, commits, adjustments, disputes, the Pro bundle link ---
  registerBillingRoutes(app, {
    store, log: deps.log, now, publicBaseUrl: deps.publicBaseUrl, requireKey, principalOf, storeFailure,
    ...(deps.stripe ? { stripe: { secretKey: deps.stripe.secretKey, ...(deps.stripe.fetch ? { fetch: deps.stripe.fetch } : {}), ...(deps.stripe.overagePriceId ? { overagePriceId: deps.stripe.overagePriceId } : {}) } } : {}),
    ...(deps.licensePublicKey ? { licensePublicKey: deps.licensePublicKey } : {}),
  });

  // --- E29 provider payouts: Connect onboarding, statements, the payout run ---
  if (deps.payouts) {
    for (const path of ["/v1/providers/*", "/v1/admin/providers", "/v1/admin/providers/*", "/v1/admin/payouts/*"]) app.use(path, wellknown.observe);
    registerPayoutRoutes(app, { ...deps.payouts, log: deps.log, now, publicBaseUrl: deps.publicBaseUrl, requireKey, principalOf });
  }

  // --- E32 the router's wallet: every router run reserved and settled here, once ---
  const routerToken = deps.routerWallet?.token ?? deps.payouts?.settlementToken;
  if (routerToken) {
    for (const path of [ROUTER_RESERVE_PATH, ROUTER_SETTLEMENTS_PATH, CONNECTOR_TOKEN_PATH]) app.use(path, wellknown.observe);
    registerRouterWalletRoutes(app, { token: routerToken, metering: meteringDeps, ...(deps.payouts ? { payouts: deps.payouts.store } : {}) });
    // E48: the calling principal's connector token for one router run (docs/api/connector-tokens.md).
    registerConnectorTokenRoute(app, { token: routerToken, keyLookup: meteringDeps.keyLookup, log: deps.log, now, ...(deps.connectorTokens?.vault ? { vault: deps.connectorTokens.vault } : {}) });
  } else if (deps.payouts) {
    app.post(ROUTER_SETTLEMENTS_PATH, (c) => agentError(c, 503, "settlements_unavailable", "Run settlements are not accepted on this gateway (ROUTER_SETTLEMENT_TOKEN unset).", "retry_after", { "retry-after": "3600" }));
  }

  // --- E50: the hosted connect flow (a human connects their own account; the agent gets a link) ---
  if (deps.connectorTokens?.vault) {
    for (const path of ["/v1/connectors", "/v1/connectors/*"]) app.use(path, wellknown.observe);
    // The principal's connections: a free read and a narrowing disconnect (E50 follow-up).
    registerConnectorStatusRoutes(app, { vault: deps.connectorTokens.vault, requireKey });
    if (deps.connectorTokens.connectSecret) {
      registerConnectorConnectRoutes(app, { secret: deps.connectorTokens.connectSecret, vault: deps.connectorTokens.vault, publicBaseUrl: deps.publicBaseUrl, requireKey, now });
    }
  }

  // --- remote approvals (E15): the page, Slack, webhooks, and the notification hook ---
  let onApprovalToken: MeteringDeps["onApprovalToken"];
  if (deps.approvals) {
    for (const path of ["/v1/webhooks", "/v1/webhooks/*", "/v1/slack/interactions", "/v1/approvals/notify"]) app.use(path, wellknown.observe);
    const approvals = createApprovalsApp({ ...deps.approvals, log: deps.log, now, publicBaseUrl: deps.publicBaseUrl }, requireKey);
    app.route("/", approvals.app);
    onApprovalToken = approvals.notifyCreated;
  }

  // --- the metered MCP proxy (E9-T5) ---
  // --- provider webhooks into memory (E44): verified here, stored by the core over the relay ---
  if (deps.ingestHooks) {
    registerIngestHookRoutes(app, deps.ingestHooks, meteringDeps, lookup, (path) => app.use(path, wellknown.observe));
  }

  app.all("/mcp", metered(onApprovalToken ? { ...meteringDeps, onApprovalToken } : meteringDeps));

  return app;
}
