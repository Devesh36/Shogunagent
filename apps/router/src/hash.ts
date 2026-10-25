/**
 * Canonical JSON and content hashing for the catalog (E8-T2) and for idempotency input hashes.
 * `node:crypto` is available on Workers under `nodejs_compat`, as apps/api already relies on.
 */
import { createHash } from "node:crypto";
import type { Json } from "../connectors/types.js";

/** JSON with object keys sorted at every level, so equal content hashes equal. */
export function canonicalJson(value: Json): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  const keys = Object.keys(value).sort();
  return `{${keys.map((k) => `${JSON.stringify(k)}:${canonicalJson(value[k] as Json)}`).join(",")}}`;
}

export function sha256Hex(text: string): string {
  return createHash("sha256").update(text).digest("hex");
}

export function contentHash(value: Json): string {
  return sha256Hex(canonicalJson(value));
}

/** Twelve hex characters of a SHA-256: enough to correlate, not an identifier anyone can reverse. */
export function shortHash(text: string): string {
  return sha256Hex(text).slice(0, 12);
}
