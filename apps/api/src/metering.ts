/**
 * Header metering (docs/implementation-plan.md E9-T5 / E9-T6): classify a call from the MCP
 * Streamable HTTP headers (`Mcp-Method`, `Mcp-Name`, `Mcp-Quantity`), quote it (`pricing.ts`),
 * reserve the quote against the wallet and the caps (`agent-store.ts`), forward the request to
 * the core, then settle — exactly once — to the actual cost. The body is never read here.
 *
 * Outcomes before the upstream is called:
 * - 401/403    the key (`agent-keys.ts`), with the scope the unit needs (`SCOPE_BY_UNIT`);
 * - 200        `Mcp-Dry-Run: true`: the quote, nothing reserved, nothing forwarded (E9-T6);
 * - 402        `payment_required` or `cap_exceeded` (`billing/402.ts`);
 * - 503        the store, the company's global breaker, or the upstream is unavailable.
 *
 * After the upstream answered: a 2xx settles the reservation at the quoted cost (the quote IS the
 * price for the flat units; a `run` settles at the router's figure); anything else settles at 0,
 * so a failed call costs nothing. The receipt id and the usage ride on response headers
 * (`X-Receipt-Id`, `X-Usage-Unit`, `X-Usage-Quantity`, `X-Usage-Cost-Micro-Usd`): the core owns
 * the body, this layer owns the metering, and the body is relayed untouched (invariant 2).
 *
 * Cost-bearing calls also book their quote against the licence ledger's global daily breaker
 * when one is wired (`deps.usage` + `deps.spendBreaker`): agent keys and licence tokens then
 * share the company's spend ceiling (E3-T4), keyed on `agentMeterSubject`.
 */
import type { Context, MiddlewareHandler } from "hono";
import { randomUUID } from "node:crypto";

import { agentError, agentMeterSubject, hasScopes, resolveAgentKey, type AgentKeyLookup, type AgentPrincipal, type AgentScope, type AgentVars } from "./agent-keys.js";
import type { AgentStore, ReserveOutcome } from "./agent-store.js";
import { paymentRequiredResponse } from "./billing/402.js";
import { commitTerms, discounted, type CommitTerms } from "./billing/ledger.js";
import { LOW_BALANCE_THRESHOLD_MICROS, capReachedEvent, crossedLowBalance, lowBalanceEvent, type Notifier } from "./billing/notify.js";
import { DEFAULT_CAPS, capsFor, periodsOf, secsUntilPeriodReset, type CapSet } from "./caps.js";
import type { LogFn } from "./logging.js";
import { classifyCall, quoteCost, usageOf, type MeterUnit, type Quote } from "./pricing.js";
import type { SpendBreaker } from "./spend.js";
import { utcDate, type UsageStore } from "./usage.js";

/** The scope a unit needs. Protocol traffic needs a valid key but no particular scope. */
export const SCOPE_BY_UNIT: Readonly<Record<MeterUnit, readonly AgentScope[]>> = Object.freeze({
  read: ["read"],
  ingest: ["write"],
  model_op: ["write"],
  run: ["run"],
  storage: ["write"],
  protocol: [],
});

export const HEADER_METHOD = "mcp-method";
export const HEADER_NAME = "mcp-name";
export const HEADER_QUANTITY = "mcp-quantity";
export const HEADER_DRY_RUN = "mcp-dry-run";
/** The router's hook for a `run`'s provider price: the tool the run targets. */
export const HEADER_TOOL_ID = "mcp-tool-id";
export const MAX_QUANTITY = 1_000_000;

