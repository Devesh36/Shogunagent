/** Audit 2026-10-10 M6: two trusted signing keys (current + next), `kid` routing, and the token
 * lifetime ceiling. Relay side of the contract shared with `crates/shogun-license` and the licence
 * API's signer: `kid` = first 16 hex of SHA-256(raw 32-byte public key); with `kid` only that key
 * is tried, without it current then next; `exp - iat <= 48h`; `iat <= now + 300`. */
import { generateKeyPairSync } from "node:crypto";
import { describe, expect, it } from "vitest";

import {
  KID_HEX_LENGTH,
  MAX_IAT_SKEW_SECONDS,
  MAX_TOKEN_LIFETIME_SECONDS,
  keyId,
  rawPublicKeyBytes,
  releaseTrustedKeysFromEnv,
  verifyLicense,
} from "../src/auth.js";
import releaseKey from "../../../packages/config/license-public-key.json";
import { makeKeys, makeRig, postBatch, sampleBody, signLicense } from "./helpers.js";

const NOW = 1_800_000_000;

describe("kid derivation", () => {
  it("is the first 16 hex characters of SHA-256 over the raw 32 public-key bytes", async () => {
    const { publicKey } = await makeKeys();
    expect(rawPublicKeyBytes(publicKey)).toHaveLength(32);
    const kid = keyId(publicKey);
    expect(kid).toMatch(new RegExp(`^[0-9a-f]{${KID_HEX_LENGTH}}$`));
    const { createHash } = await import("node:crypto");
    expect(kid).toBe(createHash("sha256").update(rawPublicKeyBytes(publicKey)).digest("hex").slice(0, 16));
  });
});

describe("current + next key verification", () => {
  it("accepts a token signed by the next key, with and without kid, when a next key is trusted", async () => {
    const current = await makeKeys();
    const next = await makeKeys();
    const trusted = { current: current.publicKey, next: next.publicKey };
    const plain = await signLicense(next.privateKey, { iat: NOW });
    expect(verifyLicense(`Bearer ${plain}`, trusted, NOW).licenseId).toBe("lic_test_1");
    const withKid = await signLicense(next.privateKey, { iat: NOW, kid: keyId(next.publicKey) });
    expect(verifyLicense(`Bearer ${withKid}`, trusted, NOW).licenseId).toBe("lic_test_1");
    // The current key keeps working through the rotation window.
    const fromCurrent = await signLicense(current.privateKey, { iat: NOW, kid: keyId(current.publicKey) });
    expect(verifyLicense(`Bearer ${fromCurrent}`, trusted, NOW).licenseId).toBe("lic_test_1");
  });

  it("rejects the next key's tokens on a relay that trusts only the current key", async () => {
    const current = await makeKeys();
    const next = await makeKeys();
    const token = await signLicense(next.privateKey, { iat: NOW });
    expect(() => verifyLicense(`Bearer ${token}`, current.publicKey, NOW)).toThrow(/invalid or expired/);
    expect(() => verifyLicense(`Bearer ${token}`, { current: current.publicKey }, NOW)).toThrow(/invalid or expired/);
  });

  it("routes by kid: a kid naming the other trusted key is refused even though the signature verifies against its own", async () => {
    const current = await makeKeys();
    const next = await makeKeys();
    const trusted = { current: current.publicKey, next: next.publicKey };
    // Signed by current, labelled as next: only the kid's key is tried, so this fails.
    const mislabelled = await signLicense(current.privateKey, { iat: NOW, kid: keyId(next.publicKey) });
    expect(() => verifyLicense(`Bearer ${mislabelled}`, trusted, NOW)).toThrow(/invalid or expired/);
  });

  it("refuses a kid that names no trusted key, and a kid of the wrong shape", async () => {
    const current = await makeKeys();
    const stranger = generateKeyPairSync("ed25519");
    for (const kid of [keyId(stranger.publicKey), "0000000000000000", "abc", "", "x".repeat(16)]) {
      const token = await signLicense(current.privateKey, { iat: NOW, kid });
      expect(() => verifyLicense(`Bearer ${token}`, current.publicKey, NOW)).toThrow(/invalid or expired/);
    }
    // A non-string kid never falls back to "try every key".
    const { privateKey } = current;
    const body = Buffer.from(JSON.stringify({ v: 1, lic: "l", plan: "standard", status: "active", device: "d", iat: NOW, exp: NOW + 3600, kid: 42 }));
    const { sign } = await import("node:crypto");
    const forged = `v1.${body.toString("base64url")}.${sign(null, body, privateKey).toString("base64url")}`;
    expect(() => verifyLicense(`Bearer ${forged}`, current.publicKey, NOW)).toThrow(/invalid or expired/);
  });

  it("drives the app's 401 for the wrong kid and 202 for the right one", async () => {
    const rig = await makeRig();
    const good = await signLicense(rig.keys.privateKey, { kid: keyId(rig.keys.publicKey) });
    expect((await postBatch(rig, sampleBody, good)).status).toBe(202);
    const bad = await signLicense(rig.keys.privateKey, { kid: "ffffffffffffffff" });
    expect((await postBatch(rig, sampleBody, bad)).status).toBe(401);
    expect(rig.gateway.createCalls).toHaveLength(1);
  });
});

