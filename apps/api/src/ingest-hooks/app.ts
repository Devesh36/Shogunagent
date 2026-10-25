/**
 * Ingest hooks (docs/implementation-plan.md E44; docs/api/ingest-sources.md): provider webhooks
 * into the principal's memory, through the core's `memory.ingest_webhook` over the same sidecar
 * relay `/mcp` uses (`mcp-proxy.ts`).
 *
 *   POST   /v1/ingest-hooks                         register a hook           [write]
 *   GET    /v1/ingest-hooks                         the principal's hooks     [read]
 *   GET    /v1/ingest-hooks/:id                     one hook (+ Notion's token while unverified) [write]
 *   POST   /v1/ingest-hooks/:id/rotate              a new secret; the old one works 24 h [write]
 *   DELETE /v1/ingest-hooks/:id                     remove                    [write]
 *   POST   /v1/ingest-hooks/{github,linear,slack,notion,google_calendar}/:hook   deliveries (signed)
 *   POST   /v1/ingest-hooks/gmail                   Gmail Pub/Sub pushes (Google OIDC token)
 *
 * Bodies (CLAUDE.md invariants 1 and 2). A delivery's signature is an HMAC over the whole body, so
 * the body is held in memory (at most `MAX_DELIVERY_BYTES`) until it is verified: nothing
 * unverified reaches a store. It is then base64-encoded into the `memory.ingest_webhook` call and
 * dropped; the gateway does not read it. It is never written, logged, put in a metric or an
 * error, and the provider is never answered with anything from it or from the core's reply (a
 * provider shows response bodies in its delivery log, which is outside the principal's store).
 * The bounded exceptions, each tested to persist nothing:
 *   - Slack's `url_verification` (≤ 1 KiB, after the signature) is parsed to echo `challenge`;
 *   - Notion's one-time `{verification_token}` (≤ 1 KiB, before any secret exists) is parsed to
 *     seal the token as the hook's signing secret;
 *   - a Gmail push's Pub/Sub envelope (≤ 16 KiB, after the OIDC token) is parsed for the message
 *     id, the history id and the mailbox (routed by its HMAC); only the two ids are relayed.
 * Calendar pushes carry no body: the typed change comes from Google's `X-Goog-*` headers.
 *
 * Billing (docs/api/billing.md): a delivery is a `memory.ingest_webhook` call, metered as ingest
 * against the principal's wallet like any other: up to `MAX_EVENTS_PER_DELIVERY` events are
 * reserved, the core's `Mcp-Quantity` settles it (a change notice, an ignored event or a replay is
 * 0). Handshakes (GitHub `ping`, Slack `url_verification`, Notion's token, Calendar `sync`, an
 * unrouted Gmail push) reserve nothing. A bad signature or an unknown hook costs nothing, reaches
 * no store, and counts against a per-IP and per-hook failure budget (`FailureGate`); past it,
 * deliveries are refused 429 before any verification work.
 */
import { createHash, randomBytes } from "node:crypto";

import type { Context, Hono } from "hono";
import { bodyLimit } from "hono/body-limit";

import { admitPrincipal, admitRefusalResponse, agentError, agentPrincipalOf, requireAgentKey, type AgentKeyLookup, type AgentPrincipal, type AgentVars } from "../agent-keys.js";
import { reserveCall, settleCall, settledListCost, settledPrice, settledQuantity, usageHeaders, type MeteringDeps } from "../metering.js";
import { McpRelayRefused } from "../mcp-proxy.js";
import { quoteCost } from "../pricing.js";
import { WindowLimiter, type RateLimiter } from "../ratelimit.js";
import { verifyGoogleOidc, type JwksSource } from "./pubsub-jwt.js";
import type { HookSecrets } from "./secrets.js";
import type { HookRecord, HookStore } from "./store.js";
import { HOOK_PROVIDERS, tokenMatches, verifyGitHub, verifyLinear, verifyNotion, verifySlack, type HookProvider } from "./verify.js";

export const INGEST_HOOKS_PATH = "/v1/ingest-hooks";
/** The core tool a delivery becomes (crates/shogun-mcp/src/webhook_ingest.rs). */
export const WEBHOOK_TOOL = "memory.ingest_webhook";
/** Largest delivery body (GitHub caps payloads at 25 MB; larger pushes are refused 413). */
export const MAX_DELIVERY_BYTES = 1024 * 1024;
const MAX_HANDSHAKE_BYTES = 1024;
const MAX_GMAIL_BYTES = 16 * 1024;
const MAX_MANAGE_BYTES = 4 * 1024;
/** Events reserved per delivery; the core never writes more (`MAX_EVENTS_PER_DELIVERY` there). */
export const MAX_EVENTS_PER_DELIVERY = 20;
export const MAX_HOOKS_PER_PRINCIPAL = 20;
/** How long the previous secret keeps working after a rotation. */
export const ROTATION_GRACE_MS = 24 * 3_600_000;
/** Failed deliveries (bad signature, unknown hook) per IP and per hook in a window. */
export const FAILURES_PER_WINDOW = 30;
export const FAILURE_WINDOW_MS = 10 * 60_000;

