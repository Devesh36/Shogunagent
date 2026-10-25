/**
 * `/.well-known/agent-card.json` (E10-T3): the A2A Agent Card, v1.0 shape — identity, provider,
 * version, capabilities, skills, security schemes — signed with an Ed25519 key when
 * `AGENT_CARD_SIGNING_KEY` is set (A2A v1.0: "signed Agent Cards for cryptographic identity";
 * research note, A2A section).
 *
 * Generated, not written (invariant 12): the skills come from SKILL.md's "When to use this skill"
 * list through `scripts/gen-agent-skills.mjs` (`agent-skills.generated.json`, checked in CI), the
 * identity from `identity.ts`, the URLs from the deployment.
 *
 * Signature: a detached JWS (RFC 7515) in `signatures[]`, as the A2A spec describes: the payload
 * is the card without `signatures`, canonicalised with the JSON Canonicalization Scheme (RFC 8785)
 * and base64url-encoded; `protected` is the base64url JOSE header (`alg: EdDSA`, `kid`, and the
 * public `jwk` so a reader can verify without a second fetch). `signature` is base64url of the
 * Ed25519 signature over `<protected>.<payload>`. Unsigned when no key is configured: the card
 * then carries no `signatures` field at all rather than an empty one.
 *
 * A2A task endpoints (JSON-RPC `message/send` and friends) are Phase 3 (plan §6); until then
 * `url` names the MCP endpoint and `preferredTransport` says so, so an A2A client knows what it
 * will find there.
 *
 * Runtime-neutral: `node:crypto`'s `sign` / `verify` / `createPrivateKey` / `createPublicKey`
 * with Ed25519 keys, which the Workers runtime provides under `nodejs_compat`.
 */
import { createHash, createPrivateKey, createPublicKey, sign, verify, type KeyObject } from "node:crypto";

import { HANDOFF_FORMAT, HANDOFF_PATH, HANDOFF_SCHEMA_URL } from "../handoff.js";
import generatedSkills from "./agent-skills.generated.json" with { type: "json" };
import { DISPLAY_NAME, DOCS_URL, LONG_DESCRIPTION, PROVIDER, VERSION, mcpUrl, origin, wellknownLinks, type WellknownContext } from "./identity.js";

export const A2A_PROTOCOL_VERSION = "1.0";

export interface AgentSkill {
  id: string;
  name: string;
  description: string;
  tags: string[];
  examples?: string[];
  inputModes?: string[];
  outputModes?: string[];
}

export interface GeneratedSkills {
  source: string;
  skill_name: string;
  skill_version: string;
  skills: AgentSkill[];
  exclusions: string[];
}

export const AGENT_SKILLS: GeneratedSkills = generatedSkills as GeneratedSkills;

export interface AgentCardSignature {
  protected: string;
  signature: string;
}

