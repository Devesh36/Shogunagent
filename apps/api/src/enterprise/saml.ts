/**
 * SAML 2.0 SP-initiated login for tenant admins (E26): the AuthnRequest (HTTP-Redirect binding)
 * and the validation of the Response the IdP posts back (HTTP-POST binding). The routes are in
 * `app.ts`.
 *
 * **Library.** XML signature validation uses `xml-crypto` (node-saml; 6.x) over `@xmldom/xmldom`.
 * Both are plain JavaScript on `node:crypto` (`createVerify`) and run on Cloudflare Workers under
 * `nodejs_compat`: `test/workers/enterprise.worker.test.ts` signs and verifies an assertion inside
 * workerd. So SAML is served on Workers and on Node alike; there is no Node-only flag.
 *
 * What is accepted (anything else is refused with a fixed code):
 * - A `samlp:Response` with status Success, `Destination` (when present) equal to this tenant's
 *   ACS URL and `InResponseTo` equal to the request id sealed in the flow cookie.
 * - Exactly one `saml:Assertion`, a direct child of the Response; an `EncryptedAssertion` is
 *   refused (not supported). The **assertion itself** must carry an enveloped XML signature
 *   (a signed Response around an unsigned assertion is refused) with exactly one reference, to the
 *   assertion's own `ID`, verified against one of the tenant's configured IdP certificates
 *   (`KeyInfo` in the message is ignored — a key the attacker sends is never trusted).
 * - Signature algorithms RSA-SHA256 / RSA-SHA512 and digests SHA-256 / SHA-512 only (no SHA-1,
 *   no HMAC).
 * - Every value below is read from the XML that `getSignedReferences()` returns — the bytes that
 *   were signed — never from the original document, so signature-wrapping cannot substitute an
 *   unsigned element: `Issuer` equals the IdP entity id; `Conditions` `NotBefore` /
 *   `NotOnOrAfter` hold within the clock skew; every `AudienceRestriction` names this SP's entity
 *   id; a bearer `SubjectConfirmation` whose `Recipient` is the ACS URL, `InResponseTo` the request
 *   id and `NotOnOrAfter` in the future.
 * - The assertion id is spent in the replay cache until it expires (`app.ts`).
 * - Strictness added in E39: `Version="2.0"` on the Response and the assertion; the Response's own
 *   `Issuer`, when present, equals the IdP entity id; the assertion's `IssueInstant` (signed) and
 *   the Response's are neither in the future nor older than `MAX_ASSERTION_AGE_MS` (the
 *   ten-minute sign-in flow), each with the clock skew; exactly one `Issuer`, `Subject` and
 *   `Conditions` in the signed assertion; a bearer `SubjectConfirmationData` whose `NotBefore` is
 *   still ahead does not confirm; an `EncryptedID` or `EncryptedAttribute` anywhere is refused
 *   like an `EncryptedAssertion`.
 * - XML Encryption is not supported: no XML-Enc library has been vetted to run here on Workers,
 *   so an encrypted assertion, NameID or attribute is refused with
 *   `saml_encrypted_assertion_unsupported` rather than half-handled. The IdP signs the assertion
 *   and does not encrypt it; the POST travels over TLS.
 *
 * No DOCTYPE is accepted (no entity expansion of any kind). Nothing here logs an assertion, a
 * NameID or an attribute.
 */
import { DOMParser } from "@xmldom/xmldom";
import { SignedXml } from "xml-crypto";

import { randomId } from "./crypto.js";

export const NS = {
  samlp: "urn:oasis:names:tc:SAML:2.0:protocol",
  saml: "urn:oasis:names:tc:SAML:2.0:assertion",
  ds: "http://www.w3.org/2000/09/xmldsig#",
} as const;

export const MAX_SAML_RESPONSE_BYTES = 256 * 1024;
/** The oldest Response / assertion `IssueInstant` accepted (before the skew): the sign-in flow's
 * own lifetime, so an assertion minted before the AuthnRequest it answers could not exist. */
