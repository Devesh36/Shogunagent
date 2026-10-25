// E26 in workerd: SAML signature validation (xml-crypto over @xmldom/xmldom on node:crypto),
// OIDC ID token verification (WebCrypto), the AuthnRequest deflate, and the sealed flow cookie
// all run in the runtime the deployed Worker uses — so SAML ships on Workers, not behind a
// Node-only flag. Keys are generated per run with WebCrypto.
import { describe, expect, it } from "vitest";
import { createPublicKey, createVerify } from "node:crypto";
import { SignedXml } from "xml-crypto";

import { derive, seal, unseal } from "../../src/enterprise/crypto.js";
import { OidcCache, verifyIdToken } from "../../src/enterprise/oidc.js";
import { SamlError, authnRequestXml, redirectBindingUrl, validateSamlResponse } from "../../src/enterprise/saml.js";
import { TEST_IDP_CERT, TEST_IDP_SIG_OVER_HELLO } from "../saml-test-idp-cert.js";

const pem = (label: string, der: ArrayBuffer): string => `-----BEGIN ${label}-----\n${Buffer.from(der).toString("base64").match(/.{1,64}/g)?.join("\n") ?? ""}\n-----END ${label}-----\n`;

async function rsaPair(): Promise<{ privatePem: string; publicPem: string; keys: CryptoKeyPair }> {
  const keys = (await crypto.subtle.generateKey({ name: "RSASSA-PKCS1-v1_5", modulusLength: 2048, publicExponent: new Uint8Array([1, 0, 1]), hash: "SHA-256" }, true, ["sign", "verify"])) as CryptoKeyPair;
  return {
    keys,
    privatePem: pem("PRIVATE KEY", (await crypto.subtle.exportKey("pkcs8", keys.privateKey)) as ArrayBuffer),
    publicPem: pem("PUBLIC KEY", (await crypto.subtle.exportKey("spki", keys.publicKey)) as ArrayBuffer),
  };
}

const NOW = Date.parse("2026-10-25T12:00:00Z");
const iso = (ms: number): string => new Date(ms).toISOString();
const ACS = "https://api.test/sso/tnt_1/saml/acs";
const SP = "https://api.test/sso/tnt_1/saml/metadata";
const IDP = "http://www.okta.com/exk1";

function signedResponse(privatePem: string, nameId = "admin@acme.example"): string {
  const xml = `<saml2p:Response xmlns:saml2p="urn:oasis:names:tc:SAML:2.0:protocol" Destination="${ACS}" ID="r1" InResponseTo="_req" IssueInstant="${iso(NOW)}" Version="2.0"><saml2:Issuer xmlns:saml2="urn:oasis:names:tc:SAML:2.0:assertion">${IDP}</saml2:Issuer><saml2p:Status><saml2p:StatusCode Value="urn:oasis:names:tc:SAML:2.0:status:Success"/></saml2p:Status><saml2:Assertion xmlns:saml2="urn:oasis:names:tc:SAML:2.0:assertion" ID="a1" IssueInstant="${iso(NOW)}" Version="2.0"><saml2:Issuer>${IDP}</saml2:Issuer><saml2:Subject><saml2:NameID Format="urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress">${nameId}</saml2:NameID><saml2:SubjectConfirmation Method="urn:oasis:names:tc:SAML:2.0:cm:bearer"><saml2:SubjectConfirmationData InResponseTo="_req" NotOnOrAfter="${iso(NOW + 300_000)}" Recipient="${ACS}"/></saml2:SubjectConfirmation></saml2:Subject><saml2:Conditions NotBefore="${iso(NOW - 60_000)}" NotOnOrAfter="${iso(NOW + 300_000)}"><saml2:AudienceRestriction><saml2:Audience>${SP}</saml2:Audience></saml2:AudienceRestriction></saml2:Conditions></saml2:Assertion></saml2p:Response>`;
  const signer = new SignedXml({ privateKey: privatePem, signatureAlgorithm: "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256", canonicalizationAlgorithm: "http://www.w3.org/2001/10/xml-exc-c14n#" });
  signer.addReference({ xpath: "//*[local-name(.)='Assertion']", digestAlgorithm: "http://www.w3.org/2001/04/xmlenc#sha256", transforms: ["http://www.w3.org/2000/09/xmldsig#enveloped-signature", "http://www.w3.org/2001/10/xml-exc-c14n#"] });
  signer.computeSignature(xml, { location: { reference: "//*[local-name(.)='Assertion']/*[local-name(.)='Issuer']", action: "after" } });
  return Buffer.from(signer.getSignedXml()).toString("base64");
}

