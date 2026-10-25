/**
 * Typed errors. Branch on the class (or `code`), then on `nextAction` — never parse `message`
 * (docs/api/402.md "Client guidance").
 */
import type { NextAction, PaymentPayload } from "./types.js";

export interface ShogunAgentErrorInit {
  status: number;
  code: string;
  message: string;
  nextAction?: NextAction | null;
  retryAfter?: number | null;
  body?: unknown;
}

/** Every error this SDK throws for an answer the server gave. */
export class ShogunAgentError extends Error {
  /** HTTP status (0 for a transport failure, 200 for a tool-level error inside a 200). */
  readonly status: number;
  /** The machine code: `payment_required`, `insufficient_scope`, `rate_limited`, … */
  readonly code: string;
  /** What to do next: `top_up`, `approve_at_url`, `retry_after`, `narrow_query`, … */
  readonly nextAction: NextAction | null;
  /** Seconds to wait before retrying, when the server said (`Retry-After` or the body). */
  readonly retryAfter: number | null;
  /** The parsed body, for anything the typed fields do not carry. */
  readonly body: unknown;

  constructor(init: ShogunAgentErrorInit) {
    super(init.message);
    this.name = new.target.name;
    this.status = init.status;
    this.code = init.code;
    this.nextAction = init.nextAction ?? null;
    this.retryAfter = init.retryAfter ?? null;
    this.body = init.body;
  }
}

/** 401: no key, a bad key, or a revoked one. `nextAction` is usually `authenticate`. */
export class AuthenticationError extends ShogunAgentError {}

/**
 * 402: the call cannot proceed for a money reason. `code` is `payment_required` (top up:
 * `payment.accepts[0].url`, then retry the same call unchanged) or `cap_exceeded` (wait
 * `payment.retry_after` seconds or send less; money does not help).
 */
export class PaymentRequiredError extends ShogunAgentError {
  /** The 402 body's `payment` block. */
  readonly payment: PaymentPayload | null;
  /** The decoded `PAYMENT-REQUIRED` header (x402-style), when present. */
  readonly paymentRequiredHeader: Record<string, unknown> | null;

  constructor(init: ShogunAgentErrorInit & { payment: PaymentPayload | null; paymentRequiredHeader: Record<string, unknown> | null }) {
    super(init);
    this.payment = init.payment;
    this.paymentRequiredHeader = init.paymentRequiredHeader;
  }

  /** Where a human tops up (the first accepted rail), or null for `cap_exceeded`. */
  get topUpUrl(): string | null {
    return this.payment?.accepts[0]?.url ?? null;
  }

  /** How many micro-USD are missing (`amount − balance`), when the server said both. */
  get shortfallMicroUsd(): number | null {
    const p = this.payment;
    if (!p || typeof p.balance_micro_usd !== "number") return null;
    return Math.max(0, p.amount_micro_usd - p.balance_micro_usd);
  }
}

/** 403: the key lacks the scope (`insufficient_scope`), or the action is not allowed here. */
export class PermissionDeniedError extends ShogunAgentError {}

/** 429: too many requests. Wait `retryAfter` seconds; do not spin. */
export class RateLimitError extends ShogunAgentError {}

/** 5xx: the gateway, the core or a provider is unavailable. Nothing was charged. */
export class ServiceUnavailableError extends ShogunAgentError {}

/** A tool answered `isError` (or a JSON-RPC error): the call reached the core and was refused. */
export class ToolError extends ShogunAgentError {}

/** The request never got an HTTP answer (DNS, connection, timeout). */
export class TransportError extends ShogunAgentError {}

/** A handoff that is malformed, for someone else, or expired (`acceptHandoff`). */
export class HandoffError extends ShogunAgentError {}

function toNumber(value: unknown): number | null {
  if (typeof value === "number" && Number.isFinite(value)) return value;
  if (typeof value === "string" && /^\d+(\.\d+)?$/.test(value.trim())) return Number(value.trim());
  return null;
}

function decodeBase64Url(text: string): string | null {
  try {
    const normalized = text.replace(/-/g, "+").replace(/_/g, "/");
    const padded = normalized + "=".repeat((4 - (normalized.length % 4)) % 4);
    const binary = atob(padded);
    const bytes = Uint8Array.from(binary, (c) => c.charCodeAt(0));
    return new TextDecoder().decode(bytes);
  } catch {
    return null;
  }
}

/** Decode the x402-style `PAYMENT-REQUIRED` header. */
export function decodePaymentRequiredHeader(value: string | null): Record<string, unknown> | null {
  if (!value) return null;
  const text = decodeBase64Url(value.trim());
  if (text === null) return null;
  try {
    const parsed: unknown = JSON.parse(text);
    return typeof parsed === "object" && parsed !== null && !Array.isArray(parsed) ? (parsed as Record<string, unknown>) : null;
  } catch {
    return null;
  }
}

/**
 * Build the typed error for a non-2xx answer. Understands the gateway's
 * `{ error: { code, message, next_action }, payment? }` (§7.2 / docs/api/402.md) and the local
 * sidecar's `{ "error": "<code>", "message"?: … }`.
 */
export function errorFromResponse(status: number, body: unknown, headers: { get(name: string): string | null }): ShogunAgentError {
  const obj = (typeof body === "object" && body !== null ? body : {}) as Record<string, unknown>;
  let code = `http_${status}`;
  let message = `The server answered ${status}.`;
  let nextAction: string | null = null;
  const err = obj.error;
  if (typeof err === "object" && err !== null) {
    const e = err as Record<string, unknown>;
    if (typeof e.code === "string") code = e.code;
    if (typeof e.message === "string") message = e.message;
    if (typeof e.next_action === "string") nextAction = e.next_action;
  } else if (typeof err === "string") {
    code = err;
    if (typeof obj.message === "string") message = obj.message;
    else if (typeof obj.detail === "string") message = obj.detail;
  }
  if (nextAction === null && typeof obj.next_action === "string") nextAction = obj.next_action;
  const payment = (typeof obj.payment === "object" && obj.payment !== null ? obj.payment : null) as PaymentPayload | null;
  const retryAfter = toNumber(headers.get("retry-after")) ?? (payment ? toNumber(payment.retry_after) : null);
  const init: ShogunAgentErrorInit = { status, code, message, nextAction, retryAfter, body };
  switch (status) {
    case 401:
      return new AuthenticationError({ ...init, nextAction: nextAction ?? "authenticate" });
    case 402:
      return new PaymentRequiredError({
        ...init,
        nextAction: nextAction ?? (code === "cap_exceeded" ? "retry_after" : "top_up"),
        payment,
        paymentRequiredHeader: decodePaymentRequiredHeader(headers.get("payment-required")),
      });
    case 403:
      return new PermissionDeniedError({ ...init, nextAction: nextAction ?? "not_allowed" });
    case 429:
      return new RateLimitError({ ...init, nextAction: nextAction ?? "retry_after" });
    default:
      if (status >= 500) return new ServiceUnavailableError({ ...init, nextAction: nextAction ?? "retry_after" });
      return new ShogunAgentError(init);
  }
}
