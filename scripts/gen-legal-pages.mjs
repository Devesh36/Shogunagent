#!/usr/bin/env node
// E23: render docs/legal/privacy-policy.md and docs/legal/terms.md into the HTML apps/api serves
// at /privacy and /terms (Workers have no filesystem, so the HTML ships as a generated module).
// The markdown is the source; the module is never edited by hand. CI runs `--check`.
//
//   node scripts/gen-legal-pages.mjs --write
//   node scripts/gen-legal-pages.mjs --check
//
// The converter is deliberately small: headings, paragraphs, bullet and numbered lists, tables,
// block quotes, and inline code / bold / italic / links. Everything is HTML-escaped first; links
// keep only http(s), mailto and relative targets.
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const OUT = "apps/api/src/wellknown/legal-content.generated.ts";
const PAGES = [
  { name: "PRIVACY", source: "docs/legal/privacy-policy.md", path: "/privacy" },
  { name: "TERMS", source: "docs/legal/terms.md", path: "/terms" },
];

export function escapeHtml(s) {
  return s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

function safeHref(href) {
  const h = href.trim();
  return /^(https?:\/\/|mailto:|\/|#)/i.test(h) ? h : "#";
}

/** Inline markdown on already-escaped text. Code spans are cut out first so nothing inside them
 * is interpreted. */
export function inline(text) {
  const codes = [];
  let s = escapeHtml(text).replace(/`([^`]+)`/g, (_, code) => {
    codes.push(code);
    return `\u0000${codes.length - 1}\u0000`;
  });
  s = s.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (_, label, href) => `<a href="${safeHref(href)}">${label}</a>`);
  s = s.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
  s = s.replace(/(^|[^*])\*([^*\s][^*]*)\*/g, "$1<em>$2</em>");
  return s.replace(/\u0000(\d+)\u0000/g, (_, i) => `<code>${codes[Number(i)]}</code>`);
}

function slug(text) {
  return text.toLowerCase().replace(/<[^>]+>/g, "").replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");
}

export function markdownToHtml(md) {
  const lines = md.replace(/\r\n/g, "\n").split("\n");
  const out = [];
  let i = 0;
  const isBlockStart = (l) => /^(#{1,6} |[-*] |\d+\. |\||> )/.test(l) || l.trim() === "";
  while (i < lines.length) {
    const line = lines[i];
    if (line.trim() === "") { i += 1; continue; }
    const h = /^(#{1,6}) (.+)$/.exec(line);
    if (h) {
      const level = h[1].length;
      const content = inline(h[2]);
      out.push(`<h${level} id="${slug(content)}">${content}</h${level}>`);
      i += 1;
      continue;
    }
    if (line.startsWith("|")) {
      const rows = [];
      while (i < lines.length && lines[i].startsWith("|")) { rows.push(lines[i]); i += 1; }
      const cells = (r) => r.replace(/^\||\|$/g, "").split("|").map((c) => c.trim());
      const header = cells(rows[0]);
      const body = rows.slice(rows[1] && /^\|[\s:|-]+\|$/.test(rows[1]) ? 2 : 1);
      out.push(`<table><thead><tr>${header.map((c) => `<th>${inline(c)}</th>`).join("")}</tr></thead><tbody>${body.map((r) => `<tr>${cells(r).map((c) => `<td>${inline(c)}</td>`).join("")}</tr>`).join("")}</tbody></table>`);
      continue;
    }
    if (line.startsWith("> ")) {
      const quoted = [];
      while (i < lines.length && lines[i].startsWith("> ")) { quoted.push(lines[i].slice(2)); i += 1; }
      out.push(`<blockquote><p>${inline(quoted.join(" "))}</p></blockquote>`);
      continue;
    }
    const bullet = /^[-*] /;
    const numbered = /^\d+\. /;
    if (bullet.test(line) || numbered.test(line)) {
      const ordered = numbered.test(line);
      const re = ordered ? numbered : bullet;
      const items = [];
      while (i < lines.length && re.test(lines[i])) {
        let item = lines[i].replace(re, "");
        i += 1;
        while (i < lines.length && /^\s{2,}\S/.test(lines[i])) { item += " " + lines[i].trim(); i += 1; }
        items.push(`<li>${inline(item)}</li>`);
      }
      out.push(ordered ? `<ol>${items.join("")}</ol>` : `<ul>${items.join("")}</ul>`);
      continue;
    }
    const para = [];
    while (i < lines.length && !isBlockStart(lines[i])) { para.push(lines[i].trim()); i += 1; }
    if (para.length === 0) { para.push(line.trim()); i += 1; }
    out.push(`<p>${inline(para.join(" "))}</p>`);
  }
  return out.join("\n");
}

export function renderPage(md) {
  const title = (/^# (.+)$/m.exec(md)?.[1] ?? "ShogunAgent").trim();
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>${escapeHtml(title)}</title>
<style>body{font:16px/1.6 system-ui,sans-serif;max-width:46rem;margin:2rem auto;padding:0 1rem;color:#111;background:#fff}
@media (prefers-color-scheme:dark){body{color:#e8e8e8;background:#111}a{color:#8ab4ff}th,td{border-color:#444}}
table{border-collapse:collapse;width:100%;font-size:.92rem}th,td{border:1px solid #ccc;padding:.4rem;text-align:left;vertical-align:top}
blockquote{margin:1rem 0;padding:.5rem 1rem;border-left:4px solid #c90;background:rgba(200,150,0,.08)}code{font-size:.9em;word-break:break-word}</style>
</head><body>
${markdownToHtml(md)}
</body></html>
`;
}

export function renderModule(pages) {
  const lines = [
    "// GENERATED by scripts/gen-legal-pages.mjs from docs/legal/*.md — do not edit. `node scripts/gen-legal-pages.mjs --check` fails when stale.",
    "// The HTML apps/api serves at /privacy and /terms (Workers have no filesystem).",
  ];
  for (const p of pages) lines.push(`export const ${p.name}_HTML: string = ${JSON.stringify(p.html)};`);
  lines.push("");
  return lines.join("\n");
}

function main() {
  const mode = process.argv[2];
  if (mode !== "--write" && mode !== "--check") {
    console.error("usage: gen-legal-pages.mjs --write | --check");
    process.exit(2);
  }
  const pages = PAGES.map((p) => ({ ...p, html: renderPage(readFileSync(join(root, p.source), "utf8")) }));
  const next = renderModule(pages);
  const abs = join(root, OUT);
  if (mode === "--write") {
    writeFileSync(abs, next);
    console.log(`wrote ${relative(root, abs)}`);
    return;
  }
  let current = "";
  try { current = readFileSync(abs, "utf8"); } catch { /* absent */ }
  if (current !== next) {
    console.error(`stale: ${OUT} — run \`pnpm legal:gen\` and commit the result.`);
    process.exit(1);
  }
  console.log("legal-content.generated.ts matches docs/legal/*.md");
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) main();
