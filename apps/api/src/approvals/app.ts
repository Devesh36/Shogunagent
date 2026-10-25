/**
 * Remote approval surfaces (plan E15): turn the sidecar's approval URL into something a human can
 * act on, and tell the principal's systems about it.
 *
 *   GET  /approve/:token            a server-rendered page: the redacted summary, Approve / Reject
 *   POST /approve/:token            the page's form (CSRF token bound to a cookie and the link)
 *   POST /v1/slack/interactions     Slack button presses (Slack-signed)
 *   POST /v1/approvals/notify       {token}: an approval was queued (the sidecar's link is proof)
 *   POST /v1/webhooks               register {url, events}                               [write]
 *   GET  /v1/webhooks               the principal's endpoints                            [read]
 *   DELETE /v1/webhooks/:id         remove one                                           [write]
 *
 * The page needs no JavaScript. Holding the link is the authority to decide, as with an emailed
 * approval link: the sidecar mints it only for the approval it names, it expires with the
 * approval, and it decides once. Every surface reaches the sidecar through `sidecar-client.ts`;
 * nothing here stores or logs a summary, a token or a Slack payload. The metered `/mcp` proxy
 * calls `notifyCreated` when the core's answer carries the `X-Shogun-Approval-Token` header, so
 * the gateway learns of an approval without reading the body (invariant 2).
 */
import { Buffer } from "node:buffer";
import { createHmac, randomBytes, timingSafeEqual } from "node:crypto";
import { randomUUID } from "node:crypto";
import { Hono, type Context, type MiddlewareHandler } from "hono";
import { bodyLimit } from "hono/body-limit";

import { agentError, agentPrincipalOf, type AgentScope, type AgentVars } from "../agent-keys.js";
import type { LogFn } from "../logging.js";
import type { ApprovalSummary, FetchFn, SidecarClient } from "./sidecar-client.js";
import { parseInteraction, postApprovalMessage, replaceMessage, slackText, verifySlackSignature, type SlackDeps } from "./slack.js";
import { verifyApprovalToken, type ApprovalClaims } from "./token.js";
import {
  MAX_WEBHOOKS_PER_PRINCIPAL, WEBHOOK_EVENTS, checkWebhookUrl, deliverEvent, webhookSecret, type DeliveryOutcome, type WebhookEvent, type WebhookStore,
} from "./webhooks.js";

export const CSRF_COOKIE = "shogun_approve_csrf";
const MAX_FORM_BYTES = 4 * 1024;
const MAX_SLACK_BYTES = 64 * 1024;
const PAGE_HEADERS: Record<string, string> = {
  "content-type": "text/html; charset=utf-8",
  "cache-control": "no-store",
  // The token is in the URL: never send it on as a referrer, never frame the page.
  "referrer-policy": "no-referrer",
  "x-frame-options": "DENY",
  "content-security-policy": "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
};

export interface ApprovalsDeps {
  /** `SHOGUN_APPROVAL_SIGNING_KEY`, shared with the sidecar. */
  signingKey: string;
  sidecar: SidecarClient;
  webhooks: WebhookStore;
  /** `WEBHOOK_SIGNING_SECRET`: endpoint secrets derive from it. */
  webhookSecret: string;
  slack?: SlackDeps;
  publicBaseUrl: string;
  log: LogFn;
  now?: () => Date;
  /** Webhook deliveries (default: global fetch). */
  fetch?: FetchFn;
  sleep?: (ms: number) => Promise<void>;
  retryDelaysMs?: readonly number[];
  /** Accept `http://` webhook URLs (development only). */
  allowHttpWebhooks?: boolean;
  /** Run background work (deliveries) past the response: Workers' `waitUntil`. Default: detached. */
  defer?: (work: Promise<unknown>) => void;
}

/** What `notifyCreated` / a decision did, for tests: Slack posted, webhook outcomes. */
export interface Fanout {
  slack: boolean;
  webhooks: DeliveryOutcome[];
}

const escapeHtml = (s: string): string =>
  s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;").replace(/'/g, "&#39;");

