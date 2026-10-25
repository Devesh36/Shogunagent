/**
 * x402 acceptance behind `X402_ENABLED` (E30; decision: docs/decisions/0002-x402.md — not
 * enabled in production; contract: docs/api/402.md "The x402 rail").
 *
 * Off (the default), nothing here runs and a 402 lists the card rail only. On:
 *
 * - A `payment_required` 402 appends an `x402` entry to `payment.accepts`: scheme `exact`, the
 *   configured network, asset USDC, `payTo`, and the amount in atomic units. USDC has 6 decimals,
 *   so one atomic unit is one micro-USD and the amount is the quote's `amount_micro_usd` exactly.
 * - A metered request carrying `X-PAYMENT` (or the v2 name `PAYMENT-SIGNATURE`) is checked here:
 *   decoded, refused if the same payload was presented before (replay), verified through the
 *   facilitator (`FacilitatorClient.verify`), and then the call proceeds funded by the payment
 *   (`funding: "x402"` on the receipt; the wallet is not touched, the hard caps still apply).
 * - After a 2xx from the upstream the payment is settled through the facilitator and the
 *   response carries `X-PAYMENT-RESPONSE` (base64 JSON: success, transaction, network, payer).
 *   A failed call is never settled, so it costs nothing — the same rule as the wallet.
 *
 * The payment payload is a signed authorization, not user content, but it is still never logged
 * or stored: the replay guard keeps its SHA-256 only.
 */
import { createHash } from "node:crypto";

import type { SqlClient } from "../sql.js";

export const X402_RAIL = "x402";
/** USDC's decimals: 1 atomic unit = 1e-6 USDC = 1 micro-USD. */
export const USDC_DECIMALS = 6;
/** A payload's replay record outlives any authorization window we accept. */
export const X402_REPLAY_KEEP_MS = 7 * 86_400_000;
export const MAX_PAYMENT_HEADER_BYTES = 8 * 1024;

export interface X402Config {
  /** CAIP-2 or x402 network name, e.g. `eip155:8453` / `base`. */
  network: string;
  /** The receiving address. */
  payTo: string;
  /** The USDC contract address on `network`. */
  asset: string;
  /** The facilitator's base URL (`/verify`, `/settle`). */
  facilitatorUrl: string;
  maxTimeoutSeconds: number;
}

/** The `accepts` entry (x402 PaymentRequirements; v2 field `amount` and v1 `maxAmountRequired`). */
export interface X402Requirement {
  rail: typeof X402_RAIL;
  scheme: "exact";
  network: string;
  asset: string;
  payTo: string;
  /** Atomic units of USDC, as a decimal string (x402 v2). */
  amount: string;
  /** The same figure under x402 v1's name. */
  maxAmountRequired: string;
  maxTimeoutSeconds: number;
  resource: string;
  description: string;
  mimeType: "application/json";
  extra: { name: "USDC"; version: "2"; amount_micro_usd: number };
}

const EVM_ADDRESS_RE = /^0x[0-9a-fA-F]{40}$/;
const NETWORK_RE = /^[a-z0-9-]{2,32}(:[A-Za-z0-9-]{1,64})?$/;

type Env = Readonly<Record<string, string | undefined>>;

/** Parse the x402 settings. `undefined` unless `X402_ENABLED=1`; then every value is required.
 * Throws an Error whose message names the variable, never a value. */
export function x402ConfigFromEnv(env: Env): X402Config | undefined {
  if (env.X402_ENABLED?.trim() !== "1") return undefined;
  const network = env.X402_NETWORK?.trim();
  if (!network || !NETWORK_RE.test(network)) throw new Error("invalid X402_NETWORK (e.g. eip155:8453 or base; X402_ENABLED=1)");
  const payTo = env.X402_PAY_TO?.trim();
  if (!payTo || !EVM_ADDRESS_RE.test(payTo)) throw new Error("invalid X402_PAY_TO (a 0x address; X402_ENABLED=1)");
  const asset = env.X402_ASSET?.trim();
  if (!asset || !EVM_ADDRESS_RE.test(asset)) throw new Error("invalid X402_ASSET (the USDC contract address on X402_NETWORK; X402_ENABLED=1)");
  const facilitatorUrl = env.X402_FACILITATOR_URL?.trim();
  if (!facilitatorUrl || !/^https:\/\/[^\s]+$/.test(facilitatorUrl)) throw new Error("invalid X402_FACILITATOR_URL (an https URL; X402_ENABLED=1)");
  const rawTimeout = env.X402_MAX_TIMEOUT_SECONDS?.trim();
  const maxTimeoutSeconds = rawTimeout ? Number(rawTimeout) : 60;
  if (!Number.isSafeInteger(maxTimeoutSeconds) || maxTimeoutSeconds < 10 || maxTimeoutSeconds > 600) throw new Error("invalid X402_MAX_TIMEOUT_SECONDS (10 to 600)");
  return { network, payTo, asset, facilitatorUrl: facilitatorUrl.replace(/\/+$/, ""), maxTimeoutSeconds };
}

