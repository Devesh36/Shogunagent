// Content negotiation in front of the static assets: agents asking for Markdown get Markdown.
import { test } from "node:test";
import assert from "node:assert/strict";
import worker, { prefersMarkdown, route } from "../src/worker.js";

const ORIGIN = "https://shogunagent.com";

test("prefersMarkdown: browsers never, agents when they say so", () => {
  const chrome = "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8";
  assert.equal(prefersMarkdown(chrome), false);
  assert.equal(prefersMarkdown(undefined), false);
  assert.equal(prefersMarkdown(""), false);
  assert.equal(prefersMarkdown("*/*"), false);
  assert.equal(prefersMarkdown("text/markdown"), true);
  assert.equal(prefersMarkdown("text/plain"), true);
  assert.equal(prefersMarkdown("text/markdown, text/html;q=0.5"), true);
  assert.equal(prefersMarkdown("text/html, text/markdown;q=0.5"), false);
  assert.equal(prefersMarkdown("text/html;q=0.9, text/markdown;q=0.9"), false, "equal q: first listed wins");
  assert.equal(prefersMarkdown("text/markdown;q=0.9, text/html;q=0.9"), true);
  assert.equal(prefersMarkdown("text/markdown;q=0"), false);
});

test("route: only / and /index.html negotiate; other paths pass through", () => {
  const req = (path, accept) => new Request(ORIGIN + path, { headers: accept ? { accept } : {} });
  assert.deepEqual(route(req("/", "text/markdown")), { path: "/", assetPath: "/index.md", negotiated: true });
  assert.deepEqual(route(req("/index.html", "text/markdown")), { path: "/index.html", assetPath: "/index.md", negotiated: true });
  assert.deepEqual(route(req("/?agent=1", "text/html")), { path: "/", assetPath: "/index.md", negotiated: true });
  assert.deepEqual(route(req("/", "text/html")), { path: "/", assetPath: "/", negotiated: false });
  assert.deepEqual(route(req("/llms.txt", "text/html")), { path: "/llms.txt", assetPath: "/llms.txt", negotiated: false });
  assert.deepEqual(route(req("/llms.txt/", "text/markdown")), { path: "/llms.txt", assetPath: "/llms.txt", negotiated: false });
});

function fakeAssets(files) {
  return {
    async fetch(request) {
      const url = new URL(request.url);
      const key = url.pathname === "/" ? "/index.html" : url.pathname;
      const body = files[key];
      if (body === undefined) return new Response("not found", { status: 404 });
      const type = key.endsWith(".html") ? "text/html; charset=utf-8" : "application/octet-stream";
      return new Response(request.method === "HEAD" ? null : body, { status: 200, headers: { "content-type": type } });
    },
  };
}

const env = { ASSETS: fakeAssets({ "/index.html": "<html>human</html>", "/index.md": "# agent", "/llms.txt": "# llms" }) };

test("worker: an agent's GET / returns Markdown with the right type and Content-Location", async () => {
  const res = await worker.fetch(new Request(ORIGIN + "/", { headers: { accept: "text/markdown" } }), env);
  assert.equal(res.status, 200);
  assert.equal(await res.text(), "# agent");
  assert.equal(res.headers.get("content-type"), "text/markdown; charset=utf-8");
  assert.equal(res.headers.get("content-location"), `${ORIGIN}/index.md`);
  assert.ok(res.headers.get("vary").includes("Accept"));
});

test("worker: a browser's GET / returns the HTML, with Link headers to the agent documents", async () => {
  const res = await worker.fetch(new Request(ORIGIN + "/", { headers: { accept: "text/html,*/*;q=0.8" } }), env);
  assert.equal(await res.text(), "<html>human</html>");
  assert.equal(res.headers.get("content-type"), "text/html; charset=utf-8");
  assert.equal(res.headers.get("content-location"), null);
  const link = res.headers.get("link");
  assert.ok(link.includes(`<${ORIGIN}/llms.txt>; rel="alternate"; type="text/plain"`));
  assert.ok(link.includes(`<${ORIGIN}/index.md>; rel="alternate"; type="text/markdown"`));
  assert.equal(res.headers.get("content-signal"), "ai-train=yes, search=yes, ai-input=yes");
  assert.equal(res.headers.get("access-control-allow-origin"), "*");
});

test("worker: text files get a charset-bearing text type whatever the asset store said", async () => {
  const res = await worker.fetch(new Request(ORIGIN + "/llms.txt"), env);
  assert.equal(res.headers.get("content-type"), "text/plain; charset=utf-8");
  assert.equal(await res.text(), "# llms");
});

test("worker: a missing path stays 404 and HEAD has no body", async () => {
  const res = await worker.fetch(new Request(ORIGIN + "/nope"), env);
  assert.equal(res.status, 404);
  const head = await worker.fetch(new Request(ORIGIN + "/llms.txt", { method: "HEAD" }), env);
  assert.equal(head.status, 200);
  assert.equal(await head.text(), "");
});
