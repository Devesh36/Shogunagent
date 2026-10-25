import { z } from "../define.js";
import type { Json } from "../types.js";

/** GitHub REST requires a User-Agent; the API version is pinned. The transport adds auth only. */
export const GITHUB_HEADERS = { "user-agent": "shogunagent-router", "x-github-api-version": "2022-11-28" } as const;

/** An owner or repository name: safe to place in a path segment. */
export const repoPart = z.string().min(1).max(100).regex(/^[A-Za-z0-9_.-]+$/, "letters, digits, _ . - only");

type Row = Record<string, Json>;

const names = (v: Json | undefined, key: string): Json[] => (Array.isArray(v) ? (v as Row[]).map((x) => (typeof x === "string" ? x : (x[key] ?? null))) : []);

/** GitHub's issue resource → the fields an agent plans with. */
export function mapIssue(r: Row): Json {
  return {
    number: r["number"] ?? null,
    title: r["title"] ?? null,
    state: r["state"] ?? null,
    labels: names(r["labels"], "name"),
    assignees: names(r["assignees"], "login"),
    author: ((r["user"] ?? {}) as Row)["login"] ?? null,
    created_at: r["created_at"] ?? null,
    updated_at: r["updated_at"] ?? null,
    url: r["html_url"] ?? null,
  };
}