/** The unsigned card. Field names follow the A2A v1.0 AgentCard object. */
export function agentCard(ctx: WellknownContext, skills: GeneratedSkills = AGENT_SKILLS): Record<string, unknown> {
  const links = wellknownLinks(ctx);
  return {
    protocolVersion: A2A_PROTOCOL_VERSION,
    name: DISPLAY_NAME,
    description: LONG_DESCRIPTION,
    url: mcpUrl(ctx),
    preferredTransport: "JSONRPC",
    additionalInterfaces: [
      { url: mcpUrl(ctx), transport: "JSONRPC", "x-protocol": "mcp", "x-protocolVersion": "2026-07-28" },
      { url: `${origin(ctx)}/openapi.json`, transport: "HTTP+JSON", "x-protocol": "rest" },
    ],
    provider: { organization: PROVIDER.organization, url: PROVIDER.url },
    version: VERSION,
    documentationUrl: DOCS_URL,
    capabilities: { streaming: true, pushNotifications: false, stateTransitionHistory: false, extensions: [] },
    securitySchemes: {
      agentKey: {
        type: "http", scheme: "bearer", bearerFormat: "sa_live_<key>",
        description: `Self-registration at POST ${links.register} (proof-of-work, no email). Scopes: read, write, run.`,
      },
    },
    // Two acceptable requirement sets: a key, or nothing (discover, inspect and narrow reads).
    security: [{ agentKey: ["read", "write", "run"] }, {}],
    defaultInputModes: ["application/json", "text/plain"],
    defaultOutputModes: ["application/json"],
    skills: skills.skills.map((s) => ({ id: s.id, name: s.name, description: s.description, tags: s.tags, inputModes: ["application/json"], outputModes: ["application/json"] })),
    supportsAuthenticatedExtendedCard: false,
    "x-shogunagent": {
      skill: { name: skills.skill_name, version: skills.skill_version, url: links.skill, source: skills.source },
      not_for: skills.exclusions,
      pricing: links.pricing,
      status: links.status,
      server_card: links.server_card,
      // E16: where and in what shape to hand this principal's state of work to another agent (the
      // `handoff` skill above, derived from SKILL.md, says when).
      handoff: {
        url: `${origin(ctx)}${HANDOFF_PATH}`, method: "POST", scope: "read", format: HANDOFF_FORMAT, schema: HANDOFF_SCHEMA_URL,
        a2a_part: { kind: "data", metadata: { schema: HANDOFF_SCHEMA_URL } },
        accept: "Ingest a received handoff into your own agent_scratch namespace (SDK acceptHandoff / accept_handoff), never the principal's world.",
      },
    },
  };
}

// --- RFC 8785 (JCS) canonicalisation ---

/** Canonical JSON: members sorted by UTF-16 code units, no whitespace, JSON number and string
 * serialisation as ECMAScript defines them (which is what RFC 8785 specifies). */