export interface MeteringDeps {
  store: AgentStore;
  log: LogFn;
  now?: () => Date;
  /** The caps in force (defaults: `DEFAULT_CAPS`). */
  caps?: CapSet;
  /** Absolute base URL of this gateway, for the 402's top-up link. */
  publicBaseUrl: string;
  /** Where metered calls go: the core (E4). Absent = 503 `upstream_unavailable`, nothing reserved. */
  mcpUpstream?: (request: Request, principal: AgentPrincipal) => Promise<Response>;
  /** The router's price (µ$) for a `run` of `toolId`. Absent, or answering `undefined`, = the run
   * cannot be priced here and is refused (400 `run_price_unavailable`). */
  runPricer?: (toolId: string | undefined, principal: AgentPrincipal) => Promise<number | undefined>;
  /** The licence ledger and the company's global daily breaker (E3-T4): cost-bearing calls book
   * against the same day as the Batch lane. Absent = not booked (development). */
  usage?: UsageStore;
  spendBreaker?: SpendBreaker;
  /** Where `wallet.low_balance` / `cap.reached` go (E19, billing/notify.ts). Absent = not emitted. */
  notifier?: Notifier;
  /** Spendable balance (µ$) under which a settlement emits `wallet.low_balance`. */
  lowBalanceThresholdMicros?: number;
  /** How a bearer resolves to a principal. Default: `store` (agent keys only); the agent app
   * passes a lookup that also resolves OAuth access tokens (E3-T2, oauth/app.ts). */
  keyLookup?: AgentKeyLookup;
  /** The Protected Resource Metadata URL a 401 points MCP clients at (RFC 9728 §5.1), when OAuth
   * is served. */
  resourceMetadataUrl?: string;
  /** Remote approvals (E15): called with the decision token when the core's answer names one in
   * `X-Shogun-Approval-Token` (an approval was queued). The header is read, never the body, and
   * is not passed on to the agent (the body already carries the URL). */
  onApprovalToken?: (token: string) => Promise<unknown>;
}


/** The `WWW-Authenticate` of a 401 on a metered route: names the resource metadata when OAuth is
 * on, so an MCP client can discover the authorization server from the 401 alone. */
export function wwwAuthenticate(deps: Pick<MeteringDeps, "resourceMetadataUrl">): string {
  return deps.resourceMetadataUrl ? `Bearer realm="shogunagent", resource_metadata="${deps.resourceMetadataUrl}"` : 'Bearer realm="shogunagent"';
}

/** The commit pricing of one call: the discounted charge, and the commit terms when one applies. */
export interface CallPrice {
  /** µ$ the call is charged (equals the list quote without a commit). */
  charged: number;
  terms?: CommitTerms;
}

/** Price a list quote for a principal: an active commit discounts it (E19). Throws when the store
 * cannot be read — a commit customer must never be charged list price by accident. */
export async function priceCall(deps: Pick<MeteringDeps, "store">, principal: AgentPrincipal, quote: Quote, nowMs: number): Promise<CallPrice> {
  if (quote.cost_micro_usd <= 0) return { charged: 0 };
  const commit = await deps.store.activeCommit(principal.principalId, principal.tenantId, nowMs);
  if (!commit) return { charged: quote.cost_micro_usd };
  const terms = commitTerms(commit);
  return { charged: discounted(quote.cost_micro_usd, terms.discountBps), terms };
}


/** Run `work` past the response: Workers' `waitUntil` when there is one, else detached. */
function deferWork(c: Context, work: Promise<unknown>): void {
  const settled = work.catch(() => undefined);
  try {
    c.executionCtx.waitUntil(settled);
  } catch {
    void settled;
  }
}

/** What one classified request is, before any money moves. */
export interface ClassifiedCall {
  method: string;
  name?: string;
  unit: MeterUnit;
  quantity: number;
  dryRun: boolean;
  toolId?: string;
}

/** Read the metering headers. Never the body. A non-POST `/mcp` request (the SSE stream, the
 * session end) is protocol traffic. */
export function classifyRequest(c: Context): ClassifiedCall | { error: string } {
  const method = c.req.method === "POST" ? (c.req.header(HEADER_METHOD)?.trim() ?? "") : "";
  const name = c.req.header(HEADER_NAME)?.trim();
  const rawQty = c.req.header(HEADER_QUANTITY)?.trim();
  let quantity = 1;
  if (rawQty !== undefined && rawQty !== "") {
    if (!/^\d+$/.test(rawQty)) return { error: `${HEADER_QUANTITY} must be a whole number` };
    quantity = Number(rawQty);
    if (!Number.isSafeInteger(quantity) || quantity < 1 || quantity > MAX_QUANTITY) return { error: `${HEADER_QUANTITY} must be between 1 and ${MAX_QUANTITY}` };
  }
  if (name !== undefined && name.length > 128) return { error: `${HEADER_NAME} is too long` };
  const unit = classifyCall(method || undefined, name);
  const dryRun = (c.req.header(HEADER_DRY_RUN)?.trim().toLowerCase() ?? "") === "true";
  const toolId = c.req.header(HEADER_TOOL_ID)?.trim();
  return {
    method: method || c.req.method,
    ...(name ? { name } : {}),
    unit,
    quantity,
    dryRun,
    ...(toolId ? { toolId } : {}),
  };
}

