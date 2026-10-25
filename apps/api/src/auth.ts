/**
 * License-token verification (docs/batch-relay-design.md §4.1).
 *
 * `Authorization: Bearer <license token>` — the FR-BIL-08 signed licence token, in the SAME
 * format the licence API issues and the device caches (`apps/website/src/lib/license.ts` /
 * `crates/shogun-license`):
 *
 *     v1.<base64url(payload JSON)>.<base64url(Ed25519 signature over the payload bytes)>
 *
 * One token format across the whole billing surface, one signing key, no second auth system.
 * Verified locally against the licence API's Ed25519 public key (the raw-32-bytes base64 that
 * `scripts/gen-license-keypair.mjs` prints); no round trip. The signature covers the decoded
 * payload bytes (the JSON), not the `v1.` prefix or the base64url text. Revocation rides on the
 * token's short `exp` (26h after issue, `TOKEN_TTL_SECONDS` in the licence API; a trial token's
 * never passes the trial's end).
 *
 * **Key rotation (audit 2026-10-10 M6).** The relay trusts up to two keys: the current release
 * key (`LICENSE_PUBKEY_B64`, `ed25519` in `packages/config/license-public-key.json`) and an
 * optional next key (`LICENSE_PUBKEY_NEXT_B64`, `ed25519_next` in the same file). A token MAY name
 * its key with `kid` = the first 16 hex characters of SHA-256 over the raw 32-byte public key:
 * with `kid`, only the matching trusted key is tried; without it, current then next. A `kid` that
 * names no trusted key is a 401, never a fallback. Independently of rotation, a token's lifetime is
 * capped (`MAX_TOKEN_LIFETIME_SECONDS`, 48h against the API's 26h TTL) and its `iat` may not sit in
 * the future beyond `MAX_IAT_SKEW_SECONDS`, so a leaked signing key cannot mint a ten-year token.
 */
import { createHash, createPublicKey, verify as edVerify, type KeyObject } from "node:crypto";
import releaseKeyJson from "../../../packages/config/license-public-key.json";

/** The shipped trust roots. `ed25519_next` is present only during a rotation window. */
const releaseKey = releaseKeyJson as { ed25519: string; ed25519_next?: string };

/** Production cannot start healthy with a key the shipped app does not trust. */
export function releaseLicensePublicKeyFromB64(raw: string): KeyObject {
  const key = licensePublicKeyFromB64(raw);
  if (key.export({ format: "jwk" }).x !== Buffer.from(releaseKey.ed25519, "base64").toString("base64url")) {
    throw new Error("LICENSE_PUBKEY_B64 does not match the shipped app; restore the existing public key.");
  }
  return key;
}

/** The keys a relay trusts: the current release key and, during a rotation, the next one. */
export interface TrustedKeys {
  current: KeyObject;
  next?: KeyObject;
}

/**
 * The trust roots from the boot environment, checked against the shipped file both ways: the
 * current key must be `ed25519`; the next key must be `ed25519_next` when the file has one, and
 * must be absent when it has none. A relay that trusts a key the app does not (or vice versa)
 * fails some tokens silently, so either mismatch refuses to boot. Messages name variables only.
 */
export function releaseTrustedKeysFromEnv(currentB64: string, nextB64: string | undefined): TrustedKeys {
  const current = releaseLicensePublicKeyFromB64(currentB64);
  const shippedNext = releaseKey.ed25519_next;
  const configuredNext = nextB64?.trim();
  if (!configuredNext) {
    if (shippedNext) throw new Error("LICENSE_PUBKEY_NEXT_B64 is missing but the shipped app trusts a next key; set it.");
    return { current };
  }
  if (!shippedNext) throw new Error("LICENSE_PUBKEY_NEXT_B64 is set but the shipped app trusts no next key; unset it.");
  const next = licensePublicKeyFromB64(configuredNext);
  if (next.export({ format: "jwk" }).x !== Buffer.from(shippedNext, "base64").toString("base64url")) {
    throw new Error("LICENSE_PUBKEY_NEXT_B64 does not match the shipped app's next key.");
  }
  if (keyId(next) === keyId(current)) throw new Error("LICENSE_PUBKEY_NEXT_B64 must differ from LICENSE_PUBKEY_B64.");
  return { current, next };
}

/** The raw 32 public-key bytes of an Ed25519 `KeyObject`. */
export function rawPublicKeyBytes(key: KeyObject): Buffer {
  const x = key.export({ format: "jwk" }).x;
  if (typeof x !== "string") throw new Error("not an Ed25519 public key");
  return Buffer.from(x, "base64url");
}