const HOOK_ID_RE = /^hk_[A-Za-z0-9_-]{22}$/;
const DELIVERY_ID_RE = /^[A-Za-z0-9._:-]{1,96}$/;
const SIGNING_SECRET_RE = /^[\x21-\x7e]{16,256}$/;
const NOTION_TOKEN_RE = /^secret_[A-Za-z0-9]{16,128}$/;
const EMAIL_RE = /^[^\s@]{1,64}@[^\s@]{1,255}$/;

/** Who supplies the secret, per provider. */
const SECRET_SOURCE: Readonly<Record<HookProvider, "derived" | "supplied" | "captured" | "oidc">> = Object.freeze({
  github: "derived",
  google_calendar: "derived",
  linear: "supplied",
  slack: "supplied",
  notion: "captured",
  gmail: "oidc",
});

/** Gmail push settings: the push subscription's OIDC audience and service account. */
export interface GmailPushConfig {
  audience: string;
  serviceAccount: string;
  jwks: JwksSource;
}

export interface IngestHookDeps {
  store: HookStore;
  secrets: HookSecrets;
  /** Absent = `/v1/ingest-hooks/gmail` answers 503 and Gmail hooks cannot be created. */
  gmail?: GmailPushConfig;
  /** Failed-delivery budget (default: in-process window). Production passes a shared limiter. */
  failureLimiter?: RateLimiter;
}

/**
 * Per-IP and per-hook failure budget. `take` is counted on each failure only; once a key is out,
 * it is refused locally until its window ends (the limiter is shared across instances, the
 * refusal cache is per instance).
 */
export class FailureGate {
  private readonly blockedUntil = new Map<string, number>();

  constructor(private readonly limiter: RateLimiter = new WindowLimiter(FAILURES_PER_WINDOW, FAILURE_WINDOW_MS)) {}

  retryAfter(keys: readonly string[], nowMs: number): number | undefined {
    let worst: number | undefined;
    for (const key of keys) {
      const until = this.blockedUntil.get(key);
      if (until === undefined) continue;
      if (until <= nowMs) {
        this.blockedUntil.delete(key);
        continue;
      }
      worst = Math.max(worst ?? 0, Math.ceil((until - nowMs) / 1000));
    }
    return worst;
  }

  async fail(keys: readonly string[], nowMs: number): Promise<void> {
    for (const key of keys) {
      let ok = true;
      try {
        ok = await this.limiter.take(key, nowMs);
      } catch {
        ok = true;
      }
      if (!ok) {
        let secs = 60;
        try {
          secs = await this.limiter.retryAfterSecs(key, nowMs);
        } catch {
          secs = 60;
        }
        this.blockedUntil.set(key, nowMs + Math.max(1, secs) * 1000);
      }
    }
    // Bounded: the refusal cache never outgrows the attackers it is refusing.
    if (this.blockedUntil.size > 10_000) this.blockedUntil.clear();
  }
}

/** A new hook id: `hk_` and 16 random bytes, base64url (also a valid Calendar channel id). */
export function newHookId(): string {
  return `hk_${randomBytes(16).toString("base64url")}`;
}

const sha256Hex = (data: Uint8Array | string): string => createHash("sha256").update(data).digest("hex");

/** The URL a provider is pointed at. Gmail's is shared: pushes route by mailbox. */
export function deliveryUrl(base: string, provider: HookProvider, hookId: string): string {
  const root = `${base.replace(/\/+$/, "")}${INGEST_HOOKS_PATH}/${provider}`;
  return provider === "gmail" ? root : `${root}/${hookId}`;
}

/** The secrets a delivery may be signed with now: the current one, and the previous one inside
 * the rotation grace. */
function candidateSecrets(hook: HookRecord, secrets: HookSecrets, nowMs: number): string[] {
  const inGrace = hook.previousValidUntil !== undefined && hook.previousValidUntil > nowMs;
  const out: string[] = [];
  if (SECRET_SOURCE[hook.provider] === "derived") {
    out.push(secrets.derived(hook.hookId, hook.generation));
    if (inGrace && hook.previousGeneration !== undefined) out.push(secrets.derived(hook.hookId, hook.previousGeneration));
  } else {
    const current = hook.sealedSecret ? secrets.open(hook.sealedSecret, hook.hookId) : undefined;
    if (current) out.push(current);
    const previous = inGrace && hook.previousSealedSecret ? secrets.open(hook.previousSealedSecret, hook.hookId) : undefined;
    if (previous) out.push(previous);
  }
  return out;
}