/** Quote a classified call. A `run` needs the router's price. */
export async function quoteCall(deps: MeteringDeps, call: ClassifiedCall, principal: AgentPrincipal): Promise<Quote | { error: "run_price_unavailable" }> {
  if (call.unit !== "run") return quoteCost(call.unit, call.quantity);
  const price = deps.runPricer ? await deps.runPricer(call.toolId, principal) : undefined;
  if (price === undefined) return { error: "run_price_unavailable" };
  return quoteCost("run", call.quantity, price);
}

export const topupUrl = (deps: Pick<MeteringDeps, "publicBaseUrl">): string => `${deps.publicBaseUrl.replace(/\/+$/, "")}/v1/wallet/topup`;

/** The usage headers a metered response carries. */
export function usageHeaders(receiptId: string, quote: Quote): Record<string, string> {
  return {
    "x-receipt-id": receiptId,
    "x-usage-unit": quote.unit,
    "x-usage-quantity": String(quote.quantity),
    "x-usage-cost-micro-usd": String(quote.cost_micro_usd),
  };
}

/** The 402 for a refused reservation, with the log event set. */
export function refusalResponse(c: Context<{ Variables: AgentVars }>, outcome: Exclude<ReserveOutcome, { kind: "ok" } | { kind: "unavailable" }>, quote: Quote, deps: MeteringDeps, nowMs: number, startedMs: number): Response {
  if (outcome.kind === "insufficient_funds") {
    c.set("relayLogEvent", "payment_required");
    return paymentRequiredResponse({ reason: "payment_required", amountMicros: quote.cost_micro_usd, unit: quote.unit, balanceMicros: outcome.availableMicros, topupUrl: topupUrl(deps) }, nowMs - startedMs);
  }
  c.set("relayLogEvent", "cap_exceeded");
  return paymentRequiredResponse({
    reason: "cap_exceeded", amountMicros: quote.cost_micro_usd, unit: quote.unit, scope: outcome.scope, period: outcome.period,
    limitMicros: outcome.limitMicros, spentMicros: outcome.spentMicros, retryAfterSecs: secsUntilPeriodReset(outcome.period, nowMs),
  }, nowMs - startedMs);
}

/**
 * Reserve `quote` for `principal` (the programmatic entry the router's `run` will use after it
 * has parsed its own body). Returns the receipt id, or the refusal response. Books the global
 * breaker too; a breaker refusal releases the wallet hold and answers 503 with `Retry-After`.
 */