/** The `kid` of a public key: the first 16 hex characters of SHA-256 over its raw 32 bytes. The
 * same derivation as the licence API's signer and the device's verifier. */
export function keyId(key: KeyObject): string {
  return createHash("sha256").update(rawPublicKeyBytes(key)).digest("hex").slice(0, KID_HEX_LENGTH);
}

export const KID_HEX_LENGTH = 16;

/** The longest `exp - iat` a token may claim. The licence API issues 26h (`TOKEN_TTL_SECONDS`);
 * anything past 48h was not issued by it. */
export const MAX_TOKEN_LIFETIME_SECONDS = 48 * 3600;

/** How far in the future `iat` may sit before the token is refused (clock skew allowance). */
export const MAX_IAT_SKEW_SECONDS = 300;

/** Plans whose licence may use the Batch lane. The Batch lane ships in Standard (§6.12.1), so
 * every paid plan qualifies. `trial` is the app's 7-day trial (issue #763): the licence API mints
 * it per device, only with status `trialing`, and never valid past the trial's end. Anything else
 * is 402. */
export const BATCH_PLANS = ["standard", "pro", "trial"] as const;

/** The plan name of the trial's token. */
export const TRIAL_PLAN = "trial";

/** The only status a `trial` token may carry. */
export const TRIAL_STATUS = "trialing";

/** Subscription statuses the lane accepts: live, in-trial, or in the payment-retry window.
 * Cancelled / unpaid tokens are refused even while their `exp` has not passed. */
export const BATCH_STATUSES = ["active", "trialing", "past_due"] as const;

export interface License {
  /** The licence id (`lic` claim) — the metering key. An opaque id, never user content. */
  licenseId: string;
  /** The anonymous device id (`device` claim) the licence API bound this token to.
   *
   * This is NOT proof the caller is that device: the token is a bearer credential, so whoever
   * holds it holds the claim too (the device caches it in plaintext `billing.json` — FR-BIL-08,
   * CLAUDE.md invariant 7's explicit exception). Real device binding needs proof-of-possession
   * and is a separate change. What the claim buys here is **blast radius**: metering and rate
   * limiting key on licence + device, so a copied token shares one budget with the Mac it was
   * issued to instead of multiplying it across every Mac it is pasted onto. */
  deviceId: string;
  plan: string;
  /** The account the token was issued through (`acc` claim, #1101 D7), trusted only after the
   * signature verified. Absent on the licence-key and device-only paths. */
  accountId?: string;
}

/**
 * The subject a licence's daily budgets (chunk cap, per-licence spend budget, speech minutes) are
 * metered on (audit 2026-10-20 H1).
 *
 * A trial token is minted per device id (`lic: trial_<device>`), and a device id is any string, so
 * metering a trial on its `lic` gives one person as many trial budgets as device ids they ask for.
 * When the trial was issued through an account (`acc`), the account is the payer and every one of
 * its devices draws from ONE budget, `account:<acc>`. A trial without `acc` (the device-only path)
 * keeps its per-device `lic`. A paid licence always keeps its licence id: it is already one
 * budget across every Mac it is used on.
 *
 * Only budgets use this. Batch ownership and idempotency stay keyed on licence + device, so a
 * batch is never readable from another device of the same account.
 */
export function meterSubject(license: License): string {
  return license.plan === TRIAL_PLAN && license.accountId !== undefined ? `account:${license.accountId}` : license.licenseId;
}

/** Whether the operator's emergency denylist (`RELAY_DENY_SUBJECTS`, audit 2026-10-20 L3) names
 * this licence: by licence id, or by its metering subject (`account:<acc>`). A token is a bearer
 * credential that stays valid until its `exp` (26h); the denylist is the only way to stop one
 * sooner, and it is meant to be short: an entry can go once every token issued before the licence
 * was revoked has expired. */
export function deniedByOperator(license: License, deny: readonly string[] | undefined): boolean {
  if (!deny || deny.length === 0) return false;
  return deny.includes(license.licenseId) || deny.includes(meterSubject(license));
}

export class AuthError extends Error {
  constructor(
    message: string,
    public readonly status: 401 | 402,
  ) {
    super(message);
    this.name = "AuthError";
  }
}

/** SPKI DER header for a raw Ed25519 public key (RFC 8410). */
const ED25519_SPKI_PREFIX = Buffer.from("302a300506032b6570032100", "hex");

