/**
 * Response shapes shared by the three verbs (implementation-plan §7.2 / §7.3). Every response is
 * self-describing: `usage`, `latency_ms`, `hints`, and on error `next_action` saying what to do.
 */
import type { Hint } from "./hints.js";
import type { PaymentRequired } from "./metering.js";
import type { Usage } from "./usage.js";

export type NextAction =
  | "top_up"
  | "approve_at_url"
  | "provide_input"
  | "retry_after"
  | "fix_input"
  | "broaden_query"
  | "discover"
  | "authenticate"
  | "try_alternative"
  | null;

export type ErrorCode =
  | "invalid_request"
  | "tool_not_found"
  | "operation_denied"
  | "invalid_input"
  | "idempotency_conflict"
  | "authentication_required"
  | "payment_required"
  | "provider_unavailable"
  | "upstream_error";

export interface ErrorBody {
  resultType: "error";
  error: { code: ErrorCode; message: string; next_action: NextAction };
  usage: Usage;
  latency_ms: number;
  hints: Hint[];
  payment?: PaymentRequired;
  details?: string[];
}

export interface Envelope<T> {
  result: T;
  usage: Usage;
  latency_ms: number;
  hints: Hint[];
  next_action: NextAction;
}

export const FREE_USAGE: Usage = { unit: "call", quantity: 1, cost_micro_usd: 0 };

export function errorBody(code: ErrorCode, message: string, next_action: NextAction, latency_ms: number, extra: Partial<Pick<ErrorBody, "hints" | "payment" | "details">> = {}): ErrorBody {
  const body: ErrorBody = {
    resultType: "error",
    error: { code, message, next_action },
    usage: { unit: "call", quantity: 0, cost_micro_usd: 0 },
    latency_ms,
    hints: extra.hints ?? [],
  };
  if (extra.payment) body.payment = extra.payment;
  if (extra.details) body.details = extra.details;
  return body;
}

export const ERROR_STATUS: Record<ErrorCode, number> = {
  invalid_request: 400,
  tool_not_found: 404,
  operation_denied: 403,
  invalid_input: 400,
  idempotency_conflict: 409,
  authentication_required: 401,
  payment_required: 402,
  provider_unavailable: 503,
  upstream_error: 502,
};
