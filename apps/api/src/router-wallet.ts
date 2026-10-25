/**
 * The router's wallet (docs/implementation-plan.md E32; contract: docs/api/payouts.md §1,
 * docs/api/billing.md "Router runs"). A router `run` is charged here, exactly once, through the
 * same reserve → settle ledger, 402 and caps as every other metered call. The router holds no
 * balance and sends no caller price: it sends the provider's price, and this module adds the take
 * (`RUN_MARKUP_BPS`, pricing.ts) once, for third-party runs only.
 *
 *   POST /v1/internal/router/reserve      hold a run's quote on the caller's wallet [ROUTER_SETTLEMENT_TOKEN]
 *        Authorization: Bearer <router token>; X-Caller-Authorization: Bearer <the caller's key>
 *        { run_id, tool_id, provider_micro_usd, first_party }
 *        → 200 { reservation_id: "rcpt_run_<run_id>", list_micro_usd, charged_micro_usd, funding }
 *        → 402 payment_required | cap_exceeded (billing/402.ts), 401/403 the caller's key, 503
 *        Idempotent by run_id: a retried reserve returns the same hold.
 *   POST /v1/internal/router/settlements  the router's outbox delivers its settlements here
 *        Each settlement, idempotently: settles the run's reservation (`receipt_id`) once at
 *        provider price + one take (third-party) or the provider price (first-party), $0 for
 *        failures and empty results, discounted under an active commit; then accrues the provider
 *        (E29, payouts.ts) when payouts are on.
 *
 * Reads stay free (invariant 10): a $0 tool reserves nothing; its settlement only counts the run.
 * Bodies here are content-free JSON the gateway defines; the caller's bearer is verified and
 * dropped, never stored or logged. Logs carry fixed words only.
 */
import type { Context, Hono } from "hono";
import { bodyLimit } from "hono/body-limit";

import { admitPrincipal, admitRefusalResponse, agentError, hasScopes, resolveAgentKey, type AgentPrincipal, type AgentVars } from "./agent-keys.js";
import { discounted } from "./billing/ledger.js";
import { reserveCall, settleCall, type MeteringDeps } from "./metering.js";
import { tokenMatches } from "./payouts-app.js";
import {
  MAX_SETTLEMENTS_PER_POST, parseSettlement, routerReceiptId, settlementRecord,
  type PayoutStore, type RunSettlementInput,
} from "./payouts.js";
import { quoteCost, type Quote } from "./pricing.js";

export const ROUTER_RESERVE_PATH = "/v1/internal/router/reserve";
export const ROUTER_SETTLEMENTS_PATH = "/v1/internal/router/settlements";
/** The method and name a router run's receipt carries (pricing.ts classifies `router.run` as `run`). */
export const ROUTER_RUN_CALL = { method: "tools/call", name: "router.run" } as const;

const MAX_RESERVE_BODY_BYTES = 2 * 1024;
const MAX_SETTLEMENT_BODY_BYTES = 64 * 1024;
const RUN_ID_RE = /^[A-Za-z0-9_.:-]{1,128}$/;
const TOOL_ID_RE = /^[a-z0-9][a-z0-9_.-]{0,255}$/;
/** A provider price above $1,000 for one run is a router bug, not a price. */
const MAX_PROVIDER_MICROS = 1_000 * 1_000_000;

export interface RouterWalletDeps {
  /** `ROUTER_SETTLEMENT_TOKEN`: the bearer the router presents. */
  token: string;
  /** The metering the rest of the gateway uses: store, caps, breaker, notifier, key lookup. */
  metering: MeteringDeps;
  /** The provider ledger (E29). Absent = settlements settle the wallet only. */
  payouts?: PayoutStore;
}

/** What the caller pays for a run of `providerMicros`: provider + one take, or the provider price
 * for a first-party tool. The router quotes the same figure (apps/router/src/take.ts). */
export function routerRunQuote(providerMicros: number, firstParty: boolean): Quote {
  if (firstParty) return { unit: "run", quantity: 1, cost_micro_usd: Math.max(0, providerMicros) };
  return quoteCost("run", 1, providerMicros);
}

async function readJson(c: Context): Promise<Record<string, unknown> | undefined> {
  try {
    const raw: unknown = await c.req.json();
    return typeof raw === "object" && raw !== null && !Array.isArray(raw) ? (raw as Record<string, unknown>) : undefined;
  } catch {
    return undefined;
  }
}

/** Settle one router run's reservation, once. Throws when the store cannot be read or written, so
 * the route answers 503 and the router's outbox redelivers. */
export async function settleRouterRun(deps: MeteringDeps, s: RunSettlementInput): Promise<"settled" | "already_settled" | "no_reservation"> {
  const receiptId = s.receipt_id ?? null;
  if (receiptId === null || receiptId !== routerReceiptId(s.run_id)) return "no_reservation";
  const receipt = await deps.store.receipt(receiptId);
  if (!receipt) return "no_reservation";
  if (receipt.settled) return "already_settled";
  const list = s.outcome === "ok" ? routerRunQuote(s.provider_micro_usd, s.first_party).cost_micro_usd : 0;
  let charged = list;
  if (receipt.commitId !== undefined && list > 0) {
    const commit = await deps.store.commit(receipt.commitId);
    if (commit) charged = discounted(list, commit.discountBps);
  }
  // The store's settle is the once-only step; it throws on a store failure (503 → redelivery).
  // The receipt records the run's settled quantity next to the reserved one (E48): the router's
  // reported quantity for an `ok` run, 0 for a failure or an empty result.
  await deps.store.settle(receiptId, charged, list, s.outcome === "ok" ? s.quantity : 0);
  // The breaker and the low-balance notice ride on the shared helper (its own settle is a no-op now).
  await settleCall(deps, receiptId, charged, list, { principalId: receipt.principalId }, receipt.funding);
  return "settled";
}

