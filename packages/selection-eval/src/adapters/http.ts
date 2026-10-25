/**
 * The one fetch wrapper the adapters share. `fetch` is injectable so tests shape requests and
 * replay responses without a network. Keys travel only in headers and are never logged: an error
 * message carries the status and the first 300 characters of the body, nothing from the request.
 */

export type FetchLike = (input: string, init: RequestInit) => Promise<Response>;

export interface HttpOptions {
  fetch?: FetchLike;
  timeoutMs?: number;
}

export async function postJson(
  url: string,
  headers: Record<string, string>,
  body: unknown,
  opts: HttpOptions = {},
): Promise<unknown> {
  const f: FetchLike = opts.fetch ?? ((i, init) => fetch(i, init));
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), opts.timeoutMs ?? 60_000);
  try {
    const res = await f(url, {
      method: "POST",
      headers: { "content-type": "application/json", ...headers },
      body: JSON.stringify(body),
      signal: controller.signal,
    });
    const text = await res.text();
    if (!res.ok) {
      throw new Error(`${new URL(url).host} responded ${res.status}: ${text.slice(0, 300)}`);
    }
    return text.length ? (JSON.parse(text) as unknown) : null;
  } finally {
    clearTimeout(timer);
  }
}

export function asRecord(v: unknown): Record<string, unknown> | null {
  return v && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
}

export function asArray(v: unknown): unknown[] {
  return Array.isArray(v) ? v : [];
}

export function tryParseJson(v: unknown): unknown {
  if (typeof v !== "string") return v;
  try {
    return JSON.parse(v) as unknown;
  } catch {
    return v;
  }
}
