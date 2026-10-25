/**
 * The provider payout routes (E29), registered on the agent app (`agent-app.ts`) when
 * `deps.payouts` is set. Contract: docs/api/payouts.md.
 *
 *   POST /v1/internal/router/settlements          the router's run settlements      [ROUTER_SETTLEMENT_TOKEN]
 *   GET  /v1/providers/me                          the provider account, status, this month [read]
 *   POST /v1/providers/me/connect                  a Stripe Connect Express onboarding link [write, key only]
 *   GET  /v1/providers/connect/return?state=       where Stripe sends the provider back (HTML, no JS)
 *   GET  /v1/providers/connect/refresh?state=      a fresh onboarding link (302), when Stripe's expired
 *   GET  /v1/providers/me/statements?month=        the provider's monthly statement   [read]
 *   POST /v1/admin/providers                       bind a listed provider to its owner principal [admin]
 *   POST /v1/admin/providers/:id/adjustments       a refund or a dispute against a provider [admin, Idempotency-Key]
 *   POST /v1/admin/providers/:id/disputes/:entry/resolve  provider_won | provider_lost [admin]
 *   POST /v1/admin/payouts/run                     run the monthly payout for a closed month [admin]
 *
 * Bodies here are JSON the gateway defines (a settlement is content-free by construction).
 * Nothing is logged but fixed words; amounts, ids and references never reach a log line.
 */
import { createHash, createHmac, timingSafeEqual } from "node:crypto";
import type { Context, Hono, MiddlewareHandler } from "hono";
import { bodyLimit } from "hono/body-limit";

import { agentError, type AgentPrincipal, type AgentScope, type AgentVars } from "./agent-keys.js";
import { canonicalJson } from "./wellknown/agent-card.js";
import { ACCOUNT_ID_RE, createAccountLink, createExpressAccount, createTransfer, retrieveAccount, statusOf, type ConnectClient, type ConnectFetch } from "./billing/connect.js";
import { monthBounds, monthClosed, monthOf } from "./billing/ledger.js";
import { StripeError } from "./billing/stripe.js";
import type { LogFn } from "./logging.js";
import {
  DEFAULT_MIN_PAYOUT_MICROS, MAX_SETTLEMENTS_PER_POST, PROVIDER_ADJUSTMENT_REASONS, PayoutStoreUnavailable, monthEndDay, parseSettlement, runProviderPayouts, settlementRecord,
  type PayoutStore, type ProviderAccount, type ProviderAdjustment, type TransferFn,
} from "./payouts.js";

export interface PayoutDeps {
  store: PayoutStore;
  /** `ROUTER_SETTLEMENT_TOKEN`: the bearer the router presents. Absent = the route answers 503. */
  settlementToken?: string;
  /** HMAC key of the onboarding return/refresh `state` (`PAYOUTS_STATE_SECRET`). */
  stateSecret: string;
  /** `PAYOUT_MIN_USD` in µ$ (default $10). */
  minPayoutMicros?: number;
  /** Stripe Connect. Absent = onboarding and transfers answer 503; accrual still works. */
  stripe?: { secretKey: string; fetch?: ConnectFetch };
}

export interface PayoutRouteDeps extends PayoutDeps {
  log: LogFn;
  now: () => Date;
  publicBaseUrl: string;
  requireKey: (scopes: readonly AgentScope[]) => MiddlewareHandler<{ Variables: AgentVars }>;
  principalOf: (c: Context<{ Variables: AgentVars }>) => AgentPrincipal;
}

const MAX_SMALL_BODY_BYTES = 4 * 1024;
const MAX_SETTLEMENT_BODY_BYTES = 64 * 1024;
const IDEM_RE = /^[\x21-\x7e]{1,255}$/;
const PROVIDER_ID_RE = /^[a-z0-9][a-z0-9_.-]{0,127}$/;
const PRINCIPAL_RE = /^[A-Za-z0-9_.:-]{1,128}$/;
const RUN_ID_RE = /^[A-Za-z0-9_.:-]{1,128}$/;
/** An onboarding `state` lives a week: long enough for a provider to finish KYC in several sittings. */
export const ONBOARDING_STATE_TTL_MS = 7 * 86_400_000;

