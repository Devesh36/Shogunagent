/**
 * The operator calls the enterprise lane makes to the Rust sidecar (E26), through one client so
 * tests swap in a fake and production has one place that holds the operator bearer
 * (`SHOGUN_SIDECAR_ADMIN_TOKEN`, the sidecar's `SHOGUN_ADMIN_TOKEN`):
 *
 *   POST   {sidecar}/v1/teams                 {tenant, team, members}         define a Team (E18)
 *   POST   {sidecar}/v1/acl/grant             {kind:"principal", principal, grantee_principal}
 *   POST   {sidecar}/v1/acl/revoke            (same body)                     world-read between teammates
 *   GET    {sidecar}/v1/audit/export?principal=<id>[&since_ms=&until_ms=]   one principal's audit export
 *   DELETE {sidecar}/v1/principals/<id>       purge a principal's store (SCIM delete, after the grace period)
 *
 * The audit export body is handed back as a stream and never read here (`app.ts` frames it). The
 * team and grant answers are read for their status only. Nothing is logged.
 */
export type SidecarFetch = (input: string, init?: RequestInit) => Promise<Response>;

export type OpResult = { ok: true } | { ok: false; status: number; code: string };
export type ExportResult = { ok: true; body: ReadableStream<Uint8Array> | null; contentType: string } | { ok: false; status: number; code: string };

export interface EnterpriseSidecar {
  setTeam(tenantId: string, teamId: string, members: readonly string[]): Promise<OpResult>;
  grantWorldRead(owner: string, grantee: string): Promise<OpResult>;
  revokeWorldRead(owner: string, grantee: string): Promise<OpResult>;
  auditExport(principalId: string, range: { sinceMs?: number; untilMs?: number }): Promise<ExportResult>;
  purgePrincipal(principalId: string): Promise<OpResult>;
}

async function codeOf(res: Response): Promise<string> {
  try {
    const body = (await res.json()) as { error?: unknown };
    const code = typeof body.error === "string" ? body.error : typeof (body.error as { code?: unknown })?.code === "string" ? (body.error as { code: string }).code : undefined;
    if (code && /^[a-z_]{1,64}$/.test(code)) return code;
  } catch {
    // A non-JSON error is reported by status only.
  }
  return "sidecar_error";
}

export function httpEnterpriseSidecar(baseUrl: string, adminToken: string, fetchFn: SidecarFetch = (input, init) => fetch(input, init)): EnterpriseSidecar {
  const base = baseUrl.replace(/\/+$/, "");
  const headers = { authorization: `Bearer ${adminToken}`, "content-type": "application/json", accept: "application/json" };
  const call = async (method: string, path: string, body?: unknown): Promise<OpResult> => {
    let res: Response;
    try {
      res = await fetchFn(`${base}${path}`, { method, headers, ...(body !== undefined ? { body: JSON.stringify(body) } : {}) });
    } catch {
      return { ok: false, status: 503, code: "sidecar_unavailable" };
    }
    if (res.ok) {
      await res.body?.cancel().catch(() => undefined);
      return { ok: true };
    }
    return { ok: false, status: res.status, code: await codeOf(res) };
  };
  return {
    setTeam: (tenantId, teamId, members) => call("POST", "/v1/teams", { tenant: tenantId, team: teamId, members }),
    grantWorldRead: (owner, grantee) => call("POST", "/v1/acl/grant", { kind: "principal", principal: owner, grantee_principal: grantee, granted_by: "system:scim" }),
    revokeWorldRead: (owner, grantee) => call("POST", "/v1/acl/revoke", { kind: "principal", principal: owner, grantee_principal: grantee, granted_by: "system:scim" }),
    async auditExport(principalId, range) {
      const q = new URLSearchParams({ principal: principalId });
      if (range.sinceMs !== undefined) q.set("since_ms", String(range.sinceMs));
      if (range.untilMs !== undefined) q.set("until_ms", String(range.untilMs));
      let res: Response;
      try {
        res = await fetchFn(`${base}/v1/audit/export?${q.toString()}`, { method: "GET", headers: { authorization: headers.authorization, accept: "application/x-ndjson" } });
      } catch {
        return { ok: false, status: 503, code: "sidecar_unavailable" };
      }
      if (!res.ok) return { ok: false, status: res.status, code: await codeOf(res) };
      return { ok: true, body: res.body, contentType: res.headers.get("content-type") ?? "application/x-ndjson" };
    },
    purgePrincipal: (principalId) => call("DELETE", `/v1/principals/${encodeURIComponent(principalId)}`),
  };
}