/** What a verified delivery relays to the core. */
interface Relayable {
  hook: HookRecord;
  deliveryId: string;
  event?: string;
  body?: Uint8Array;
  changed?: Record<string, string>;
}

type Verdict =
  | { kind: "reject" }
  | { kind: "handshake"; response: Response }
  | { kind: "relay"; delivery: Relayable };

function fixedJson(status: 200 | 202, body: Record<string, unknown>, headers: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json", "cache-control": "no-store", ...headers } });
}

/** The public view of a hook. `extra` carries a secret only on the responses that show it once. */
function hookView(base: string, h: HookRecord, extra: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    hook_id: h.hookId,
    provider: h.provider,
    url: deliveryUrl(base, h.provider, h.hookId),
    status: h.status,
    created_at: new Date(h.createdAt).toISOString(),
    rotated_at: h.rotatedAt !== undefined ? new Date(h.rotatedAt).toISOString() : null,
    last_delivery_at: h.lastDeliveryAt !== undefined ? new Date(h.lastDeliveryAt).toISOString() : null,
    previous_secret_valid_until: h.previousValidUntil !== undefined && h.previousValidUntil > 0 ? new Date(h.previousValidUntil).toISOString() : null,
    ...extra,
  };
}

/** The secret fields a create or rotate shows, once. Derived values can be recomputed, so a
 * replayed create shows them again. */
function shownSecret(h: HookRecord, secrets: HookSecrets): Record<string, unknown> {
  switch (h.provider) {
    case "github":
      return { secret: secrets.derived(h.hookId, h.generation), setup: "In the repository or organization: Settings → Webhooks → Add webhook. Payload URL = url, Content type = application/json, Secret = secret." };
    case "google_calendar":
      return { channel_id: h.hookId, channel_token: secrets.derived(h.hookId, h.generation), setup: "Watch the calendar with events.watch: id = channel_id, type = web_hook, address = url, token = channel_token. The core does this itself once Google Calendar is connected." };
    case "linear":
      return { setup: "Linear: Settings → API → Webhooks. URL = url; the signing secret Linear shows is the one you registered here." };
    case "slack":
      return { setup: "Slack app: Event Subscriptions → Request URL = url (Slack's challenge is answered here), subscribe to message.channels / message.im / app_mention." };
    case "notion":
      return { setup: "Notion integration: Webhooks → Create subscription with url. Then GET this hook: verification_token appears once Notion has sent it; paste it into Notion's Verify form." };
    case "gmail":
      return { setup: "Gmail users.watch on the operator's Pub/Sub topic; the push subscription points at url with authentication on. The mailbox you registered routes the pushes here." };
  }
}

function bodyHashId(body: Uint8Array): string {
  return `sha256-${sha256Hex(body).slice(0, 64)}`;
}

function headerId(raw: string | undefined, body: Uint8Array): string {
  const v = raw?.trim();
  return v && DELIVERY_ID_RE.test(v) ? v : bodyHashId(body);
}

/** Small JSON bodies the bounded exceptions read. `undefined` on anything else. */
function smallJson(body: Uint8Array, max: number): Record<string, unknown> | undefined {
  if (body.byteLength === 0 || body.byteLength > max) return undefined;
  try {
    const v: unknown = JSON.parse(new TextDecoder().decode(body));
    return typeof v === "object" && v !== null && !Array.isArray(v) ? (v as Record<string, unknown>) : undefined;
  } catch {
    return undefined;
  }
}