export function registerRouterWalletRoutes(app: Hono<{ Variables: AgentVars }>, deps: RouterWalletDeps): void {
  const { metering } = deps;
  const now = metering.now ?? (() => new Date());
  const refuseToken = (c: Context<{ Variables: AgentVars }>): Response | undefined => {
    if (tokenMatches(c.req.header("authorization"), deps.token)) return undefined;
    c.set("relayLogEvent", "settlement_rejected");
    return agentError(c, 401, "invalid_token", "This route takes the router's service token.");
  };
  const unavailable = (c: Context<{ Variables: AgentVars }>): Response => {
    c.set("relayLogEvent", "wallet_unavailable");
    return agentError(c, 503, "store_unavailable", "The wallet or the payout ledger is unavailable. Retry shortly; the run id keeps the retry idempotent.", "retry_after", { "retry-after": "1" });
  };

  // --- reserve: the hold, priced here ---
  app.post(ROUTER_RESERVE_PATH, bodyLimit({ maxSize: MAX_RESERVE_BODY_BYTES, onError: (c) => agentError(c, 413, "body_too_large", "The reserve body is a few ids and a price.") }), async (c) => {
    const refused = refuseToken(c);
    if (refused) return refused;
    const startedMs = now().getTime();
    const body = await readJson(c);
    const runId = body?.run_id;
    const toolId = body?.tool_id;
    const provider = body?.provider_micro_usd;
    const firstParty = body?.first_party;
    if (typeof runId !== "string" || !RUN_ID_RE.test(runId) || typeof toolId !== "string" || !TOOL_ID_RE.test(toolId)
      || typeof provider !== "number" || !Number.isSafeInteger(provider) || provider < 0 || provider > MAX_PROVIDER_MICROS || typeof firstParty !== "boolean") {
      return agentError(c, 400, "invalid_reserve", "The body must be {\"run_id\", \"tool_id\", \"provider_micro_usd\" (a non-negative integer), \"first_party\" (a boolean)}.", "fix_input");
    }
    // The caller's own key, forwarded by the router: verified here, never stored.
    const resolved = await resolveAgentKey(c.req.header("x-caller-authorization"), metering.keyLookup ?? metering.store, startedMs);
    if (!resolved.ok) {
      c.set("relayLogEvent", "agent_key_rejected");
      return agentError(c, 401, resolved.code, "The caller's agent key is not valid. Send it as the run's bearer.", "authenticate");
    }
    const principal: AgentPrincipal = resolved.principal;
    c.set("relayLogAgentKey", principal.keyId);
    const admit = await admitPrincipal(metering.keyLookup ?? metering.store, principal, startedMs);
    if (admit) return admitRefusalResponse(c, admit);
    if (!hasScopes(principal.scopes, ["run"])) return agentError(c, 403, "insufficient_scope", "This key lacks the run scope.");
    const receiptId = routerReceiptId(runId);
    const quote = routerRunQuote(provider, firstParty);
    try {
      const existing = await metering.store.receipt(receiptId);
      if (existing) {
        // A settled run id is spent: reserving it again would run without a hold.
        if (existing.principalId !== principal.principalId || existing.settled) return agentError(c, 409, "run_id_conflict", "This run id is already used; the router mints a new one per run.");
        c.set("relayLogEvent", "router_reserve_replayed");
        return c.json({ reservation_id: receiptId, list_micro_usd: quote.cost_micro_usd, charged_micro_usd: existing.estimateMicros, funding: existing.funding, replayed: true }, 200, { "cache-control": "no-store" });
      }
    } catch {
      return unavailable(c);
    }
    const reserved = await reserveCall(c, metering, principal, ROUTER_RUN_CALL, quote, startedMs, { receiptId });
    if (!reserved.ok) return reserved.response;
    c.set("relayLogEvent", "router_reserved");
    return c.json({
      reservation_id: receiptId, list_micro_usd: quote.cost_micro_usd, charged_micro_usd: reserved.price.charged,
      funding: reserved.price.terms?.funding ?? "wallet", replayed: false,
    }, 200, { "cache-control": "no-store" });
  });

  // --- settlements: the router's outbox delivers here ---
  app.post(ROUTER_SETTLEMENTS_PATH, bodyLimit({ maxSize: MAX_SETTLEMENT_BODY_BYTES, onError: (c) => agentError(c, 413, "body_too_large", `At most ${MAX_SETTLEMENTS_PER_POST} settlements per request.`) }), async (c) => {
    const refused = refuseToken(c);
    if (refused) return refused;
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
    let walletSettled = 0;
    try {
      for (const p of parsed) {
        if (!p.ok) continue;
        // The wallet first: a crash between the two steps leaves the accrual to the redelivery,
        // never a run accrued to the provider but not charged to the caller.
        if ((await settleRouterRun(metering, p.value)) === "settled") walletSettled += 1;
        if (deps.payouts) {
          const out = await deps.payouts.recordSettlement(settlementRecord(p.value));
          if (out === "recorded") recorded += 1; else duplicates += 1;
        }
      }
    } catch {
      // 503 makes the router's outbox retry; run ids and receipt ids keep the retry idempotent.
      return unavailable(c);
    }
    c.set("relayLogEvent", "settlements_recorded");
    return c.json({ recorded, duplicates, wallet_settled: walletSettled, accrual: deps.payouts ? "on" : "off" }, 200, { "cache-control": "no-store" });
  });
}