export const MAX_ASSERTION_AGE_MS = 10 * 60_000;
const SAML_VERSION = "2.0";
const SUCCESS = "urn:oasis:names:tc:SAML:2.0:status:Success";
const BEARER = "urn:oasis:names:tc:SAML:2.0:cm:bearer";
const ACCEPTED_SIGNATURE_ALGS = ["http://www.w3.org/2001/04/xmldsig-more#rsa-sha256", "http://www.w3.org/2001/04/xmldsig-more#rsa-sha512"] as const;
const ACCEPTED_DIGEST_ALGS = ["http://www.w3.org/2001/04/xmlenc#sha256", "http://www.w3.org/2001/04/xmlenc#sha512"] as const;
/** The transforms a reference may name. A `Transform` without an accepted `Algorithm` is refused:
 * xml-crypto 6 silently SKIPS one with no `Algorithm` (E38 fuzzing surfaced its deprecation
 * warning), and a signature whose transforms are not exactly what was signed must not verify. */
const ACCEPTED_TRANSFORMS = [
  "http://www.w3.org/2000/09/xmldsig#enveloped-signature", "http://www.w3.org/2001/10/xml-exc-c14n#",
  "http://www.w3.org/2001/10/xml-exc-c14n#WithComments", "http://www.w3.org/TR/2001/REC-xml-c14n-20010315",
] as const;
const EMAIL_ATTRIBUTES = ["email", "mail", "emailaddress", "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/emailaddress", "urn:oid:0.9.2342.19200300.100.1.3"];

export type SamlErrorCode =
  | "saml_malformed" | "saml_doctype_refused" | "saml_too_large" | "saml_status_not_success" | "saml_destination_mismatch" | "saml_in_response_to_mismatch"
  | "saml_assertion_count" | "saml_encrypted_assertion_unsupported" | "saml_assertion_unsigned" | "saml_algorithm_refused" | "saml_signature_invalid"
  | "saml_reference_mismatch" | "saml_issuer_mismatch" | "saml_not_yet_valid" | "saml_expired" | "saml_audience_mismatch" | "saml_recipient_mismatch"
  | "saml_subject_missing" | "saml_bearer_missing";

export class SamlError extends Error {
  constructor(readonly code: SamlErrorCode) {
    super(code);
    this.name = "SamlError";
  }
}

// --- the AuthnRequest ---

const xmlEscape = (s: string): string => s.replace(/[&<>"']/g, (ch) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&apos;" })[ch] ?? ch);

export function newRequestId(): string {
  // An NCName: starts with a letter or underscore.
  return `_${randomId(20).replace(/[^A-Za-z0-9]/g, "x")}`;
}

export function authnRequestXml(p: { id: string; issueInstant: string; destination: string; acsUrl: string; spEntityId: string }): string {
  return `<samlp:AuthnRequest xmlns:samlp="${NS.samlp}" xmlns:saml="${NS.saml}" ID="${xmlEscape(p.id)}" Version="2.0" IssueInstant="${xmlEscape(p.issueInstant)}" Destination="${xmlEscape(p.destination)}" ProtocolBinding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST" AssertionConsumerServiceURL="${xmlEscape(p.acsUrl)}"><saml:Issuer>${xmlEscape(p.spEntityId)}</saml:Issuer><samlp:NameIDPolicy Format="urn:oasis:names:tc:SAML:1.1:nameid-format:unspecified" AllowCreate="false"/></samlp:AuthnRequest>`;
}

async function deflateRaw(text: string): Promise<Uint8Array> {
  const stream = new Blob([text]).stream().pipeThrough(new CompressionStream("deflate-raw"));
  return new Uint8Array(await new Response(stream).arrayBuffer());
}

/** The HTTP-Redirect binding URL: `SAMLRequest` = base64(deflate-raw(xml)), plus `RelayState`. */
export async function redirectBindingUrl(ssoUrl: string, xml: string, relayState: string): Promise<string> {
  const url = new URL(ssoUrl);
  url.searchParams.set("SAMLRequest", Buffer.from(await deflateRaw(xml)).toString("base64"));
  url.searchParams.set("RelayState", relayState);
  return url.toString();
}