describe("token lifetime ceiling", () => {
  it("rejects exp - iat beyond 48h, however valid the signature", async () => {
    const { privateKey, publicKey } = await makeKeys();
    const tenYears = await signLicense(privateKey, { iat: NOW, exp: NOW + 10 * 365 * 86_400 });
    expect(() => verifyLicense(`Bearer ${tenYears}`, publicKey, NOW)).toThrow(/invalid or expired/);
    const justOver = await signLicense(privateKey, { iat: NOW, exp: NOW + MAX_TOKEN_LIFETIME_SECONDS + 1 });
    expect(() => verifyLicense(`Bearer ${justOver}`, publicKey, NOW)).toThrow(/invalid or expired/);
    const atCeiling = await signLicense(privateKey, { iat: NOW, exp: NOW + MAX_TOKEN_LIFETIME_SECONDS });
    expect(verifyLicense(`Bearer ${atCeiling}`, publicKey, NOW).licenseId).toBe("lic_test_1");
    // The licence API's real TTL (26h) is well inside it.
    const real = await signLicense(privateKey, { iat: NOW, exp: NOW + 26 * 3600 });
    expect(verifyLicense(`Bearer ${real}`, publicKey, NOW).licenseId).toBe("lic_test_1");
  });

  it("rejects an iat more than 300s in the future and accepts one within the skew", async () => {
    const { privateKey, publicKey } = await makeKeys();
    const future = await signLicense(privateKey, { iat: NOW + MAX_IAT_SKEW_SECONDS + 1, expiresIn: "24h" });
    expect(() => verifyLicense(`Bearer ${future}`, publicKey, NOW)).toThrow(/invalid or expired/);
    const skewed = await signLicense(privateKey, { iat: NOW + MAX_IAT_SKEW_SECONDS, expiresIn: "24h" });
    expect(verifyLicense(`Bearer ${skewed}`, publicKey, NOW).licenseId).toBe("lic_test_1");
    // No lower bound: a relay clock ahead of the licence API must not lock paying users out.
    const old = await signLicense(privateKey, { iat: NOW - 20 * 3600, expiresIn: "24h" });
    expect(verifyLicense(`Bearer ${old}`, publicKey, NOW).licenseId).toBe("lic_test_1");
  });

  it("refuses a token without iat: the lifetime ceiling must not be dodged by omitting it", async () => {
    const { privateKey, publicKey } = await makeKeys();
    // Every token the licence API ever issued carries iat, and the device verifier rejects one
    // without it — so an iat-less token is not legacy, it is the one mint that would escape the
    // 48h ceiling (a leaked signing key asking for a decade on exp alone).
    const noIat = await signLicense(privateKey, { iat: null, exp: NOW + 20 * 3600 });
    expect(() => verifyLicense(`Bearer ${noIat}`, publicKey, NOW)).toThrow(/invalid or expired/);
    const decade = await signLicense(privateKey, { iat: null, exp: NOW + 10 * 365 * 86_400 });
    expect(() => verifyLicense(`Bearer ${decade}`, publicKey, NOW)).toThrow(/invalid or expired/);
    const kidNoIat = await signLicense(privateKey, { iat: null, exp: NOW + 3600, kid: keyId(publicKey) });
    expect(() => verifyLicense(`Bearer ${kidNoIat}`, publicKey, NOW)).toThrow(/invalid or expired/);
    const badIat = `v1.${Buffer.from(JSON.stringify({ v: 1, lic: "l", plan: "standard", status: "active", device: "d", iat: "soon", exp: NOW + 3600 })).toString("base64url")}`;
    const { sign } = await import("node:crypto");
    const body = Buffer.from(badIat.split(".")[1]!, "base64url");
    expect(() => verifyLicense(`Bearer ${badIat}.${sign(null, body, privateKey).toString("base64url")}`, publicKey, NOW)).toThrow(/invalid or expired/);
  });
});

describe("boot: trusted keys from the environment", () => {
  it("accepts the shipped current key alone while the trust file names no next key", () => {
    if ("ed25519_next" in releaseKey) return; // rotation window: covered by the branches below
    const trusted = releaseTrustedKeysFromEnv(releaseKey.ed25519, undefined);
    expect(trusted.current.asymmetricKeyType).toBe("ed25519");
    expect(trusted.next).toBeUndefined();
    expect(releaseTrustedKeysFromEnv(releaseKey.ed25519, "   ").next).toBeUndefined();
  });

  it("refuses a next key the shipped app does not trust, naming the variable only", () => {
    if ("ed25519_next" in releaseKey) return;
    const x = generateKeyPairSync("ed25519").publicKey.export({ format: "jwk" }).x!;
    const raw = Buffer.from(x, "base64url").toString("base64");
    expect(() => releaseTrustedKeysFromEnv(releaseKey.ed25519, raw)).toThrow(/LICENSE_PUBKEY_NEXT_B64/);
    try {
      releaseTrustedKeysFromEnv(releaseKey.ed25519, raw);
    } catch (e) {
      expect((e as Error).message).not.toContain(raw);
    }
  });

  it("still refuses a wrong current key", () => {
    const x = generateKeyPairSync("ed25519").publicKey.export({ format: "jwk" }).x!;
    expect(() => releaseTrustedKeysFromEnv(Buffer.from(x, "base64url").toString("base64"), undefined)).toThrow(/does not match the shipped app/);
  });
});
