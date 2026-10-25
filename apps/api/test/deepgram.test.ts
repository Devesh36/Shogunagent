import { describe, expect, it, vi } from "vitest";
import { DeepgramGrantError, FetchDeepgramGateway } from "../src/deepgram.js";

describe("company Deepgram grant transport", () => {
  it("uses only the fixed endpoint and a 60-second grant; never returns the company key", async () => {
    const request = vi.fn<typeof fetch>().mockResolvedValue(Response.json({ access_token: "a.b.c", expires_in: 60, secret: "discard" }));
    const gateway = new FetchDeepgramGateway("company-key-test", request);
    expect(await gateway.grant()).toEqual({ access_token: "a.b.c", expires_in: 60 });
    expect(request).toHaveBeenCalledWith("https://api.deepgram.com/v1/auth/grant", expect.objectContaining({
      method: "POST", redirect: "manual", body: '{"ttl_seconds":60}',
      headers: { authorization: "Token company-key-test", "content-type": "application/json" },
    }));
    expect(JSON.stringify(gateway)).not.toContain("company-key-test");
  });
  it.each([401, 403, 429, 500])("discards upstream error bodies (%s)", async status => {
    const request = vi.fn<typeof fetch>().mockResolvedValue(new Response("private-provider-body", { status }));
    const error = await new FetchDeepgramGateway("test", request).grant().catch(e => e);
    expect(error).toBeInstanceOf(DeepgramGrantError);
    expect(error.status).toBe(status);
    expect(String(error)).not.toContain("private-provider-body");
    expect(request).toHaveBeenCalledTimes(1);
  });
  it.each([{}, { access_token: "a.b.c" }, { access_token: "a.b.c", expires_in: 61 },
    { access_token: "a.b.c", expires_in: 0 }, { access_token: "a.b.c", expires_in: 1.5 },
    { access_token: "bad\nheader", expires_in: 60 }])("rejects malformed or overlong grants", async body => {
    const request = vi.fn<typeof fetch>().mockResolvedValue(Response.json(body));
    await expect(new FetchDeepgramGateway("test", request).grant()).rejects.toBeInstanceOf(DeepgramGrantError);
  });
  it("bounds response size", async () => {
    const request = vi.fn<typeof fetch>().mockResolvedValue(new Response("x".repeat(16_385)));
    await expect(new FetchDeepgramGateway("test", request).grant()).rejects.toBeInstanceOf(DeepgramGrantError);
  });
  it("sanitizes transport failures without retrying a possibly accepted mint", async () => {
    const request = vi.fn<typeof fetch>().mockRejectedValue(new Error("secret transport detail"));
    await expect(new FetchDeepgramGateway("test", request).grant()).rejects.toThrow("speech grant unavailable");
    expect(request).toHaveBeenCalledTimes(1);
  });
});
