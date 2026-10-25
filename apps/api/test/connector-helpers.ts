/** A fake principal sidecar for the connector routes (E50): it speaks the contract of
 * `crates/shogun-mcp/src/connector_vault.rs` (admin credential + the principal's sidecar bearer,
 * `POST /v1/internal/connectors/{token,connect/start,connect/finish}`), verifies the derived `sgw1`
 * bearer the way the Rust `DerivedBearers` does, and keeps grants in memory. The Rust side of the
 * same contract is proved in `crates/shogun-core/tests/connector_vault.rs`. */
import { derivedSidecarBearer, type FetchFn } from "../src/mcp-proxy.js";

export const SIDECAR_URL = "http://sidecar.test";
export const ADMIN_TOKEN = "sidecar-admin-token-0123456789";
export const BEARER_SECRET = "gateway-sidecar-derived-bearer-secret-e50-0123";

export interface FakeGrant {
  accessToken: string;
  expiresAt: number;
}

export interface SidecarCall {
  path: string;
  authorization: string | null;
  principalHeader: string | null;
  body: Record<string, unknown>;
}

export class FakeSidecar {
  /** principal → slot → grant. */
  readonly grants = new Map<string, Map<string, FakeGrant>>();
  /** state → { principal, provider }, single use. */
  readonly flows = new Map<string, { principal: string; provider: string }>();
  readonly calls: SidecarCall[] = [];
  /** Forced answer for the next calls (status + body), e.g. a 503 or a 410. */
  forced: { status: number; body: unknown } | undefined;
  down = false;
  /** The code the fake vendor accepts. */
  goodCode = "vendor-code-1";
  /** The token the fake vendor issues on a finished flow. */
  connectedToken = "ya29.connected-by-flow-0001";
  /** What GitHub answers the next device polls: "pending" until it is "approved" or "denied". */
  devicePolls: ("pending" | "approved" | "denied")[] = [];
  /** What the next disconnect's vendor revocation does. */
  vendorRevoke = "revoked";

  grant(principal: string, slot: string, g: FakeGrant): void {
    let slots = this.grants.get(principal);
    if (!slots) this.grants.set(principal, (slots = new Map()));
    slots.set(slot, g);
  }

  /** The principal the bearer names, when it is a derived bearer signed with the shared secret. */
  private principalOf(header: string | null): string | undefined {
    const bearer = header?.replace(/^Bearer\s+/i, "") ?? "";
    const parts = bearer.split(".");
    if (parts.length !== 4 || parts[0] !== "sgw1") return undefined;
    const [, principalId, keyId] = parts as [string, string, string, string];
    return derivedSidecarBearer(BEARER_SECRET, { principalId, keyId }) === bearer ? principalId : undefined;
  }

  readonly fetch: FetchFn = async (input, init) => {
    if (this.down) throw new TypeError("fetch failed");
    const url = new URL(input);
    const headers = new Headers(init?.headers);
    const body = JSON.parse(typeof init?.body === "string" ? init.body : "{}") as Record<string, unknown>;
    this.calls.push({ path: url.pathname, authorization: headers.get("authorization"), principalHeader: headers.get("x-shogun-principal-authorization"), body });
    const json = (status: number, b: unknown) => new Response(JSON.stringify(b), { status, headers: { "content-type": "application/json", "cache-control": "no-store" } });
    if (url.origin !== SIDECAR_URL) return json(404, { error: "not_found" });
    if (this.forced) return json(this.forced.status, this.forced.body);
    if (headers.get("authorization") !== `Bearer ${ADMIN_TOKEN}`) return json(401, { error: "unauthorized" });
    const principal = this.principalOf(headers.get("x-shogun-principal-authorization"));
    if (!principal) return json(401, { error: "unauthorized" });
    await Promise.resolve();
    switch (url.pathname) {
      case "/v1/internal/connectors/token": {
        const g = this.grants.get(principal)?.get(String(body.slot));
        if (!g) return json(404, { error: "connector_not_connected", message: "…", next_action: "authenticate" });
        return json(200, { access_token: g.accessToken, token_type: "Bearer", expires_at: g.expiresAt });
      }
      case "/v1/internal/connectors/connect/start": {
        const state = String(body.state);
        this.flows.set(state, { principal, provider: String(body.provider) });
        if (body.provider === "github") {
          return json(200, { authorize_url: "https://github.com/login/device", expires_at: 1, device: { user_code: "WDJB-MJHT", verification_uri: "https://github.com/login/device", interval_ms: 5000 } });
        }
        return json(200, { authorize_url: `https://accounts.vendor.test/auth?state=${encodeURIComponent(state)}&code_challenge=c&code_challenge_method=S256`, expires_at: 1 });
      }
      case "/v1/internal/connectors/connect/poll": {
        const flow = this.flows.get(String(body.state));
        if (!flow || flow.principal !== principal || flow.provider !== "github") return json(404, { error: "connect_flow_unknown" });
        const next = this.devicePolls.shift() ?? "pending";
        if (next === "pending") return json(200, { status: "pending", device: { user_code: "WDJB-MJHT", verification_uri: "https://github.com/login/device", interval_ms: 5000 }, expires_at: 1 });
        this.flows.delete(String(body.state));
        if (next === "denied") return json(400, { error: "connect_rejected" });
        this.grant(principal, "github-tokenset", { accessToken: "ghu_connected_by_device_1", expiresAt: Date.now() + 3_600_000 });
        return json(200, { connected: true, provider: "github", slot: "github-tokenset" });
      }
      case "/v1/internal/connectors/status": {
        const slots = this.grants.get(principal) ?? new Map<string, FakeGrant>();
        const row = (service: string, provider: string | null, slot: string | null) => ({
          service, provider, state: slot === null ? "coming_soon" : slots.has(slot) ? "connected" : "disconnected", connected: slot !== null && slots.has(slot), connectable: slot !== null, expires_at: slot && slots.get(slot) ? slots.get(slot)?.expiresAt : null,
          // A field the gateway must not pass on.
          account_label: "someone@example.test",
        });
        return json(200, { connectors: [row("gmail", "gmail", "gmail-tokenset"), row("gcal", "calendar", "gcal-tokenset"), row("gdrive", null, null), row("github", "github", "github-tokenset")] });
      }
      case "/v1/internal/connectors/disconnect": {
        const provider = String(body.provider);
        const slot = provider === "calendar" ? "gcal-tokenset" : `${provider}-tokenset`;
        const removed = this.grants.get(principal)?.delete(slot) ?? false;
        return json(200, { disconnected: true, provider, slot, removed, vendor_revoke: removed ? this.vendorRevoke : "nothing_stored" });
      }
      case "/v1/internal/connectors/connect/finish": {
        const flow = this.flows.get(String(body.state));
        this.flows.delete(String(body.state));
        if (!flow || flow.principal !== principal) return json(404, { error: "connect_flow_unknown" });
        if (body.code !== this.goodCode) return json(400, { error: "connect_rejected" });
        const slot = flow.provider === "calendar" ? "gcal-tokenset" : `${flow.provider}-tokenset`;
        this.grant(principal, slot, { accessToken: this.connectedToken, expiresAt: Date.now() + 3_600_000 });
        return json(200, { connected: true, provider: flow.provider, slot });
      }
      default:
        return json(404, { error: "not_found" });
    }
  };
}
