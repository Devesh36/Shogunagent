/**
 * Provider signature schemes for the ingest hooks (docs/implementation-plan.md E44,
 * docs/api/ingest-sources.md). Pure functions over the raw body bytes and header values: nothing
 * here parses a body, logs, or keeps anything. Every comparison is constant-time over fixed-length
 * digests (`timingSafeEqual`), and a malformed header costs the same comparison as a wrong one.
 *
 *   github           X-Hub-Signature-256: sha256=<hex HMAC-SHA256(hook secret, body)>
 *   linear           Linear-Signature: <hex HMAC-SHA256(webhook signing secret, body)>
 *   slack            X-Slack-Signature: v0=<hex HMAC-SHA256(signing secret, "v0:" ts ":" body)>,
 *                    X-Slack-Request-Timestamp within ±5 minutes
 *   notion           X-Notion-Signature: sha256=<hex HMAC-SHA256(verification_token, body)>
 *   google_calendar  X-Goog-Channel-Token equal to the hook's channel token; X-Goog-Channel-ID
 *                    equal to the hook id (no body)
 *   gmail            a Google-signed OIDC token on the Pub/Sub push (`pubsub-jwt.ts`)
 */
import { createHmac, timingSafeEqual } from "node:crypto";

export const HOOK_PROVIDERS = ["github", "linear", "slack", "notion", "google_calendar", "gmail"] as const;
export type HookProvider = (typeof HOOK_PROVIDERS)[number];

/** How far a Slack request timestamp may be from the gateway's clock. */
export const SLACK_TOLERANCE_SECONDS = 300;

const HEX64 = /^[0-9a-f]{64}$/i;
const ZERO32 = Buffer.alloc(32);

function hmac(key: string | Buffer, ...parts: Array<Uint8Array | string>): Buffer {
  const h = createHmac("sha256", key);
  for (const part of parts) h.update(part);
  return h.digest();
}

/** Compare a presented hex digest with the expected one. A malformed value is compared against a
 * zero digest so the work done does not depend on what was sent. */
function hexMatches(given: string | undefined, expected: Buffer): boolean {
  const ok = given !== undefined && HEX64.test(given);
  const presented = ok ? Buffer.from(given, "hex") : ZERO32;
  return timingSafeEqual(presented, expected) && ok;
}

/** `true` when any of `secrets` produced `given`. Every candidate is checked (no early exit). */
function anyMatches(secrets: readonly string[], digest: (secret: string) => Buffer, given: string | undefined): boolean {
  let matched = false;
  for (const secret of secrets) {
    if (hexMatches(given, digest(secret))) matched = true;
  }
  return matched;
}

function stripPrefix(header: string | undefined, prefix: string): string | undefined {
  if (header === undefined) return undefined;
  const v = header.trim();
  return v.startsWith(prefix) ? v.slice(prefix.length) : undefined;
}

export function verifyGitHub(secrets: readonly string[], body: Uint8Array, header: string | undefined): boolean {
  return anyMatches(secrets, (s) => hmac(s, body), stripPrefix(header, "sha256="));
}

export function verifyLinear(secrets: readonly string[], body: Uint8Array, header: string | undefined): boolean {
  return anyMatches(secrets, (s) => hmac(s, body), header?.trim());
}

export function verifyNotion(secrets: readonly string[], body: Uint8Array, header: string | undefined): boolean {
  return anyMatches(secrets, (s) => hmac(s, body), stripPrefix(header, "sha256="));
}

export function verifySlack(
  secrets: readonly string[],
  body: Uint8Array,
  timestamp: string | undefined,
  signature: string | undefined,
  nowSeconds: number,
  toleranceSeconds = SLACK_TOLERANCE_SECONDS,
): boolean {
  const ts = timestamp?.trim() ?? "";
  const fresh = /^\d{1,12}$/.test(ts) && Math.abs(nowSeconds - Number(ts)) <= toleranceSeconds;
  const matched = anyMatches(secrets, (s) => hmac(s, `v0:${ts}:`, body), stripPrefix(signature, "v0="));
  return matched && fresh;
}

/** A shared-token check (Google Calendar's channel token): both values are hashed under a fixed
 * key first, so the comparison is over equal-length digests whatever the lengths presented. */
export function tokenMatches(expected: readonly string[], given: string | undefined): boolean {
  const key = "shogun/ingest-hook/token-compare/v1";
  const presented = hmac(key, given ?? "");
  let matched = false;
  for (const token of expected) {
    if (timingSafeEqual(hmac(key, token), presented) && given !== undefined && given.length > 0) matched = true;
  }
  return matched;
}

/** The test-side signer of each scheme (fixtures and the docs' examples). */
export const sign = {
  github: (secret: string, body: Uint8Array | string): string => `sha256=${hmac(secret, body).toString("hex")}`,
  linear: (secret: string, body: Uint8Array | string): string => hmac(secret, body).toString("hex"),
  notion: (secret: string, body: Uint8Array | string): string => `sha256=${hmac(secret, body).toString("hex")}`,
  slack: (secret: string, body: Uint8Array | string, ts: number): string => `v0=${hmac(secret, `v0:${ts}:`, body).toString("hex")}`,
};
