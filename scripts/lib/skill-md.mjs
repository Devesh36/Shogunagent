// Parse SKILL.md (E10-T5) for the generators that derive other surfaces from it (invariant 12):
// the A2A agent card's skills (E10-T3) and the llms.txt index (E10-T4). Pure functions over the
// file's text; no I/O here. Shared by `scripts/gen-*.mjs` and the apps/api tests.

/** The YAML frontmatter as flat strings (`name`, `version`, `description`). Only the subset
 * SKILL.md uses: `key: value` lines and a `>-` folded block for the description. */
export function parseFrontmatter(text) {
  const m = /^---\r?\n([\s\S]*?)\r?\n---\r?\n/.exec(text);
  if (!m) throw new Error("SKILL.md: no YAML frontmatter");
  const out = {};
  const lines = m[1].split(/\r?\n/);
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const kv = /^([A-Za-z_][A-Za-z0-9_-]*):\s*(.*)$/.exec(line);
    if (!kv) continue;
    const key = kv[1];
    let value = kv[2].trim();
    if (value === ">-" || value === ">" || value === "|" || value === "|-") {
      const folded = [];
      while (i + 1 < lines.length && /^\s+\S/.test(lines[i + 1])) folded.push(lines[++i].trim());
      value = value.startsWith(">") ? folded.join(" ") : folded.join("\n");
    }
    out[key] = value.replace(/^"(.*)"$/, "$1");
  }
  return out;
}

/** The body after the frontmatter. */
export function body(text) {
  return text.replace(/^---\r?\n[\s\S]*?\r?\n---\r?\n/, "");
}

/** The lines of one `## <heading>` section (up to the next `## `), without the heading. */
export function section(text, heading) {
  const lines = body(text).split(/\r?\n/);
  const start = lines.findIndex((l) => l.trim() === `## ${heading}`);
  if (start < 0) throw new Error(`SKILL.md: no section "## ${heading}"`);
  const out = [];
  for (let i = start + 1; i < lines.length; i++) {
    if (/^## /.test(lines[i])) break;
    out.push(lines[i]);
  }
  return out;
}

/** The top-level bullets of a section, each as one string (continuation lines joined). */
export function bullets(lines) {
  const out = [];
  for (const line of lines) {
    if (/^- /.test(line)) out.push(line.slice(2).trim());
    else if (out.length > 0 && /^\s+\S/.test(line)) out[out.length - 1] += " " + line.trim();
  }
  return out;
}

/** The "When to use this skill" bullets as A2A skills: `- **Title** rest (`tool.a`, `tool.b`).` */
export function skillsFromWhenToUse(text) {
  const items = bullets(whenToUseOnly(section(text, "When to use this skill")));
  return items.map((item) => {
    const titled = /^\*\*(.+?)\*\*:?\s*(.*)$/.exec(item);
    const title = titled ? titled[1].trim() : item.split(/[.:(]/)[0].trim();
    const rest = titled ? titled[2] : item;
    const tools = [...rest.matchAll(/`([a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)+)`/g)].map((m) => m[1]);
    // The whole bullet as one sentence: the bold phrase is its start, not a separate label.
    const description = item
      .replace(/\*\*/g, "")
      .replace(/\s*\((?:`[^`]+`(?:,\s*)?)+\)\s*/g, " ") // the trailing `(tool, tool)` list
      .replace(/`([^`]+)`/g, "$1")
      .replace(/\s+/g, " ")
      .replace(/\s+([.,;:])/g, "$1")
      .trim();
    const id = title
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-+|-+$/g, "");
    return { id, name: title, description, tags: [...new Set(tools)] };
  });
}

/** The "Do not use it for" bullets, plain text. */
export function exclusions(text) {
  return bullets(afterDoNotUse(section(text, "When to use this skill"))).map((b) => b.replace(/`([^`]+)`/g, "$1"));
}

function afterDoNotUse(lines) {
  const i = lines.findIndex((l) => /^Do not use it for/.test(l.trim()));
  return i < 0 ? [] : lines.slice(i + 1);
}

/** The "When to use" bullets end at the "Do not use it for:" line. */
export function whenToUseOnly(lines) {
  const i = lines.findIndex((l) => /^Do not use it for/.test(l.trim()));
  return i < 0 ? lines : lines.slice(0, i);
}
