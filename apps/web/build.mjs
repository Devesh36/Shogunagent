#!/usr/bin/env node
// shogunagent.com, generated from content/site.json.
//
// Agent-first means the machine-readable documents come first and the HTML is derived from the
// same data (CLAUDE.md invariant 12): `render()` produces index.md, llms.txt, llms-full.txt,
// setup-prompt.txt, robots.txt and sitemap.xml, and index.html is a view over the same object.
// No framework, no runtime dependency: Node 22 and this file.
//
//   node build.mjs            writes dist/
//   import { render } ...     returns { "/index.html": string, ... } for tests

import { readFileSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));

/** Escape text for an HTML text node or attribute value. */
export function esc(s) {
  return String(s)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");
}

/** `code` spans in site.json prose become <code> in HTML; everything else is escaped. */
function inline(s) {
  return esc(s).replace(/`([^`]+)`/g, "<code>$1</code>");
}

function surfaceUrl(site, s) {
  switch (s.host) {
    case "site":
      return site.url + s.path;
    case "api":
      return site.api_url + s.path;
    case "repo":
      return `${site.repo}/blob/main/${s.path}`;
    case "registry":
      return `https://registry.modelcontextprotocol.io/servers/${s.path}`;
    default:
      throw new Error(`unknown surface host: ${s.host}`);
  }
}

const STATUS_LABEL = { live: "live", phase1: "Phase 1" };

/** Every tool wire name, in page order. The HTML and the Markdown must list the same set. */
export function toolNames(site) {
  return site.tools.groups.flatMap((g) => g.items.map((t) => t.name));
}

// ───────────────────────────── Markdown ─────────────────────────────

/** The page as Markdown: `/index.md`, and what `Accept: text/markdown` returns for `/`. */
export function renderMarkdown(site) {
  const L = [];
  const p = (...xs) => L.push(...xs, "");
  p(`# ${site.name}`);
  p(`> ${site.tagline} ${site.motto}`);
  p(site.one_sentence);
  p(`**Status:** ${site.status_line}`);
  p(
    "Reading this as an agent? This page is also served as Markdown for `Accept: text/markdown`, " +
      `and indexed at ${site.url}/llms.txt. Everything that sends, posts or pays is L3 and waits for a human at an approval URL; do not try to route around it.`
  );

  p("## For agents");
  p("| Surface | What | Why an agent needs it | Status |", "|---|---|---|---|");
  for (const s of site.surfaces) {
    L.push(`| ${surfaceUrl(site, s)} | ${s.what} | ${s.why} | ${STATUS_LABEL[s.status]} |`);
  }
  L.push("");
  p("Setup prompt for a coding agent (also at `/setup-prompt.txt`):", "", "```", site.setup_prompt, "```");
  p(`Works with: ${site.works_with.join(", ")}.`);

  p("## What it is");
  p(site.developer_30s);
  p(site.not_a_memory_api);
  for (const pl of site.pillars) {
    p(`### ${pl.title}`, pl.claim, "", `*Proof in the code:* ${pl.proof}`);
  }

  p("## Three faces, one behavior");
  p(site.faces.intro, "", "MCP:", "", "```json", site.faces.mcp, "```", "", "REST:", "", "```sh", site.faces.rest, "```", "", "CLI:", "", "```sh", site.faces.cli, "```");
  p("Every response carries `usage`, `latency_ms`, `hints` and, on error, `next_action`:", "", "```json", site.faces.response, "```");
  p("An exhausted wallet answers 402 in the x402 `payment-required` shape:", "", "```http", site.faces.payment_required, "```");
  p("An L3 action answers with an approval URL, never with a side effect:", "", "```json", site.faces.l3, "```");

  p("## Permission tiers");
  p("| Tier | Name | What happens | Examples | MCP annotation | Cost |", "|---|---|---|---|---|---|");
  for (const t of site.tiers) {
    L.push(`| ${t.level} | ${t.name} | ${t.what} | ${t.examples} | ${t.mcp} | ${t.cost} |`);
  }
  L.push("");

  p("## Tools");
  p(site.tools.note);
  for (const g of site.tools.groups) {
    p(`### ${g.title}`);
    for (const t of g.items) L.push(`- \`${t.name}\`: ${t.task}`);
    L.push("");
  }

  p("## Pricing");
  p(site.pricing.line);
  p("| Metered item | Unit price | Notes |", "|---|---|---|");
  for (const r of site.pricing.rows) L.push(`| ${r.item} | ${r.price} | ${r.note} |`);
  L.push("");
  p(site.pricing.disclaimer);

  p("## Runs everywhere");
  for (const d of site.deploy) p(`### ${d.title}`, d.body);

  p("## Not a memory API");
  p(site.comparison.intro);
  p("| | Memory APIs | ShogunAgent |", "|---|---|---|");
  for (const r of site.comparison.rows) L.push(`| ${r.axis} | ${r.memory} | ${r.shogun} |`);
  L.push("");

  p("## Open source");
  p(`**Open (${site.license}):** ${site.open_source.open}`, "", `**Closed:** ${site.open_source.closed}`, "", site.open_source.telemetry);
  p(`Repository: ${site.repo}`);

  p("## FAQ");
  for (const f of site.faq) p(`### ${f.q}`, f.a);

  return L.join("\n").trimEnd() + "\n";
}

