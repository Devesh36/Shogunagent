/**
 * The one client every approval surface (web page, Slack, notifications) uses to reach the Rust
 * sidecar's approval API (plan E15):
 *
 *   GET  {sidecar}/v1/approvals/{id}          X-Shogun-Approval-Token: <token>   → redacted summary
 *   POST {sidecar}/v1/approvals/{id}/decide   X-Shogun-Approval-Token: <token>
 *        {"decision": "approve"|"reject", "decided_by": "<kind>:<id>", "nonce": "<token nonce>"}
 *
 * The summary is the sidecar's redacted view (kind, tier, recipient class, a masked one-line
 * preview, times, status, decision) and this module keeps only those fields — anything else the
 * sidecar might add never reaches a page, a Slack message or a webhook. Nothing here is logged or
 * stored.
 */
import { TOKEN_HEADER } from "./token.js";

export type FetchFn = (input: string, init?: RequestInit) => Promise<Response>;

/** The redacted summary, allowlisted. */
export interface ApprovalSummary {
  approval_id: number;
  status: string;
  level: string;
  kind?: string;
  recipient_class?: string;
  preview?: string;
  created_at?: string;
  expires_at?: string;
  decision?: { decision: string; decided_by: string; decided_at: string };
}

export interface DecisionResult {
  approval_id: number;
  decision: "approve" | "reject";
  status: string;
  decided_by: string;
  decided_at: string;
}

export type SidecarResult<T> = { ok: true; value: T } | { ok: false; status: number; code: string };

export interface SidecarClient {
  summary(token: string, approvalId: number): Promise<SidecarResult<ApprovalSummary>>;
  decide(token: string, approvalId: number, decision: "approve" | "reject", decidedBy: string, nonce: string): Promise<SidecarResult<DecisionResult>>;
}

const str = (v: unknown): string | undefined => (typeof v === "string" ? v : undefined);

/** Keep the allowlisted summary fields of a sidecar answer. */
export function pickSummary(raw: unknown): ApprovalSummary | undefined {
  if (typeof raw !== "object" || raw === null) return undefined;
  const r = raw as Record<string, unknown>;
  if (typeof r.approval_id !== "number" || typeof r.status !== "string") return undefined;
  const out: ApprovalSummary = { approval_id: r.approval_id, status: r.status, level: str(r.level) ?? "L3" };
  for (const key of ["kind", "recipient_class", "preview", "created_at", "expires_at"] as const) {
    const v = str(r[key]);
    if (v !== undefined) out[key] = v;
  }
  if (typeof r.decision === "object" && r.decision !== null) {
    const d = r.decision as Record<string, unknown>;
    const decision = str(d.decision), by = str(d.decided_by), at = str(d.decided_at);
    if (decision && by && at) out.decision = { decision, decided_by: by, decided_at: at };
  }
  return out;
}

async function errorOf(res: Response): Promise<{ ok: false; status: number; code: string }> {
  let code = "sidecar_error";
  try {
    const body = (await res.json()) as { error?: unknown };
    if (typeof body.error === "string" && /^[a-z_]{1,64}$/.test(body.error)) code = body.error;
  } catch {
    // A non-JSON error is reported by status only.
  }
  return { ok: false, status: res.status, code };
}

/** The client over HTTP. `baseUrl` is the sidecar (`SHOGUN_SIDECAR_URL`, e.g. `http://127.0.0.1:7464`). */
export function httpSidecarClient(baseUrl: string, fetchFn: FetchFn = (input, init) => fetch(input, init)): SidecarClient {
  const base = baseUrl.replace(/\/+$/, "");
  const call = async (path: string, init: RequestInit): Promise<Response | undefined> => {
    try {
      return await fetchFn(`${base}${path}`, init);
    } catch {
      return undefined;
    }
  };
  return {
    async summary(token, approvalId) {
      const res = await call(`/v1/approvals/${approvalId}`, { method: "GET", headers: { [TOKEN_HEADER]: token, accept: "application/json" } });
      if (!res) return { ok: false, status: 503, code: "sidecar_unavailable" };
      if (!res.ok) return errorOf(res);
      const body = (await res.json().catch(() => undefined)) as { approval?: unknown } | undefined;
      const summary = pickSummary(body?.approval);
      return summary ? { ok: true, value: summary } : { ok: false, status: 502, code: "sidecar_malformed" };
    },
    async decide(token, approvalId, decision, decidedBy, nonce) {
      const res = await call(`/v1/approvals/${approvalId}/decide`, {
        method: "POST",
        headers: { [TOKEN_HEADER]: token, "content-type": "application/json" },
        body: JSON.stringify({ decision, decided_by: decidedBy, nonce }),
      });
      if (!res) return { ok: false, status: 503, code: "sidecar_unavailable" };
      if (!res.ok) return errorOf(res);
      const body = (await res.json().catch(() => undefined)) as Record<string, unknown> | undefined;
      if (!body || typeof body.approval_id !== "number") return { ok: false, status: 502, code: "sidecar_malformed" };
      return {
        ok: true,
        value: {
          approval_id: body.approval_id, decision, status: str(body.status) ?? decision,
          decided_by: str(body.decided_by) ?? decidedBy, decided_at: str(body.decided_at) ?? "",
        },
      };
    },
  };
}
