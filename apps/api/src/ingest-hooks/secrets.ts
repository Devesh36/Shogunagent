/**
 * Hook secrets (E44; CLAUDE.md invariant 7: never written in clear). Two kinds, both from one
 * operator secret, `INGEST_HOOKS_SECRET` (≥ 32 characters, from the environment or KMS):
 *
 * - **Derived** (GitHub's webhook secret, Google Calendar's channel token): the gateway makes the
 *   secret, so it never stores it. `derived(hook, generation)` is HMAC-SHA256 of the hook id and
 *   its rotation generation under a key derived from the operator secret. Rotating bumps the
 *   generation; the stored row holds only the number.
 * - **Sealed** (Linear's and Slack's signing secrets, which the provider issues, and the
 *   verification token Notion sends): the gateway needs the value to verify an HMAC, so it keeps
 *   an AES-256-GCM envelope bound to the hook id (associated data). A row copied to another hook
 *   fails to open.
 *
 * A Gmail mailbox is routed by `mailboxKey` (an HMAC of the lower-cased address), never stored as
 * the address.
 */
import { createCipheriv, createDecipheriv, createHmac, randomBytes } from "node:crypto";

export const MIN_HOOKS_SECRET_CHARS = 32;
const SEALED_PREFIX = "v1.";
const NONCE_LEN = 12;
const TAG_LEN = 16;

export class HookSecrets {
  private readonly deriveKey: Buffer;
  private readonly sealKey: Buffer;
  private readonly routeKey: Buffer;

  constructor(master: string) {
    if (master.length < MIN_HOOKS_SECRET_CHARS) throw new Error(`INGEST_HOOKS_SECRET must be at least ${MIN_HOOKS_SECRET_CHARS} characters`);
    const sub = (label: string): Buffer => createHmac("sha256", master).update(`shogun/ingest-hook/${label}/v1`).digest();
    this.deriveKey = sub("derive");
    this.sealKey = sub("seal");
    this.routeKey = sub("route");
  }

  /** The secret / channel token of `hookId` at `generation` (base64url, 43 characters). */
  derived(hookId: string, generation: number): string {
    return createHmac("sha256", this.deriveKey).update(`${hookId}\n${generation}`).digest("base64url");
  }

  /** An envelope for `plaintext`, bound to `hookId`. */
  seal(plaintext: string, hookId: string): string {
    const nonce = randomBytes(NONCE_LEN);
    const cipher = createCipheriv("aes-256-gcm", this.sealKey, nonce, { authTagLength: TAG_LEN });
    cipher.setAAD(Buffer.from(hookId, "utf8"));
    const ct = Buffer.concat([cipher.update(plaintext, "utf8"), cipher.final()]);
    return SEALED_PREFIX + Buffer.concat([nonce, ct, cipher.getAuthTag()]).toString("base64url");
  }

  /** The plaintext of an envelope, or `undefined` when it does not open for `hookId`. */
  open(sealed: string, hookId: string): string | undefined {
    if (!sealed.startsWith(SEALED_PREFIX)) return undefined;
    const raw = Buffer.from(sealed.slice(SEALED_PREFIX.length), "base64url");
    if (raw.length < NONCE_LEN + TAG_LEN + 1) return undefined;
    try {
      const decipher = createDecipheriv("aes-256-gcm", this.sealKey, raw.subarray(0, NONCE_LEN), { authTagLength: TAG_LEN });
      decipher.setAAD(Buffer.from(hookId, "utf8"));
      decipher.setAuthTag(raw.subarray(raw.length - TAG_LEN));
      return Buffer.concat([decipher.update(raw.subarray(NONCE_LEN, raw.length - TAG_LEN)), decipher.final()]).toString("utf8");
    } catch {
      return undefined;
    }
  }

  /** The routing key of a Gmail mailbox: HMAC of the trimmed, lower-cased address (hex). */
  mailboxKey(address: string): string {
    return createHmac("sha256", this.routeKey).update(address.trim().toLowerCase()).digest("hex");
  }
}