/** `/llms.txt` per llmstxt.org: H1, blockquote, short prose, then link lists. */
export function renderLlmsTxt(site) {
  const live = site.surfaces.filter((s) => s.status === "live");
  const planned = site.surfaces.filter((s) => s.status !== "live");
  const L = [];
  const p = (...xs) => L.push(...xs, "");
  p(`# ${site.name}`);
  p(`> ${site.tagline} ${site.one_sentence}`);
  p(site.developer_30s);
  p(`Status: ${site.status_line}`);
  p(
    "Rules an agent should know before calling anything: reads are L1 and free; writes to the user's data are L2 and return `input_required`; " +
      "sending, posting and paying are L3 and return an approval URL for a human. Every write takes an idempotency key. Every response carries `usage`, `latency_ms` and, on error, `next_action`."
  );
  p("## Read first");
  for (const s of live) L.push(`- [${s.path}](${surfaceUrl(site, s)}): ${s.what}`);
  L.push(`- [README](${site.repo}#readme): strategy, pillars, pricing and roadmap in full (English and Japanese)`);
  L.push(`- [Implementation plan](${site.repo}/blob/main/docs/implementation-plan.md): epics, tasks and acceptance criteria`);
  L.push("");
  p("## Tools (core wire names, frozen)");
  for (const g of site.tools.groups) {
    L.push(`- ${g.title}: ${g.items.map((t) => `\`${t.name}\``).join(", ")}`);
  }
  L.push("");
  p("## Pricing");
  for (const r of site.pricing.rows) L.push(`- ${r.item}: ${r.price}${r.note ? ` (${r.note})` : ""}`);
  L.push("");
  p("## Phase 1 (planned, not live yet)");
  for (const s of planned) L.push(`- [${s.path}](${surfaceUrl(site, s)}): ${s.what}`);
  L.push("");
  p("## Optional");
  L.push(`- [Repository](${site.repo}): ${site.license}, Rust engine, MCP / REST / CLI`);
  L.push(`- [Setup prompt](${site.url}/setup-prompt.txt): paste into a coding agent`);
  return L.join("\n").trimEnd() + "\n";
}

// ───────────────────────────── HTML ─────────────────────────────

const CSS = String.raw`
:root{
  --bg:#f7f8fa;--paper:#ffffff;--fg:#141922;--muted:#5b6578;--line:#dfe3ea;--line-strong:#c5ccd8;
  --accent:#2c4a8a;--accent-ink:#ffffff;--accent-soft:#e7edf8;--ok:#1f7a4d;--ok-soft:#e3f3ea;
  --warn:#8a5a2c;--warn-soft:#f6ecdf;--code-bg:#0f1420;--code-fg:#dbe3f0;--code-muted:#7f8ca3;
  --sans:"IBM Plex Sans",system-ui,-apple-system,"Segoe UI",Roboto,sans-serif;
  --mono:"IBM Plex Mono",ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;
  --maxw:1180px;--side:240px;--gap:clamp(16px,4vw,56px);
}
@media (prefers-color-scheme:dark){:root:not([data-theme="light"]){
  --bg:#0f131a;--paper:#151b25;--fg:#e6eaf1;--muted:#98a3b7;--line:#28303d;--line-strong:#3a4455;
  --accent:#8fa9e0;--accent-ink:#0f131a;--accent-soft:#1f2a44;--ok:#6cc79a;--ok-soft:#173127;
  --warn:#e0b07f;--warn-soft:#3a2a18;--code-bg:#0b0f16;--code-fg:#dbe3f0;--code-muted:#7f8ca3;}}
:root[data-theme="dark"]{
  --bg:#0f131a;--paper:#151b25;--fg:#e6eaf1;--muted:#98a3b7;--line:#28303d;--line-strong:#3a4455;
  --accent:#8fa9e0;--accent-ink:#0f131a;--accent-soft:#1f2a44;--ok:#6cc79a;--ok-soft:#173127;
  --warn:#e0b07f;--warn-soft:#3a2a18;--code-bg:#0b0f16;--code-fg:#dbe3f0;--code-muted:#7f8ca3;}
*{box-sizing:border-box}
html{scroll-behavior:smooth;-webkit-text-size-adjust:100%}
body{margin:0;background:var(--bg);color:var(--fg);font-family:var(--sans);font-size:16px;line-height:1.65;
  -webkit-font-smoothing:antialiased}
a{color:var(--accent);text-decoration:underline;text-decoration-thickness:1px;text-underline-offset:3px}
a:hover{text-decoration-thickness:2px}
:focus-visible{outline:2px solid var(--accent);outline-offset:2px;border-radius:2px}
code,pre,kbd{font-family:var(--mono);font-size:.92em}
code{background:var(--accent-soft);padding:.08em .35em;border-radius:4px}
pre{margin:0;background:var(--code-bg);color:var(--code-fg);padding:16px 18px;border-radius:8px;overflow:auto;
  line-height:1.55;font-size:13.5px;tab-size:2}
pre code{background:none;padding:0;font-size:inherit}
h1,h2,h3{font-weight:600;letter-spacing:-.01em;line-height:1.2;margin:0;text-wrap:balance}
h1{font-size:clamp(30px,4.6vw,46px)}
h2{font-size:clamp(23px,3vw,30px);margin-bottom:10px}
h3{font-size:18px}
p{margin:0 0 1em}
.muted{color:var(--muted)}
.eyebrow{font-family:var(--mono);font-size:12px;letter-spacing:.08em;text-transform:uppercase;color:var(--muted)}
.skip{position:absolute;left:-999px;top:0;background:var(--paper);padding:8px 12px;z-index:9}
.skip:focus{left:8px;top:8px}

.shell{max-width:var(--maxw);margin:0 auto;padding:0 16px;display:grid;grid-template-columns:var(--side) minmax(0,1fr);gap:var(--gap)}
.side{position:sticky;top:0;align-self:start;height:100vh;overflow:auto;padding:28px 0 28px;border-right:1px solid var(--line);
  padding-right:20px;scrollbar-width:thin}
.brand{display:flex;align-items:center;gap:10px;color:var(--fg);text-decoration:none;font-weight:600;font-size:17px}
.brand svg{width:26px;height:26px;flex:none}
.brand small{display:block;font-weight:400;color:var(--muted);font-size:12px;font-family:var(--mono);letter-spacing:.04em}
.audience{display:inline-flex;margin:18px 0 20px;border:1px solid var(--line-strong);border-radius:8px;padding:3px;background:var(--paper)}
.audience a,.audience button{appearance:none;border:0;background:transparent;color:var(--muted);font:inherit;font-size:13px;
  padding:5px 12px;border-radius:6px;cursor:pointer;text-decoration:none}
.audience [aria-pressed="true"]{background:var(--accent);color:var(--accent-ink)}
.side nav a{display:block;padding:6px 0 6px 12px;border-left:2px solid transparent;color:var(--fg);text-decoration:none;font-size:14.5px}
.side nav a:hover{color:var(--accent)}
.side nav a.current{border-left-color:var(--accent);color:var(--accent)}
.side .ext{margin-top:18px;padding-top:14px;border-top:1px dashed var(--line-strong)}
.side .ext a{color:var(--muted);font-size:13.5px}
.side .ext a::after{content:" ↗";font-size:.85em}

main{padding:28px 0 80px;min-width:0}
section{padding:56px 0 8px;border-top:1px solid var(--line)}
section:first-of-type{border-top:0;padding-top:12px}
.lede{font-size:clamp(17px,1.6vw,20px);line-height:1.6;max-width:62ch}
.notice{display:flex;gap:10px;align-items:baseline;font-size:13.5px;border:1px solid var(--line);background:var(--paper);
  border-radius:8px;padding:10px 14px;margin:0 0 22px;font-family:var(--mono)}
.notice .dot{width:8px;height:8px;border-radius:50%;background:var(--warn);display:inline-block;flex:none;position:relative;top:-1px}
.notice span:last-child{flex:1;min-width:0}
.cta{display:flex;gap:10px;flex-wrap:wrap;margin:26px 0 14px}
.btn{display:inline-flex;align-items:center;gap:8px;font-family:var(--mono);font-size:13px;letter-spacing:.04em;text-transform:uppercase;
  padding:11px 16px;border-radius:6px;border:1px solid var(--accent);color:var(--accent);background:var(--paper);text-decoration:none;cursor:pointer}
.btn.primary{background:var(--accent);color:var(--accent-ink)}
.btn.ghost{border-color:var(--line-strong);color:var(--fg)}
.btn:hover{filter:brightness(1.06)}
.prompt{display:grid;grid-template-columns:1fr auto;align-items:center;gap:10px;border:1px dashed var(--line-strong);border-radius:8px;
  padding:10px 12px 10px 14px;background:var(--paper);margin:0 0 22px;font-family:var(--mono);font-size:13px}
.prompt .text{white-space:pre-wrap;color:var(--muted)}
.prompt .text b{color:var(--fg);font-weight:600}
.prompt button{appearance:none;border:1px solid var(--line-strong);background:transparent;color:var(--fg);font:inherit;font-size:12px;
  letter-spacing:.05em;text-transform:uppercase;padding:7px 10px;border-radius:6px;cursor:pointer;white-space:nowrap}
.works{display:flex;gap:8px;flex-wrap:wrap;align-items:center;margin:8px 0 0}
.works .eyebrow{margin-right:6px}
.chip{font-family:var(--mono);font-size:12.5px;border:1px solid var(--line);border-radius:999px;padding:3px 10px;background:var(--paper);color:var(--fg)}
.term{margin:24px 0 0;border-radius:10px;overflow:hidden;border:1px solid var(--line-strong)}
.term .bar{display:flex;gap:6px;align-items:center;padding:9px 12px;background:var(--code-bg);border-bottom:1px solid #222a38;color:var(--code-muted);
  font-family:var(--mono);font-size:12px}
.term .bar i{width:10px;height:10px;border-radius:50%;background:#2a3344;display:inline-block}
.term pre{border-radius:0}
.term .c{color:var(--code-muted)}
.term .k{color:#8fa9e0}
.term .s{color:#9ad1a8}

table{width:100%;border-collapse:collapse;font-size:14.5px;margin:12px 0 18px}
th,td{text-align:left;vertical-align:top;padding:9px 10px;border-bottom:1px solid var(--line)}
th{font-weight:600;font-size:12.5px;letter-spacing:.06em;text-transform:uppercase;color:var(--muted);font-family:var(--mono)}
td code{white-space:nowrap}
.tbl{overflow-x:auto;-webkit-overflow-scrolling:touch}
.tag{display:inline-block;font-family:var(--mono);font-size:11px;letter-spacing:.05em;text-transform:uppercase;padding:2px 7px;border-radius:4px;
  border:1px solid transparent;white-space:nowrap}
.tag.live{background:var(--ok-soft);color:var(--ok);border-color:transparent}
.tag.phase1{background:var(--warn-soft);color:var(--warn)}
.tag.l1{background:var(--ok-soft);color:var(--ok)}
.tag.l2{background:var(--accent-soft);color:var(--accent)}
.tag.l3{background:var(--warn-soft);color:var(--warn)}

.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(240px,1fr));gap:14px;margin:18px 0}
.card{background:var(--paper);border:1px solid var(--line);border-radius:10px;padding:18px 18px 14px}
.card h3{margin-bottom:8px}
.card .proof{font-size:13.5px;color:var(--muted);border-top:1px dashed var(--line);margin-top:12px;padding-top:10px}
.card .proof b{color:var(--fg);font-weight:600}
.num{font-family:var(--mono);color:var(--muted);font-size:12px;letter-spacing:.08em;margin-bottom:8px;display:block}

.tabs{margin:18px 0 10px}
.tabs input{position:absolute;opacity:0;pointer-events:none}
.tabs .labels{display:flex;gap:4px;border-bottom:1px solid var(--line);margin-bottom:0}
.tabs label{font-family:var(--mono);font-size:13px;letter-spacing:.04em;padding:8px 14px;cursor:pointer;color:var(--muted);
  border:1px solid transparent;border-bottom:0;border-radius:6px 6px 0 0;margin-bottom:-1px}
.tabs .panel{display:none}
.tabs .panel pre{border-radius:0 8px 8px 8px}
#f-mcp:checked~.labels label[for=f-mcp],#f-rest:checked~.labels label[for=f-rest],#f-cli:checked~.labels label[for=f-cli]{
  color:var(--fg);background:var(--code-bg);color:var(--code-fg);border-color:var(--line-strong)}
#f-mcp:checked~.p-mcp,#f-rest:checked~.p-rest,#f-cli:checked~.p-cli{display:block}
.twocol{display:grid;grid-template-columns:repeat(auto-fit,minmax(300px,1fr));gap:14px;margin:14px 0}
.twocol .eyebrow{margin:0 0 6px}
.tools-group{margin:14px 0 6px}
.tools-group h3{font-size:14px;font-family:var(--mono);font-weight:500;letter-spacing:.03em;color:var(--muted);margin:16px 0 6px}
.tools{list-style:none;margin:0;padding:0;display:grid;grid-template-columns:repeat(auto-fill,minmax(300px,1fr));gap:6px 18px}
.tools li{display:grid;grid-template-columns:max-content 1fr;gap:10px;font-size:14px;padding:5px 0;border-bottom:1px solid var(--line)}
.tools code{background:none;padding:0;color:var(--accent);white-space:nowrap}
details{border:1px solid var(--line);border-radius:8px;background:var(--paper);padding:0 16px;margin:0 0 8px}
summary{cursor:pointer;font-weight:600;padding:12px 0;list-style:none;display:flex;justify-content:space-between;gap:10px}
summary::-webkit-details-marker{display:none}
summary::after{content:"+";font-family:var(--mono);color:var(--muted)}
details[open] summary::after{content:"–"}
details p{margin:0 0 14px}
footer{border-top:1px solid var(--line);margin-top:48px;padding:22px 0 0;font-size:13.5px;color:var(--muted);display:flex;flex-wrap:wrap;gap:10px 22px;
  justify-content:space-between}
footer a{color:var(--muted)}
.agentview{display:none}
body.agent .agentview{display:block}
body.agent .humanview{display:none}
.agentview pre{font-size:13px;white-space:pre-wrap}
@media (max-width:860px){
  .shell{display:block}
  .side{position:static;height:auto;border-right:0;border-bottom:1px solid var(--line);padding:18px 0 14px}
  .side nav{display:flex;gap:2px 14px;flex-wrap:wrap}
  .side nav a{padding:4px 0;border-left:0;font-size:13.5px}
  .side .ext{display:flex;gap:14px;flex-wrap:wrap;margin-top:12px;padding-top:12px}
  .side .ext a{display:inline}
  .prompt{grid-template-columns:1fr}
  section{padding-top:40px}
  .tbl table{min-width:720px}
  .tbl{margin:0 -16px;padding:0 16px}
}
@media (prefers-reduced-motion:reduce){html{scroll-behavior:auto}}
`;

const LOGO = `<svg viewBox="0 0 32 32" aria-hidden="true" focusable="false"><rect x="2" y="2" width="28" height="28" rx="7" fill="var(--accent)"/><path d="M9 11.5h14M9 16h9M9 20.5h14" stroke="var(--accent-ink)" stroke-width="2.4" stroke-linecap="round"/><circle cx="22.5" cy="16" r="1.9" fill="var(--accent-ink)"/></svg>`;

function term(lines) {
  return `<div class="term"><div class="bar"><i></i><i></i><i></i><span>&nbsp;sh</span></div><pre><code>${lines}</code></pre></div>`;
}

export function renderHtml(site, md) {
  const title = `${site.name}: ${site.tagline.replace(/\.$/, "")}`;
  const description = site.one_sentence;
  const live = site.surfaces.filter((s) => s.status === "live");

  const jsonld = {
    "@context": "https://schema.org",
    "@type": "SoftwareApplication",
    name: site.name,
    url: site.url,
    description,
    applicationCategory: "DeveloperApplication",
    operatingSystem: "Linux, macOS",
    license: "https://www.apache.org/licenses/LICENSE-2.0",
    codeRepository: site.repo,
    offers: { "@type": "Offer", price: "0", priceCurrency: "USD", description: "Reads are free; usage-based pricing for ingest, runs and storage." },
    softwareHelp: { "@type": "WebPage", url: `${site.url}/llms.txt` },
  };

  const navHtml = site.nav.map((n) => `<a href="#${n.id}">${esc(n.label)}</a>`).join("");

  const surfacesRows = site.surfaces
    .map((s) => {
      const url = surfaceUrl(site, s);
      const label = s.host === "registry" ? s.path : s.host === "repo" ? s.path : url.replace(/^https:\/\//, "");
      return `<tr><td><a href="${esc(url)}"><code>${esc(label)}</code></a></td><td>${inline(s.what)}</td><td class="muted">${esc(s.why)}</td><td><span class="tag ${s.status}">${STATUS_LABEL[s.status]}</span></td></tr>`;
    })
    .join("\n");

  const pillars = site.pillars
    .map(
      (p, i) =>
        `<div class="card"><span class="num">0${i + 1}</span><h3>${esc(p.title)}</h3><p>${esc(p.claim)}</p><p class="proof"><b>Proof in the code.</b> ${esc(p.proof)}</p></div>`
    )
    .join("\n");

  const tiers = site.tiers
    .map(
      (t) =>
        `<tr><td><span class="tag ${t.level.toLowerCase()}">${t.level}</span></td><td><b>${esc(t.name)}</b><br><span class="muted">${inline(t.what)}</span></td><td><code>${esc(t.examples)}</code></td><td><code>${esc(t.mcp)}</code></td><td>${esc(t.cost)}</td></tr>`
    )
    .join("\n");

  const tools = site.tools.groups
    .map(
      (g) =>
        `<div class="tools-group"><h3>${esc(g.title)}</h3><ul class="tools">${g.items
          .map((t) => `<li><code>${esc(t.name)}</code><span>${esc(t.task)}</span></li>`)
          .join("")}</ul></div>`
    )
    .join("\n");

  const pricing = site.pricing.rows
    .map((r) => `<tr><td>${esc(r.item)}</td><td><b>${esc(r.price)}</b></td><td class="muted">${esc(r.note)}</td></tr>`)
    .join("\n");

  const deploy = site.deploy
    .map((d, i) => `<div class="card"><span class="num">0${i + 1}</span><h3>${esc(d.title)}</h3><p>${esc(d.body)}</p></div>`)
    .join("\n");

  const compare = site.comparison.rows
    .map((r) => `<tr><td class="muted">${esc(r.axis)}</td><td>${esc(r.memory)}</td><td><b>${esc(r.shogun)}</b></td></tr>`)
    .join("\n");

  const faq = site.faq.map((f) => `<details><summary>${esc(f.q)}</summary><p>${esc(f.a)}</p></details>`).join("\n");

  const chips = site.works_with.map((w) => `<span class="chip">${esc(w)}</span>`).join("");

  const heroTerm = term(
    `<span class="c"># this page, for agents</span>\n<span class="k">curl</span> -sH <span class="s">'Accept: text/markdown'</span> https://${site.domain}/\n<span class="k">curl</span> -s https://${site.domain}/llms.txt\n\n<span class="c"># one command, your coding agent knows your open loops (Phase 1)</span>\n<span class="k">npx</span> shogunagent`
  );

  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${esc(title)}</title>
<meta name="description" content="${esc(description)}">
<link rel="canonical" href="${site.url}/">
<link rel="alternate" type="text/markdown" title="This page as Markdown" href="${site.url}/index.md">
<link rel="alternate" type="text/plain" title="llms.txt" href="${site.url}/llms.txt">
<link rel="alternate" type="text/plain" title="llms-full.txt" href="${site.url}/llms-full.txt">
<link rel="code-repository" href="${site.repo}">
<meta property="og:type" content="website">
<meta property="og:site_name" content="${esc(site.name)}">
<meta property="og:title" content="${esc(title)}">
<meta property="og:description" content="${esc(description)}">
<meta property="og:url" content="${site.url}/">
<meta name="twitter:card" content="summary">
<meta name="color-scheme" content="light dark">
<meta name="theme-color" content="#2c4a8a">
<link rel="icon" href="data:image/svg+xml,${encodeURIComponent(LOGO.replace("var(--accent)", "#2c4a8a").replaceAll("var(--accent-ink)", "#ffffff"))}">
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=IBM+Plex+Mono:wght@400;500&family=IBM+Plex+Sans:wght@400;500;600&display=swap">
<script type="application/ld+json">${JSON.stringify(jsonld)}</script>
<style>${CSS}</style>
</head>
<body>
<a class="skip" href="#main">Skip to content</a>
<div class="shell">
<aside class="side">
  <a class="brand" href="/">${LOGO}<span>${esc(site.name)}<small>${esc(site.domain)}</small></span></a>
  <div class="audience" role="group" aria-label="Audience">
    <button type="button" data-aud="human" aria-pressed="true">Human</button>
    <button type="button" data-aud="agent" aria-pressed="false" title="Show this page as the Markdown an agent receives">Agent</button>
  </div>
  <nav aria-label="On this page" class="humanview">${navHtml}</nav>
  <div class="ext">
    <a href="/llms.txt">llms.txt</a>
    <a href="${esc(site.repo)}">GitHub</a>
    <a href="${esc(site.repo)}/blob/main/docs/implementation-plan.md">Plan</a>
  </div>
</aside>
<main id="main">
<div class="agentview" aria-live="polite">
  <p class="eyebrow">What an agent receives for <code>GET /</code> with <code>Accept: text/markdown</code>. Also at <a href="/index.md">/index.md</a>.</p>
  <pre><code>${esc(md)}</code></pre>
</div>
<div class="humanview">

<section id="top">
  <p class="notice"><span class="dot"></span><span>${esc(site.status_line)}</span></p>
  <p class="eyebrow">${esc(site.motto)}</p>
  <h1>${esc(site.tagline)}</h1>
  <p class="lede" style="margin-top:14px">${esc(site.one_sentence)}</p>
  <div class="cta">
    <a class="btn primary" href="/llms.txt">Read llms.txt</a>
    <a class="btn" href="${esc(site.repo)}">GitHub ↗</a>
    <a class="btn ghost" href="#agents">For agents ↓</a>
  </div>
  <div class="prompt">
    <span class="text"><b>Setup prompt</b> · paste into Claude Code, Codex or Cursor\n${esc(site.setup_prompt)}</span>
    <button type="button" data-copy="${esc(site.setup_prompt)}">Copy setup prompt</button>
  </div>
  <div class="works"><span class="eyebrow">Works with</span>${chips}</div>
  ${heroTerm}
</section>

<section id="agents">
  <p class="eyebrow">For agents</p>
  <h2>Machine-readable at every well-known location.</h2>
  <p class="lede">The same identity, tool names and schemas appear everywhere an agent might look. The human page you are reading is generated from the same data. ${live.length} of ${site.surfaces.length} surfaces are live today; the rest are Phase 1 deliverables and are labeled as such.</p>
  <div class="tbl"><table>
    <thead><tr><th>Surface</th><th>What</th><th>Why an agent needs it</th><th>Status</th></tr></thead>
    <tbody>${surfacesRows}</tbody>
  </table></div>
  <p class="muted" style="font-size:14px">Six principles behind this list: zero-config first call; every response self-describing (<code>usage</code>, <code>latency_ms</code>, <code>next_action</code>, <code>hints</code>); descriptions written for ranking, not marketing, and gated by a selection eval; at most one human approval, and only for money; stability an agent can measure (frozen wire names, idempotency keys, honored <code>Retry-After</code>); no self-preferencing in the router.</p>
</section>

<section id="what">
  <p class="eyebrow">What it is</p>
  <h2>${esc(site.motto)}</h2>
  <p class="lede">${esc(site.developer_30s)}</p>
  <p>${esc(site.not_a_memory_api)}</p>
  <div class="grid">${pillars}</div>
</section>

<section id="faces">
  <p class="eyebrow">Three faces</p>
  <h2>One behavior over MCP, REST and CLI.</h2>
  <p class="lede">${esc(site.faces.intro)} A tool that exists on one face exists on all three.</p>
  <div class="tabs">
    <input type="radio" name="face" id="f-mcp" checked>
    <input type="radio" name="face" id="f-rest">
    <input type="radio" name="face" id="f-cli">
    <div class="labels"><label for="f-mcp">MCP</label><label for="f-rest">REST</label><label for="f-cli">CLI</label></div>
    <div class="panel p-mcp"><pre><code>${esc(site.faces.mcp)}</code></pre></div>
    <div class="panel p-rest"><pre><code>${esc(site.faces.rest)}</code></pre></div>
    <div class="panel p-cli"><pre><code>${esc(site.faces.cli)}</code></pre></div>
  </div>
  <p class="eyebrow" style="margin-top:18px">Same answer on every face</p>
  <pre><code>${esc(site.faces.response)}</code></pre>
  <div class="twocol">
    <div><p class="eyebrow">Out of credit: 402 in the x402 shape</p><pre><code>${esc(site.faces.payment_required)}</code></pre></div>
    <div><p class="eyebrow">An L3 action: approval URL, no side effect</p><pre><code>${esc(site.faces.l3)}</code></pre></div>
  </div>
</section>

<section id="tiers">
  <p class="eyebrow">Permission tiers</p>
  <h2>Reads run. Writes confirm. Sends wait for a human.</h2>
  <p class="lede">Three tiers, decided by the type of the action, identical for every agent and every human. An operation not in the scope table is denied; <code>ExternalSend</code> is always L3.</p>
  <div class="tbl"><table>
    <thead><tr><th>Tier</th><th>What happens</th><th>Examples</th><th>MCP</th><th>Cost</th></tr></thead>
    <tbody>${tiers}</tbody>
  </table></div>
</section>

<section id="tools">
  <p class="eyebrow">Tools</p>
  <h2>The core wire names, frozen.</h2>
  <p class="lede">${esc(site.tools.note)}</p>
  ${tools}
</section>

<section id="pricing">
  <p class="eyebrow">Pricing</p>
  <h2>Reads are free. You pay for what you ingest and run.</h2>
  <p class="lede">${esc(site.pricing.line)}</p>
  <div class="tbl"><table>
    <thead><tr><th>Metered item</th><th>Unit price</th><th>Notes</th></tr></thead>
    <tbody>${pricing}</tbody>
  </table></div>
  <p class="muted" style="font-size:14px">${esc(site.pricing.disclaimer)} A machine-readable rate card ships at <code>/.well-known/pricing.json</code> in Phase 1.</p>
</section>

<section id="deploy">
  <p class="eyebrow">Runs everywhere</p>
  <h2>Same API on your laptop, hosted, or in your cloud.</h2>
  <div class="grid">${deploy}</div>
</section>

<section id="compare">
  <p class="eyebrow">Not a memory API</p>
  <h2>Not what an agent remembers. What is true now.</h2>
  <p class="lede">${esc(site.comparison.intro)}</p>
  <div class="tbl"><table>
    <thead><tr><th></th><th>Memory APIs</th><th>${esc(site.name)}</th></tr></thead>
    <tbody>${compare}</tbody>
  </table></div>
</section>

<section id="open">
  <p class="eyebrow">Open source</p>
  <h2>The engine is open. The hosting is the business.</h2>
  <div class="twocol">
    <div class="card"><h3>Open · ${esc(site.license)}</h3><p>${esc(site.open_source.open)}</p></div>
    <div class="card"><h3>Closed</h3><p>${esc(site.open_source.closed)}</p><p class="muted" style="margin-bottom:0;font-size:14px">${esc(site.open_source.telemetry)}</p></div>
  </div>
  <div class="cta"><a class="btn primary" href="${esc(site.repo)}">Read the code ↗</a><a class="btn" href="${esc(site.repo)}/blob/main/README.md">Strategy README ↗</a></div>
</section>

<section id="faq">
  <p class="eyebrow">FAQ</p>
  <h2>Questions agents and their operators ask.</h2>
  ${faq}
</section>

<footer>
  <span>© ${new Date().getUTCFullYear()} ${esc(site.name)} · ${esc(site.license)}</span>
  <span><a href="/llms.txt">llms.txt</a> · <a href="/llms-full.txt">llms-full.txt</a> · <a href="/index.md">index.md</a> · <a href="/setup-prompt.txt">setup prompt</a> · <a href="${esc(site.repo)}">GitHub</a></span>
</footer>
</div>
</main>
</div>
<script>
(function(){
  var body=document.body, btns=document.querySelectorAll('[data-aud]');
  function setAud(a){ body.classList.toggle('agent',a==='agent'); btns.forEach(function(b){b.setAttribute('aria-pressed',String(b.dataset.aud===a));});
    try{ var u=new URL(location.href); if(a==='agent'){u.searchParams.set('agent','1');}else{u.searchParams.delete('agent');} history.replaceState(null,'',u);}catch(e){} }
  btns.forEach(function(b){ b.addEventListener('click',function(){ setAud(b.dataset.aud); }); });
  try{ if(new URL(location.href).searchParams.get('agent')==='1') setAud('agent'); }catch(e){}
  document.querySelectorAll('[data-copy]').forEach(function(b){ b.addEventListener('click',function(){
    var t=b.textContent; (navigator.clipboard?navigator.clipboard.writeText(b.dataset.copy):Promise.reject()).then(function(){b.textContent='Copied';},function(){b.textContent='Select and copy above';});
    setTimeout(function(){b.textContent=t;},1800); }); });
  var links=document.querySelectorAll('.side nav a'), secs=[].map.call(links,function(a){return document.querySelector(a.getAttribute('href'));});
  if('IntersectionObserver' in window){ var io=new IntersectionObserver(function(es){ es.forEach(function(e){ if(e.isIntersecting){ links.forEach(function(a){a.classList.toggle('current',a.getAttribute('href')==='#'+e.target.id);}); } }); },{rootMargin:'-20% 0px -70% 0px'});
    secs.forEach(function(s){ if(s) io.observe(s); }); }
})();
</script>
</body>
</html>
`;
}

// ───────────────────────────── Other files ─────────────────────────────

export function renderRobots(site) {
  return [
    "User-agent: *",
    "Allow: /",
    "",
    `Sitemap: ${site.url}/sitemap.xml`,
    `LLMs-Txt: ${site.url}/llms.txt`,
    "",
    "# Agents are the audience. Training, search and inference use are all welcome.",
    "Content-Signal: ai-train=yes, search=yes, ai-input=yes",
    "",
  ].join("\n");
}

export function renderSitemap(site, paths) {
  const urls = paths.map((p) => `  <url><loc>${site.url}${p === "/index.html" ? "/" : p}</loc></url>`).join("\n");
  return `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${urls}\n</urlset>\n`;
}

/** Everything the site publishes, keyed by URL path. */
export function render(site) {
  const md = renderMarkdown(site);
  const out = {
    "/index.md": md,
    "/llms.txt": renderLlmsTxt(site),
    "/llms-full.txt": md,
    "/setup-prompt.txt": site.setup_prompt + "\n",
    "/robots.txt": renderRobots(site),
  };
  out["/index.html"] = renderHtml(site, md);
  out["/sitemap.xml"] = renderSitemap(site, ["/index.html", "/index.md", "/llms.txt", "/llms-full.txt", "/setup-prompt.txt"]);
  return out;
}

export function loadSite() {
  return JSON.parse(readFileSync(join(here, "content", "site.json"), "utf8"));
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  const site = loadSite();
  const dist = join(here, "dist");
  rmSync(dist, { recursive: true, force: true });
  mkdirSync(dist, { recursive: true });
  const files = render(site);
  for (const [p, body] of Object.entries(files)) {
    const target = join(dist, p);
    mkdirSync(dirname(target), { recursive: true });
    writeFileSync(target, body);
  }
  console.log(`wrote ${Object.keys(files).length} files to ${dist}`);
}