export async function reserveCall(
  c: Context<{ Variables: AgentVars }>,
  deps: MeteringDeps,
  principal: AgentPrincipal,
  call: Pick<ClassifiedCall, "method" | "name">,
  quote: Quote,
  startedMs: number,
): Promise<{ ok: true; receiptId: string; price: CallPrice } | { ok: false; response: Response }> {
  const now = (deps.now ?? (() => new Date()))();
  const nowMs = now.getTime();
  const { day, month } = periodsOf(nowMs);
  const receiptId = `rcpt_${randomUUID()}`;
  const unavailable = (): { ok: false; response: Response } => {
    c.set("relayLogEvent", "wallet_unavailable");
    return { ok: false, response: agentError(c, 503, "wallet_unavailable", "The wallet store is unavailable. Retry shortly.", "retry_after", { "retry-after": "1" }) };
  };
  let price: CallPrice;
  try {
    price = await priceCall(deps, principal, quote, nowMs);
  } catch {
    return unavailable();
  }
  const principalRow = await deps.store.principal(principal.principalId).catch(() => undefined);
  const caps = capsFor(deps.caps ?? DEFAULT_CAPS, principal, principalRow ?? {});
  const outcome = await deps.store.reserve({
    receiptId, principalId: principal.principalId, keyId: principal.keyId, method: call.method,
    ...(call.name !== undefined ? { name: call.name } : {}),
    unit: quote.unit, quantity: quote.quantity, micros: price.charged, ts: nowMs, day, month, caps,
    ...(price.terms ? { listMicros: quote.cost_micro_usd, commit: price.terms } : {}),
  });
  if (outcome.kind === "unavailable") return unavailable();
  if (outcome.kind !== "ok") {
    if (outcome.kind === "cap_exceeded" && deps.notifier) {
      const event = capReachedEvent(principal.principalId, principal.keyId, {
        scope: outcome.scope, period: outcome.period, periodKey: outcome.period === "day" ? day : month,
        limitMicros: outcome.limitMicros, spentMicros: outcome.spentMicros, retryAfterSecs: secsUntilPeriodReset(outcome.period, nowMs),
      }, nowMs);
      await deps.notifier.emit(event).catch(() => undefined);
    }
    return { ok: false, response: refusalResponse(c, outcome, { ...quote, cost_micro_usd: price.charged }, deps, nowMs, startedMs) };
  }
  // The company's global breaker (E3-T4): the same day row the Batch lane books against.
  if (price.charged > 0 && deps.usage && deps.spendBreaker) {
    const booked = await deps.usage.tryReserve(agentMeterSubject(principal), utcDate(now), 1, Number.MAX_SAFE_INTEGER, undefined, {
      batchId: receiptId, micros: price.charged, capMicros: deps.spendBreaker.capMicros,
    });
    if (booked !== "ok") {
      await deps.store.settle(receiptId, 0).catch(() => deps.log("agent_settle_failure"));
      c.set("relayLogEvent", booked === "spend_capped" ? "capacity_exhausted" : "metering_unavailable");
      const retry = booked === "spend_capped" ? secsUntilPeriodReset("day", nowMs) : 1;
      return { ok: false, response: agentError(c, 503, booked === "spend_capped" ? "capacity_exhausted" : "metering_unavailable", booked === "spend_capped" ? "The service's daily capacity is exhausted. Retry after the UTC day rolls over." : "Metering is unavailable. Retry shortly.", "retry_after", { "retry-after": String(retry) }) };
    }
  }
  return { ok: true, receiptId, price };
}

/** Settle a reservation at `actualMicros` (`listMicros` at list price), once, on both ledgers,
 * and emit `wallet.low_balance` when this charge took the wallet under the threshold. Never throws. */
export async function settleCall(deps: MeteringDeps, receiptId: string, actualMicros: number, listMicros: number = actualMicros, principal?: Pick<AgentPrincipal, "principalId">, funding: "wallet" | "commit" = "wallet"): Promise<void> {
  await deps.store.settle(receiptId, actualMicros, listMicros).catch(() => deps.log("agent_settle_failure"));
  if (deps.usage && deps.spendBreaker) await deps.usage.settleSpend(receiptId, actualMicros).catch(() => deps.log("spend_settle_failure"));
  if (deps.notifier && principal && funding === "wallet" && actualMicros > 0) {
    const threshold = deps.lowBalanceThresholdMicros ?? LOW_BALANCE_THRESHOLD_MICROS;
    const wallet = await deps.store.wallet(principal.principalId).catch(() => undefined);
    if (wallet) {
      const available = Math.max(0, wallet.balanceMicros - wallet.reservedMicros);
      if (crossedLowBalance(available, actualMicros, threshold)) {
        const nowMs = (deps.now ?? (() => new Date()))().getTime();
        await deps.notifier.emit(lowBalanceEvent(principal.principalId, available, threshold, topupUrl(deps), nowMs)).catch(() => undefined);
      }
    }
  }
}

/** The settled figures of a call whose list charge is `listMicros`: discounted under a commit. */
export function settledPrice(price: CallPrice, listMicros: number): { charged: number; list: number } {
  return { charged: price.terms ? discounted(listMicros, price.terms.discountBps) : listMicros, list: listMicros };
}

/**
 * The metered proxy: auth → classify → (dry run) → reserve → forward → settle. Mounted on
 * `/mcp`; `fixed` pins the classification for a REST route that is one known tool.
 */