export function spMetadataXml(p: { spEntityId: string; acsUrl: string }): string {
  return `<?xml version="1.0" encoding="UTF-8"?>
<md:EntityDescriptor xmlns:md="urn:oasis:names:tc:SAML:2.0:metadata" entityID="${xmlEscape(p.spEntityId)}"><md:SPSSODescriptor AuthnRequestsSigned="false" WantAssertionsSigned="true" protocolSupportEnumeration="${NS.samlp}"><md:NameIDFormat>urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress</md:NameIDFormat><md:AssertionConsumerService Binding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST" Location="${xmlEscape(p.acsUrl)}" index="0" isDefault="true"/></md:SPSSODescriptor></md:EntityDescriptor>
`;
}

// --- the Response ---

function parseXml(xml: string): Document {
  if (/<!DOCTYPE/i.test(xml) || /<!ENTITY/i.test(xml)) throw new SamlError("saml_doctype_refused");
  let failed = false;
  let doc: Document;
  try {
    doc = new DOMParser({ errorHandler: (level: string) => { if (level !== "warning") failed = true; } })
      .parseFromString(xml, "text/xml") as unknown as Document;
  } catch {
    throw new SamlError("saml_malformed");
  }
  if (failed || !doc || !doc.documentElement) throw new SamlError("saml_malformed");
  return doc;
}

function children(el: Element, ns: string, local: string): Element[] {
  const out: Element[] = [];
  for (let n = el.firstChild; n; n = n.nextSibling) {
    if (n.nodeType === 1 && (n as Element).namespaceURI === ns && (n as Element).localName === local) out.push(n as Element);
  }
  return out;
}

function child(el: Element | undefined, ns: string, local: string): Element | undefined {
  return el ? children(el, ns, local)[0] : undefined;
}

const text = (el: Element | undefined): string => (el?.textContent ?? "").trim();

function instant(raw: string | null | undefined): number | undefined {
  if (!raw) return undefined;
  const ms = Date.parse(raw);
  return Number.isFinite(ms) ? ms : undefined;
}

export interface SamlValidationParams {
  spEntityId: string;
  acsUrl: string;
  idpEntityId: string;
  /** PEM certificates or public keys the IdP signs with. */
  certificates: readonly string[];
  /** The AuthnRequest id this response must answer. */
  requestId: string;
  nowMs: number;
  skewSecs: number;
}

export interface SamlAssertion {
  assertionId: string;
  issuer: string;
  nameId: string;
  nameIdFormat?: string;
  email?: string;
  /** When the assertion stops being usable (the earliest NotOnOrAfter): the replay window. */
  expiresAt: number;
}

/** Verify the signature on the assertion and return the signed assertion XML (canonical form). */
function verifiedAssertionXml(xml: string, assertion: Element, certificates: readonly string[]): string {
  const signatures = children(assertion, NS.ds, "Signature");
  if (signatures.length !== 1 || !signatures[0]) throw new SamlError("saml_assertion_unsigned");
  const signature = signatures[0];
  const signedInfo = child(signature, NS.ds, "SignedInfo");
  const method = child(signedInfo, NS.ds, "SignatureMethod")?.getAttribute("Algorithm") ?? "";
  const refs = signedInfo ? children(signedInfo, NS.ds, "Reference") : [];
  if (!(ACCEPTED_SIGNATURE_ALGS as readonly string[]).includes(method)) throw new SamlError("saml_algorithm_refused");
  if (refs.length !== 1 || !refs[0]) throw new SamlError("saml_reference_mismatch");
  const digest = child(refs[0], NS.ds, "DigestMethod")?.getAttribute("Algorithm") ?? "";
  if (!(ACCEPTED_DIGEST_ALGS as readonly string[]).includes(digest)) throw new SamlError("saml_algorithm_refused");
  const transforms = child(refs[0], NS.ds, "Transforms");
  for (const t of transforms ? children(transforms, NS.ds, "Transform") : []) {
    if (!(ACCEPTED_TRANSFORMS as readonly string[]).includes(t.getAttribute("Algorithm") ?? "")) throw new SamlError("saml_algorithm_refused");
  }
  const id = assertion.getAttribute("ID") ?? "";
  if (!id || refs[0].getAttribute("URI") !== `#${id}`) throw new SamlError("saml_reference_mismatch");
  for (const cert of certificates) {
    const verifier = new SignedXml({ publicCert: cert, getCertFromKeyInfo: () => null });
    // Only the accepted algorithms are registered: anything else cannot verify.
    verifier.SignatureAlgorithms = Object.fromEntries(ACCEPTED_SIGNATURE_ALGS.map((a) => [a, verifier.SignatureAlgorithms[a]]).filter(([, v]) => v !== undefined));
    verifier.HashAlgorithms = Object.fromEntries(ACCEPTED_DIGEST_ALGS.map((a) => [a, verifier.HashAlgorithms[a]]).filter(([, v]) => v !== undefined));
    let ok = false;
    try {
      verifier.loadSignature(signature as unknown as Node);
      ok = verifier.checkSignature(xml);
    } catch {
      ok = false;
    }
    if (!ok) continue;
    const signed = verifier.getSignedReferences();
    const references = verifier.getReferences();
    if (signed.length !== 1 || !signed[0] || references.length !== 1 || references[0]?.uri !== `#${id}`) throw new SamlError("saml_reference_mismatch");
    return signed[0];
  }
  throw new SamlError("saml_signature_invalid");
}