describe("enterprise SSO in workerd", () => {
  it("validates a signed SAML assertion and refuses a tampered one", async () => {
    const { privatePem, publicPem } = await rsaPair();
    const params = { spEntityId: SP, acsUrl: ACS, idpEntityId: IDP, certificates: [publicPem], requestId: "_req", nowMs: NOW, skewSecs: 120 };
    const b64 = signedResponse(privatePem);
    expect(validateSamlResponse(b64, params)).toMatchObject({ assertionId: "a1", nameId: "admin@acme.example", email: "admin@acme.example" });
    const tampered = Buffer.from(Buffer.from(b64, "base64").toString("utf8").replace(">admin@acme.example<", ">attacker@acme.example<")).toString("base64");
    expect(() => validateSamlResponse(tampered, params)).toThrow(SamlError);
    const other = await rsaPair();
    expect(() => validateSamlResponse(b64, { ...params, certificates: [other.publicPem] })).toThrow(SamlError);
  });

  it("deflates the AuthnRequest for the redirect binding", async () => {
    const url = new URL(await redirectBindingUrl("https://idp.example.com/sso", authnRequestXml({ id: "_x", issueInstant: iso(NOW), destination: "https://idp.example.com/sso", acsUrl: ACS, spEntityId: SP }), "rs"));
    expect(url.searchParams.get("SAMLRequest")).toBeTruthy();
    expect(url.searchParams.get("RelayState")).toBe("rs");
  });

  it("verifies an RS256 ID token against a JWKS", async () => {
    const { keys } = await rsaPair();
    const jwk = { ...(await crypto.subtle.exportKey("jwk", keys.publicKey)), kid: "k1", use: "sig" };
    const now = Math.floor(NOW / 1000);
    const enc = (v: unknown): string => Buffer.from(JSON.stringify(v)).toString("base64url");
    const signingInput = `${enc({ alg: "RS256", kid: "k1" })}.${enc({ iss: "https://idp.example.com", sub: "s", aud: "c", iat: now, exp: now + 60, nonce: "n" })}`;
    const sig = Buffer.from(await crypto.subtle.sign("RSASSA-PKCS1-v1_5", keys.privateKey, new TextEncoder().encode(signingInput))).toString("base64url");
    const fetch = () => Promise.resolve(new Response(JSON.stringify({ keys: [jwk] }), { headers: { "content-type": "application/json" } }));
    const claims = await verifyIdToken(`${signingInput}.${sig}`, { issuer: "https://idp.example.com", clientId: "c", nonce: "n", jwksUri: "https://idp.example.com/jwks", nowMs: NOW }, { fetch }, new OidcCache());
    expect(claims.sub).toBe("s");
  });

  it("verifies with an X.509 certificate PEM (what IdPs publish), as xml-crypto does", () => {
    const v = createVerify("RSA-SHA256");
    v.update("hello");
    expect(v.verify(TEST_IDP_CERT, TEST_IDP_SIG_OVER_HELLO, "base64")).toBe(true);
    expect(createPublicKey(TEST_IDP_CERT).asymmetricKeyType).toBe("rsa");
  });

  it("seals and opens the flow cookie", async () => {
    const key = derive("w".repeat(40), "sso-flow");
    expect(await unseal(key, "sso-flow", await seal(key, "sso-flow", "verifier"))).toBe("verifier");
  });
});
