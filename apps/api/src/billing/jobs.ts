/**
 * The periodic billing jobs (E19, E14-T3), run from the relay's sweep (boot.ts `createRelay`,
 * the Worker's Cron Trigger). Both are idempotent: running one twice, or concurrently on two
 * isolates, changes nothing the second time. Counts only in what they return; nothing is logged
 * here.
 *
 * - `grantProBundle`: $20 of credit per linked ShogunAI Pro licence subject per UTC month. The
 *   topup id (`grant:shogunai-pro:<subject>:<YYYY-MM>`) is the idempotency key, so a subject earns
 *   one grant a month whichever principal it is linked to. A link counts while its licence was
 *   verified within `PRO_LINK_FRESH_MS` (the Mac app re-links with each fresh token).
 * - `reportCommitOverage`: commit overage → Stripe usage records on the commit's metered
 *   subscription item, in whole cents, with the report id as the Stripe Idempotency-Key.
 */
import type { AgentStore } from "../agent-store.js";
import { PRO_BUNDLE_MICROS, PRO_LINK_FRESH_MS, monthOf, proGrantId } from "./ledger.js";
import { reportOverageUsage, type StripeFetch } from "./stripe.js";

export async function grantProBundle(store: AgentStore, nowMs: number): Promise<{ checked: number; granted: number }> {
  const month = monthOf(nowMs);
  const links = await store.licenceLinks("pro", nowMs - PRO_LINK_FRESH_MS);
  let granted = 0;
  for (const link of links) {
    const out = await store.credit(link.principalId, proGrantId(link.licenceSubject, month), PRO_BUNDLE_MICROS, "shogunai_pro_bundle", nowMs);
    if (out === "credited") granted += 1;
  }
  return { checked: links.length, granted };
}

export const OVERAGE_REPORTS_PER_TICK = 100;

export async function reportCommitOverage(
  store: AgentStore,
  stripe: { secretKey: string; fetch?: StripeFetch } | undefined,
  nowMs: number,
): Promise<{ claimed: number; sent: number; failed: number }> {
  if (!stripe) return { claimed: 0, sent: 0, failed: 0 };
  const reports = await store.claimOverageReports(nowMs, OVERAGE_REPORTS_PER_TICK);
  const fetch = stripe.fetch ?? (globalThis.fetch as unknown as StripeFetch);
  let sent = 0;
  let failed = 0;
  for (const r of reports) {
    try {
      await reportOverageUsage({ secretKey: stripe.secretKey, fetch, itemId: r.subscriptionItemId, cents: r.cents, timestampSecs: Math.floor(r.ts / 1000), reportId: r.reportId });
      await store.markOverageReportSent(r.reportId, nowMs);
      sent += 1;
    } catch {
      // Left unsent; the next tick resends with the same idempotency key.
      failed += 1;
    }
  }
  return { claimed: reports.length, sent, failed };
}
