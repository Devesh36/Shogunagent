/**
 * The E19 billing routes, registered on the agent app (`agent-app.ts`) behind the same key
 * middleware and logger. Documented in docs/api/billing.md and apps/api/README-agent-api.md.
 *
 *   GET  /v1/statements?month=YYYY-MM&format=json|csv   the month's statement       [read]
 *   GET  /v1/commit                                      the active commit, remaining [read]
 *   POST /v1/agents/me/licence                           link a ShogunAI licence (Pro bundle) [write]
 *   POST /v1/admin/commits                               create an annual commit     [admin, Idempotency-Key]
 *   POST /v1/admin/adjustments                           credit or refund, a ledger entry [admin, Idempotency-Key]
 *   POST /v1/admin/disputes/:id/resolve                  resolve a dispute (credit or reject) [admin]
 *
 * Admin routes take an `admin` agent key, never an OAuth access token (an OAuth grant cannot carry
 * `admin`). Request bodies here are JSON the gateway defines; nothing is logged but fixed words.
 */
import { createHash } from "node:crypto";
import type { KeyObject } from "node:crypto";
import type { Context, Hono, MiddlewareHandler } from "hono";
import { bodyLimit } from "hono/body-limit";

import { agentError, type AgentPrincipal, type AgentScope, type AgentVars } from "../agent-keys.js";
import type { AgentStore } from "../agent-store.js";
import { AuthError, meterSubject, verifyLicense, type TrustedKeys } from "../auth.js";
import type { LogFn } from "../logging.js";
import {
  ADJUSTMENT_REASONS, MAX_COMMIT_DISCOUNT_BPS, MAX_COMMIT_PERIOD_MS, MIN_COMMIT_DISCOUNT_BPS, PRO_BUNDLE_MICROS,
  buildStatement, commitActive, commitRemaining, monthBounds, monthClosed, monthOf, proGrantId, statementCsv,
  type CommitRecord,
} from "./ledger.js";
import { StripeError, createOverageSubscription, createRefund, type StripeFetch } from "./stripe.js";

const MAX_SMALL_BODY_BYTES = 4 * 1024;
const IDEM_RE = /^[\x21-\x7e]{1,255}$/;
const ID_RE = /^[A-Za-z0-9_.:-]{1,128}$/;

export interface BillingRouteDeps {
  store: AgentStore;
  log: LogFn;
  now: () => Date;
  publicBaseUrl: string;
  requireKey: (scopes: readonly AgentScope[]) => MiddlewareHandler<{ Variables: AgentVars }>;
  principalOf: (c: Context<{ Variables: AgentVars }>) => AgentPrincipal;
  storeFailure: (c: Context) => Response;
  stripe?: { secretKey: string; fetch?: StripeFetch; overagePriceId?: string };
  /** The licence trust roots, for `POST /v1/agents/me/licence` (E14-T3). Absent = 503. */
  licensePublicKey?: KeyObject | TrustedKeys;
}

async function readJson(c: Context): Promise<Record<string, unknown> | undefined> {
  try {
    const raw: unknown = await c.req.json();
    return typeof raw === "object" && raw !== null && !Array.isArray(raw) ? (raw as Record<string, unknown>) : undefined;
  } catch {
    return undefined;
  }
}

const iso = (ms: number): string => new Date(ms).toISOString();
const posInt = (v: unknown): number | undefined => (typeof v === "number" && Number.isSafeInteger(v) && v > 0 ? v : undefined);

export function commitBody(c: CommitRecord, nowMs: number): Record<string, unknown> {
  return {
    commit_id: c.commitId, subject: c.subjectKind, subject_id: c.subjectId, discount_bps: c.discountBps,
    amount_micro_usd: c.amountMicros, drawn_micro_usd: c.drawnMicros, remaining_micro_usd: commitRemaining(c),
    overage_micro_usd: c.overageMicros, overage_reported_micro_usd: c.overageReportedMicros,
    overage_billing: c.stripeSubscriptionItemId !== undefined, active: commitActive(c, nowMs),
    starts_at: iso(c.startsAt), ends_at: iso(c.endsAt),
  };
}

