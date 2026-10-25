/** Company-key grant transport. Never expose the long-lived key to a device.
 * Admission and usage accounting must run before calling this transport.
 * Token expiry limits new handshakes, NOT the duration of an established stream.
 */
export const DEEPGRAM_GRANT_TTL_SECONDS = 60;
const MAX_GRANT_BYTES = 16_384;

export interface DeepgramGrant { access_token: string; expires_in: number }
export interface DeepgramGateway { grant(): Promise<DeepgramGrant> }

/** Status only: upstream errors may contain credentials; never retain their bodies/causes. */
export class DeepgramGrantError extends Error {
  constructor(readonly status: number) {
    super("speech grant unavailable");
    this.name = "DeepgramGrantError";
  }
}

export class FetchDeepgramGateway implements DeepgramGateway {
  #apiKey: string;
  constructor(apiKey: string, private readonly request: typeof fetch = (input, init) => fetch(input, init)) {
    if (!apiKey.trim()) throw new Error("DEEPGRAM_API_KEY is required");
    this.#apiKey = apiKey.trim();
  }

  async grant(): Promise<DeepgramGrant> {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 10_000);
    try {
      const response = await this.request("https://api.deepgram.com/v1/auth/grant", {
        method: "POST", redirect: "manual", signal: controller.signal,
        headers: { authorization: `Token ${this.#apiKey}`, "content-type": "application/json" },
        body: JSON.stringify({ ttl_seconds: DEEPGRAM_GRANT_TTL_SECONDS }),
      });
      if (!response.ok) {
        await response.body?.cancel().catch(() => undefined);
        throw new DeepgramGrantError(response.status);
      }
      const reader = response.body?.getReader();
      if (!reader) throw new DeepgramGrantError(502);
      let bytes = 0;
      let text = "";
      const decoder = new TextDecoder();
      try {
        for (;;) {
          const chunk = await reader.read();
          if (chunk.done) break;
          bytes += chunk.value.byteLength;
          if (bytes > MAX_GRANT_BYTES) throw new DeepgramGrantError(502);
          text += decoder.decode(chunk.value, { stream: true });
        }
        text += decoder.decode();
      } finally {
        await reader.cancel().catch(() => undefined);
        reader.releaseLock();
      }
      const body: unknown = JSON.parse(text);
      if (!body || typeof body !== "object") throw new DeepgramGrantError(502);
      const { access_token: token, expires_in: expires } = body as Record<string, unknown>;
      if (typeof token !== "string" || !/^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+$/.test(token)
        || typeof expires !== "number" || !Number.isInteger(expires) || expires <= 0
        || expires > DEEPGRAM_GRANT_TTL_SECONDS) throw new DeepgramGrantError(502);
      return { access_token: token, expires_in: expires };
    } catch (error) {
      if (error instanceof DeepgramGrantError) throw error;
      throw new DeepgramGrantError(0);
    } finally {
      clearTimeout(timeout);
    }
  }
}
