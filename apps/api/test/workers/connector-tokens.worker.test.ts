// E48: the connector-token envelope (connector-tokens.ts `KmsSealer`) inside workerd, the runtime
// the deployed Worker uses: `node:crypto` ChaCha20-Poly1305 under `nodejs_compat` seals and opens a
// row, and a row bound to another principal does not open.
import { describe, expect, it } from "vitest";

import { KmsSealer, RowUnreadable, type KeyWrapper, type WrappedDataKey } from "../../src/connector-tokens.js";

/** A stand-in KMS for this test only: the data key travels in clear inside the "wrapped" blob,
 * with the context it was bound to. Not encryption. */
const wrapper: KeyWrapper = {
  provider: "test",
  generate(context: string): Promise<{ key: Uint8Array; wrapped: WrappedDataKey }> {
    const key = crypto.getRandomValues(new Uint8Array(32));
    const ctx = new TextEncoder().encode(context);
    const blob = new Uint8Array(32 + ctx.length);
    blob.set(key, 0);
    blob.set(ctx, 32);
    return Promise.resolve({ key: new Uint8Array(key), wrapped: { keyVersion: "test/v1", blob } });
  },
  unwrap(wrapped: WrappedDataKey, context: string): Promise<Uint8Array> {
    if (new TextDecoder().decode(wrapped.blob.subarray(32)) !== context) return Promise.reject(new Error("wrong context"));
    return Promise.resolve(new Uint8Array(wrapped.blob.subarray(0, 32)));
  },
};

describe("KmsSealer in workerd", () => {
  it("seals and opens a tokenset, bound to its principal and slot", async () => {
    const sealer = new KmsSealer(wrapper);
    const value = JSON.stringify({ access_token: "ya29.worker-sentinel", refresh_token: null, expires_at_ms: 1 });
    const row = await sealer.seal("prn_a", "gmail-tokenset", value);
    expect(await sealer.open("prn_a", "gmail-tokenset", row)).toBe(value);
    await expect(sealer.open("prn_b", "gmail-tokenset", row)).rejects.toBeInstanceOf(RowUnreadable);
  });
});