const sha = (s: string): Buffer => createHash("sha256").update(s).digest();
const iso = (ms: number): string => new Date(ms).toISOString();

/** Constant-time comparison of a presented bearer with the configured token. */
export function tokenMatches(presented: string | undefined, expected: string): boolean {
  if (!presented) return false;
  const m = /^Bearer\s+(\S+)$/i.exec(presented.trim());
  if (!m?.[1]) return false;
  return timingSafeEqual(sha(m[1]), sha(expected));
}

/** `<base64url(provider_id)>.<exp ms>.<hex hmac>`: binds a return/refresh URL to one provider. */
export function signState(secret: string, providerId: string, expMs: number): string {
  const head = `${Buffer.from(providerId, "utf8").toString("base64url")}.${expMs}`;
  return `${head}.${createHmac("sha256", secret).update(head).digest("hex")}`;
}

export function verifyState(secret: string, state: string | undefined, nowMs: number): string | undefined {
  if (!state) return undefined;
  const parts = state.split(".");
  if (parts.length !== 3) return undefined;
  const [b64, exp, sig] = parts as [string, string, string];
  if (!/^\d{1,16}$/.test(exp) || Number(exp) <= nowMs || !/^[0-9a-f]{64}$/.test(sig)) return undefined;
  const expected = createHmac("sha256", secret).update(`${b64}.${exp}`).digest();
  if (!timingSafeEqual(Buffer.from(sig, "hex"), expected)) return undefined;
  const providerId = Buffer.from(b64, "base64url").toString("utf8");
  return PROVIDER_ID_RE.test(providerId) ? providerId : undefined;
}

async function readJson(c: Context): Promise<Record<string, unknown> | undefined> {
  try {
    const raw: unknown = await c.req.json();
    return typeof raw === "object" && raw !== null && !Array.isArray(raw) ? (raw as Record<string, unknown>) : undefined;
  } catch {
    return undefined;
  }
}

function accountBody(a: ProviderAccount): Record<string, unknown> {
  return {
    provider_id: a.providerId, status: a.status, connected: a.stripeAccountId !== undefined,
    // The bound reference is the provider's own Connect id, shown to the provider only.
    payout_account_ref: a.payoutAccountRef ?? null,
    created_at: iso(a.createdAt), updated_at: iso(a.updatedAt),
  };
}

function adjustmentBody(a: ProviderAdjustment): Record<string, unknown> {
  return {
    entry_id: a.entryId, kind: a.kind, status: a.status, amount_micro_usd: a.micros, reason: a.reason, run_id: a.runId ?? null,
    ts: iso(a.ts), resolved_at: a.resolvedAt !== undefined ? iso(a.resolvedAt) : null,
  };
}

function htmlPage(title: string, paragraphs: string[]): string {
  const esc = (s: string): string => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
  return `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>${esc(title)}</title>
<style>body{font:16px/1.5 system-ui,sans-serif;max-width:640px;margin:40px auto;padding:0 16px;color:#1a1a1a;background:#fff}@media (prefers-color-scheme:dark){body{background:#111;color:#eee}}</style></head>
<body><h1>${esc(title)}</h1>${paragraphs.map((p) => `<p>${esc(p)}</p>`).join("")}</body></html>`;
}

const HTML_HEADERS = {
  "content-type": "text/html; charset=UTF-8",
  "cache-control": "no-store",
  "content-security-policy": "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
  "x-content-type-options": "nosniff",
};