export function metered(deps: MeteringDeps, fixed?: { method: string; name: string }): MiddlewareHandler<{ Variables: AgentVars }> {
  const now = deps.now ?? (() => new Date());
  return async (c) => {
    const startedMs = now().getTime();
    const classified = fixed
      ? { ...classifyRequest(c), method: fixed.method, name: fixed.name, unit: classifyCall(fixed.method, fixed.name) }
      : classifyRequest(c);
    if ("error" in classified) return agentError(c, 400, "invalid_metering_header", classified.error);
    const call: ClassifiedCall = classified;
    const resolved = await resolveAgentKey(c.req.header("authorization"), deps.keyLookup ?? deps.store, startedMs);
    if (!resolved.ok) {
      c.set("relayLogEvent", "agent_key_rejected");
      return agentError(c, 401, resolved.code, "Send a valid agent key as `Authorization: Bearer sa_live_…` (register with POST /v1/agents), or an OAuth access token.", "register", { "www-authenticate": wwwAuthenticate(deps) });
    }
    const principal = resolved.principal;
    c.set("relayLogAgentKey", principal.keyId);
    c.set("agentPrincipal", principal);
    const scopes = SCOPE_BY_UNIT[call.unit];
    if (!hasScopes(principal.scopes, scopes)) return agentError(c, 403, "insufficient_scope", `This call needs the ${scopes.join(", ")} scope.`);
    const quote = await quoteCall(deps, call, principal);
    if ("error" in quote) {
      c.set("relayLogEvent", "run_price_unavailable");
      return agentError(c, 400, "run_price_unavailable", "A run cannot be priced from headers alone. Call POST /v1/router/run, which prices the tool itself, or quote it first with POST /v1/quote.");
    }
    if (call.dryRun) {
      c.set("relayLogEvent", "dry_run");
      const wallet = await deps.store.wallet(principal.principalId).catch(() => undefined);
      const available = wallet ? Math.max(0, wallet.balanceMicros - wallet.reservedMicros) : undefined;
      const price = await priceCall(deps, principal, quote, startedMs).catch(() => undefined);
      if (!price) return agentError(c, 503, "wallet_unavailable", "The wallet store is unavailable. Retry shortly.", "retry_after", { "retry-after": "1" });
      const commitFunded = price.terms?.funding === "commit";
      return c.json({
        result: null, dry_run: true, quote: { ...usageOf(quote), cost_micro_usd: price.charged, ...(price.terms ? { list_cost_micro_usd: quote.cost_micro_usd, commit_id: price.terms.commitId, discount_bps: price.terms.discountBps } : {}) },
        usage: { unit: quote.unit, quantity: 0, cost_micro_usd: 0 },
        ...(available !== undefined ? { balance_micro_usd: available, sufficient: commitFunded || available >= price.charged } : {}),
        latency_ms: Math.max(0, now().getTime() - startedMs), next_action: null,
      }, 200, { "cache-control": "no-store" });
    }
    if (!deps.mcpUpstream) {
      c.set("relayLogEvent", "upstream_unavailable");
      return agentError(c, 503, "upstream_unavailable", "The core is not reachable from this gateway yet. Retry shortly.", "retry_after", { "retry-after": "30" });
    }
    const reserved = await reserveCall(c, deps, principal, call, quote, startedMs);
    if (!reserved.ok) return reserved.response;
    let upstream: Response;
    try {
      upstream = await deps.mcpUpstream(c.req.raw, principal);
    } catch {
      await settleCall(deps, reserved.receiptId, 0);
      c.set("relayLogEvent", "upstream_failure");
      return agentError(c, 503, "upstream_unavailable", "The core did not answer. Nothing was charged. Retry shortly.", "retry_after", { "retry-after": "5" });
    }
    const { charged, list } = settledPrice(reserved.price, upstream.status >= 200 && upstream.status < 300 ? quote.cost_micro_usd : 0);
    await settleCall(deps, reserved.receiptId, charged, list, principal, reserved.price.terms?.funding ?? "wallet");
    const headers = new Headers(upstream.headers);
    const approvalToken = headers.get("x-shogun-approval-token");
    headers.delete("x-shogun-approval-token");
    if (approvalToken && deps.onApprovalToken) deferWork(c, deps.onApprovalToken(approvalToken));
    for (const [k, v] of Object.entries(usageHeaders(reserved.receiptId, { ...quote, cost_micro_usd: charged }))) headers.set(k, v);
    if (reserved.price.terms) headers.set("x-usage-list-cost-micro-usd", String(list));
    headers.set("x-latency-ms", String(Math.max(0, now().getTime() - startedMs)));
    return new Response(upstream.body, { status: upstream.status, headers });
  };
}
