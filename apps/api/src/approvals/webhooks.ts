/**
 * Outbound approval webhooks (plan E15). A principal registers an HTTPS endpoint for
 * `approval.created` and/or `approval.decided`; each delivery is
 *
 *   POST <url>
 *   Content-Type: application/json
 *   X-Shogun-Event: approval.created | approval.decided
 *   X-Shogun-Delivery: whd_<uuid>
 *   X-Shogun-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256(secret, "<t>.<raw body>")>
 *
 * and the body is the redacted summary only (`ApprovalSummary`: kind, tier, recipient class, a
 * masked preview, times, status, decision) — never a request or response body (invariant 2).
 * Failed deliveries (network error, 429, 5xx) are retried with backoff; a 2xx or another 4xx ends
 * the delivery.
 *
 * The endpoint's signing secret is derived (`HMAC(WEBHOOK_SIGNING_SECRET, "webhook." + id)`),
 * returned once at registration and never stored (invariant 7): the store holds the id, the
 * principal, the URL, the events and the time.
 */
import { Buffer } from "node:buffer";
import { createHmac, randomUUID, timingSafeEqual } from "node:crypto";

import type { SqlClient } from "../sql.js";
import type { ApprovalSummary, FetchFn } from "./sidecar-client.js";

export const WEBHOOK_EVENTS = ["approval.created", "approval.decided"] as const;
export type WebhookEvent = (typeof WEBHOOK_EVENTS)[number];
export const MAX_WEBHOOKS_PER_PRINCIPAL = 10;
export const MAX_WEBHOOK_URL_LENGTH = 2048;
/** Backoff before each retry (ms): 4 attempts in all. */
export const DEFAULT_RETRY_DELAYS_MS: readonly number[] = Object.freeze([1_000, 5_000, 30_000]);
const ATTEMPT_TIMEOUT_MS = 10_000;

export interface WebhookRecord {
  webhookId: string;
  principalId: string;
  url: string;
  events: WebhookEvent[];
  createdAt: number;
}

export interface WebhookStore {
  insert(record: WebhookRecord): Promise<void>;
  listByPrincipal(principalId: string): Promise<WebhookRecord[]>;
  /** Remove the principal's own webhook. False when it is not theirs or does not exist. */
  remove(webhookId: string, principalId: string, at: number): Promise<boolean>;
}

export class InMemoryWebhookStore implements WebhookStore {
  private readonly rows = new Map<string, WebhookRecord>();
  insert(record: WebhookRecord): Promise<void> {
    this.rows.set(record.webhookId, { ...record, events: [...record.events] });
    return Promise.resolve();
  }
  listByPrincipal(principalId: string): Promise<WebhookRecord[]> {
    return Promise.resolve([...this.rows.values()].filter((r) => r.principalId === principalId).sort((a, b) => a.createdAt - b.createdAt));
  }
  remove(webhookId: string, principalId: string): Promise<boolean> {
    const row = this.rows.get(webhookId);
    if (!row || row.principalId !== principalId) return Promise.resolve(false);
    this.rows.delete(webhookId);
    return Promise.resolve(true);
  }
}

/** The webhook table (sql/approval-webhooks.sql). */
export const WEBHOOK_TABLES = ["agent_webhooks"] as const;

export class SqlWebhookStore implements WebhookStore {
  constructor(private readonly db: SqlClient) {}

  async insert(r: WebhookRecord): Promise<void> {
    await this.db.query(
      "INSERT INTO agent_webhooks (webhook_id, principal_id, url, events, created_at) VALUES ($1::text, $2::text, $3::text, $4::text, $5::bigint)",
      [r.webhookId, r.principalId, r.url, r.events.join(","), r.createdAt],
    );
  }

  async listByPrincipal(principalId: string): Promise<WebhookRecord[]> {
    const rows = await this.db.query(
      "SELECT webhook_id, principal_id, url, events, created_at FROM agent_webhooks WHERE principal_id=$1::text AND deleted_at IS NULL ORDER BY created_at",
      [principalId],
    );
    return rows.map((r) => ({
      webhookId: String(r.webhook_id), principalId: String(r.principal_id), url: String(r.url),
      events: String(r.events).split(",").filter((e): e is WebhookEvent => (WEBHOOK_EVENTS as readonly string[]).includes(e)),
      createdAt: Number(r.created_at),
    }));
  }

  async remove(webhookId: string, principalId: string, at: number): Promise<boolean> {
    const rows = await this.db.query(
      "UPDATE agent_webhooks SET deleted_at=$3::bigint WHERE webhook_id=$1::text AND principal_id=$2::text AND deleted_at IS NULL RETURNING webhook_id",
      [webhookId, principalId, at],
    );
    return rows.length > 0;
  }

  async checkSchema(): Promise<void> {
    for (const table of WEBHOOK_TABLES) await this.db.query(`SELECT 1 FROM ${table} LIMIT 1`);
  }
}

/** The endpoint's signing secret: derived, never stored. */
export function webhookSecret(masterSecret: string, webhookId: string): string {
  return createHmac("sha256", masterSecret).update(`webhook.${webhookId}`).digest("hex");
}