/** An `IssueInstant` that is present, not ahead of now and not older than the sign-in flow, each
 * within the skew. */
function checkIssueInstant(raw: string | null, nowMs: number, skewMs: number): void {
  const at = instant(raw);
  if (at === undefined) throw new SamlError("saml_malformed");
  if (at - skewMs > nowMs) throw new SamlError("saml_not_yet_valid");
  if (at + MAX_ASSERTION_AGE_MS + skewMs <= nowMs) throw new SamlError("saml_expired");
}

/** Validate a posted `SAMLResponse` (base64). Throws `SamlError` with a fixed code. */
export function validateSamlResponse(samlResponseB64: string, p: SamlValidationParams): SamlAssertion {
  if (samlResponseB64.length > (MAX_SAML_RESPONSE_BYTES * 4) / 3 + 4) throw new SamlError("saml_too_large");
  const xml = Buffer.from(samlResponseB64.replace(/\s+/g, ""), "base64").toString("utf8");
  if (xml.length === 0) throw new SamlError("saml_malformed");
  const doc = parseXml(xml);
  const root = doc.documentElement;
  if (root.namespaceURI !== NS.samlp || root.localName !== "Response") throw new SamlError("saml_malformed");
  if (root.getAttribute("Version") !== SAML_VERSION) throw new SamlError("saml_malformed");
  const skewMs = p.skewSecs * 1000;
  // The issue instant of the Response (unsigned here, so it can only refuse, never admit).
  checkIssueInstant(root.getAttribute("IssueInstant"), p.nowMs, skewMs);
  const responseIssuers = children(root, NS.saml, "Issuer");
  if (responseIssuers.length > 1 || (responseIssuers[0] && text(responseIssuers[0]) !== p.idpEntityId)) throw new SamlError("saml_issuer_mismatch");
  const status = child(child(root, NS.samlp, "Status"), NS.samlp, "StatusCode")?.getAttribute("Value");
  if (status !== SUCCESS) throw new SamlError("saml_status_not_success");
  const destination = root.getAttribute("Destination");
  if (destination && destination !== p.acsUrl) throw new SamlError("saml_destination_mismatch");
  const responseTo = root.getAttribute("InResponseTo");
  if (responseTo !== p.requestId) throw new SamlError("saml_in_response_to_mismatch");
  for (const encrypted of ["EncryptedAssertion", "EncryptedID", "EncryptedAttribute"]) {
    if (doc.getElementsByTagNameNS(NS.saml, encrypted).length > 0) throw new SamlError("saml_encrypted_assertion_unsupported");
  }
  const all = doc.getElementsByTagNameNS(NS.saml, "Assertion");
  const direct = children(root, NS.saml, "Assertion");
  if (all.length !== 1 || direct.length !== 1 || !direct[0]) throw new SamlError("saml_assertion_count");
  const signedXml = verifiedAssertionXml(xml, direct[0], p.certificates);

  // From here on, only the signed bytes are read.
  const a = parseXml(signedXml).documentElement;
  if (a.namespaceURI !== NS.saml || a.localName !== "Assertion" || a.getAttribute("ID") !== direct[0].getAttribute("ID")) throw new SamlError("saml_reference_mismatch");
  if (a.getAttribute("Version") !== SAML_VERSION) throw new SamlError("saml_malformed");
  checkIssueInstant(a.getAttribute("IssueInstant"), p.nowMs, skewMs);
  for (const name of ["Issuer", "Subject", "Conditions"]) {
    if (children(a, NS.saml, name).length > 1) throw new SamlError("saml_malformed");
  }
  const issuer = text(child(a, NS.saml, "Issuer"));
  if (issuer !== p.idpEntityId) throw new SamlError("saml_issuer_mismatch");
  const conditions = child(a, NS.saml, "Conditions");
  if (!conditions) throw new SamlError("saml_audience_mismatch");
  const notBefore = instant(conditions.getAttribute("NotBefore"));
  const notOnOrAfter = instant(conditions.getAttribute("NotOnOrAfter"));
  if (notBefore !== undefined && notBefore - skewMs > p.nowMs) throw new SamlError("saml_not_yet_valid");
  if (notOnOrAfter === undefined || notOnOrAfter + skewMs <= p.nowMs) throw new SamlError("saml_expired");
  const restrictions = children(conditions, NS.saml, "AudienceRestriction");
  if (restrictions.length === 0 || !restrictions.every((r) => children(r, NS.saml, "Audience").some((au) => text(au) === p.spEntityId))) throw new SamlError("saml_audience_mismatch");
  const subject = child(a, NS.saml, "Subject");
  const nameIdEl = child(subject, NS.saml, "NameID");
  const nameId = text(nameIdEl);
  if (!subject || !nameId) throw new SamlError("saml_subject_missing");
  const confirmations = children(subject, NS.saml, "SubjectConfirmation").filter((sc) => sc.getAttribute("Method") === BEARER);
  if (confirmations.length === 0) throw new SamlError("saml_bearer_missing");
  let confirmedUntil: number | undefined;
  let recipientSeen = false;
  let startsLater = false;
  for (const sc of confirmations) {
    const data = child(sc, NS.saml, "SubjectConfirmationData");
    if (!data || data.getAttribute("Recipient") !== p.acsUrl) continue;
    recipientSeen = true;
    if (data.getAttribute("InResponseTo") !== p.requestId) continue;
    const from = instant(data.getAttribute("NotBefore"));
    if (from !== undefined && from - skewMs > p.nowMs) {
      startsLater = true;
      continue;
    }
    const until = instant(data.getAttribute("NotOnOrAfter"));
    if (until === undefined || until + skewMs <= p.nowMs) continue;
    confirmedUntil = until;
    break;
  }
  if (confirmedUntil === undefined) throw new SamlError(startsLater ? "saml_not_yet_valid" : recipientSeen ? "saml_expired" : "saml_recipient_mismatch");
  const format = nameIdEl?.getAttribute("Format") ?? undefined;
  let email: string | undefined = format === "urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress" && nameId.includes("@") ? nameId : undefined;
  if (!email) {
    const attrs = a.getElementsByTagNameNS(NS.saml, "Attribute");
    for (let i = 0; i < attrs.length; i += 1) {
      const attr = attrs.item(i);
      if (!attr) continue;
      const name = (attr.getAttribute("Name") ?? "").toLowerCase();
      if (!EMAIL_ATTRIBUTES.includes(name)) continue;
      const value = text(child(attr, NS.saml, "AttributeValue"));
      if (value.includes("@")) { email = value; break; }
    }
  }
  return {
    assertionId: a.getAttribute("ID") ?? "", issuer, nameId, ...(format ? { nameIdFormat: format } : {}),
    ...(email ? { email: email.toLowerCase() } : {}), expiresAt: Math.min(notOnOrAfter, confirmedUntil) + skewMs,
  };
}
