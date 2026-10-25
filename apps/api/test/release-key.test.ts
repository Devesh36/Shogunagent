import { generateKeyPairSync } from "node:crypto";
import { describe, expect, it } from "vitest";
import { releaseLicensePublicKeyFromB64 } from "../src/auth.js";
import releaseKey from "../../../packages/config/license-public-key.json";

describe("production licence trust root", () => {
  it("accepts the shipped public key", () => {
    expect(releaseLicensePublicKeyFromB64(releaseKey.ed25519).asymmetricKeyType).toBe("ed25519");
  });
  it("rejects a well-formed but different key before the server starts", () => {
    const x = generateKeyPairSync("ed25519").publicKey.export({ format: "jwk" }).x!;
    const raw = Buffer.from(x, "base64url").toString("base64");
    expect(() => releaseLicensePublicKeyFromB64(raw)).toThrow("does not match the shipped app");
  });
  it("rejects malformed configuration", () => {
    expect(() => releaseLicensePublicKeyFromB64("not-a-key")).toThrow();
  });
});
