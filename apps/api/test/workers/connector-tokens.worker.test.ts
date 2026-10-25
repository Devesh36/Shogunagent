// E48 / E50: the connector pieces inside workerd, the runtime the deployed Worker uses: the hosted
// connect flow's sealed links and states (`node:crypto` AES-256-GCM under `nodejs_compat`), and
// the sidecar vault client over a binding-style `fetch` (the same seam the /mcp relay uses).
import { describe, expect, it } from "vitest";

import { openClaims, sealClaims } from "../../src/connector-connect.js";
import { sidecarConnectorVault } from "../../src/connector-tokens.js";

describe("connector credentials in workerd", () => {
  it("seals and opens a connect state, bound to its purpose", () => {
    const secret = "connector-connect-secret-0123456789abcdef";
    const sealed = sealClaims(secret, "state", { v: 1, p: "prn_a", k: "ak_1", prov: "gmail", exp: 1, n: "x", f: "y" });
    expect(sealed).toMatch(/^[A-Za-z0-9_-]+$/);
    expect(sealed).not.toContain("prn_a");
    expect(openClaims(secret, "state", sealed)).toMatchObject({ p: "prn_a", prov: "gmail" });
    expect(openClaims(secret, "link", sealed)).toBeUndefined();
    expect(openClaims("another-secret-of-sufficient-length-0123", "state", sealed)).toBeUndefined();
  });

  it("asks the sidecar through the injected fetch, as the principal's derived bearer", async () => {
    const seen: Array<{ url: string; principal: string | null }> = [];
    const vault = sidecarConnectorVault(
      { relay: { url: "http://sidecar.internal", bearerSecret: "gateway-sidecar-derived-bearer-secret-0123" }, adminToken: "sidecar-admin-token-0123" },
      (input, init) => {
        seen.push({ url: input, principal: new Headers(init?.headers).get("x-shogun-principal-authorization") });
        return Promise.resolve(new Response(JSON.stringify({ access_token: "ya29.worker-sentinel", token_type: "Bearer", expires_at: 5 }), { status: 200 }));
      },
    );
    expect(await vault.accessToken({ principalId: "prn_a", keyId: "ak_1" }, "gmail-tokenset")).toEqual({ kind: "token", accessToken: "ya29.worker-sentinel", expiresAt: 5 });
    expect(seen[0]?.url).toBe("http://sidecar.internal/v1/internal/connectors/token");
    expect(seen[0]?.principal).toMatch(/^Bearer sgw1\.prn_a\.ak_1\./);
  });
});
