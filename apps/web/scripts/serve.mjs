// Local preview of dist/ with the same content negotiation as the deployed Worker.
//   pnpm --filter @shogun-ai/web dev        → http://127.0.0.1:8788/
// Loopback only. Not a production server.
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { dirname, join, normalize } from "node:path";
import { fileURLToPath } from "node:url";
import worker from "../src/worker.js";

const dist = join(dirname(fileURLToPath(import.meta.url)), "..", "dist");
const port = Number(process.env.PORT ?? 8788);

const TYPES = { ".html": "text/html; charset=utf-8", ".md": "text/markdown; charset=utf-8", ".txt": "text/plain; charset=utf-8", ".xml": "application/xml; charset=utf-8" };

const ASSETS = {
  async fetch(request) {
    const url = new URL(request.url);
    let p = decodeURIComponent(url.pathname);
    if (p.endsWith("/")) p += "index.html";
    const file = normalize(join(dist, p));
    if (!file.startsWith(dist)) return new Response("forbidden", { status: 403 });
    try {
      const body = await readFile(file);
      const ext = file.slice(file.lastIndexOf("."));
      return new Response(body, { status: 200, headers: { "content-type": TYPES[ext] ?? "application/octet-stream" } });
    } catch {
      return new Response("not found", { status: 404 });
    }
  },
};

createServer(async (req, res) => {
  const request = new Request(`http://127.0.0.1:${port}${req.url}`, { method: req.method, headers: req.headers });
  const response = await worker.fetch(request, { ASSETS });
  res.writeHead(response.status, Object.fromEntries(response.headers));
  res.end(Buffer.from(await response.arrayBuffer()));
}).listen(port, "127.0.0.1", () => console.log(`shogunagent.com preview: http://127.0.0.1:${port}/`));