function cookieOf(header: string | undefined, name: string): string | undefined {
  for (const part of (header ?? "").split(";")) {
    const [k, ...v] = part.trim().split("=");
    if (k === name) return v.join("=");
  }
  return undefined;
}

/** The form's CSRF value: bound to the browser's cookie and to this link. */
export function csrfFor(signingKey: string, cookie: string, token: string): string {
  return createHmac("sha256", signingKey.trim()).update(`csrf.${cookie}.${token}`).digest("hex");
}

function page(title: string, body: string): string {
  return `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="robots" content="noindex"><title>${escapeHtml(title)}</title><style>body{font:16px/1.5 system-ui,sans-serif;max-width:40rem;margin:2rem auto;padding:0 1rem;color:#111;background:#fff}dl{display:grid;grid-template-columns:auto 1fr;gap:.25rem 1rem}dt{color:#555}blockquote{margin:1rem 0;padding:.5rem 1rem;border-left:3px solid #999;background:#f5f5f5}button{font:inherit;padding:.5rem 1.25rem;margin-right:.5rem}@media (prefers-color-scheme:dark){body{color:#eee;background:#111}dt{color:#aaa}blockquote{background:#222}}</style></head><body>${body}</body></html>`;
}

function summaryHtml(s: ApprovalSummary): string {
  const row = (k: string, v: string | undefined) => (v ? `<dt>${escapeHtml(k)}</dt><dd>${escapeHtml(v)}</dd>` : "");
  return `<dl>${row("Action", s.kind)}${row("Tier", s.level)}${row("Goes to", s.recipient_class)}${row("Requested", s.created_at)}${row("Expires", s.expires_at)}${row("Status", s.status)}</dl>${s.preview ? `<blockquote>${escapeHtml(s.preview)}</blockquote>` : ""}`;
}

const TOKEN_REFUSAL: Record<"malformed" | "bad_signature" | "expired", [number, string]> = {
  malformed: [400, "This approval link is not valid."],
  bad_signature: [403, "This approval link was not issued by this service."],
  expired: [410, "This approval link has expired. Ask the agent to submit the request again."],
};

