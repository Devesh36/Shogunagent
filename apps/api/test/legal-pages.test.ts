/** E23: /privacy and /terms are rendered from docs/legal/*.md by scripts/gen-legal-pages.mjs and
 * served as generated HTML; the converter escapes everything and keeps only safe link targets. */
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

import { DPA_HTML, PRIVACY_HTML, SLA_HTML, TERMS_HTML, ZDR_HTML } from "../src/wellknown/legal-content.generated.js";
import { makeAgentRig } from "./agent-helpers.js";
// eslint-disable-next-line @typescript-eslint/ban-ts-comment
// @ts-ignore — plain ESM without types
import { markdownToHtml, renderPage } from "../../../scripts/gen-legal-pages.mjs";

const read = (rel: string): string => readFileSync(new URL(`../../../${rel}`, import.meta.url), "utf8");

describe("legal pages", () => {
  it("the committed module is what the generator renders from the markdown", () => {
    expect(PRIVACY_HTML).toBe(renderPage(read("docs/legal/privacy-policy.md")));
    expect(TERMS_HTML).toBe(renderPage(read("docs/legal/terms.md")));
    expect(DPA_HTML).toBe(renderPage(read("docs/legal/dpa.md")));
    expect(ZDR_HTML).toBe(renderPage(read("docs/legal/zdr.md")));
    expect(SLA_HTML).toBe(renderPage(read("docs/legal/sla.md")));
  });

  it("both are marked DRAFT for legal review", () => {
    for (const html of [PRIVACY_HTML, TERMS_HTML, DPA_HTML, ZDR_HTML, SLA_HTML]) expect(html).toContain("<strong>DRAFT — for legal review. Not in force.</strong>");
    expect(DPA_HTML).toContain("Standard Contractual Clauses");
    expect(DPA_HTML).toContain("Personal data breaches");
    expect(ZDR_HTML).toContain("Paths that retain nothing");
    expect(ZDR_HTML).toContain("<code>/mcp</code>");
    expect(SLA_HTML).toContain("99.9% monthly availability");
    expect(PRIVACY_HTML).toContain("We do not train models on your data");
    expect(PRIVACY_HTML).toContain("Export is always free");
  });

  it("are served at /privacy and /terms with a locked-down CSP", async () => {
    const rig = makeAgentRig();
    for (const [path, html] of [["/privacy", PRIVACY_HTML], ["/terms", TERMS_HTML], ["/legal/dpa", DPA_HTML], ["/legal/zdr", ZDR_HTML], ["/legal/sla", SLA_HTML]] as const) {
      const res = await rig.app.request(path);
      expect(res.status).toBe(200);
      expect(res.headers.get("content-type")).toContain("text/html");
      expect(res.headers.get("content-security-policy")).toContain("default-src 'none'");
      expect(await res.text()).toBe(html);
    }
    expect(rig.logLines.some((l) => l.startsWith("GET /privacy 200"))).toBe(true);
  });

  it("escapes HTML and drops unsafe link targets", () => {
    const html = markdownToHtml("# T <x>\n\nA <script>alert(1)</script> [ok](https://a.example) [bad](javascript:alert(1)) `<b>` **b**\n\n- one\n- two\n\n| a | b |\n|---|---|\n| <i> | 2 |");
    expect(html).toContain("&lt;script&gt;");
    expect(html).not.toContain("<script>");
    expect(html).toContain('<a href="https://a.example">ok</a>');
    expect(html).toContain('<a href="#">bad</a>');
    expect(html).toContain("<code>&lt;b&gt;</code>");
    expect(html).toContain("<ul><li>one</li><li>two</li></ul>");
    expect(html).toContain("<td>&lt;i&gt;</td>");
  });
});
