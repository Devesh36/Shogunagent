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
 *   ALL  /mcp                            the metered MCP proxy (metering.ts)       [by unit]
 *
 * Bodies are read only where the route's contract is JSON the gateway itself defines
 * (registration, caps, top-up, quote, dispute, the Stripe event); `/mcp` bodies are relayed
 * untouched and never parsed.
 */
import { Hono, type Context } from "hono";
import { bodyLimit } from "hono/body-limit";
import { randomUUID } from "node:crypto";

import {
  DEFAULT_PBKDF2_ITERATIONS, agentError, agentPrincipalOf, generateAgentKey, hashAgentKey, requireAgentKey,
  type AgentKeyRecord, type AgentPrincipal, type AgentScope, type AgentVars,
} from "./agent-keys.js";
import { capSubjectKey, capSubjectPrincipal, type AgentStore } from "./agent-store.js";
import { MAX_TOPUP_MICROS, MIN_TOPUP_MICROS, StripeError, createCheckoutSession, creditFromEvent, verifyStripeSignature, type StripeFetch } from "./billing/stripe.js";
import { DEFAULT_CAPS, capsFor, parseCapMicros, periodsOf, type CapSet } from "./caps.js";
import type { LogFn } from "./logging.js";
import { classifyCall, pricingDocument, quoteCost, usageOf, FREE_ALLOWANCE_MICROS, type MeterUnit } from "./pricing.js";
import { DEFAULT_POW_BITS, MAX_POW_BITS, challengeHash, mintChallenge, verifyPow } from "./pow.js";
import { WindowLimiter, type RateLimiter } from "./ratelimit.js";
import { metered, type MeteringDeps } from "./metering.js";

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
  const requireKey = (scopes: readonly AgentScope[]) => requireAgentKey(store, scopes, now);
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
  app.post("/v1/keys/:id/revoke", requireKey([]), async (c) => {
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
    const credit = creditFromEvent(event);
    if (!credit) return c.json({ received: true, credited: false }, 200, { "cache-control": "no-store" });
    try {
      const outcome = await store.credit(credit.principalId, credit.eventId, credit.micros, "stripe_checkout", now().getTime());
      c.set("relayLogEvent", outcome === "credited" ? "topup_credited" : "topup_duplicate");
      return c.json({ received: true, credited: outcome === "credited" }, 200, { "cache-control": "no-store" });
    } catch {
      // 503 makes Stripe retry the delivery; the event id keeps the retry idempotent.
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
      providerPrice = typeof given === "number" ? given : deps.runPricer ? await deps.runPricer(typeof body.tool_id === "string" ? body.tool_id : undefined, p) : undefined;
      if (providerPrice === undefined) return agentError(c, 400, "run_price_unavailable", "Pass provider_price_micro_usd (from router.inspect) to quote a run.");
    }
    const quote = quoteCost(unit, quantity, providerPrice);
    try {
      const wallet = await store.wallet(p.principalId);
      const available = Math.max(0, wallet.balanceMicros - wallet.reservedMicros);
      const effective = await effectiveCaps(p);
      return c.json({
        quote: usageOf(quote), method, name: name ?? null,
        balance_micro_usd: available, sufficient: available >= quote.cost_micro_usd, caps: capsBody(effective),
        usage: { unit, quantity: 0, cost_micro_usd: 0 }, next_action: available >= quote.cost_micro_usd ? null : "top_up",
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
          cost_micro_usd: r.costMicros, reserved_micro_usd: r.settled ? null : r.estimateMicros, settled: r.settled, ts: new Date(r.ts).toISOString(),
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

  // --- the metered MCP proxy (E9-T5) ---
  app.all("/mcp", metered(deps));

  return app;
}
