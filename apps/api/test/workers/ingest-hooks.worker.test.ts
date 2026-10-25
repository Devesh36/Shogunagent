// E44: the ingest hooks' crypto inside workerd, the runtime the deployed Worker uses: the HMAC
// signature schemes and the AES-256-GCM hook secret envelope under `nodejs_compat`, and the Gmail
// push's Google OIDC token verified with WebCrypto RS256.
import { describe, expect, it } from "vitest";

import { verifyGoogleOidc, type JwksSource, type RsaJwk } from "../../src/ingest-hooks/pubsub-jwt.js";
import { HookSecrets } from "../../src/ingest-hooks/secrets.js";
import { sign, tokenMatches, verifyGitHub, verifySlack } from "../../src/ingest-hooks/verify.js";

const b64url = (bytes: Uint8Array): string => btoa(String.fromCharCode(...bytes)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
const json64 = (v: unknown): string => b64url(new TextEncoder().encode(JSON.stringify(v)));

describe("ingest hooks in workerd", () => {
  it("verifies HMAC schemes and seals hook secrets bound to the hook", () => {
    const body = new TextEncoder().encode('{"action":"opened"}');
    expect(verifyGitHub(["s1"], body, sign.github("s1", body))).toBe(true);
    expect(verifyGitHub(["s1"], body, sign.github("s2", body))).toBe(false);
    expect(verifySlack(["ss"], body, "1761393600", sign.slack("ss", body, 1761393600), 1761393610)).toBe(true);
    expect(tokenMatches(["tok"], "tok")).toBe(true);
    const secrets = new HookSecrets("workerd-ingest-hooks-master-secret-0000");
    const sealed = secrets.seal("slack-signing-secret-sentinel", "hk_aaaaaaaaaaaaaaaaaaaaaa");
    expect(secrets.open(sealed, "hk_aaaaaaaaaaaaaaaaaaaaaa")).toBe("slack-signing-secret-sentinel");
    expect(secrets.open(sealed, "hk_bbbbbbbbbbbbbbbbbbbbbb")).toBeUndefined();
    expect(secrets.derived("hk_aaaaaaaaaaaaaaaaaaaaaa", 1)).toMatch(/^[A-Za-z0-9_-]{43}$/);
  });

  it("verifies a Google-signed push token with WebCrypto", async () => {
    const pair = (await crypto.subtle.generateKey({ name: "RSASSA-PKCS1-v1_5", modulusLength: 2048, publicExponent: new Uint8Array([1, 0, 1]), hash: "SHA-256" }, true, ["sign", "verify"])) as CryptoKeyPair;
    const pub = (await crypto.subtle.exportKey("jwk", pair.publicKey)) as { n: string; e: string };
    const jwks: JwksSource = { keys: () => Promise.resolve([{ kid: "k1", kty: "RSA", n: pub.n, e: pub.e } satisfies RsaJwk]) };
    const nowMs = Date.parse("2026-10-25T12:00:00Z");
    const claims = { iss: "https://accounts.google.com", aud: "https://api.test/v1/ingest-hooks/gmail", email: "push@p.iam.gserviceaccount.com", email_verified: true, iat: nowMs / 1000 - 5, exp: nowMs / 1000 + 3600 };
    const signingInput = `${json64({ alg: "RS256", kid: "k1" })}.${json64(claims)}`;
    const sig = new Uint8Array(await crypto.subtle.sign("RSASSA-PKCS1-v1_5", pair.privateKey, new TextEncoder().encode(signingInput)));
    const token = `${signingInput}.${b64url(sig)}`;
    const expect_ = { audience: claims.aud, serviceAccount: claims.email };
    expect(await verifyGoogleOidc(`Bearer ${token}`, expect_, jwks, nowMs)).toEqual({ ok: true });
    expect(await verifyGoogleOidc(`Bearer ${token}`, { ...expect_, audience: "https://other" }, jwks, nowMs)).toEqual({ ok: false, reason: "claims" });
    expect(await verifyGoogleOidc(`Bearer ${token.slice(0, -4)}AAAA`, expect_, jwks, nowMs)).toEqual({ ok: false, reason: "bad_signature" });
  });
});