export function canonicalJson(value: unknown): string {
  if (value === null || typeof value === "boolean") return JSON.stringify(value);
  if (typeof value === "number") {
    if (!Number.isFinite(value)) throw new Error("canonical JSON: non-finite number");
    return JSON.stringify(value);
  }
  if (typeof value === "string") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map((v) => canonicalJson(v === undefined ? null : v)).join(",")}]`;
  if (typeof value === "object") {
    const obj = value as Record<string, unknown>;
    const keys = Object.keys(obj).filter((k) => obj[k] !== undefined).sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
    return `{${keys.map((k) => `${JSON.stringify(k)}:${canonicalJson(obj[k])}`).join(",")}}`;
  }
  throw new Error(`canonical JSON: unsupported value of type ${typeof value}`);
}

const b64url = (b: Uint8Array | string): string => Buffer.from(b).toString("base64url");

// --- keys ---

/** PKCS#8 prefix of an Ed25519 private key (RFC 8410): the 32-byte seed follows. */
const PKCS8_ED25519_PREFIX = Buffer.from("302e020100300506032b657004220420", "hex");

/**
 * Parse `AGENT_CARD_SIGNING_KEY`: a PEM PKCS#8 block, or the 32-byte seed as hex, base64 or
 * base64url (48 bytes of PKCS#8 DER are accepted too). Returns `undefined` for an empty value;
 * throws on anything that is not an Ed25519 private key — a misconfigured key must stop boot, not
 * silently publish an unsigned card.
 */
export function parseSigningKey(raw: string | undefined): KeyObject | undefined {
  const text = raw?.trim();
  if (!text) return undefined;
  if (text.startsWith("-----BEGIN")) return assertEd25519(createPrivateKey({ key: text, format: "pem" }));
  let bytes: Buffer;
  if (/^[0-9a-fA-F]{64}$/.test(text)) bytes = Buffer.from(text, "hex");
  else bytes = Buffer.from(text.replace(/-/g, "+").replace(/_/g, "/"), "base64");
  if (bytes.length === 32) bytes = Buffer.concat([PKCS8_ED25519_PREFIX, bytes]);
  if (bytes.length !== 48) throw new Error("AGENT_CARD_SIGNING_KEY must be an Ed25519 private key: PKCS#8 PEM, or the 32-byte seed as hex/base64");
  return assertEd25519(createPrivateKey({ key: bytes, format: "der", type: "pkcs8" }));
}

function assertEd25519(key: KeyObject): KeyObject {
  if (key.asymmetricKeyType !== "ed25519") throw new Error(`AGENT_CARD_SIGNING_KEY must be Ed25519, not ${key.asymmetricKeyType ?? "unknown"}`);
  return key;
}

type PublicJwk = { kty: "OKP"; crv: "Ed25519"; x: string };

function publicJwkOf(privateKey: KeyObject): PublicJwk {
  const jwk = createPublicKey(privateKey).export({ format: "jwk" }) as { kty?: string; crv?: string; x?: string };
  if (jwk.kty !== "OKP" || jwk.crv !== "Ed25519" || typeof jwk.x !== "string") throw new Error("not an Ed25519 key");
  return { kty: "OKP", crv: "Ed25519", x: jwk.x };
}

/** A stable key id: the first 16 hex characters of SHA-256 over the raw public key. */
export function keyIdOf(jwk: PublicJwk): string {
  return createHash("sha256").update(Buffer.from(jwk.x, "base64url")).digest("hex").slice(0, 16);
}

/** The card with one detached JWS over its JCS form. The input is not mutated. */
export function signAgentCard(card: Record<string, unknown>, privateKey: KeyObject): Record<string, unknown> {
  const { signatures: _drop, ...unsigned } = card;
  void _drop;
  const jwk = publicJwkOf(privateKey);
  const header = { alg: "EdDSA", kid: keyIdOf(jwk), jwk };
  const protectedB64 = b64url(JSON.stringify(header));
  const payloadB64 = b64url(canonicalJson(unsigned));
  const signature = sign(null, Buffer.from(`${protectedB64}.${payloadB64}`), privateKey);
  return { ...unsigned, signatures: [{ protected: protectedB64, signature: b64url(signature) }] };
}

export type AgentCardVerification =
  | { ok: true; kid: string; jwk: PublicJwk }
  | { ok: false; reason: "unsigned" | "malformed" | "unsupported_alg" | "bad_signature" };

/** Verify the first signature with the public key it carries (or `publicKey`, when given). */
export function verifyAgentCard(card: Record<string, unknown>, publicKey?: KeyObject): AgentCardVerification {
  const sigs = card.signatures;
  if (!Array.isArray(sigs) || sigs.length === 0) return { ok: false, reason: "unsigned" };
  const first = sigs[0] as Partial<AgentCardSignature> | undefined;
  if (!first || typeof first.protected !== "string" || typeof first.signature !== "string") return { ok: false, reason: "malformed" };
  let header: { alg?: string; kid?: string; jwk?: PublicJwk };
  try {
    header = JSON.parse(Buffer.from(first.protected, "base64url").toString("utf8")) as typeof header;
  } catch {
    return { ok: false, reason: "malformed" };
  }
  if (header.alg !== "EdDSA") return { ok: false, reason: "unsupported_alg" };
  const jwk = header.jwk;
  if (!jwk || jwk.kty !== "OKP" || jwk.crv !== "Ed25519" || typeof jwk.x !== "string") return { ok: false, reason: "malformed" };
  let key: KeyObject;
  try {
    key = publicKey ?? createPublicKey({ key: jwk, format: "jwk" });
  } catch {
    return { ok: false, reason: "malformed" };
  }
  const { signatures: _drop, ...unsigned } = card;
  void _drop;
  const payloadB64 = b64url(canonicalJson(unsigned));
  let valid = false;
  try {
    valid = verify(null, Buffer.from(`${first.protected}.${payloadB64}`), key, Buffer.from(first.signature, "base64url"));
  } catch {
    valid = false;
  }
  return valid ? { ok: true, kid: header.kid ?? keyIdOf(jwk), jwk } : { ok: false, reason: "bad_signature" };
}