/** Import the licence public key from the `SHOGUN_LICENSE_PUBKEY`-style value: base64 of the raw
 * 32-byte Ed25519 key (what `gen-license-keypair.mjs` prints). Throws on anything else — a relay
 * that cannot verify tokens must not boot. */
export function licensePublicKeyFromB64(b64: string): KeyObject {
  const raw = Buffer.from(b64.trim(), "base64");
  if (raw.length !== 32) {
    throw new Error("license public key must be base64 of 32 raw Ed25519 bytes");
  }
  return createPublicKey({
    key: Buffer.concat([ED25519_SPKI_PREFIX, raw]),
    format: "der",
    type: "spki",
  });
}

/** Canonical unpadded base64url: the alphabet, nothing else. */
const B64URL = /^[A-Za-z0-9_-]+$/;

/** Decode one token segment, or `undefined` for anything that is not the canonical unpadded
 * base64url of its bytes — padding, `+`/`/`, whitespace, illegal characters, or non-zero
 * trailing bits. Node's decoder is lenient (it skips what it does not understand), so the
 * alphabet is checked first and the bytes are re-encoded and compared, exactly the strictness of
 * `URL_SAFE_NO_PAD` on the device (`crates/shogun-license`). Never throws: every malformed
 * segment is the same 401 as a forged one. */
function b64urlDecode(part: string): Buffer | undefined {
  if (!B64URL.test(part)) return undefined;
  const buf = Buffer.from(part, "base64url");
  if (buf.length === 0) return undefined;
  // Plain comparison: both strings are the caller's own token, not a secret of ours.
  if (buf.toString("base64url") !== part) return undefined;
  return buf;
}

/** Verify the Authorization header and return the licence, or throw an AuthError (401 for a
 * missing/malformed/forged/expired token, 402 for a plan or status the Batch lane does not
 * cover). `nowSecs` is injectable for tests. `publicKey` is a single key or the `TrustedKeys`
 * pair. `exp` must be in the future; the lifetime and `iat` skew caps are in the module header.
 * There is no lower bound on `iat` (clock skew between the licence API and the relay must not
 * lock paying users out), and a token without a numeric `iat` is refused: there would be no
 * lifetime to cap. */
export function verifyLicense(
  authorization: string | undefined,
  publicKey: KeyObject | TrustedKeys,
  nowSecs: number = Math.floor(Date.now() / 1000),
): License {
  const trusted: TrustedKeys = "current" in publicKey ? publicKey : { current: publicKey };
  if (!authorization || !authorization.startsWith("Bearer ")) {
    throw new AuthError("missing bearer license token", 401);
  }
  const token = authorization.slice("Bearer ".length).trim();
  // Deliberately unspecific error strings below: the device re-verifies its licence either way,
  // and the reason must not require logging token material to diagnose here.
  const invalid = new AuthError("invalid or expired license token", 401);
  const [version, bodyB64, sigB64, ...rest] = token.split(".");
  if (version !== "v1" || !bodyB64 || !sigB64 || rest.length > 0) throw invalid;
  const body = b64urlDecode(bodyB64);
  const sig = b64urlDecode(sigB64);
  // Both segments are canonical (padding/whitespace smuggling rejected in b64urlDecode) before
  // the crypto runs.
  if (!body || !sig || sig.length !== 64) throw invalid;

  // The payload is parsed before the signature check only to read `kid`, which picks the key.
  // Nothing else from it is trusted until a signature has verified.
  let payload: Record<string, unknown>;
  try {
    const parsed: unknown = JSON.parse(body.toString("utf8"));
    if (typeof parsed !== "object" || parsed === null) throw invalid;
    payload = parsed as Record<string, unknown>;
  } catch {
    throw invalid;
  }
  const candidates = keysForKid(trusted, payload.kid);
  if (candidates.length === 0 || !candidates.some((key) => edVerify(null, body, key, sig))) throw invalid;

  if (payload.v !== 1) throw invalid;
  const exp = payload.exp;
  if (typeof exp !== "number" || exp <= nowSecs) throw invalid;
  // `iat` is required: the licence API issues it on licence and trial tokens alike and the device
  // verifier rejects a token without it, so an `iat`-less token is not a legacy shape but the one
  // mint that would dodge the lifetime ceiling (audit M6).
  const iat = payload.iat;
  if (typeof iat !== "number" || !Number.isFinite(iat)) throw invalid;
  if (exp - iat > MAX_TOKEN_LIFETIME_SECONDS) throw invalid;
  if (iat > nowSecs + MAX_IAT_SKEW_SECONDS) throw invalid;
  const licenseId = payload.lic;
  if (typeof licenseId !== "string" || licenseId.length === 0) {
    throw new AuthError("license token missing lic", 401);
  }
  const plan = payload.plan;
  if (typeof plan !== "string" || !(BATCH_PLANS as readonly string[]).includes(plan)) {
    throw new AuthError("plan does not include the Batch lane", 402);
  }
  const status = payload.status;
  if (typeof status !== "string" || !(BATCH_STATUSES as readonly string[]).includes(status)) {
    throw new AuthError("subscription status does not include the Batch lane", 402);
  }
  if (plan === TRIAL_PLAN && status !== TRIAL_STATUS) {
    throw new AuthError("trial token outside its trial", 402);
  }
  // Required, not optional: every token the licence API issues carries it (apps/website/src/lib
  // /license.ts) and the device refuses one that does not match its own id
  // (crates/shogun-license). A token without the claim is one this relay cannot meter per
  // device, so it is refused rather than metered on the licence alone.
  const deviceId = payload.device;
  if (typeof deviceId !== "string" || deviceId.length === 0) {
    throw new AuthError("license token missing device", 401);
  }
  // `acc` is optional; when present it must be an opaque id (it becomes a metering key).
  const acc = payload.acc;
  if (acc !== undefined && (typeof acc !== "string" || !/^[A-Za-z0-9_-]{1,128}$/.test(acc))) throw invalid;
  return { licenseId, deviceId, plan, ...(typeof acc === "string" ? { accountId: acc } : {}) };
}

