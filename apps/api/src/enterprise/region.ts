/**
 * Region separation (E26). A tenant has a `region` (us | eu | jp | …). Each region is its own
 * deployment — its own Worker, its own Postgres, its own sidecar fleet and KMS — and a deployment
 * serves exactly one region (`SHOGUN_REGION`). Deployments share no database and never call each
 * other: there is no cross-region data path to configure, so none exists. What a deployment knows
 * about other regions is only their public base URLs (`SHOGUN_REGION_ENDPOINTS`), so it can tell a
 * caller where to go instead of answering a bare 401.
 *
 * A request whose principal belongs to a tenant of another region (a principal that moved, or a
 * routing stub the operator left on the old deployment) is refused before anything is read or
 * booked: `421 wrong_region`, `next_action: "use_region_endpoint"`, `region` and
 * `region_endpoint` in the error.
 */
import type { AdmitRefusal, AgentPrincipal } from "../agent-keys.js";
import type { EnterpriseStore } from "./store.js";

export const REGION_RE = /^[a-z]{2}(-[a-z0-9]{1,16})?$/;
export const DEFAULT_REGION = "us";
const ADMISSION_TTL_MS = 60_000;
const ADMISSION_CACHE_MAX = 10_000;

export interface RegionConfig {
  /** The region this deployment serves. */
  served: string;
  /** Public base URL per region (`{"eu": "https://eu.api.example"}`), this one included or not. */
  endpoints: Readonly<Record<string, string>>;
}

/** Whether `region` is one this deployment knows (its own, or one with an endpoint). */
export function knownRegion(cfg: RegionConfig, region: string): boolean {
  return region === cfg.served || Object.prototype.hasOwnProperty.call(cfg.endpoints, region);
}

export function regionRefusal(cfg: RegionConfig, region: string): AdmitRefusal {
  const endpoint = cfg.endpoints[region];
  return {
    status: 421, code: "wrong_region",
    message: endpoint
      ? `This principal is served in region ${region}. Send the request to ${endpoint} instead; this deployment (${cfg.served}) holds none of its data.`
      : `This principal is served in region ${region}, which this deployment (${cfg.served}) does not serve. Ask the operator for that region's endpoint.`,
    nextAction: "use_region_endpoint",
    extra: { region, ...(endpoint ? { region_endpoint: endpoint } : {}) },
  };
}

/** The `admit` hook for the key lookup: refuse principals homed in another region. Membership and
 * tenant region are cached per isolate for a minute (a principal's region changes only when an
 * operator moves its tenant, which is a migration, not a toggle). */
export function regionAdmission(store: Pick<EnterpriseStore, "membership" | "tenant">, cfg: RegionConfig): (principal: AgentPrincipal, nowMs: number) => Promise<AdmitRefusal | undefined> {
  const cache = new Map<string, { region: string | null; until: number }>();
  return async (principal, nowMs) => {
    let hit = cache.get(principal.principalId);
    if (!hit || hit.until <= nowMs) {
      const tenantId = await store.membership(principal.principalId);
      const tenant = tenantId ? await store.tenant(tenantId) : undefined;
      hit = { region: tenant?.region ?? null, until: nowMs + ADMISSION_TTL_MS };
      if (cache.size >= ADMISSION_CACHE_MAX) cache.clear();
      cache.set(principal.principalId, hit);
    }
    if (hit.region === null || hit.region === cfg.served) return undefined;
    return regionRefusal(cfg, hit.region);
  };
}

/** Parse `SHOGUN_REGION_ENDPOINTS` (a JSON object of region → https origin). Throws a plain
 * `Error` naming the problem, never echoing a value. */
export function parseRegionEndpoints(raw: string | undefined): Record<string, string> {
  if (raw === undefined || raw.trim() === "") return {};
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    throw new Error("not JSON");
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) throw new Error("not an object");
  const out: Record<string, string> = {};
  for (const [region, url] of Object.entries(parsed as Record<string, unknown>)) {
    if (!REGION_RE.test(region)) throw new Error("a region key is not a region code");
    if (typeof url !== "string" || !/^https:\/\/[^\s/]+$/.test(url)) throw new Error("a value is not an https origin");
    out[region] = url;
  }
  return out;
}