/** The provider's statement for `month` (docs/api/payouts.md). Amounts are integer µ$. */
export async function providerStatement(store: PayoutStore, providerId: string, month: string, nowMs: number, minPayoutMicros: number): Promise<Record<string, unknown> | undefined> {
  const bounds = monthBounds(month);
  const end = monthEndDay(month);
  if (!bounds || !end) return undefined;
  const days = await store.accruals(providerId, `${month}-01`, end);
  const totals = days.reduce((t, d) => ({
    runs: t.runs + d.runs, ok_runs: t.ok_runs + d.okRuns, empty_runs: t.empty_runs + d.emptyRuns, failed_runs: t.failed_runs + d.failedRuns,
    provider_micro_usd: t.provider_micro_usd + d.providerMicros, take_micro_usd: t.take_micro_usd + d.takeMicros,
  }), { runs: 0, ok_runs: 0, empty_runs: 0, failed_runs: 0, provider_micro_usd: 0, take_micro_usd: 0 });
  const adjustments = (await store.adjustments(providerId)).filter((a) => (a.ts >= bounds.start && a.ts < bounds.end) || a.status === "open" || (a.resolvedAt !== undefined && a.resolvedAt >= bounds.start && a.resolvedAt < bounds.end));
  const payout = await store.payout(providerId, month);
  const payee = days[days.length - 1]?.payee ?? null;
  const body: Record<string, unknown> = {
    schema: "shogunagent/provider-statement/v1",
    provider_id: providerId,
    month,
    status: monthClosed(month, nowMs) ? "closed" : "open",
    currency: "USD",
    amount_unit: "micro_usd",
    payee,
    totals: { ...totals, charged_micro_usd: totals.provider_micro_usd + totals.take_micro_usd },
    days: days.map((d) => ({
      day: d.day, runs: d.runs, ok_runs: d.okRuns, empty_runs: d.emptyRuns, failed_runs: d.failedRuns,
      provider_micro_usd: d.providerMicros, take_micro_usd: d.takeMicros, charged_micro_usd: d.providerMicros + d.takeMicros,
    })),
    adjustments: adjustments.map(adjustmentBody),
    payout: payout ? {
      status: payout.status, reason: payout.reason ?? null,
      earned_to_date_micro_usd: payout.earnedMicros, refunded_micro_usd: payout.refundedMicros, disputes_lost_micro_usd: payout.disputesLostMicros,
      previously_paid_micro_usd: payout.previouslyPaidMicros, payable_micro_usd: payout.payableMicros, amount_micro_usd: payout.amountMicros,
      transfer_id: payout.transferId ?? null, decided_at: iso(payout.updatedAt),
    } : null,
    minimum_payout_micro_usd: minPayoutMicros,
    take_bps: 500,
    notes: "Failures and empty results count as runs at $0. The take is ours on third-party runs only. A payout is cumulative: held or carried balances are paid by a later month, never twice.",
  };
  const { hash: _drop, ...rest } = body;
  return { ...body, hash: `sha256:${createHash("sha256").update(canonicalJson(rest as never)).digest("hex")}` };
}

