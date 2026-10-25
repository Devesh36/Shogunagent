import { operational } from "./logging.js";
import { Hono, type Context } from "hono";
import { bodyLimit } from "hono/body-limit";
import { createHmac, type KeyObject } from "node:crypto";
import { AuthError, deniedByOperator, isExpiredTrialToken, meterSubject, verifyLicense, type License, type TrustedKeys } from "./auth.js";
import { AsrRefused, type AsrStore } from "./asr-store.js";
import { DeepgramGrantError, type DeepgramGateway } from "./deepgram.js";

type AsrEnv = { Variables: { license: License } };
function failure(error: unknown, c: Context<AsrEnv>) {
  if (error instanceof AuthError) return c.json({ error: error.status === 402 ? "plan_required" : "unauthorized" }, error.status);
  if (error instanceof AsrRefused) return c.json({ error: error.reason }, error.reason === "blocked" ? 403 : 429);
  if (error instanceof DeepgramGrantError) operational.warn(`speech_provider_status=${error.status}`);
  return c.json({ error: "speech_unavailable" }, 503);
}

export interface AsrAppOptions {
  /** The global daily spend ceiling this licence's speech reservations may reach (audit
   * 2026-10-20 H3): the breaker's cap for a paid licence, only the trial share for a trial
   * (`planSpendCapMicros`). Absent = the store's own cap (the whole breaker). */
  spendCapFor?: (license: License) => number;
  /** The operator's emergency denylist (`RELAY_DENY_SUBJECTS`, audit 2026-10-20 L3). */
  denySubjects?: readonly string[];
}

/**
 * Whether a licence may receive a company-funded speech grant at all.
 *
 * - Without an allowlist (`ASR_ALLOWED_SUBJECTS` absent, the default): every valid paid licence
 *   and every live trial (owner decision 2026-10-22, reversing audit 2026-10-20 H3's trial
 *   refusal). A trial now starts only with a card at Checkout (2026-10-20), and its speech
 *   reserves against the trial share of the global daily cap (`spendCapFor`), next to the
 *   per-subject monthly allowance and the Deepgram project's prepaid budget.
 * - With one (present): only the subjects it names, matched on the licence id or on the metering
 *   subject (`account:<acc>` for an account's trial). A present but empty list admits nobody: the
 *   operator's way to narrow company speech again without a deploy.
 */
export function speechAdmitted(license: License, allowedSubjects: readonly string[] | undefined): boolean {
  if (allowedSubjects !== undefined) return allowedSubjects.includes(license.licenseId) || allowedSubjects.includes(meterSubject(license));
  return true;
}

export function asrApp(keys: KeyObject | TrustedKeys, store: AsrStore, gateway: DeepgramGateway, tagKey: string, allowedSubjects?: readonly string[], beforeMint?: (subject: string, tag: string) => Promise<void>, options: AsrAppOptions = {}) {
  const app = new Hono<AsrEnv>();
  // Speech minutes and the subject tag are metered on the payer (audit 2026-10-20 H1): one
  // account's trial devices share one monthly allowance. The device stays the pacing/session key.
  const subjectOf = (license: License): string => meterSubject(license);
  const tagOf = (subject: string): string => createHmac("sha256", tagKey).update(`asr:${subject}`).digest("hex");
  app.use("*", async (c, next) => {
    c.header("cache-control", "no-store");
    try {
      const authorization = c.req.header("authorization");
      let license: License;
      try {
        license = verifyLicense(authorization, keys);
      } catch (error) {
        // An ended trial is 402 `plan_required`, as on every relay route (app.ts `authorize`).
        if (error instanceof AuthError && error.status === 401 && isExpiredTrialToken(authorization, keys)) throw new AuthError("plan_required", 402);
        throw error;
      }
      if (deniedByOperator(license, options.denySubjects)) throw new AuthError("invalid or expired license token", 401);
      c.set("license", license);
    } catch (error) { return failure(error, c); }
    if (!speechAdmitted(c.get("license"), allowedSubjects)) return c.json({ error: "speech_unavailable" }, 403);
    await next();
  });
  app.use("*", bodyLimit({ maxSize: 512 }));
  app.post("/grant", async c => {
    try {
      const license = c.get("license");
      const subject = subjectOf(license);
      const raw: unknown = await c.req.json().catch(() => null);
      const streams = raw && typeof raw === "object" && "streams" in raw ? raw.streams : 1;
      if (streams !== 1 && streams !== 2) return c.json({ error: "invalid_request" }, 400);
      const tag = tagOf(subject);
      // Pacing first: a refused request must not cost the provider usage reads in `beforeMint`.
      await store.checkPacing(subject, license.deviceId);
      await beforeMint?.(subject, tag);
      const ticket = await store.reserve(subject, license.deviceId, streams, options.spendCapFor?.(license));
      // No retry/refund on unknown mint outcome: the provider may already have accepted it.
      let grant;
      try { grant = await gateway.grant(); }
      catch (error) {
        if (error instanceof DeepgramGrantError && error.status >= 400 && error.status < 500 && error.status !== 408) {
          await store.finish(subject, license.deviceId, ticket.id).catch(() => undefined);
        }
        throw error;
      }
      return c.json({ ...grant, user_tag: tag, session_id: ticket.id, max_session_seconds: ticket.seconds });
    } catch (error) { return failure(error, c); }
  });
  app.post("/refresh", async c => {
    try {
      const raw: unknown = await c.req.json().catch(() => null);
      if (!raw || typeof raw !== "object" || !("session_id" in raw) || typeof raw.session_id !== "string" || !/^[a-f0-9-]{36}$/.test(raw.session_id)) return c.json({ error: "invalid_request" }, 400);
      const license = c.get("license");
      const subject = subjectOf(license);
      await store.checkPacing(subject, license.deviceId);
      await beforeMint?.(subject, tagOf(subject));
      await store.refresh(subject, license.deviceId, raw.session_id, options.spendCapFor?.(license));
      return c.json(await gateway.grant());
    } catch (error) { return failure(error, c); }
  });
  app.post("/finish", async c => {
    try {
      const raw: unknown = await c.req.json();
      if (!raw || typeof raw !== "object" || !("session_id" in raw) || typeof raw.session_id !== "string" || !/^[a-f0-9-]{36}$/.test(raw.session_id)) return c.json({ error: "invalid_request" }, 400);
      const license = c.get("license");
      await store.finish(subjectOf(license), license.deviceId, raw.session_id);
      return c.json({ ok: true });
    } catch (error) { return failure(error, c); }
  });
  return app;
}
