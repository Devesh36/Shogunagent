/** E38: property/fuzz test of the SAML Response parser with malformed XML. Seeded mutations of a
 * validly signed response, plus random input. Properties:
 * - `validateSamlResponse` either returns or throws a `SamlError` with a fixed code — never a
 *   TypeError, a parser exception or anything else that would become a 500;
 * - whenever a mutated response is still accepted, what it asserts (NameID, e-mail, issuer,
 *   assertion id) is exactly what the IdP signed: no mutation can change an identity. */
import { describe, expect, it } from "vitest";

import { SamlError, validateSamlResponse, type SamlValidationParams } from "../src/enterprise/saml.js";
import { SAML_IDP_ENTITY, samlKeys, samlResponse } from "./enterprise-helpers.js";

const NOW = Date.parse("2026-10-25T12:00:00Z");
const SP = "https://api.test/sso/t1/saml/metadata";
const ACS = "https://api.test/sso/t1/saml/acs";
const REQUEST_ID = "_req_fuzz";

function rng(seed: number): () => number {
  let s = seed >>> 0 || 1;
  return () => {
    s ^= s << 13; s >>>= 0;
    s ^= s >>> 17;
    s ^= s << 5; s >>>= 0;
    return s;
  };
}

const SNIPPETS = [
  "<!DOCTYPE x [<!ENTITY a \"b\">]>", "<!-- c -->", "<![CDATA[x]]>", "&amp;", "&lt;", "&#x0;", "&unknown;", "<", ">", "\"", "'",
  "<saml2:Assertion xmlns:saml2=\"urn:oasis:names:tc:SAML:2.0:assertion\" ID=\"evil\"><saml2:Subject><saml2:NameID>attacker@evil.example</saml2:NameID></saml2:Subject></saml2:Assertion>",
  "<saml2:NameID>attacker@evil.example</saml2:NameID>", "<saml2:EncryptedAssertion xmlns:saml2=\"urn:oasis:names:tc:SAML:2.0:assertion\"/>",
  "<?xml version=\"1.0\"?>", "<?pi x?>", "\u0000", "￿", "😀", " xmlns:saml2=\"urn:x\"", " ID=\"dup\"", "</saml2:Assertion>", "<a>".repeat(50),
];

function mutate(xml: string, next: () => number): string {
  let out = xml;
  const n = 1 + (next() % 3);
  for (let k = 0; k < n; k++) {
    const at = next() % (out.length + 1);
    switch (next() % 6) {
      case 0: // flip one character
        out = out.slice(0, at) + String.fromCharCode(32 + (next() % 95)) + out.slice(at + 1);
        break;
      case 1: // delete a span
        out = out.slice(0, at) + out.slice(at + 1 + (next() % 40));
        break;
      case 2: // insert a snippet
        out = out.slice(0, at) + SNIPPETS[next() % SNIPPETS.length] + out.slice(at);
        break;
      case 3: // truncate
        out = out.slice(0, at);
        break;
      case 4: // duplicate a span (element wrapping / repetition)
        out = out.slice(0, at) + out.slice(at, at + (next() % 400)) + out.slice(at);
        break;
      default: // swap the e-mail inside text only
        out = out.replace("admin@acme.example", "attacker@evil.example");
    }
  }
  return out;
}

function outcome(b64: string, p: SamlValidationParams): { ok: true; value: ReturnType<typeof validateSamlResponse> } | { ok: false; code: string } {
  try {
    return { ok: true, value: validateSamlResponse(b64, p) };
  } catch (e) {
    if (!(e instanceof SamlError)) throw new Error(`non-SamlError thrown: ${e instanceof Error ? e.constructor.name : typeof e}`);
    return { ok: false, code: e.code };
  }
}

describe("SAML response parser under malformed input", () => {
  const keys = samlKeys();
  const p: SamlValidationParams = { spEntityId: SP, acsUrl: ACS, idpEntityId: SAML_IDP_ENTITY, certificates: [keys.publicPem], requestId: REQUEST_ID, nowMs: NOW, skewSecs: 120 };
  const signedXml = Buffer.from(samlResponse({
    inResponseTo: REQUEST_ID, destination: ACS, recipient: ACS, audience: SP, nameId: "admin@acme.example", email: "admin@acme.example", nowMs: NOW, keys,
  }), "base64").toString("utf8");
  const original = validateSamlResponse(Buffer.from(signedXml).toString("base64"), p);

  it("the unmutated fixture is accepted", () => {
    expect(original).toMatchObject({ nameId: "admin@acme.example", email: "admin@acme.example", issuer: SAML_IDP_ENTITY });
  });

  it("mutated responses are refused with a fixed code, or accepted with the signed identity unchanged", () => {
    let accepted = 0;
    for (let seed = 1; seed <= 600; seed++) {
      const next = rng(seed * 2654435761);
      const xml = mutate(signedXml, next);
      const r = outcome(Buffer.from(xml).toString("base64"), p);
      if (r.ok) {
        accepted += 1;
        expect({ seed, nameId: r.value.nameId, email: r.value.email, issuer: r.value.issuer, id: r.value.assertionId })
          .toEqual({ seed, nameId: original.nameId, email: original.email, issuer: original.issuer, id: original.assertionId });
      } else {
        expect(r.code).toMatch(/^saml_[a-z_]+$/);
      }
    }
    // Some mutations land outside the signed bytes (e.g. in the Response's own attributes'
    // whitespace) and stay valid; most do not.
    expect(accepted).toBeLessThan(600);
  });

  it("a reference Transform without an accepted Algorithm is refused, not skipped (E38 finding)", () => {
    const stripped = signedXml.replace(' Algorithm="http://www.w3.org/2000/09/xmldsig#enveloped-signature"', "");
    expect(stripped).not.toBe(signedXml);
    expect(outcome(Buffer.from(stripped).toString("base64"), p)).toEqual({ ok: false, code: "saml_algorithm_refused" });
    const other = signedXml.replace('Algorithm="http://www.w3.org/2000/09/xmldsig#enveloped-signature"', 'Algorithm="http://www.w3.org/TR/1999/REC-xslt-19991116"');
    expect(other).not.toBe(signedXml);
    expect(outcome(Buffer.from(other).toString("base64"), p)).toEqual({ ok: false, code: "saml_algorithm_refused" });
  });

  it("random and hostile non-XML input is refused with a fixed code", () => {
    const next = rng(0xC0FFEE);
    const inputs = [
      "", "====", "not base64 at all !!", Buffer.from("<x/>").toString("base64"), Buffer.from("\u0000\u0001\u0002").toString("base64"),
      Buffer.from("<samlp:Response xmlns:samlp=\"urn:oasis:names:tc:SAML:2.0:protocol\"/>").toString("base64"),
      Buffer.from(`<!DOCTYPE r [<!ENTITY x SYSTEM "file:///etc/passwd">]>${signedXml}`).toString("base64"),
      Buffer.from(`<${"a>".repeat(5000)}`).toString("base64"),
      "A".repeat(400_000),
    ];
    for (let k = 0; k < 300; k++) inputs.push(Buffer.from(Array.from({ length: next() % 300 }, () => next() % 256)).toString("base64"));
    for (const b64 of inputs) {
      const r = outcome(b64, p);
      expect(r.ok).toBe(false);
    }
  });
});