export function registerPayoutRoutes(app: Hono<{ Variables: AgentVars }>, deps: PayoutRouteDeps): void {
  const { store, requireKey, principalOf } = deps;
  const base = deps.publicBaseUrl.replace(/\/+$/, "");
  const minPayout = deps.minPayoutMicros ?? DEFAULT_MIN_PAYOUT_MICROS;
  const small = bodyLimit({ maxSize: MAX_SMALL_BODY_BYTES, onError: (c) => agentError(c, 413, "body_too_large", "The body is too large for this route.") });
  const unavailable = (c: Context): Response => {
    c.set("relayLogEvent", "payout_store_unavailable");
    return agentError(c, 503, "store_unavailable", "The payout ledger is unavailable. Retry shortly.", "retry_after", { "retry-after": "1" });
  };
  const connect = (): ConnectClient | undefined =>
    deps.stripe ? { secretKey: deps.stripe.secretKey, fetch: deps.stripe.fetch ?? (globalThis.fetch as unknown as ConnectFetch) } : undefined;
  const requireAdmin: MiddlewareHandler<{ Variables: AgentVars }> = async (c, next) => {
    if (principalOf(c).via === "oauth") return agentError(c, 403, "oauth_not_allowed", "Admin routes take an admin agent key, not an OAuth access token.");
    await next();
  };
  const keyOnly: MiddlewareHandler<{ Variables: AgentVars }> = async (c, next) => {
    if (principalOf(c).via === "oauth") return agentError(c, 403, "oauth_not_allowed", "Provider onboarding takes the provider's agent key, not an OAuth access token.");
    await next();
  };
  const onboardingUrls = (providerId: string, nowMs: number): { refreshUrl: string; returnUrl: string } => {
    const state = encodeURIComponent(signState(deps.stateSecret, providerId, nowMs + ONBOARDING_STATE_TTL_MS));
    return { refreshUrl: `${base}/v1/providers/connect/refresh?state=${state}`, returnUrl: `${base}/v1/providers/connect/return?state=${state}` };
  };

  /** The provider this key acts for: its principal's only provider, or `?provider_id=` among several. */
  const myProvider = async (c: Context<{ Variables: AgentVars }>): Promise<ProviderAccount | Response> => {
    const owned = await store.accountsOwnedBy(principalOf(c).principalId);
    const wanted = c.req.query("provider_id")?.trim();
    if (wanted) {
      const hit = owned.find((a) => a.providerId === wanted);
      return hit ?? agentError(c, 404, "provider_not_found", "This key's principal does not own that provider.");
    }
    if (owned.length === 1 && owned[0]) return owned[0];
    if (owned.length > 1) return agentError(c, 400, "provider_id_required", `This principal owns ${owned.length} providers; pass ?provider_id=.`, "fix_input");
    return agentError(c, 404, "provider_not_found", "This key's principal is not bound to a listed provider. An operator binds it when the listing PR is merged.");
  };

  // --- the settlement seam (router → ledger) ---
  app.post("/v1/internal/router/settlements", bodyLimit({ maxSize: MAX_SETTLEMENT_BODY_BYTES, onError: (c) => agentError(c, 413, "body_too_large", `At most ${MAX_SETTLEMENTS_PER_POST} settlements per request.`) }), async (c) => {
    if (!deps.settlementToken) return agentError(c, 503, "settlements_unavailable", "Run settlements are not accepted on this gateway (ROUTER_SETTLEMENT_TOKEN unset).", "retry_after", { "retry-after": "3600" });
    if (!tokenMatches(c.req.header("authorization"), deps.settlementToken)) {
      c.set("relayLogEvent", "settlement_rejected");
      return agentError(c, 401, "invalid_token", "This route takes the router's settlement token.");
    }
    const body = await readJson(c);
    const list = body?.settlements;
    if (!Array.isArray(list) || list.length === 0 || list.length > MAX_SETTLEMENTS_PER_POST) return agentError(c, 400, "invalid_settlements", `The body must be {"settlements": [...]} with 1 to ${MAX_SETTLEMENTS_PER_POST} entries.`, "fix_input");
    const parsed = list.map(parseSettlement);
    const bad = parsed.findIndex((p) => !p.ok);
    if (bad >= 0) {
      const p = parsed[bad];
      return agentError(c, 400, "invalid_settlement", `settlements[${bad}]: ${p && !p.ok ? p.error : "invalid"}. Nothing was recorded.`, "fix_input");
    }
    let recorded = 0;
    let duplicates = 0;
    try {
      for (const p of parsed) {
        if (!p.ok) continue;
        const out = await store.recordSettlement(settlementRecord(p.value));
        if (out === "recorded") recorded += 1; else duplicates += 1;
      }
    } catch {
      // 503 makes the router retry; run ids keep the retry idempotent.
      return unavailable(c);
    }
    c.set("relayLogEvent", "settlements_recorded");
    return c.json({ recorded, duplicates }, 200, { "cache-control": "no-store" });
  });

  // --- the provider's own view ---
  app.get("/v1/providers/me", requireKey(["read"]), async (c) => {
    try {
      const mine = await myProvider(c);
      if (mine instanceof Response) return mine;
      let account = mine;
      // Refresh from Stripe while onboarding is unfinished, so the status is current without a webhook.
      const client = connect();
      if (client && account.stripeAccountId && account.status !== "active") {
        try {
          const state = await retrieveAccount(client, account.stripeAccountId);
          account = (await store.setAccountStatus(account.providerId, statusOf(state), deps.now().getTime())) ?? account;
        } catch {
          // Stripe unreachable: the stored status stands.
        }
      }
      const month = monthOf(deps.now().getTime());
      const end = monthEndDay(month) ?? month;
      const days = await store.accruals(account.providerId, `${month}-01`, end);
      return c.json({
        provider: accountBody(account),
        this_month: {
          month, runs: days.reduce((n, d) => n + d.runs, 0), provider_micro_usd: days.reduce((n, d) => n + d.providerMicros, 0),
        },
        minimum_payout_micro_usd: minPayout,
        next_action: account.status === "active" ? null : "approve_at_url",
        onboarding: account.status === "active" ? null : { method: "POST", url: `${base}/v1/providers/me/connect` },
      }, 200, { "cache-control": "no-store" });
    } catch (e) {
      if (e instanceof PayoutStoreUnavailable) return unavailable(c);
      throw e;
    }
  });

  app.post("/v1/providers/me/connect", requireKey(["write"]), keyOnly, async (c) => {
    const client = connect();
    if (!client) return agentError(c, 503, "connect_unavailable", "Payout onboarding is not configured on this gateway.", "retry_after", { "retry-after": "3600" });
    try {
      const mine = await myProvider(c);
      if (mine instanceof Response) return mine;
      const nowMs = deps.now().getTime();
      let accountId = mine.stripeAccountId;
      if (!accountId) {
        accountId = (await createExpressAccount(client, mine.providerId)).accountId;
        const stored = await store.setStripeAccount(mine.providerId, accountId, nowMs);
        accountId = stored?.stripeAccountId ?? accountId;
      }
      const link = await createAccountLink(client, { accountId, ...onboardingUrls(mine.providerId, nowMs) });
      c.set("relayLogEvent", "connect_link_created");
      return c.json({
        provider_id: mine.providerId, url: link.url, expires_at: link.expiresAt !== undefined ? iso(link.expiresAt) : null,
        next_action: "approve_at_url",
        instructions: "A human at the provider opens url and completes Stripe's onboarding. Payouts start once Stripe enables them; GET /v1/providers/me shows the status.",
      }, 201, { "cache-control": "no-store" });
    } catch (e) {
      if (e instanceof PayoutStoreUnavailable) return unavailable(c);
      c.set("relayLogEvent", "stripe_failure");
      deps.log(`connect_failure status=${e instanceof StripeError ? e.status : 0}`);
      return agentError(c, 503, "connect_unavailable", "The payout processor did not answer. Retry shortly.", "retry_after", { "retry-after": "30" });
    }
  });

  app.get("/v1/providers/connect/return", async (c) => {
    const nowMs = deps.now().getTime();
    const providerId = verifyState(deps.stateSecret, c.req.query("state"), nowMs);
    if (!providerId) return c.newResponse(htmlPage("Link expired", ["This onboarding link is invalid or has expired. Ask the provider's agent for a new one (POST /v1/providers/me/connect)."]), 400, HTML_HEADERS);
    const client = connect();
    try {
      const account = await store.account(providerId);
      if (!account?.stripeAccountId || !client) return c.newResponse(htmlPage("Onboarding not started", ["No payout account exists for this provider yet."]), 404, HTML_HEADERS);
      let status = account.status;
      try {
        status = statusOf(await retrieveAccount(client, account.stripeAccountId));
        await store.setAccountStatus(providerId, status, nowMs);
      } catch {
        // Stripe unreachable: show the stored status; GET /v1/providers/me refreshes it later.
      }
      const msg = status === "active"
        ? ["Payouts are enabled. Earnings are paid monthly once the balance reaches the minimum. You can close this page."]
        : status === "restricted"
          ? ["Stripe needs more information before payouts can start. Open a new onboarding link from the provider's agent to finish."]
          : ["Onboarding is not finished yet. Open a new onboarding link to continue where you left off."];
      return c.newResponse(htmlPage(status === "active" ? "Payouts enabled" : "Onboarding in progress", msg), 200, HTML_HEADERS);
    } catch (e) {
      if (e instanceof PayoutStoreUnavailable) return c.newResponse(htmlPage("Try again", ["The payout ledger is unavailable. Reload this page in a minute."]), 503, HTML_HEADERS);
      throw e;
    }
  });

  app.get("/v1/providers/connect/refresh", async (c) => {
    const nowMs = deps.now().getTime();
    const providerId = verifyState(deps.stateSecret, c.req.query("state"), nowMs);
    if (!providerId) return c.newResponse(htmlPage("Link expired", ["This onboarding link is invalid or has expired. Ask the provider's agent for a new one (POST /v1/providers/me/connect)."]), 400, HTML_HEADERS);
    const client = connect();
    try {
      const account = await store.account(providerId);
      if (!account?.stripeAccountId || !client || !ACCOUNT_ID_RE.test(account.stripeAccountId)) return c.newResponse(htmlPage("Onboarding not started", ["No payout account exists for this provider yet."]), 404, HTML_HEADERS);
      const link = await createAccountLink(client, { accountId: account.stripeAccountId, ...onboardingUrls(providerId, nowMs) });
      return c.redirect(link.url, 302);
    } catch {
      return c.newResponse(htmlPage("Try again", ["The payout processor did not answer. Reload this page in a minute."]), 503, HTML_HEADERS);
    }
  });

  app.get("/v1/providers/me/statements", requireKey(["read"]), async (c) => {
    const nowMs = deps.now().getTime();
    const month = c.req.query("month")?.trim() || monthOf(nowMs);
    if (!monthBounds(month)) return agentError(c, 400, "invalid_month", "month must be YYYY-MM.", "fix_input");
    try {
      const mine = await myProvider(c);
      if (mine instanceof Response) return mine;
      const body = await providerStatement(store, mine.providerId, month, nowMs, minPayout);
      return c.json(body, 200, { "cache-control": "no-store", "x-statement-hash": String(body?.hash ?? "") });
    } catch (e) {
      if (e instanceof PayoutStoreUnavailable) return unavailable(c);
      throw e;
    }
  });

  // --- operator routes ---
  app.post("/v1/admin/providers", requireKey(["admin"]), requireAdmin, small, async (c) => {
    const body = await readJson(c);
    const providerId = body?.provider_id;
    const owner = body?.owner_principal_id;
    if (typeof providerId !== "string" || !PROVIDER_ID_RE.test(providerId)) return agentError(c, 400, "invalid_provider", "provider_id must be the provider's catalog id.", "fix_input");
    if (typeof owner !== "string" || !PRINCIPAL_RE.test(owner)) return agentError(c, 400, "invalid_owner", "owner_principal_id must be a principal id.", "fix_input");
    try {
      const account = await store.upsertAccount(providerId, owner, deps.now().getTime());
      return c.json({ provider: accountBody(account), owner_principal_id: account.ownerPrincipalId }, 200, { "cache-control": "no-store" });
    } catch {
      return unavailable(c);
    }
  });

  app.post("/v1/admin/providers/:id/adjustments", requireKey(["admin"]), requireAdmin, small, async (c) => {
    const idem = c.req.header("idempotency-key")?.trim();
    if (!idem || !IDEM_RE.test(idem)) return agentError(c, 400, "idempotency_key_required", "Send an Idempotency-Key header (1 to 255 printable characters); retrying with the same key repeats the outcome without a second effect.");
    const providerId = c.req.param("id");
    const body = await readJson(c);
    if (!body) return agentError(c, 400, "invalid_json", "The body must be JSON.");
    const kind = body.kind;
    const amount = body.amount_micro_usd;
    const reason = body.reason;
    const runId = body.run_id;
    if (kind !== "refund" && kind !== "dispute") return agentError(c, 400, "invalid_kind", "kind must be refund or dispute.", "fix_input");
    if (typeof amount !== "number" || !Number.isSafeInteger(amount) || amount <= 0) return agentError(c, 400, "invalid_amount", "amount_micro_usd must be a positive integer.", "fix_input");
    if (typeof reason !== "string" || !(PROVIDER_ADJUSTMENT_REASONS as readonly string[]).includes(reason)) return agentError(c, 400, "invalid_reason", `reason must be one of ${PROVIDER_ADJUSTMENT_REASONS.join(", ")}.`, "fix_input");
    if (runId !== undefined && (typeof runId !== "string" || !RUN_ID_RE.test(runId))) return agentError(c, 400, "invalid_run", "run_id must be a router run id.", "fix_input");
    try {
      if (!(await store.account(providerId))) return agentError(c, 404, "provider_not_found", "No such provider account.");
      const out = await store.addAdjustment({
        entryId: `padj:${providerId}:${createHash("sha256").update(idem).digest("hex").slice(0, 24)}`, providerId, kind, micros: amount,
        status: kind === "refund" ? "applied" : "open", reason: reason as ProviderAdjustment["reason"], ...(typeof runId === "string" ? { runId } : {}), ts: deps.now().getTime(),
      });
      return c.json({ adjustment: adjustmentBody(out.adjustment), created: out.outcome === "created" }, out.outcome === "created" ? 201 : 200, { "cache-control": "no-store" });
    } catch {
      return unavailable(c);
    }
  });

  app.post("/v1/admin/providers/:id/disputes/:entry/resolve", requireKey(["admin"]), requireAdmin, small, async (c) => {
    const body = await readJson(c);
    const outcome = body?.outcome;
    if (outcome !== "provider_won" && outcome !== "provider_lost") return agentError(c, 400, "invalid_outcome", "outcome must be provider_won (nothing deducted) or provider_lost (deducted from the provider's next payout).", "fix_input");
    try {
      const out = await store.resolveDispute(c.req.param("id"), c.req.param("entry"), outcome === "provider_won" ? "won" : "lost", deps.now().getTime());
      if (!out) return agentError(c, 404, "dispute_not_found", "No such dispute for this provider.");
      return c.json({ adjustment: adjustmentBody(out) }, 200, { "cache-control": "no-store" });
    } catch {
      return unavailable(c);
    }
  });

  app.post("/v1/admin/payouts/run", requireKey(["admin"]), requireAdmin, small, async (c) => {
    const body = await readJson(c);
    const month = body?.month;
    if (typeof month !== "string" || !monthBounds(month)) return agentError(c, 400, "invalid_month", "month must be YYYY-MM.", "fix_input");
    try {
      const counts = await runProviderPayouts(store, transferFor(deps), month, deps.now().getTime(), minPayout);
      if ("error" in counts) return agentError(c, 409, counts.error, counts.error === "month_open" ? "That month has not closed yet (one hour after it ends, UTC)." : "month must be YYYY-MM.", "retry_after");
      c.set("relayLogEvent", "payouts_run");
      return c.json({ ...counts }, 200, { "cache-control": "no-store" });
    } catch {
      return unavailable(c);
    }
  });
}

/** The transfer function for a deployment, or `undefined` without Stripe (pending rows wait). */
export function transferFor(deps: Pick<PayoutDeps, "stripe">): TransferFn | undefined {
  if (!deps.stripe) return undefined;
  const client: ConnectClient = { secretKey: deps.stripe.secretKey, fetch: deps.stripe.fetch ?? (globalThis.fetch as unknown as ConnectFetch) };
  return (t) => createTransfer(client, t);
}