export function createApprovalsApp(deps: ApprovalsDeps, requireKey: (scopes: readonly AgentScope[]) => MiddlewareHandler<{ Variables: AgentVars }>): {
  app: Hono<{ Variables: AgentVars }>;
  notifyCreated: (token: string) => Promise<Fanout | undefined>;
} {
  const app = new Hono<{ Variables: AgentVars }>();
  const now = deps.now ?? (() => new Date());
  const defer = deps.defer ?? ((work: Promise<unknown>) => void work.catch(() => undefined));
  const secure = deps.publicBaseUrl.startsWith("https://");
  const origin = new URL(deps.publicBaseUrl).origin;
  /** Nonces this isolate already announced: a repeated notification posts nothing twice. */
  const announced = new Map<string, number>();

  const verify = (token: string): ReturnType<typeof verifyApprovalToken> => verifyApprovalToken(token, deps.signingKey, now().getTime());

  const fanout = async (event: WebhookEvent, claims: ApprovalClaims, summary: ApprovalSummary, token: string | undefined): Promise<Fanout> => {
    const slack = event === "approval.created" && deps.slack && token ? await postApprovalMessage(deps.slack, summary, token) : false;
    const webhooks = claims.principal
      ? await deliverEvent({
        store: deps.webhooks, masterSecret: deps.webhookSecret, now,
        fetch: deps.fetch ?? ((input, init) => fetch(input, init)),
        ...(deps.sleep ? { sleep: deps.sleep } : {}),
        ...(deps.retryDelaysMs ? { retryDelaysMs: deps.retryDelaysMs } : {}),
      }, claims.principal, event, summary).catch(() => [])
      : [];
    const failed = webhooks.filter((w) => !w.delivered).length;
    deps.log(`approval_fanout event=${event} slack=${slack ? 1 : 0} webhooks=${webhooks.length} failed=${failed}`);
    return { slack, webhooks };
  };

  const notifyCreated = async (token: string): Promise<Fanout | undefined> => {
    const checked = verify(token);
    if (!checked.ok) return undefined;
    const nowMs = now().getTime();
    for (const [nonce, exp] of announced) if (exp <= nowMs) announced.delete(nonce);
    if (announced.has(checked.claims.nonce)) return undefined;
    announced.set(checked.claims.nonce, checked.claims.expiresMs);
    const summary = await deps.sidecar.summary(token, checked.claims.approvalId);
    if (!summary.ok) return undefined;
    return fanout("approval.created", checked.claims, summary.value, token);
  };

  /** Decide through the sidecar, then tell the principal's webhooks. */
  const decide = async (token: string, claims: ApprovalClaims, decision: "approve" | "reject", decidedBy: string) => {
    const result = await deps.sidecar.decide(token, claims.approvalId, decision, decidedBy, claims.nonce);
    if (result.ok) {
      const work = deps.sidecar.summary(token, claims.approvalId).then((s) => (s.ok ? fanout("approval.decided", claims, s.value, undefined) : undefined));
      defer(work);
    }
    return result;
  };

  // --- the approval page ---
  app.get("/approve/:token", async (c) => {
    const token = c.req.param("token");
    const checked = verify(token);
    if (!checked.ok) {
      c.set("relayLogEvent", "approval_refused");
      const [status, message] = TOKEN_REFUSAL[checked.code];
      return c.html(page("Approval link", `<h1>Approval link</h1><p>${escapeHtml(message)}</p>`), status as 400, PAGE_HEADERS);
    }
    const summary = await deps.sidecar.summary(token, checked.claims.approvalId);
    if (!summary.ok) {
      c.set("relayLogEvent", "approval_refused");
      return c.html(page("Approval", `<h1>Approval</h1><p>This request cannot be shown right now (${escapeHtml(summary.code)}). It may have been decided or expired.</p>`), (summary.status === 404 ? 404 : 503) as 404, PAGE_HEADERS);
    }
    c.set("relayLogEvent", "approval_page");
    const s = summary.value;
    if (s.status !== "pending") {
      return c.html(page("Approval", `<h1>Already ${escapeHtml(s.status)}</h1>${summaryHtml(s)}`), 200, PAGE_HEADERS);
    }
    const cookie = cookieOf(c.req.header("cookie"), CSRF_COOKIE) ?? randomBytes(16).toString("hex");
    const csrf = csrfFor(deps.signingKey, cookie, token);
    const form = `<form method="post" action="/approve/${escapeHtml(token)}"><input type="hidden" name="csrf" value="${csrf}"><button type="submit" name="decision" value="approve">Approve</button><button type="submit" name="decision" value="reject">Reject</button></form>`;
    const setCookie = `${CSRF_COOKIE}=${cookie}; Path=/approve; HttpOnly; SameSite=Strict; Max-Age=900${secure ? "; Secure" : ""}`;
    return c.html(page("Approve this request?", `<h1>Approve this request?</h1><p>An agent asked to do this on your behalf. Nothing happens until you decide.</p>${summaryHtml(s)}${form}`), 200, { ...PAGE_HEADERS, "set-cookie": setCookie });
  });

  app.post("/approve/:token", bodyLimit({ maxSize: MAX_FORM_BYTES, onError: (c) => c.text("Too large", 413) }), async (c) => {
    const token = c.req.param("token");
    const refuse = (status: number, message: string) => {
      c.set("relayLogEvent", "approval_refused");
      return c.html(page("Approval", `<h1>Not decided</h1><p>${escapeHtml(message)}</p>`), status as 403, PAGE_HEADERS);
    };
    const sentOrigin = c.req.header("origin");
    if (sentOrigin !== undefined && sentOrigin !== origin) return refuse(403, "This form must be submitted from the approval page.");
    const checked = verify(token);
    if (!checked.ok) {
      const [status, message] = TOKEN_REFUSAL[checked.code];
      return refuse(status, message);
    }
    const form = await c.req.parseBody().catch(() => ({} as Record<string, unknown>));
    const cookie = cookieOf(c.req.header("cookie"), CSRF_COOKIE);
    const csrf = typeof form.csrf === "string" ? form.csrf : "";
    const expected = cookie ? Buffer.from(csrfFor(deps.signingKey, cookie, token)) : undefined;
    if (!expected || csrf.length !== expected.length || !timingSafeEqual(Buffer.from(csrf), expected)) {
      return refuse(403, "The form expired or was not sent from the approval page. Open the link again.");
    }
    const decision = form.decision === "approve" ? "approve" : form.decision === "reject" ? "reject" : undefined;
    if (!decision) return refuse(400, "Choose Approve or Reject.");
    const result = await decide(token, checked.claims, decision, "web:approval-page");
    if (!result.ok) {
      const message = result.code === "token_replayed" || result.code === "not_pending"
        ? "This request was already decided."
        : result.code === "approval_expired" || result.code === "token_expired"
          ? "The request expired before the decision; nothing was approved."
          : "The decision could not be recorded. Try again in a moment.";
      return refuse(result.status === 409 || result.status === 410 ? 409 : 503, message);
    }
    c.set("relayLogEvent", "approval_decided");
    const done = decision === "approve" ? "Approved" : "Rejected";
    return c.html(page(done, `<h1>${done}</h1><p>The agent sees this on its next status check. You can close this page.</p>`), 200, PAGE_HEADERS);
  });

  // --- Slack ---
  app.post("/v1/slack/interactions", bodyLimit({ maxSize: MAX_SLACK_BYTES, onError: (c) => c.text("Too large", 413) }), async (c) => {
    if (!deps.slack) return agentError(c, 404, "slack_unavailable", "The Slack approval app is not configured on this gateway.");
    const raw = await c.req.text();
    if (!verifySlackSignature(deps.slack.signingSecret, raw, c.req.header("x-slack-request-timestamp"), c.req.header("x-slack-signature"), Math.floor(now().getTime() / 1000))) {
      c.set("relayLogEvent", "slack_rejected");
      return agentError(c, 401, "invalid_signature", "The Slack signature did not verify.");
    }
    const press = parseInteraction(raw);
    if (!press) return c.body(null, 200);
    const checked = verify(press.token);
    const reply = async (text: string): Promise<Response> => {
      if (press.responseUrl && deps.slack) await replaceMessage(deps.slack, press.responseUrl, text);
      return c.body(null, 200);
    };
    if (!checked.ok) {
      c.set("relayLogEvent", "approval_refused");
      return reply(TOKEN_REFUSAL[checked.code][1]);
    }
    const result = await decide(press.token, checked.claims, press.decision, `slack:${press.userId}`);
    if (!result.ok) {
      c.set("relayLogEvent", "approval_refused");
      return reply(result.status === 409 ? "This request was already decided." : result.status === 410 ? "This request expired; nothing was approved." : "The decision could not be recorded. Try again.");
    }
    c.set("relayLogEvent", "approval_decided");
    return reply(`${press.decision === "approve" ? "Approved" : "Rejected"} by <@${slackText(press.userId)}>.`);
  });

  // --- notifications from the core ---
  app.post("/v1/approvals/notify", bodyLimit({ maxSize: MAX_FORM_BYTES, onError: (c) => c.text("Too large", 413) }), async (c) => {
    const body = (await c.req.json().catch(() => undefined)) as { token?: unknown } | undefined;
    const token = typeof body?.token === "string" ? body.token : c.req.header("x-shogun-approval-token");
    if (!token) return agentError(c, 400, "invalid_token", "Send {\"token\": \"<approval token>\"}.");
    const checked = verify(token);
    if (!checked.ok) return agentError(c, checked.code === "bad_signature" ? 403 : 400, "invalid_token", TOKEN_REFUSAL[checked.code][1]);
    c.set("relayLogEvent", "approval_notified");
    defer(notifyCreated(token));
    return c.json({ accepted: true }, 202 as 200, { "cache-control": "no-store" });
  });

  // --- webhooks ---
  app.post("/v1/webhooks", requireKey(["write"]), bodyLimit({ maxSize: MAX_FORM_BYTES, onError: (c) => c.text("Too large", 413) }), async (c: Context<{ Variables: AgentVars }>) => {
    const p = agentPrincipalOf(c);
    if (!p) return agentError(c, 401, "unauthorized", "An agent key is required.");
    const body = (await c.req.json().catch(() => undefined)) as Record<string, unknown> | undefined;
    if (!body) return agentError(c, 400, "invalid_json", "The body must be JSON: {\"url\": \"https://…\", \"events\": [\"approval.created\", \"approval.decided\"]}.");
    const url = checkWebhookUrl(body.url, deps.allowHttpWebhooks === true);
    if (!url.ok) return agentError(c, 400, "invalid_url", url.message);
    const events = Array.isArray(body.events) ? body.events : [...WEBHOOK_EVENTS];
    if (events.length === 0 || !events.every((e): e is WebhookEvent => typeof e === "string" && (WEBHOOK_EVENTS as readonly string[]).includes(e))) {
      return agentError(c, 400, "invalid_events", `events must be a non-empty subset of ${WEBHOOK_EVENTS.join(", ")}.`);
    }
    try {
      const existing = await deps.webhooks.listByPrincipal(p.principalId);
      if (existing.length >= MAX_WEBHOOKS_PER_PRINCIPAL) return agentError(c, 409, "too_many_webhooks", `A principal may register at most ${MAX_WEBHOOKS_PER_PRINCIPAL} webhooks; delete one first.`);
      const webhookId = `wh_${randomUUID()}`;
      const createdAt = now().getTime();
      const unique = [...new Set(events)];
      await deps.webhooks.insert({ webhookId, principalId: p.principalId, url: url.url, events: unique, createdAt });
      c.set("relayLogEvent", "webhook_registered");
      return c.json({
        id: webhookId, url: url.url, events: unique, created_at: new Date(createdAt).toISOString(),
        // Shown once; derived, never stored. Verify X-Shogun-Signature with it.
        secret: webhookSecret(deps.webhookSecret, webhookId),
        signature: "X-Shogun-Signature: t=<unix>,v1=hex(HMAC-SHA256(secret, \"<t>.<raw body>\"))",
      }, 201, { "cache-control": "no-store" });
    } catch {
      return agentError(c, 503, "store_unavailable", "The webhook store is unavailable. Retry shortly.", "retry_after", { "retry-after": "1" });
    }
  });

  app.get("/v1/webhooks", requireKey(["read"]), async (c) => {
    const p = agentPrincipalOf(c);
    if (!p) return agentError(c, 401, "unauthorized", "An agent key is required.");
    try {
      const rows = await deps.webhooks.listByPrincipal(p.principalId);
      return c.json({ webhooks: rows.map((r) => ({ id: r.webhookId, url: r.url, events: r.events, created_at: new Date(r.createdAt).toISOString() })) }, 200, { "cache-control": "no-store" });
    } catch {
      return agentError(c, 503, "store_unavailable", "The webhook store is unavailable. Retry shortly.", "retry_after", { "retry-after": "1" });
    }
  });

  app.delete("/v1/webhooks/:id", requireKey(["write"]), async (c) => {
    const p = agentPrincipalOf(c);
    if (!p) return agentError(c, 401, "unauthorized", "An agent key is required.");
    try {
      const removed = await deps.webhooks.remove(c.req.param("id"), p.principalId, now().getTime());
      if (!removed) return agentError(c, 404, "webhook_not_found", "No such webhook.");
      return c.json({ id: c.req.param("id"), deleted: true }, 200, { "cache-control": "no-store" });
    } catch {
      return agentError(c, 503, "store_unavailable", "The webhook store is unavailable. Retry shortly.", "retry_after", { "retry-after": "1" });
    }
  });

  return { app, notifyCreated };
}
