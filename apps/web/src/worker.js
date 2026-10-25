// shogunagent.com on Cloudflare Workers with static assets (wrangler.jsonc `assets`).
//
// The assets binding serves dist/ as is. This script runs in front of it for one reason: an
// agent that asks for `/` with `Accept: text/markdown` (or `text/plain`) ranked above HTML, or
// with `?agent=1`, gets the Markdown rendering of the page instead of the HTML. Every response
// also carries `Link` headers pointing at /llms.txt and /index.md, and a `Content-Signal`
// header, so an agent that only looks at headers still finds the machine-readable documents.
//
// No body is read, logged or stored here; nothing is personal. There is no analytics.

const MARKDOWN_TYPES = ["text/markdown", "text/plain"];

/** Parse an Accept header into [{type, q}] in preference order. */
export function parseAccept(header) {
  if (!header) return [];
  return header
    .split(",")
    .map((part, index) => {
      const [rawType, ...params] = part.trim().split(";");
      const type = (rawType ?? "").trim().toLowerCase();
      let q = 1;
      for (const p of params) {
        const [k, v] = p.trim().split("=");
        if (k === "q" && v !== undefined) {
          const n = Number(v);
          q = Number.isFinite(n) ? n : 0;
        }
      }
      return { type, q, index };
    })
    .filter((e) => e.type && e.q > 0)
    .sort((a, b) => b.q - a.q || a.index - b.index);
}

/** True when the client ranks Markdown or plain text above HTML. A browser never does. */
export function prefersMarkdown(acceptHeader) {
  const entries = parseAccept(acceptHeader);
  if (entries.length === 0) return false;
  const score = (pred) => {
    const hit = entries.find(pred);
    return hit ? hit.q : 0;
  };
  const md = score((e) => MARKDOWN_TYPES.includes(e.type));
  const html = score((e) => e.type === "text/html" || e.type === "application/xhtml+xml");
  if (md === 0) return false;
  if (html === 0) return true;
  if (md > html) return true;
  if (md < html) return false;
  // Equal q: the one listed first wins.
  const mdIdx = entries.find((e) => MARKDOWN_TYPES.includes(e.type)).index;
  const htmlIdx = entries.find((e) => e.type === "text/html" || e.type === "application/xhtml+xml").index;
  return mdIdx < htmlIdx;
}

const TEXT_TYPES = {
  ".md": "text/markdown; charset=utf-8",
  ".txt": "text/plain; charset=utf-8",
  ".xml": "application/xml; charset=utf-8",
};

function extOf(pathname) {
  const i = pathname.lastIndexOf(".");
  return i === -1 ? "" : pathname.slice(i).toLowerCase();
}

/** Decide which asset path answers `request`. Exported for tests; pure. */
export function route(request) {
  const url = new URL(request.url);
  const path = url.pathname.replace(/\/+$/, "") || "/";
  const wantsMarkdown =
    (path === "/" || path === "/index.html") &&
    (url.searchParams.get("agent") === "1" || prefersMarkdown(request.headers.get("accept")));
  return { path, assetPath: wantsMarkdown ? "/index.md" : path, negotiated: wantsMarkdown };
}

function withHeaders(response, origin, { negotiated, assetPath }) {
  const headers = new Headers(response.headers);
  headers.set("Link", `<${origin}/llms.txt>; rel="alternate"; type="text/plain", <${origin}/index.md>; rel="alternate"; type="text/markdown"`);
  headers.set("Content-Signal", "ai-train=yes, search=yes, ai-input=yes");
  headers.set("X-Content-Type-Options", "nosniff");
  headers.set("Referrer-Policy", "strict-origin-when-cross-origin");
  headers.set("Access-Control-Allow-Origin", "*");
  const ext = extOf(assetPath);
  if (TEXT_TYPES[ext]) headers.set("Content-Type", TEXT_TYPES[ext]);
  if (headers.get("Content-Type")?.split(";")[0].trim().toLowerCase() === "text/html") {
    headers.set("Content-Type", "text/html; charset=utf-8");
  }
  headers.append("Vary", "Accept");
  if (negotiated) headers.set("Content-Location", `${origin}/index.md`);
  return new Response(response.body, { status: response.status, statusText: response.statusText, headers });
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.hostname === "www.shogunagent.com") {
      url.hostname = "shogunagent.com";
      url.protocol = "https:";
      return Response.redirect(url.href, 301);
    }
    const r = route(request);
    const assetUrl = new URL(r.assetPath, url.origin);
    const assetRequest = new Request(assetUrl, { method: request.method === "HEAD" ? "HEAD" : "GET", headers: request.headers });
    const response = await env.ASSETS.fetch(assetRequest);
    return withHeaders(response, url.origin, r);
  },
};