export function registerIngestHookRoutes(
  app: Hono<{ Variables: AgentVars }>,
  hooks: IngestHookDeps,
  metering: MeteringDeps,
  lookup: AgentKeyLookup,
  observe?: (path: string) => void,
): void {
  const now = metering.now ?? (() => new Date());
  const base = metering.publicBaseUrl;
  const gate = new FailureGate(hooks.failureLimiter);
  const requireKey = (scopes: Parameters<typeof requireAgentKey>[1]) => requireAgentKey(lookup, scopes, now);
  const tooLarge = (c: Context): Response => agentError(c, 413, "body_too_large", "The delivery is larger than this endpoint accepts (1 MiB). Nothing was stored or charged.");
  const principalOf = (c: Context<{ Variables: AgentVars }>): AgentPrincipal => {
    const p = agentPrincipalOf(c);
    if (!p) throw new Error("route registered without requireAgentKey");
    return p;
  };
  const storeDown = (c: Context): Response => {
    c.set("relayLogEvent", "ingest_hook_store_unavailable");
    return agentError(c, 503, "store_unavailable", "The hook store is unavailable. Retry shortly.", "retry_after", { "retry-after": "5" });
  };
  observe?.(INGEST_HOOKS_PATH);
  observe?.(`${INGEST_HOOKS_PATH}/*`);

  // --- deliveries (registered before the `:id` routes; provider segments are literals) ---

  const deliver = (provider: Exclude<HookProvider, "gmail">) =>
    async (c: Context<{ Variables: AgentVars }>): Promise<Response> => {
      const hookId = c.req.param("hook") ?? "";
      const ip = clientIpOf(c);
      const keys = [`ingest-hook:ip:${ip}`, `ingest-hook:hook:${hookId.slice(0, 32)}`];
      const nowMs = now().getTime();
      const blocked = gate.retryAfter(keys, nowMs);
      if (blocked !== undefined) {
        c.set("relayLogEvent", "ingest_hook_rate_limited");
        return agentError(c, 429, "too_many_failures", "Too many deliveries to this endpoint failed verification. Nothing was stored or charged. Retry later.", "retry_after", { "retry-after": String(blocked) });
      }
      const body = new Uint8Array(await c.req.arrayBuffer());
      let verdict: Verdict;
      try {
        verdict = HOOK_ID_RE.test(hookId) ? await verify(c, provider, hookId, body, nowMs) : { kind: "reject" };
      } catch {
        return storeDown(c);
      }
      if (verdict.kind === "reject") {
        await gate.fail(keys, nowMs);
        c.set("relayLogEvent", "ingest_hook_rejected");
        return agentError(c, 401, "invalid_signature", "The delivery's signature did not verify for this hook. Nothing was stored or charged. Check the hook's secret, or rotate it.", "fix_input");
      }
      if (verdict.kind === "handshake") {
        c.set("relayLogEvent", "ingest_hook_handshake");
        return verdict.response;
      }
      return relay(c, verdict.delivery, nowMs);
    };

  const verify = async (c: Context, provider: Exclude<HookProvider, "gmail">, hookId: string, body: Uint8Array, nowMs: number): Promise<Verdict> => {
    const hook = await hooks.store.get(hookId);
    if (!hook || hook.provider !== provider) return { kind: "reject" };
    const header = (name: string): string | undefined => c.req.header(name);
    switch (provider) {
      case "github": {
        if (!verifyGitHub(candidateSecrets(hook, hooks.secrets, nowMs), body, header("x-hub-signature-256"))) return { kind: "reject" };
        const event = header("x-github-event")?.trim() ?? "";
        if (event === "ping") return { kind: "handshake", response: fixedJson(200, { received: true, handshake: "ping" }) };
        return { kind: "relay", delivery: { hook, deliveryId: headerId(header("x-github-delivery"), body), ...(/^[a-z_]{1,64}$/.test(event) ? { event } : {}), body } };
      }
      case "linear": {
        if (!verifyLinear(candidateSecrets(hook, hooks.secrets, nowMs), body, header("linear-signature"))) return { kind: "reject" };
        const event = header("linear-event")?.trim() ?? "";
        return { kind: "relay", delivery: { hook, deliveryId: headerId(header("linear-delivery"), body), ...(/^[A-Za-z]{1,64}$/.test(event) ? { event } : {}), body } };
      }
      case "slack": {
        if (!verifySlack(candidateSecrets(hook, hooks.secrets, nowMs), body, header("x-slack-request-timestamp"), header("x-slack-signature"), Math.floor(nowMs / 1000))) return { kind: "reject" };
        // Bounded exception: Slack's URL verification is answered from its own `challenge`.
        const small = smallJson(body, MAX_HANDSHAKE_BYTES);
        if (small?.type === "url_verification" && typeof small.challenge === "string" && small.challenge.length <= 256) {
          return { kind: "handshake", response: fixedJson(200, { challenge: small.challenge }) };
        }
        return { kind: "relay", delivery: { hook, deliveryId: bodyHashId(body), body } };
      }
      case "notion": {
        const signature = header("x-notion-signature");
        if (signature === undefined && hook.status === "awaiting_verification" && !hook.sealedSecret) {
          // Bounded exception: Notion's one-time verification request carries the token that will
          // sign every later event. It is sealed at once and shown only to the hook's owner.
          const small = smallJson(body, MAX_HANDSHAKE_BYTES);
          const token = small?.verification_token;
          if (!small || Object.keys(small).length !== 1 || typeof token !== "string" || !NOTION_TOKEN_RE.test(token)) return { kind: "reject" };
          const updated = await hooks.store.update({ ...hook, sealedSecret: hooks.secrets.seal(token, hook.hookId) });
          if (!updated) return { kind: "reject" };
          return { kind: "handshake", response: fixedJson(200, { received: true, handshake: "verification_token" }) };
        }
        if (!verifyNotion(candidateSecrets(hook, hooks.secrets, nowMs), body, signature)) return { kind: "reject" };
        if (hook.status === "awaiting_verification") {
          const active: HookRecord = { ...hook, status: "active" };
          await hooks.store.update(active);
          return { kind: "relay", delivery: { hook: active, deliveryId: bodyHashId(body), body } };
        }
        return { kind: "relay", delivery: { hook, deliveryId: bodyHashId(body), body } };
      }
      case "google_calendar": {
        if (body.byteLength > 0) return { kind: "reject" };
        if (header("x-goog-channel-id")?.trim() !== hook.hookId) return { kind: "reject" };
        if (!tokenMatches(candidateSecrets(hook, hooks.secrets, nowMs), header("x-goog-channel-token")?.trim())) return { kind: "reject" };
        const state = header("x-goog-resource-state")?.trim() ?? "";
        if (state === "sync") return { kind: "handshake", response: fixedJson(200, { received: true, handshake: "sync" }) };
        const message = header("x-goog-message-number")?.trim() ?? "";
        const resourceId = header("x-goog-resource-id")?.trim() ?? "";
        const resourceUri = header("x-goog-resource-uri")?.trim() ?? "";
        if (!/^\d{1,20}$/.test(message) || !/^[a-z_]{1,32}$/.test(state) || !/^[\x21-\x7e]{1,256}$/.test(resourceId)) return { kind: "reject" };
        const changed: Record<string, string> = { channel_id: hook.hookId, resource_id: resourceId, resource_state: state, message_number: message };
        if (/^https:\/\/www\.googleapis\.com\/[\x21-\x7e]{1,480}$/.test(resourceUri)) changed.resource_uri = resourceUri;
        return { kind: "relay", delivery: { hook, deliveryId: `${hook.hookId}:${message}`, changed } };
      }
    }
  };

  /** One verified delivery → `memory.ingest_webhook` on the principal's core, metered as ingest. */
  const relay = async (c: Context<{ Variables: AgentVars }>, d: Relayable, startedMs: number): Promise<Response> => {
    const principal: AgentPrincipal = { keyId: d.hook.hookId, mode: "live", principalId: d.hook.principalId, tenantId: d.hook.tenantId, scopes: ["write"] };
    c.set("relayLogAgentKey", d.hook.hookId);
    const refused = await admitPrincipal(lookup, principal, startedMs);
    if (refused) {
      c.set("relayLogEvent", refused.status === 503 ? "tenant_directory_unavailable" : "region_refused");
      return admitRefusalResponse(c, refused);
    }
    if (!metering.mcpUpstream) {
      c.set("relayLogEvent", "upstream_unavailable");
      return agentError(c, 503, "upstream_unavailable", "This gateway has no core behind it yet. Nothing was stored or charged. Retry later.", "retry_after", { "retry-after": "30" });
    }
    const relayRefused = metering.mcpAdmit?.(principal);
    if (relayRefused) {
      c.set("relayLogEvent", relayRefused.code === "wrong_shard" ? "wrong_shard" : "upstream_unavailable");
      return admitRefusalResponse(c, relayRefused);
    }
    const idempotencyKey = `hook:${d.hook.provider}:${d.deliveryId}`;
    const args: Record<string, unknown> = {
      provider: d.hook.provider,
      delivery_id: d.deliveryId,
      idempotency_key: idempotencyKey,
      hook_id: d.hook.hookId,
      received_ms: startedMs,
      ...(d.event ? { event: d.event } : {}),
      // The verified bytes, encoded, never read here (the core parses them).
      ...(d.body ? { payload_b64: Buffer.from(d.body).toString("base64") } : {}),
      ...(d.changed ? { changed: d.changed } : {}),
    };
    const rpc = JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name: WEBHOOK_TOOL, arguments: args } });
    const quote = quoteCost("ingest", MAX_EVENTS_PER_DELIVERY);
    const reserved = await reserveCall(c, metering, principal, { method: "tools/call", name: WEBHOOK_TOOL }, quote, startedMs);
    if (!reserved.ok) return reserved.response;
    const request = new Request(`${base.replace(/\/+$/, "")}/mcp`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        accept: "application/json",
        "mcp-method": "tools/call",
        "mcp-name": WEBHOOK_TOOL,
        "idempotency-key": idempotencyKey,
      },
      body: rpc,
    });
    let upstream: Response;
    try {
      upstream = await metering.mcpUpstream(request, principal);
    } catch (e) {
      await settleCall(metering, reserved.receiptId, 0);
      if (e instanceof McpRelayRefused) {
        c.set("relayLogEvent", "upstream_unavailable");
        return admitRefusalResponse(c, e.refusal);
      }
      c.set("relayLogEvent", "upstream_failure");
      return agentError(c, 503, "upstream_unavailable", "The core did not answer. Nothing was stored or charged. The provider will retry.", "retry_after", { "retry-after": "30" });
    }
    const quantity = settledQuantity(upstream, quote.quantity);
    const rpcError = upstream.headers.get("mcp-error-code")?.trim();
    const status = upstream.status;
    // The core's reply carries the principal's event ids and receipt: it goes no further than here.
    await upstream.body?.cancel().catch(() => undefined);
    const { charged, list } = settledPrice(reserved.price, settledListCost(quote, quantity));
    await settleCall(metering, reserved.receiptId, charged, list, principal, reserved.price.terms?.funding ?? "wallet", quantity);
    const retryable = status >= 500 || status === 429 || rpcError === "-32000";
    if (retryable) {
      c.set("relayLogEvent", "upstream_failure");
      return agentError(c, 503, "upstream_unavailable", "The core could not store this delivery now. Nothing was charged. The provider will retry; the delivery id makes the retry safe.", "retry_after", { "retry-after": "30" });
    }
    await hooks.store.touch(d.hook.hookId, startedMs).catch(() => undefined);
    const accepted = status >= 200 && status < 300 && rpcError === undefined;
    c.set("relayLogEvent", accepted ? "ingest_hook_relayed" : "ingest_hook_refused_by_core");
    const headers: Record<string, string> = { ...usageHeaders(reserved.receiptId, { ...quote, quantity, cost_micro_usd: charged }), "x-latency-ms": String(Math.max(0, now().getTime() - startedMs)) };
    // A refusal for the payload's shape is final: answering 2xx stops the provider retrying a
    // delivery that can never be stored. Nothing about it is echoed.
    return fixedJson(200, { received: true, stored_events: quantity, ...(accepted ? {} : { refused: true }) }, headers);
  };

  // GitHub, Linear, Slack, Notion, Google Calendar: signed per hook.
  for (const provider of ["github", "linear", "slack", "notion", "google_calendar"] as const) {
    app.post(`${INGEST_HOOKS_PATH}/${provider}/:hook`, bodyLimit({ maxSize: MAX_DELIVERY_BYTES, onError: tooLarge }), deliver(provider));
  }

  // Gmail: one Pub/Sub push endpoint for every mailbox, authenticated by Google's OIDC token.
  app.post(`${INGEST_HOOKS_PATH}/gmail`, bodyLimit({ maxSize: MAX_GMAIL_BYTES, onError: tooLarge }), async (c) => {
    const nowMs = now().getTime();
    const ip = clientIpOf(c);
    const keys = [`ingest-hook:ip:${ip}`];
    const blocked = gate.retryAfter(keys, nowMs);
    if (blocked !== undefined) {
      c.set("relayLogEvent", "ingest_hook_rate_limited");
      return agentError(c, 429, "too_many_failures", "Too many pushes to this endpoint failed verification. Retry later.", "retry_after", { "retry-after": String(blocked) });
    }
    if (!hooks.gmail) {
      c.set("relayLogEvent", "upstream_unavailable");
      return agentError(c, 503, "gmail_push_unconfigured", "Gmail push is not configured on this gateway (INGEST_HOOKS_GMAIL_AUDIENCE). Nothing was stored.", "retry_after", { "retry-after": "3600" });
    }
    const verdict = await verifyGoogleOidc(c.req.header("authorization"), hooks.gmail, hooks.gmail.jwks, nowMs);
    if (!verdict.ok) {
      if (verdict.reason === "keys_unavailable") {
        c.set("relayLogEvent", "upstream_unavailable");
        return agentError(c, 503, "verification_unavailable", "Google's signing keys could not be read. Nothing was stored. Pub/Sub will retry.", "retry_after", { "retry-after": "30" });
      }
      await gate.fail(keys, nowMs);
      c.set("relayLogEvent", "ingest_hook_rejected");
      return agentError(c, 401, "invalid_signature", "The push's OIDC token did not verify. Nothing was stored or charged.", "fix_input");
    }
    // Bounded exception: the Pub/Sub envelope is read for its message id and the Gmail
    // notification inside it ({emailAddress, historyId}); the address is only hashed for routing.
    const body = new Uint8Array(await c.req.arrayBuffer());
    const envelope = smallJson(body, MAX_GMAIL_BYTES);
    const message = envelope?.message as Record<string, unknown> | undefined;
    const messageId = typeof message?.messageId === "string" ? message.messageId : typeof message?.message_id === "string" ? message.message_id : "";
    const data = typeof message?.data === "string" ? message.data : "";
    let note: Record<string, unknown> | undefined;
    try {
      note = data.length <= 2048 ? smallJson(new Uint8Array(Buffer.from(data, "base64")), 2048) : undefined;
    } catch {
      note = undefined;
    }
    const address = typeof note?.emailAddress === "string" ? note.emailAddress : "";
    const historyId = typeof note?.historyId === "number" || typeof note?.historyId === "string" ? String(note.historyId) : "";
    if (!DELIVERY_ID_RE.test(messageId) || !EMAIL_RE.test(address) || !/^\d{1,24}$/.test(historyId)) {
      // A well-signed push we cannot read: acknowledged (Pub/Sub would redeliver it forever).
      c.set("relayLogEvent", "ingest_hook_handshake");
      return fixedJson(200, { received: true, routed: false });
    }
    let hook: HookRecord | undefined;
    try {
      hook = await hooks.store.byRouteKey("gmail", hooks.secrets.mailboxKey(address));
    } catch {
      return storeDown(c);
    }
    if (!hook) {
      // A mailbox no hook claims (removed since the watch was set): acknowledged, free, dropped.
      c.set("relayLogEvent", "ingest_hook_handshake");
      return fixedJson(200, { received: true, routed: false });
    }
    return relay(c, { hook, deliveryId: messageId, changed: { history_id: historyId, message_id: messageId } }, nowMs);
  });

  // --- management (the principal's own agent key) ---

  app.post(INGEST_HOOKS_PATH, requireKey(["write"]), bodyLimit({ maxSize: MAX_MANAGE_BYTES, onError: tooLarge }), async (c) => {
    const p = principalOf(c);
    const nowMs = now().getTime();
    let body: Record<string, unknown>;
    try {
      const raw: unknown = await c.req.json();
      body = typeof raw === "object" && raw !== null && !Array.isArray(raw) ? (raw as Record<string, unknown>) : {};
    } catch {
      return agentError(c, 400, "invalid_json", "Send a JSON object: {\"provider\": \"github\"}.", "fix_input");
    }
    const provider = body.provider;
    if (typeof provider !== "string" || !(HOOK_PROVIDERS as readonly string[]).includes(provider)) {
      return agentError(c, 400, "invalid_provider", `provider must be one of ${HOOK_PROVIDERS.join(", ")}.`, "fix_input");
    }
    const prov = provider as HookProvider;
    const rawKey = c.req.header("idempotency-key")?.trim();
    if (rawKey !== undefined && (rawKey.length === 0 || rawKey.length > 128)) return agentError(c, 400, "invalid_idempotency_key", "Idempotency-Key is 1-128 characters.", "fix_input");
    const createKeyHash = rawKey ? sha256Hex(`${p.principalId}\n${rawKey}`) : undefined;
    try {
      if (createKeyHash) {
        const earlier = await hooks.store.byCreateKey(p.principalId, createKeyHash);
        if (earlier) {
          if (earlier.provider !== prov) return agentError(c, 409, "idempotency_key_reused", "This Idempotency-Key created a hook for another provider. Use a new key.", "fix_input");
          c.set("relayLogEvent", "ingest_hook_replayed");
          return c.json(hookView(base, earlier, { ...shownSecret(earlier, hooks.secrets), replayed: true }), 200, { "cache-control": "no-store" });
        }
      }
      if ((await hooks.store.listByPrincipal(p.principalId)).length >= MAX_HOOKS_PER_PRINCIPAL) {
        return agentError(c, 409, "too_many_hooks", `A principal has at most ${MAX_HOOKS_PER_PRINCIPAL} ingest hooks. Delete one first.`, "fix_input");
      }
    } catch {
      return storeDown(c);
    }
    const hookId = newHookId();
    const record: HookRecord = {
      hookId, principalId: p.principalId, tenantId: p.tenantId, keyId: p.keyId, provider: prov,
      status: prov === "notion" ? "awaiting_verification" : "active", generation: 1, createdAt: nowMs,
      ...(createKeyHash ? { createKeyHash } : {}),
    };
    if (SECRET_SOURCE[prov] === "supplied") {
      const s = body.signing_secret;
      if (typeof s !== "string" || !SIGNING_SECRET_RE.test(s)) return agentError(c, 400, "invalid_signing_secret", `A ${prov} hook needs signing_secret: the signing secret ${prov === "slack" ? "of your Slack app (Basic Information)" : "Linear shows for the webhook"}, 16-256 visible characters.`, "fix_input");
      record.sealedSecret = hooks.secrets.seal(s, hookId);
    }
    if (prov === "gmail") {
      if (!hooks.gmail) return agentError(c, 503, "gmail_push_unconfigured", "Gmail push is not configured on this gateway (INGEST_HOOKS_GMAIL_AUDIENCE). Ask the operator.", "contact_operator", { "retry-after": "3600" });
      const mailbox = body.mailbox;
      if (typeof mailbox !== "string" || !EMAIL_RE.test(mailbox.trim())) return agentError(c, 400, "invalid_mailbox", "A gmail hook needs mailbox: the Gmail address whose pushes come here.", "fix_input");
      record.routeKey = hooks.secrets.mailboxKey(mailbox);
    }
    let inserted: Awaited<ReturnType<HookStore["insert"]>>;
    try {
      inserted = await hooks.store.insert(record);
    } catch {
      return storeDown(c);
    }
    if (inserted === "route_taken") return agentError(c, 409, "mailbox_taken", "Another hook already receives this mailbox's pushes. Delete that hook first.", "fix_input");
    if (inserted === "duplicate_create") return agentError(c, 409, "idempotency_key_reused", "This Idempotency-Key is being used by a concurrent request. Retry it.", "retry_after", { "retry-after": "1" });
    c.set("relayLogEvent", "ingest_hook_created");
    return c.json(hookView(base, record, shownSecret(record, hooks.secrets)), 201, { "cache-control": "no-store" });
  });

  app.get(INGEST_HOOKS_PATH, requireKey(["read"]), async (c) => {
    try {
      const rows = await hooks.store.listByPrincipal(principalOf(c).principalId);
      return c.json({ hooks: rows.map((h) => hookView(base, h)) }, 200, { "cache-control": "no-store" });
    } catch {
      return storeDown(c);
    }
  });

  const ownHook = async (c: Context<{ Variables: AgentVars }>): Promise<HookRecord | Response> => {
    const id = c.req.param("id") ?? "";
    if (!HOOK_ID_RE.test(id)) return agentError(c, 404, "hook_not_found", "No such ingest hook for this principal.");
    let hook: HookRecord | undefined;
    try {
      hook = await hooks.store.get(id);
    } catch {
      return storeDown(c);
    }
    if (!hook || hook.principalId !== principalOf(c).principalId) return agentError(c, 404, "hook_not_found", "No such ingest hook for this principal.");
    return hook;
  };

  app.get(`${INGEST_HOOKS_PATH}/:id`, requireKey(["write"]), async (c) => {
    const hook = await ownHook(c);
    if (hook instanceof Response) return hook;
    // Notion shows its token to whoever controls the endpoint; the owner reads it here to paste
    // into Notion's Verify form, until the first signed event arrives.
    const token = hook.provider === "notion" && hook.status === "awaiting_verification" && hook.sealedSecret ? hooks.secrets.open(hook.sealedSecret, hook.hookId) : undefined;
    return c.json(hookView(base, hook, token ? { verification_token: token } : {}), 200, { "cache-control": "no-store" });
  });

  app.post(`${INGEST_HOOKS_PATH}/:id/rotate`, requireKey(["write"]), bodyLimit({ maxSize: MAX_MANAGE_BYTES, onError: tooLarge }), async (c) => {
    const hook = await ownHook(c);
    if (hook instanceof Response) return hook;
    const nowMs = now().getTime();
    const source = SECRET_SOURCE[hook.provider];
    if (source === "oidc") return agentError(c, 400, "nothing_to_rotate", "A gmail hook has no secret of its own: Google signs each push.", "fix_input");
    const next: HookRecord = { ...hook, rotatedAt: nowMs, previousValidUntil: nowMs + ROTATION_GRACE_MS };
    if (source === "derived") {
      next.previousGeneration = hook.generation;
      next.generation = hook.generation + 1;
    } else if (source === "supplied") {
      let body: Record<string, unknown> = {};
      try {
        const raw: unknown = await c.req.json();
        if (typeof raw === "object" && raw !== null && !Array.isArray(raw)) body = raw as Record<string, unknown>;
      } catch {
        body = {};
      }
      const s = body.signing_secret;
      if (typeof s !== "string" || !SIGNING_SECRET_RE.test(s)) return agentError(c, 400, "invalid_signing_secret", "Send the provider's new signing_secret (16-256 visible characters).", "fix_input");
      if (hook.sealedSecret) next.previousSealedSecret = hook.sealedSecret;
      next.sealedSecret = hooks.secrets.seal(s, hook.hookId);
    } else {
      // Notion: start over. The old token keeps verifying for the grace period while Notion is
      // pointed at the new subscription.
      if (hook.sealedSecret) next.previousSealedSecret = hook.sealedSecret;
      delete next.sealedSecret;
      next.status = "awaiting_verification";
    }
    try {
      if (!(await hooks.store.update(next))) return agentError(c, 404, "hook_not_found", "No such ingest hook for this principal.");
    } catch {
      return storeDown(c);
    }
    c.set("relayLogEvent", "ingest_hook_rotated");
    return c.json(hookView(base, next, shownSecret(next, hooks.secrets)), 200, { "cache-control": "no-store" });
  });

  app.delete(`${INGEST_HOOKS_PATH}/:id`, requireKey(["write"]), async (c) => {
    const hook = await ownHook(c);
    if (hook instanceof Response) return hook;
    try {
      await hooks.store.remove(hook.hookId, hook.principalId, now().getTime());
    } catch {
      return storeDown(c);
    }
    c.set("relayLogEvent", "ingest_hook_deleted");
    return c.body(null, 204, { "cache-control": "no-store" });
  });
}

/** The client IP as the edge reports it (the same rule as agent-app.ts `clientIp`). */
function clientIpOf(c: Context): string {
  const cf = c.req.header("cf-connecting-ip")?.trim();
  if (cf) return cf.slice(0, 64);
  const xff = c.req.header("x-forwarded-for")?.split(",")[0]?.trim();
  if (xff) return xff.slice(0, 64);
  return c.req.header("x-real-ip")?.trim().slice(0, 64) || "unknown";
}