/** The requirement a call priced at `amountMicros` asks for. */
export function x402Requirement(cfg: X402Config, amountMicros: number, resource: string): X402Requirement {
  const atomic = String(Math.max(0, Math.round(amountMicros)));
  return {
    rail: X402_RAIL, scheme: "exact", network: cfg.network, asset: cfg.asset, payTo: cfg.payTo,
    amount: atomic, maxAmountRequired: atomic, maxTimeoutSeconds: cfg.maxTimeoutSeconds, resource,
    description: "One metered ShogunAgent call, at the quoted price. Settled only if the call succeeds.",
    mimeType: "application/json",
    extra: { name: "USDC", version: "2", amount_micro_usd: Math.max(0, Math.round(amountMicros)) },
  };
}

/** A decoded `X-PAYMENT` header: x402's PaymentPayload. Only the routing fields are read here; the
 * signed `payload` goes to the facilitator untouched. */
export interface X402PaymentPayload {
  x402Version: number;
  scheme: string;
  network: string;
  payload: Record<string, unknown>;
  [k: string]: unknown;
}

export function decodePaymentHeader(raw: string | undefined): { ok: true; payload: X402PaymentPayload; hash: string } | { ok: false; reason: string } {
  if (!raw) return { ok: false, reason: "missing" };
  const text = raw.trim();
  if (text.length === 0 || text.length > MAX_PAYMENT_HEADER_BYTES || !/^[A-Za-z0-9+/_=-]+$/.test(text)) return { ok: false, reason: "not base64" };
  let parsed: unknown;
  try {
    parsed = JSON.parse(Buffer.from(text, text.includes("-") || text.includes("_") ? "base64url" : "base64").toString("utf8"));
  } catch {
    return { ok: false, reason: "not JSON" };
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return { ok: false, reason: "not an object" };
  const p = parsed as Record<string, unknown>;
  // v2 nests the scheme and network under `accepted`; v1 has them at the top level.
  const accepted = typeof p.accepted === "object" && p.accepted !== null ? (p.accepted as Record<string, unknown>) : {};
  const scheme = typeof p.scheme === "string" ? p.scheme : accepted.scheme;
  const network = typeof p.network === "string" ? p.network : accepted.network;
  if (typeof p.x402Version !== "number" || typeof scheme !== "string" || typeof network !== "string") return { ok: false, reason: "missing x402Version, scheme or network" };
  if (typeof p.payload !== "object" || p.payload === null || Array.isArray(p.payload)) return { ok: false, reason: "missing payload" };
  // The replay key is the signed content, not the encoding: the same authorization re-encoded is
  // the same payment.
  const hash = createHash("sha256").update(JSON.stringify(p.payload)).digest("hex");
  return { ok: true, payload: { ...p, x402Version: p.x402Version, scheme, network, payload: p.payload as Record<string, unknown> }, hash };
}

export interface VerifyResult { isValid: boolean; invalidReason?: string; payer?: string }
export interface SettleResult { success: boolean; transaction?: string; network?: string; payer?: string; errorReason?: string }

/** The facilitator seam: verify a payment against the requirement, settle it on-chain. */
export interface FacilitatorClient {
  verify(payment: X402PaymentPayload, requirement: X402Requirement): Promise<VerifyResult>;
  settle(payment: X402PaymentPayload, requirement: X402Requirement): Promise<SettleResult>;
}

export type FacilitatorFetch = (url: string, init: { method: "POST"; headers: Record<string, string>; body: string }) => Promise<{ status: number; json(): Promise<unknown> }>;

/** The x402 requirement as the facilitator expects it (without our `rail` and `extra.amount_micro_usd`). */
function wireRequirement(r: X402Requirement): Record<string, unknown> {
  const { rail: _rail, extra, ...rest } = r;
  return { ...rest, extra: { name: extra.name, version: extra.version } };
}

/** The HTTP facilitator (`X402_FACILITATOR_URL`): `POST /verify` and `POST /settle`. */
export function httpFacilitator(baseUrl: string, fetchFn?: FacilitatorFetch): FacilitatorClient {
  const doFetch = fetchFn ?? (globalThis.fetch as unknown as FacilitatorFetch);
  const post = async (path: string, payment: X402PaymentPayload, requirement: X402Requirement): Promise<Record<string, unknown>> => {
    const res = await doFetch(`${baseUrl}${path}`, {
      method: "POST",
      headers: { "content-type": "application/json", accept: "application/json" },
      body: JSON.stringify({ x402Version: payment.x402Version, paymentPayload: payment, paymentRequirements: wireRequirement(requirement) }),
    });
    const body = (await res.json().catch(() => ({}))) as unknown;
    if (res.status >= 500) throw new Error("facilitator unavailable");
    return typeof body === "object" && body !== null ? (body as Record<string, unknown>) : {};
  };
  const s = (v: unknown): string | undefined => (typeof v === "string" && v.length > 0 && v.length <= 200 ? v : undefined);
  return {
    async verify(payment, requirement) {
      const b = await post("/verify", payment, requirement);
      return { isValid: b.isValid === true, ...(s(b.invalidReason) ? { invalidReason: s(b.invalidReason) as string } : {}), ...(s(b.payer) ? { payer: s(b.payer) as string } : {}) };
    },
    async settle(payment, requirement) {
      const b = await post("/settle", payment, requirement);
      return {
        success: b.success === true,
        ...(s(b.transaction) ? { transaction: s(b.transaction) as string } : {}),
        ...(s(b.network) ? { network: s(b.network) as string } : {}),
        ...(s(b.payer) ? { payer: s(b.payer) as string } : {}),
        ...(s(b.errorReason) ? { errorReason: s(b.errorReason) as string } : {}),
      };
    },
  };
}

/** The `X-PAYMENT-RESPONSE` header value: base64 of the settlement result. */
export function paymentResponseHeader(r: SettleResult): string {
  return Buffer.from(JSON.stringify({ success: r.success, transaction: r.transaction ?? null, network: r.network ?? null, payer: r.payer ?? null, ...(r.errorReason ? { errorReason: r.errorReason } : {}) }), "utf8").toString("base64");
}

/**
 * What became of a presented payment: `claimed` (in use), `settled`, `failed` (rejected by the
 * facilitator, or the settlement failed), `unsettled` (the call failed; nothing was settled — the
 * payload is still spent as far as this gateway is concerned), or `retryable` (nothing happened:
 * the facilitator or the store was unavailable, or a cap refused before the call ran — the same
 * payload may be presented again).
 */
export type X402PaymentState = "settled" | "failed" | "unsettled" | "retryable";

/** The replay guard: one use per payment payload (by SHA-256), ever within the keep window. */
export interface X402Store {
  /** Claim a payload for a receipt. False when it was presented before (a replay), unless it was
   * left `retryable`. */
  claim(payloadHash: string, receiptId: string, nowMs: number): Promise<boolean>;
  mark(payloadHash: string, state: X402PaymentState, nowMs: number): Promise<void>;
  maintenance(nowMs: number): Promise<void>;
}

export class InMemoryX402Store implements X402Store {
  readonly rows = new Map<string, { receiptId: string; state: string; at: number }>();
  claim(payloadHash: string, receiptId: string, nowMs: number): Promise<boolean> {
    const existing = this.rows.get(payloadHash);
    if (existing && existing.state !== "retryable") return Promise.resolve(false);
    this.rows.set(payloadHash, { receiptId, state: "claimed", at: nowMs });
    return Promise.resolve(true);
  }
  mark(payloadHash: string, state: X402PaymentState, nowMs: number): Promise<void> {
    const r = this.rows.get(payloadHash);
    if (r) { r.state = state; r.at = nowMs; }
    return Promise.resolve();
  }
  maintenance(nowMs: number): Promise<void> {
    for (const [k, r] of this.rows) if (r.at < nowMs - X402_REPLAY_KEEP_MS) this.rows.delete(k);
    return Promise.resolve();
  }
}

/** The replay guard over Postgres (sql/x402-payments.sql): the payload hash is the primary key,
 * so the first insert wins and every later one is a replay — unless the row was left
 * `retryable`, which the conditional upsert reclaims in the same statement. */
export class SqlX402Store implements X402Store {
  constructor(private readonly db: SqlClient) {}

  async checkSchema(): Promise<void> {
    await this.db.query("SELECT 1 FROM x402_payments LIMIT 1");
  }

  async claim(payloadHash: string, receiptId: string, nowMs: number): Promise<boolean> {
    const rows = await this.db.query(`INSERT INTO x402_payments(payload_hash,receipt_id,state,created_at,updated_at)
      VALUES($1::text,$2::text,'claimed',$3::bigint,$3::bigint)
      ON CONFLICT(payload_hash) DO UPDATE SET receipt_id=excluded.receipt_id,state='claimed',updated_at=excluded.updated_at
      WHERE x402_payments.state='retryable'
      RETURNING payload_hash`, [payloadHash, receiptId, nowMs]);
    return rows.length > 0;
  }

  async mark(payloadHash: string, state: X402PaymentState, nowMs: number): Promise<void> {
    await this.db.query("UPDATE x402_payments SET state=$2::text,updated_at=$3::bigint WHERE payload_hash=$1::text", [payloadHash, state, nowMs]);
  }

  async maintenance(nowMs: number): Promise<void> {
    await this.db.query("DELETE FROM x402_payments WHERE updated_at<$1::bigint", [nowMs - X402_REPLAY_KEEP_MS]);
  }
}