/** `t=<unix>,v1=<hex>` over `<t>.<body>`. */
export function signDelivery(secret: string, body: string, unixSeconds: number): string {
  const v1 = createHmac("sha256", secret).update(`${unixSeconds}.${body}`).digest("hex");
  return `t=${unixSeconds},v1=${v1}`;
}

/** Verify a delivery signature (what a receiver does; used by the tests). */
export function verifyDelivery(secret: string, body: string, header: string, nowSeconds: number, toleranceSeconds = 300): boolean {
  const parts = Object.fromEntries(header.split(",").map((kv) => kv.split("=", 2) as [string, string]));
  const t = Number(parts.t);
  if (!Number.isSafeInteger(t) || Math.abs(nowSeconds - t) > toleranceSeconds || typeof parts.v1 !== "string") return false;
  const expected = createHmac("sha256", secret).update(`${t}.${body}`).digest();
  const given = /^[0-9a-f]{64}$/.test(parts.v1) ? Buffer.from(parts.v1, "hex") : Buffer.alloc(0);
  return given.length === expected.length && timingSafeEqual(given, expected);
}

const PRIVATE_HOST = /^(localhost|.*\.localhost|.*\.local|.*\.internal|0\.0\.0\.0|127\.\d+\.\d+\.\d+|10\.\d+\.\d+\.\d+|192\.168\.\d+\.\d+|172\.(1[6-9]|2\d|3[01])\.\d+\.\d+|169\.254\.\d+\.\d+|\[?::1\]?|\[?f[cd][0-9a-f]{2}:.*)$/i;

/** Validate a registration URL: HTTPS (HTTP only when `allowHttp`), no credentials, no
 * loopback / private / link-local host. The message is the refusal's. */
export function checkWebhookUrl(raw: unknown, allowHttp = false): { ok: true; url: string } | { ok: false; message: string } {
  if (typeof raw !== "string" || raw.length === 0 || raw.length > MAX_WEBHOOK_URL_LENGTH) return { ok: false, message: `url is required, at most ${MAX_WEBHOOK_URL_LENGTH} characters.` };
  let url: URL;
  try {
    url = new URL(raw);
  } catch {
    return { ok: false, message: "url must be an absolute https:// URL." };
  }
  if (url.protocol !== "https:" && !(allowHttp && url.protocol === "http:")) return { ok: false, message: "url must use https://." };
  if (url.username || url.password) return { ok: false, message: "url must not carry credentials." };
  if (PRIVATE_HOST.test(url.hostname)) return { ok: false, message: "url must reach a public host, not a loopback or private address." };
  return { ok: true, url: url.toString() };
}

export interface DeliveryDeps {
  store: WebhookStore;
  masterSecret: string;
  fetch: FetchFn;
  now: () => Date;
  sleep?: (ms: number) => Promise<void>;
  retryDelaysMs?: readonly number[];
}

/** One delivery's outcome per endpoint (tests and the log line; never the body). */
export interface DeliveryOutcome {
  webhookId: string;
  attempts: number;
  delivered: boolean;
  lastStatus: number;
}

/** The body of an event: the redacted summary, nothing else. */
export function eventBody(event: WebhookEvent, principalId: string, summary: ApprovalSummary, deliveryId: string, at: Date): string {
  return JSON.stringify({ id: deliveryId, event, created_at: at.toISOString(), principal_id: principalId, approval: summary });
}

/** Deliver `event` to every endpoint of `principalId` that subscribed to it, with retries. */
export async function deliverEvent(deps: DeliveryDeps, principalId: string, event: WebhookEvent, summary: ApprovalSummary): Promise<DeliveryOutcome[]> {
  const sleep = deps.sleep ?? ((ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms)));
  const delays = deps.retryDelaysMs ?? DEFAULT_RETRY_DELAYS_MS;
  const hooks = (await deps.store.listByPrincipal(principalId)).filter((h) => h.events.includes(event));
  return Promise.all(hooks.map(async (hook): Promise<DeliveryOutcome> => {
    const deliveryId = `whd_${randomUUID()}`;
    const body = eventBody(event, principalId, summary, deliveryId, deps.now());
    const secret = webhookSecret(deps.masterSecret, hook.webhookId);
    let lastStatus = 0;
    for (let attempt = 0; attempt <= delays.length; attempt++) {
      if (attempt > 0) await sleep(delays[attempt - 1] ?? 0);
      const unix = Math.floor(deps.now().getTime() / 1000);
      try {
        const res = await deps.fetch(hook.url, {
          method: "POST",
          headers: {
            "content-type": "application/json",
            "x-shogun-event": event,
            "x-shogun-delivery": deliveryId,
            "x-shogun-signature": signDelivery(secret, body, unix),
          },
          body,
          redirect: "manual",
          signal: AbortSignal.timeout(ATTEMPT_TIMEOUT_MS),
        });
        lastStatus = res.status;
        await res.body?.cancel().catch(() => undefined);
        if (res.status >= 200 && res.status < 300) return { webhookId: hook.webhookId, attempts: attempt + 1, delivered: true, lastStatus };
        if (res.status !== 429 && res.status < 500) return { webhookId: hook.webhookId, attempts: attempt + 1, delivered: false, lastStatus };
      } catch {
        lastStatus = 0;
      }
    }
    return { webhookId: hook.webhookId, attempts: delays.length + 1, delivered: false, lastStatus };
  }));
}