/** Whether `authorization` carries a genuine trial token (signed by a trusted key, `plan:
 * "trial"`) whose TRIAL is over: its `exp` has passed AND its signed `period_end` (the trial's
 * end, which the licence API puts on every trial token and never sets before `exp`) has passed
 * too. `verifyLicense` refuses it as 401 like any expired token; `authorize` answers 402
 * `plan_required` for it instead (#940), so the Mac stops asking for this lane rather than
 * re-verifying a licence it does not have.
 *
 * A trial token that merely expired (`exp` past, `period_end` still ahead: the 26 h token ran out
 * mid-trial) is NOT an ended trial — it stays 401, the answer on which the Mac fetches a fresh
 * trial token and resends (issue #777). So is one without a numeric `period_end`: the relay cannot
 * tell it ended. Nothing but the signature, `v`, `plan`, `exp` and `period_end` is read, and
 * nothing is trusted unless the signature holds. */
export function isExpiredTrialToken(
  authorization: string | undefined,
  publicKey: KeyObject | TrustedKeys,
  nowSecs: number = Math.floor(Date.now() / 1000),
): boolean {
  const trusted: TrustedKeys = "current" in publicKey ? publicKey : { current: publicKey };
  if (!authorization || !authorization.startsWith("Bearer ")) return false;
  const [version, bodyB64, sigB64, ...rest] = authorization.slice("Bearer ".length).trim().split(".");
  if (version !== "v1" || !bodyB64 || !sigB64 || rest.length > 0) return false;
  const body = b64urlDecode(bodyB64);
  const sig = b64urlDecode(sigB64);
  if (!body || !sig || sig.length !== 64) return false;
  let payload: Record<string, unknown>;
  try {
    const parsed: unknown = JSON.parse(body.toString("utf8"));
    if (typeof parsed !== "object" || parsed === null) return false;
    payload = parsed as Record<string, unknown>;
  } catch {
    return false;
  }
  const candidates = keysForKid(trusted, payload.kid);
  if (candidates.length === 0 || !candidates.some((key) => edVerify(null, body, key, sig))) return false;
  return payload.v === 1 && payload.plan === TRIAL_PLAN
    && typeof payload.exp === "number" && payload.exp <= nowSecs
    && typeof payload.period_end === "number" && payload.period_end <= nowSecs;
}

/** The trusted keys a token's `kid` claim selects: exactly the matching key when `kid` is a
 * string, current then next when it is absent, and none at all for a `kid` of any other type or
 * one that names no trusted key (a forged `kid` must not fall back to "try everything"). */
function keysForKid(trusted: TrustedKeys, kid: unknown): KeyObject[] {
  const all = trusted.next ? [trusted.current, trusted.next] : [trusted.current];
  if (kid === undefined) return all;
  if (typeof kid !== "string" || kid.length !== KID_HEX_LENGTH) return [];
  return all.filter((key) => keyId(key) === kid);
}
