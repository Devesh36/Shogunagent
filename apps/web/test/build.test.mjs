// The site's agent-first contract: the Markdown an agent receives and the HTML a human sees are
// two views over one object, so they must agree on the facts an agent would act on.
import { test } from "node:test";
import assert from "node:assert/strict";
import { render, loadSite, toolNames, renderLlmsTxt } from "../build.mjs";

const site = loadSite();
const out = render(site);
const html = out["/index.html"];
const md = out["/index.md"];
const llms = out["/llms.txt"];

test("site.json is well-formed for what the renderers need", () => {
  assert.match(site.url, /^https:\/\/shogunagent\.com$/);
  assert.equal(site.domain, "shogunagent.com");
  for (const s of site.surfaces) {
    assert.ok(["live", "phase1"].includes(s.status), `${s.path}: status`);
    assert.ok(["site", "api", "repo", "registry"].includes(s.host), `${s.path}: host`);
  }
  for (const n of site.nav) assert.ok(html.includes(`<section id="${n.id}">`), `nav target #${n.id} exists`);
});

test("every live surface the site promises is actually built", () => {
  for (const s of site.surfaces.filter((x) => x.status === "live" && x.host === "site")) {
    assert.ok(out[s.path], `${s.path} is in the build output`);
  }
});

// docs/implementation-plan.md §7.1. The plan's heading says "core 22" but enumerates these 21
// (pinned core 23 minus the four meeting.* tools, plus memory.ingest and memory.recall); the
// page follows the enumeration and never states a count.
const PLAN_CORE = [
  "memory.search", "memory.get_context", "memory.get_context_pack", "memory.get_events", "memory.read_session",
  "memory.get_wrap", "memory.get_periods", "memory.recall", "calendar.upcoming", "profile.whoami",
  "state.commitments.list", "state.open_loops.list", "state.people.list", "state.projects.list",
  "state.people.get", "state.projects.get", "wiki.evidence", "actions.status",
  "memory.append_note", "memory.ingest", "actions.execute",
];

test("the core tool names match the plan and are identical in HTML, Markdown and llms.txt", () => {
  const names = toolNames(site);
  const core = names.filter((n) => n.includes("."));
  assert.deepEqual([...core].sort(), [...PLAN_CORE].sort());
  assert.ok(!/core 2\d/.test(html), "the page states no tool count");
  for (const n of names) {
    assert.ok(html.includes(`<code>${n}</code>`), `${n} in HTML`);
    assert.ok(md.includes(`\`${n}\``), `${n} in index.md`);
    assert.ok(llms.includes(`\`${n}\``), `${n} in llms.txt`);
  }
});

test("llms.txt follows the llmstxt.org shape and separates live from planned", () => {
  const lines = llms.split("\n");
  assert.equal(lines[0], `# ${site.name}`);
  assert.ok(lines[2].startsWith("> "), "blockquote summary on line 3");
  assert.ok(llms.includes("## Read first"));
  assert.ok(llms.includes("## Phase 1 (planned, not live yet)"));
  const readFirst = llms.slice(llms.indexOf("## Read first"), llms.indexOf("## Tools"));
  assert.ok(!readFirst.includes(site.api_url), "nothing on the hosted API is listed as readable today");
  assert.ok(llms.includes(site.status_line));
});

test("the HTML declares its machine-readable alternates and no third-party script", () => {
  assert.ok(html.includes(`<link rel="alternate" type="text/markdown" title="This page as Markdown" href="${site.url}/index.md">`));
  assert.ok(html.includes(`<link rel="alternate" type="text/plain" title="llms.txt" href="${site.url}/llms.txt">`));
  assert.ok(html.includes('<script type="application/ld+json">'));
  assert.ok(!/<script[^>]+src=/.test(html), "no external script");
  assert.ok(html.includes('<html lang="en">'));
  assert.ok(html.includes('name="viewport"'));
});

test("the HTML embeds the exact Markdown an agent would receive", () => {
  const escaped = md.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;").replaceAll('"', "&quot;");
  assert.ok(html.includes(escaped), "agent view contains index.md verbatim (escaped)");
  assert.equal(out["/llms-full.txt"], md);
});

test("the setup prompt is one paragraph, mentions llms.txt and the L3 rule", () => {
  const prompt = out["/setup-prompt.txt"].trim();
  assert.ok(!prompt.includes("\n"));
  assert.ok(prompt.includes(`${site.url}/llms.txt`));
  assert.ok(/L3/.test(prompt));
  assert.ok(html.includes(`data-copy="${prompt.replaceAll('"', "&quot;")}"`));
});

test("robots.txt and sitemap.xml point at llms.txt and the built pages", () => {
  assert.ok(out["/robots.txt"].includes(`LLMs-Txt: ${site.url}/llms.txt`));
  assert.ok(out["/robots.txt"].includes("Content-Signal:"));
  for (const p of ["/", "/index.md", "/llms.txt", "/llms-full.txt", "/setup-prompt.txt"]) {
    assert.ok(out["/sitemap.xml"].includes(`<loc>${site.url}${p}</loc>`), `sitemap has ${p}`);
  }
});

test("nothing claims a hosted endpoint is live", () => {
  // Hosted surfaces are Phase 1; the page must not label any of them live until they are.
  for (const s of site.surfaces.filter((x) => x.host !== "site")) {
    assert.equal(s.status, "phase1", `${s.path} is hosted or external and must stay Phase 1 until it ships`);
  }
  assert.ok(renderLlmsTxt(site).includes("not live yet"));
});

test("no literal secrets or personal data in the published files", () => {
  for (const [p, body] of Object.entries(out)) {
    assert.ok(!/sk-[A-Za-z0-9]{10,}|Bearer [A-Za-z0-9._-]{20,}|@gmail\.com/.test(body), `${p} is clean`);
  }
});