/** The idempotency key every admin write requires. */
function idempotencyKey(c: Context): string | Response {
  const idem = c.req.header("idempotency-key")?.trim();
  if (!idem || !IDEM_RE.test(idem)) return agentError(c, 400, "idempotency_key_required", "Send an Idempotency-Key header (1 to 255 printable characters); retrying with the same key repeats the outcome without a second effect.");
  return idem;
}

const shortHash = (s: string): string => createHash("sha256").update(s).digest("hex").slice(0, 24);

export function registerBillingRoutes(app: Hono<{ Variables: AgentVars }>, deps: BillingRouteDeps): void {
  const { store, requireKey, principalOf, storeFailure } = deps;
  const small = bodyLimit({ maxSize: MAX_SMALL_BODY_BYTES, onError: (c) => agentError(c, 413, "body_too_large", "The body is too large for this route.") });
  /** Admin: an `admin` key, and not an OAuth token. */
  const requireAdmin: MiddlewareHandler<{ Variables: AgentVars }> = async (c, next) => {
    const p = principalOf(c);
    if (p.via === "oauth") return agentError(c, 403, "oauth_not_allowed", "Admin routes take an admin agent key, not an OAuth access token.");
    await next();
  };
  const stripeClient = (): { secretKey: string; fetch: StripeFetch } | undefined =>
    deps.stripe ? { secretKey: deps.stripe.secretKey, fetch: deps.stripe.fetch ?? (globalThis.fetch as unknown as StripeFetch) } : undefined;

  // --- statements ---
  app.get("/v1/statements", requireKey(["read"]), async (c) => {
    const p = principalOf(c);
    const nowMs = deps.now().getTime();
    const month = c.req.query("month")?.trim() || monthOf(nowMs);
    const bounds = monthBounds(month);
    if (!bounds) return agentError(c, 400, "invalid_month", "month must be YYYY-MM (UTC).");
    if (bounds.start > nowMs) return agentError(c, 400, "month_not_started", "That month has not started yet; ask for the current month or an earlier one.");
    const accept = c.req.header("accept") ?? "";
    const format = (c.req.query("format")?.trim().toLowerCase() || (accept.includes("text/csv") ? "csv" : "json"));
    if (format !== "json" && format !== "csv") return agentError(c, 400, "invalid_format", "format must be json or csv.");
    let body: Record<string, unknown>;
    try {
      if (monthClosed(month, nowMs)) {
        let stored = await store.storedStatement(p.principalId, month);
        if (!stored) {
          const rendered = buildStatement(p.principalId, month, await store.statementInputs(p.principalId, month, nowMs), "closed", nowMs);
          stored = await store.saveStatement({ principalId: p.principalId, month, body: JSON.stringify(rendered), hash: String(rendered.hash), closedAt: nowMs });
          c.set("relayLogEvent", "statement_closed");
        }
        body = JSON.parse(stored.body) as Record<string, unknown>;
      } else {
        body = buildStatement(p.principalId, month, await store.statementInputs(p.principalId, month, nowMs), "open", nowMs);
      }
    } catch {
      return storeFailure(c);
    }
    const headers = { "cache-control": "no-store", "x-statement-hash": String(body.hash) };
    if (format === "csv") {
      return c.body(statementCsv(body), 200, { ...headers, "content-type": "text/csv; charset=utf-8", "content-disposition": `attachment; filename="statement-${month}.csv"` });
    }
    return c.json(body, 200, headers);
  });

  // --- commit ---
  app.get("/v1/commit", requireKey(["read"]), async (c) => {
    const p = principalOf(c);
    const nowMs = deps.now().getTime();
    try {
      const commit = await store.activeCommit(p.principalId, p.tenantId, nowMs);
      return c.json({ commit: commit ? commitBody(commit, nowMs) : null, next_action: null }, 200, { "cache-control": "no-store" });
    } catch {
      return storeFailure(c);
    }
  });

  app.post("/v1/admin/commits", requireKey(["admin"]), requireAdmin, small, async (c) => {
    const idem = idempotencyKey(c);
    if (typeof idem !== "string") return idem;
    const body = await readJson(c);
    if (!body) return agentError(c, 400, "invalid_json", "The body must be a JSON object.");
    const subject = body.subject === "tenant" ? "tenant" : body.subject === "principal" || body.subject === undefined ? "principal" : undefined;
    const subjectId = body.subject_id;
    const amount = posInt(body.amount_micro_usd);
    const bps = body.discount_bps;
    const startsAt = typeof body.starts_at === "string" ? Date.parse(body.starts_at) : Number.NaN;
    const endsAt = typeof body.ends_at === "string" ? Date.parse(body.ends_at) : Number.NaN;
    const customer = body.stripe_customer_id;
    if (!subject || typeof subjectId !== "string" || !ID_RE.test(subjectId)) return agentError(c, 400, "invalid_subject", "subject must be principal or tenant and subject_id an id.");
    if (amount === undefined) return agentError(c, 400, "invalid_amount", "amount_micro_usd must be a positive integer of micro-USD.");
    if (typeof bps !== "number" || !Number.isSafeInteger(bps) || bps < MIN_COMMIT_DISCOUNT_BPS || bps > MAX_COMMIT_DISCOUNT_BPS) {
      return agentError(c, 400, "invalid_discount", `discount_bps must be an integer between ${MIN_COMMIT_DISCOUNT_BPS} and ${MAX_COMMIT_DISCOUNT_BPS}.`);
    }
    if (!Number.isFinite(startsAt) || !Number.isFinite(endsAt) || endsAt <= startsAt || endsAt - startsAt > MAX_COMMIT_PERIOD_MS) {
      return agentError(c, 400, "invalid_period", "starts_at and ends_at must be ISO-8601 instants, ends_at after starts_at, at most 367 days apart.");
    }
    if (customer !== undefined && (typeof customer !== "string" || !/^cus_[A-Za-z0-9]{1,64}$/.test(customer))) return agentError(c, 400, "invalid_customer", "stripe_customer_id must be a Stripe customer id (cus_…).");
    const commitId = `cmt_${shortHash(idem)}`;
    const nowMs = deps.now().getTime();
    try {
      const existing = await store.commit(commitId);
      if (existing) return c.json({ commit: commitBody(existing, nowMs), created: false }, 200, { "cache-control": "no-store" });
      if (subject === "principal" && !(await store.principal(subjectId))) return agentError(c, 404, "principal_not_found", "No such principal.");
    } catch {
      return storeFailure(c);
    }
    let item: { customerId: string; itemId: string } | undefined;
    if (typeof customer === "string") {
      const client = stripeClient();
      if (!client || !deps.stripe?.overagePriceId) return agentError(c, 400, "overage_billing_unavailable", "Overage billing needs STRIPE_SECRET_KEY and STRIPE_COMMIT_OVERAGE_PRICE_ID on this gateway; omit stripe_customer_id to create a commit without it.");
      try {
        const sub = await createOverageSubscription({ ...client, customerId: customer, priceId: deps.stripe.overagePriceId, commitId });
        item = { customerId: customer, itemId: sub.itemId };
      } catch (e) {
        c.set("relayLogEvent", "stripe_failure");
        deps.log(`stripe_failure status=${e instanceof StripeError ? e.status : 0}`);
        return agentError(c, 503, "stripe_unavailable", "The card processor did not answer. Retry with the same Idempotency-Key.", "retry_after", { "retry-after": "30" });
      }
    }
    const record: CommitRecord = {
      commitId, subjectKind: subject, subjectId, amountMicros: amount, discountBps: bps, startsAt, endsAt,
      drawnMicros: 0, overageMicros: 0, overageReportedMicros: 0, createdAt: nowMs,
      ...(item ? { stripeCustomerId: item.customerId, stripeSubscriptionItemId: item.itemId } : {}),
    };
    try {
      const outcome = await store.createCommit(record);
      if (outcome === "overlap") return agentError(c, 409, "commit_overlap", "This subject already has a commit for part of that period. End or wait for it before adding another.");
      const stored = (await store.commit(commitId)) ?? record;
      c.set("relayLogEvent", "commit_created");
      return c.json({ commit: commitBody(stored, nowMs), created: outcome === "created" }, outcome === "created" ? 201 : 200, { "cache-control": "no-store" });
    } catch {
      return storeFailure(c);
    }
  });

  // --- adjustments (credits, refunds) ---
  app.post("/v1/admin/adjustments", requireKey(["admin"]), requireAdmin, small, async (c) => {
    const idem = idempotencyKey(c);
    if (typeof idem !== "string") return idem;
    const body = await readJson(c);
    if (!body) return agentError(c, 400, "invalid_json", "The body must be a JSON object.");
    const principalId = body.principal_id;
    const kind = body.kind;
    const amount = posInt(body.amount_micro_usd);
    const reason = body.reason ?? (kind === "refund" ? "refund" : "goodwill");
    const chargeId = body.charge_id;
    if (typeof principalId !== "string" || !ID_RE.test(principalId)) return agentError(c, 400, "invalid_principal", "principal_id is required.");
    if (kind !== "credit" && kind !== "refund") return agentError(c, 400, "invalid_kind", "kind must be credit or refund.");
    if (amount === undefined) return agentError(c, 400, "invalid_amount", "amount_micro_usd must be a positive integer of micro-USD.");
    if (typeof reason !== "string" || !(ADJUSTMENT_REASONS as readonly string[]).includes(reason)) return agentError(c, 400, "invalid_reason", `reason must be one of ${ADJUSTMENT_REASONS.join(", ")}.`);
    if (chargeId !== undefined && (kind !== "refund" || typeof chargeId !== "string" || !/^(ch|py)_[A-Za-z0-9]{1,64}$/.test(chargeId))) return agentError(c, 400, "invalid_charge", "charge_id (ch_…) is only for a refund.");
    const nowMs = deps.now().getTime();
    const entryId = `adj:${shortHash(idem)}`;
    try {
      if (!(await store.principal(principalId))) return agentError(c, 404, "principal_not_found", "No such principal.");
    } catch {
      return storeFailure(c);
    }
    if (typeof chargeId === "string") {
      const client = stripeClient();
      if (!client) return agentError(c, 400, "refunds_unavailable", "Card refunds need STRIPE_SECRET_KEY on this gateway; omit charge_id for a ledger-only refund.");
      let refund: { refundId: string; chargeId: string; cumulativeMicros: number };
      try {
        refund = await createRefund({ ...client, chargeId, amountMicros: amount, principalId, idempotencyKey: entryId });
      } catch (e) {
        c.set("relayLogEvent", "stripe_failure");
        deps.log(`stripe_failure status=${e instanceof StripeError ? e.status : 0}`);
        return agentError(c, 503, "stripe_unavailable", "The card processor did not answer. Retry with the same Idempotency-Key.", "retry_after", { "retry-after": "30" });
      }
      try {
        const out = await store.recordChargeRefund(principalId, refund.chargeId, refund.cumulativeMicros, nowMs);
        c.set("relayLogEvent", "refund_debited");
        return c.json({ entry_id: entryId, kind, refund_id: refund.refundId, charge_id: refund.chargeId, debited_micro_usd: out.debitedMicros, wallet: await walletBody(store, principalId) }, 201, { "cache-control": "no-store" });
      } catch {
        return storeFailure(c);
      }
    }
    try {
      const out = await store.adjust({ entryId, principalId, micros: kind === "credit" ? amount : -amount, kind, reason, ...(typeof body.ref === "string" && ID_RE.test(body.ref) ? { ref: body.ref } : {}), ts: nowMs });
      c.set("relayLogEvent", "adjustment_applied");
      return c.json({
        entry_id: out.adjustment.entryId, kind: out.adjustment.kind, reason: out.adjustment.reason, amount_micro_usd: out.adjustment.micros,
        applied_micro_usd: out.adjustment.appliedMicros, created: out.outcome === "applied", wallet: await walletBody(store, principalId),
      }, out.outcome === "applied" ? 201 : 200, { "cache-control": "no-store" });
    } catch {
      return storeFailure(c);
    }
  });

  // --- dispute resolution ---
  app.post("/v1/admin/disputes/:id/resolve", requireKey(["admin"]), requireAdmin, small, async (c) => {
    const id = c.req.param("id");
    const body = await readJson(c);
    if (!body) return agentError(c, 400, "invalid_json", "The body must be JSON: {\"outcome\": \"credit\" | \"reject\"}.");
    const outcome = body.outcome;
    if (outcome !== "credit" && outcome !== "reject") return agentError(c, 400, "invalid_outcome", "outcome must be credit or reject.");
    const nowMs = deps.now().getTime();
    try {
      const dispute = await store.disputeById(id);
      if (!dispute) return agentError(c, 404, "dispute_not_found", "No such dispute.");
      let credit = 0;
      if (outcome === "credit") {
        const given = body.amount_micro_usd;
        if (given !== undefined && posInt(given) === undefined) return agentError(c, 400, "invalid_amount", "amount_micro_usd must be a positive integer of micro-USD.");
        credit = posInt(given) ?? (await store.receipt(dispute.receiptId))?.costMicros ?? 0;
      }
      const resolved = await store.resolveDispute(id, outcome === "credit" ? "resolved" : "rejected", credit, nowMs);
      if (!resolved) return agentError(c, 404, "dispute_not_found", "No such dispute.");
      // The credit is its own idempotent entry, so a retry after a partial failure completes it.
      if (resolved.status === "resolved" && (resolved.creditMicros ?? 0) > 0) {
        await store.adjust({ entryId: `dispute:${resolved.disputeId}`, principalId: resolved.principalId, micros: resolved.creditMicros ?? 0, kind: "credit", reason: "dispute", ref: resolved.disputeId, ts: resolved.resolvedAt ?? nowMs });
      }
      c.set("relayLogEvent", "dispute_resolved");
      return c.json({
        dispute_id: resolved.disputeId, receipt_id: resolved.receiptId, status: resolved.status, credit_micro_usd: resolved.creditMicros ?? 0,
        resolved_at: resolved.resolvedAt === undefined ? null : iso(resolved.resolvedAt),
      }, 200, { "cache-control": "no-store" });
    } catch {
      return storeFailure(c);
    }
  });

  // --- E14-T3: link a ShogunAI licence; Pro earns the monthly bundle ---
  app.post("/v1/agents/me/licence", requireKey(["write"]), async (c) => {
    const p = principalOf(c);
    if (p.via === "oauth") return agentError(c, 403, "oauth_not_allowed", "Link a licence with the principal's own agent key.");
    if (!deps.licensePublicKey) return agentError(c, 503, "licence_link_unavailable", "Licence linking is not configured on this gateway.", "retry_after", { "retry-after": "3600" });
    const token = c.req.header("x-shogun-licence")?.trim();
    if (!token) return agentError(c, 400, "licence_missing", "Send the ShogunAI licence token as the X-Shogun-Licence header.");
    const nowMs = deps.now().getTime();
    let subject: string;
    let plan: string;
    try {
      const licence = verifyLicense(`Bearer ${token}`, deps.licensePublicKey, Math.floor(nowMs / 1000));
      subject = meterSubject(licence);
      plan = licence.plan;
    } catch (e) {
      const status = e instanceof AuthError && e.status === 402 ? 403 : 401;
      return agentError(c, status, status === 403 ? "licence_plan_not_eligible" : "invalid_licence", status === 403 ? "This licence's plan or status cannot be linked." : "The licence token did not verify. Fetch a fresh one from the ShogunAI app and retry.");
    }
    try {
      await store.linkLicence({ licenceSubject: subject, principalId: p.principalId, plan, verifiedAt: nowMs });
      let granted = false;
      const month = monthOf(nowMs);
      if (plan === "pro") granted = (await store.credit(p.principalId, proGrantId(subject, month), PRO_BUNDLE_MICROS, "shogunai_pro_bundle", nowMs)) === "credited";
      c.set("relayLogEvent", "licence_linked");
      return c.json({
        linked: true, plan, principal_id: p.principalId,
        pro_bundle: plan === "pro" ? { month, amount_micro_usd: PRO_BUNDLE_MICROS, credited_now: granted } : null,
      }, 200, { "cache-control": "no-store" });
    } catch {
      return storeFailure(c);
    }
  });
}

async function walletBody(store: AgentStore, principalId: string): Promise<Record<string, number>> {
  const w = await store.wallet(principalId);
  return { balance_micro_usd: w.balanceMicros, reserved_micro_usd: w.reservedMicros, available_micro_usd: Math.max(0, w.balanceMicros - w.reservedMicros) };
}
