#!/usr/bin/env python3
"""CLAUDE.md log rule guard: captured user text must never reach a log line.

"テレメトリ・ログにキャプチャ内容（ユーザーのテキスト）を含めない。デバッグログも同様" — and the
2026-08-13 audit found two live violations of exactly this shape: a meeting diagnostic that
printed the focused WINDOW TITLE, and a self-test that printed an `Action` Debug carrying
notification and clipboard text. Both looked like ordinary diagnostics in review.

The check is a co-occurrence heuristic, like check-media-writes.py: a bare `eprintln!`/`println!`
that interpolates an identifier whose name means "user-derived content". It cannot know what a
variable holds, so two escape hatches exist and both are honest:

- Route the line through `elog!` (shogun_core::log_redact), which scrubs keys, emails and URLs
  before writing. That is the fix, not the exemption.
- Mark the site `// log-hygiene-ok: <reason>` when the identifier is structural despite its name
  (a window label, a provider status). The reason lives at the site, where the next reader is.

What counts (#913): every identifier the line formats, inline (`{title}`) or positional
(`"{}", title`), whose `_`-separated parts include a content word and which does not end in a
shape word (`title_len`, `note_id` are allowed). A method chain on such an identifier counts
as the identifier (`title.trim()`, `msg.to_string()`, `title.chars().take(40).collect()`),
unless a link in it returns a shape — a number or a yes/no (`text.len()`, `body.is_empty()`,
`title.chars().count()`, `title.trim().len()`). A free function call is skipped
(`report::render_summary(..)`): what it returns cannot be judged from its name. A positional
`{:?}` or `{:#?}` of a content-bearing type (`action`, `record`, …) is flagged like its inline
form. `eprint!`, `print!` and `dbg!` are scanned as well, and so are `tracing::trace!` …
`tracing::error!` written with that path, including the values of the fields before the format
string (`title = %title`); a tracing macro imported by name (`use tracing::info; info!(..)`) is
not scanned. A call that rustfmt wrapped onto later lines is joined until its parentheses close
(string and char literals do not count) and checked as one call (#915); the marker may sit on the
line above the call or on any of its lines.

Operational metrics and traces (plan E37). The same rule covers the server's ops telemetry: a
metric label or a span attribute is a log line that a dashboard keeps. Every TypeScript
`observability.ts` under `apps/*/src` must declare `METRIC_LABELS` and `SPAN_ATTRIBUTES` as array
literals drawn only from the allowlists below (no label name may be a content or credential word),
may read only the `Mcp-Method` / `Mcp-Name` headers (by literal name), and may not read a request
or response body, a query string or the raw request (`c.req.json()`, `c.req.query()`,
`searchParams`, `c.res.text()`, …). A new label therefore needs a change here, in review.

Run from the repo root:  python3 scripts/check-log-hygiene.py
Self-test the detector:   python3 scripts/check-log-hygiene.py --self-test
"""

import pathlib
import re
import sys

# Bare stderr/stdout logging. `elog!` is deliberately absent — it is the sanctioned path.
LOG_RE = re.compile(
    r"\b(?:eprintln|println|eprint|print|dbg)!\s*\("
    r"|\btracing::(?:trace|debug|info|warn|error)!\s*\("
)

# Words that mean "text a human wrote or a screen showed". `label`/`name` are intentionally NOT
# here: in this codebase they are window labels and app names (already the documented, allowed
# thing to log), so including them would be noise that trains people to ignore this check.
CONTENT_WORDS = frozenset(
    "title text body content snippet transcript chunk subject query prompt answer excerpt summary "
    "note msg message rationale utterance caption".split()
)
# A last part that makes the identifier a shape of the content, not the content: `title_len`.
SHAPE_WORDS = frozenset(
    "len lens count counts chars bytes hash hashes digest id ids ms kind kinds n size sizes max min "
    "limit cap".split()
)

# A whole `Action`/struct printed with {:?} can carry content fields even when the variable name
# is innocent — that is precisely how the notch self-test leaked. Flag Debug-printing of the
# known content-bearing types.
DEBUG_TYPES_RE = re.compile(r"\{\s*(?:a\.action|action|record|event|item|pack)\s*:#?\?\s*\}")
# `{:?}` and the pretty `{:#?}`.
DEBUG_SPEC_RE = re.compile(r":#?\?")
# Methods whose result is a shape of the receiver, not its text: a number or a yes/no. Anywhere in
# a chain they end the content (`title.trim().len()`, `title.chars().count()`). `chars`, `lines`,
# `trim`, `clone`, `to_string`, `take`, `collect` … are not here: they still carry the text, so
# `title.chars().take(40).collect::<String>()` prints the title.
SHAPE_METHODS = frozenset(
    "len is_empty count is_some is_none is_ok is_err starts_with ends_with contains".split()
)
DEBUG_TYPES = frozenset("action record event item pack".split())
# Methods that hand back the same value (or a view of it), so a Debug of the result is a Debug of
# the content-bearing type: `a.action.clone()`. Any other method on such a value returns
# something else (`event.state()`), which its name does not tell us about.
SAME_VALUE_METHODS = frozenset(
    "clone to_owned as_ref as_deref as_mut borrow unwrap expect unwrap_or_default".split()
)

STRING_RE = re.compile(r'"((?:[^"\\]|\\.)*)"')
IDENT_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_.]*")


def is_content(ident):
    """An identifier whose `_` parts name user text and whose last part is not a shape word."""
    parts = [p for p in ident.lower().split("_") if p]
    return bool(parts) and parts[-1] not in SHAPE_WORDS and any(p in CONTENT_WORDS for p in parts)


# `.method` after a value: the start of one link in a method chain.
METHOD_RE = re.compile(r"\s*\.\s*([A-Za-z_]\w*)")
# A tracing `target: "…",` argument, which comes before the format string.
TRACING_TARGET_RE = re.compile(r'^\s*target\s*:\s*"(?:[^"\\]|\\.)*"\s*,')
# A tracing field name (`title = …`): the value after it is what gets recorded.
FIELD_NAME_RE = re.compile(r"\b[A-Za-z_][\w.]*\s*=(?!=)")


def _skip_balanced(text, i, open_, close):
    """Index just past the bracket group that opens at `text[i]`."""
    depth = 0
    while i < len(text):
        if text[i] == open_:
            depth += 1
        elif text[i] == close:
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    return len(text)


def _call_paren(text, i):
    """Where the `(` of a call starts, if `text[i:]` is one (`(…)` or `::<T>(…)`), else None."""
    while i < len(text) and text[i].isspace():
        i += 1
    if text.startswith("::", i):
        i += 2
        while i < len(text) and text[i].isspace():
            i += 1
        if not text.startswith("<", i):
            return None
        i = _skip_balanced(text, i, "<", ">")
        while i < len(text) and text[i].isspace():
            i += 1
    return i if text.startswith("(", i) else None


def _chain(text, i):
    """The methods called, in order, on the value that ends at `text[i]` (`.a(..).b::<T>(..)?`)."""
    names = []
    while True:
        m = METHOD_RE.match(text, i)
        if not m:
            return names
        paren = _call_paren(text, m.end())
        if paren is None:  # a field (`.0`, `.title`): it is read on its own by the caller's scan
            return names
        names.append(m.group(1))
        i = _skip_balanced(text, paren, "(", ")")
        while i < len(text) and text[i] == "?":
            i += 1


def _argument_hits(args, debug_fmt):
    """Content values among the formatted arguments (string literals already blanked)."""
    found = []
    for ident in IDENT_RE.finditer(args):
        parts = ident.group(0).split(".")
        paren = _call_paren(args, ident.end())
        if paren is not None:
            if len(parts) == 1:
                # A free function (`render_summary(..)`), or a later link of a chain already
                # judged from its receiver. What it returns cannot be judged from its name.
                continue
            # `a.b.method(..)…`: the value is `a.b`'s, carried through the chain.
            value = parts[-2]
            methods = [parts[-1]] + _chain(args, _skip_balanced(args, paren, "(", ")"))
            shown = ident.group(0) + "()"
        else:
            value = parts[-1]
            methods = _chain(args, ident.end())
            shown = ident.group(0)
        if any(m in SHAPE_METHODS for m in methods):
            continue  # a length or a yes/no about the content, not the content
        if is_content(value):
            found.append(shown)
        elif debug_fmt and value in DEBUG_TYPES and all(m in SAME_VALUE_METHODS for m in methods):
            found.append(shown + " (Debug)")
    return found


def violations(line):
    """Why this line's log call formats user content, or [] when it does not (or is no log)."""
    call = LOG_RE.search(line)
    if not call:
        return []
    rest = re.sub(r"\s//.*$", "", line[call.end():])
    rest = TRACING_TARGET_RE.sub("", rest)
    fmt_match = STRING_RE.search(rest)
    fmt = fmt_match.group(1).replace("{{", "").replace("}}", "") if fmt_match else ""
    found = []
    if DEBUG_TYPES_RE.search(fmt):
        found.append("debug")
    for inner in re.findall(r"\{([^{}]*)\}", fmt):
        name = inner.split(":")[0].strip()
        if name and IDENT_RE.fullmatch(name) and is_content(name.split(".")[-1]):
            found.append(name)
    if fmt_match:
        # tracing's fields come before the format string (`title = %title, "x"`): read their
        # values, not their names. For the print macros this part is empty.
        before = FIELD_NAME_RE.sub(" ", STRING_RE.sub('""', rest[: fmt_match.start()]))
        after = STRING_RE.sub('""', rest[fmt_match.end():])
    else:
        before, after = "", STRING_RE.sub('""', rest)
    debug_fmt = bool(DEBUG_SPEC_RE.search(fmt))
    found += _argument_hits(before, debug_fmt) + _argument_hits(after, debug_fmt)
    return found


OK_MARKER = "log-hygiene-ok"

# `.claude` holds local agent worktrees: copies of the repo, not its source.
SKIP_DIRS = {"target", "node_modules", ".git", "dist", ".next", ".claude"}
# Dev-only probes that exist to print what the pipeline produced. Not shipped, not a log.
SKIP_PATH_PARTS = {"examples"}


def repo_rust_files():
    root = pathlib.Path(".")
    for path in sorted(root.rglob("*.rs")):
        if any(part in SKIP_DIRS or part in SKIP_PATH_PARTS for part in path.parts):
            continue
        try:
            yield path.as_posix(), path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue


CHAR_LITERAL_RE = re.compile(r"'(?:[^'\\]|\\.|\\u\{[0-9A-Fa-f]+\})'")
RAW_STRING_RE = re.compile(r'(?<![A-Za-z0-9_])b?r(#*)"')
LINE_COMMENT_RE = re.compile(r"(?:^|\s)//.*$")
# A wrapped call longer than this is not a log call worth joining; stop rather than run away.
MAX_CALL_LINES = 40


def _call_closes(text, i):
    """Whether the bracket group opening at `text[i]` closes within `text`, skipping literals."""
    depth = 0
    n = len(text)
    while i < n:
        c = text[i]
        raw = RAW_STRING_RE.match(text, i) if c in "br" else None
        if raw:
            end = text.find('"' + raw.group(1), raw.end())
            if end < 0:
                return False
            i = end + 1 + len(raw.group(1))
            continue
        if c == '"':
            i += 1
            while i < n and text[i] != '"':
                i += 2 if text[i] == "\\" else 1
            if i >= n:
                return False
            i += 1
            continue
        if c == "'":
            lit = CHAR_LITERAL_RE.match(text, i)
            i = lit.end() if lit else i + 1  # otherwise a lifetime (`'a`)
            continue
        if c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
            if depth == 0:
                return True
        i += 1
    return False


def log_call(lines, i):
    """The log call that starts on `lines[i]`, joined across the lines rustfmt wrapped it onto.

    Returns the joined text and the index of its last line. A line with no log call, or a call
    that closes on its own line, is returned unchanged.
    """
    line = lines[i]
    call = LOG_RE.search(line)
    if not call:
        return line, i
    joined = line
    j = i
    while not _call_closes(joined, call.end() - 1):
        if j + 1 >= len(lines) or j + 1 - i >= MAX_CALL_LINES:
            break
        j += 1
        if joined is line:
            joined = LINE_COMMENT_RE.sub("", line)
        joined += " " + LINE_COMMENT_RE.sub("", lines[j]).strip()
    return joined, j


def scan(files):
    """Yield (path, lineno, line) for every bare log call interpolating user content."""
    hits = []
    for path, text in files:
        lines = text.splitlines()
        for i, line in enumerate(lines):
            stripped = line.strip()
            if stripped.startswith(("//", "///", "*")):
                continue
            call, last = log_call(lines, i)
            if not violations(call):
                continue
            # Escape hatch: the comment line directly above the call, or any line of it.
            prev = lines[i - 1] if i > 0 else ""
            if OK_MARKER in prev or any(OK_MARKER in l for l in lines[i : last + 1]):
                continue
            hits.append((path, i + 1, stripped))
    return hits


# --- E37: ops telemetry label allowlist (TypeScript) ---

ALLOWED_METRIC_LABELS = ("route", "method", "status", "mcp_method", "mcp_name")
ALLOWED_SPAN_ATTRIBUTES = (
    "http.route",
    "http.request.method",
    "http.response.status_code",
    "mcp.method.name",
    "mcp.tool.name",
)
# A label name made of any of these is refused even if someone adds it to the allowlist above.
FORBIDDEN_LABEL_WORDS = frozenset(
    "body text content prompt query key token secret authorization auth cookie bearer principal "
    "licence license user email name_raw path url header input output argument arguments result "
    "message summary title id ids ip".split()
)
ALLOWED_HEADERS = frozenset({"mcp-method", "mcp-name"})
# Reads that return a body, a query string or the raw request/response.
BODY_READ_RE = re.compile(
    r"\.req\.(?:json|text|arrayBuffer|formData|parseBody|blob|query|queries|raw)\b"
    r"|\bsearchParams\b"
    r"|\bc\.res\.(?:text|json|clone|body|arrayBuffer|blob)\b"
    r"|\brequest\.(?:json|text|body|arrayBuffer|formData)\b"
)
HEADER_CALL_RE = re.compile(r"\b(?:header|headers\.get)\(\s*([^)]*?)\s*\)")
ARRAY_DECL_RE = re.compile(r"\b(METRIC_LABELS|SPAN_ATTRIBUTES)\s*=\s*\[([^\]]*)\]")
TS_BLOCK_COMMENT_RE = re.compile(r"/\*.*?\*/", re.S)
TS_LINE_COMMENT_RE = re.compile(r"(?<![:\"'\w\\])//.*$", re.M)


def _ts_code(text):
    """The source with comments removed (strings kept), for the pattern checks."""
    return TS_LINE_COMMENT_RE.sub("", TS_BLOCK_COMMENT_RE.sub("", text))


def telemetry_violations(path, text):
    """Why an `observability.ts` could put content into a label, or [] when it cannot."""
    code = _ts_code(text)
    found = []
    decls = {m.group(1): re.findall(r'"([^"]*)"', m.group(2)) for m in ARRAY_DECL_RE.finditer(code)}
    for name, allowed in (("METRIC_LABELS", ALLOWED_METRIC_LABELS), ("SPAN_ATTRIBUTES", ALLOWED_SPAN_ATTRIBUTES)):
        if name not in decls:
            found.append(f"{path}: no `{name} = [...]` array literal")
            continue
        for label in decls[name]:
            words = set(re.split(r"[._]", label.lower()))
            if label not in allowed:
                found.append(f"{path}: {name} has `{label}`, which is not in the allowlist")
            elif words & FORBIDDEN_LABEL_WORDS:
                found.append(f"{path}: {name} has `{label}`, a content or credential word")
    for m in BODY_READ_RE.finditer(code):
        line = code.count("\n", 0, m.start()) + 1
        found.append(f"{path}:{line}: reads a body, query or raw request (`{m.group(0)}`)")
    for m in HEADER_CALL_RE.finditer(code):
        arg = m.group(1)
        literal = re.fullmatch(r'"([^"]*)"', arg)
        line = code.count("\n", 0, m.start()) + 1
        if not literal:
            found.append(f"{path}:{line}: reads a header by a computed name (`{m.group(0)}`)")
        elif literal.group(1).lower() not in ALLOWED_HEADERS:
            found.append(f"{path}:{line}: reads the `{literal.group(1)}` header")
    return found


def repo_telemetry_files():
    for path in sorted(pathlib.Path("apps").glob("*/src/**/observability.ts")):
        if any(part in SKIP_DIRS for part in path.parts):
            continue
        yield path.as_posix(), path.read_text(encoding="utf-8", errors="replace")


def telemetry_self_test():
    good = """
// a comment may mention c.req.json() and header("authorization") freely
/* and so may c.res.text() */
export const METRIC_LABELS = ["route", "method", "status", "mcp_method", "mcp_name"] as const;
export const SPAN_ATTRIBUTES = ["http.route", "http.request.method", "http.response.status_code", "mcp.method.name", "mcp.tool.name"] as const;
const m = c.req.header("mcp-method") ?? c.res.headers.get("mcp-name");
const url = "https://collector.example/v1/traces";
"""
    assert not telemetry_violations("apps/x/src/observability.ts", good), telemetry_violations("apps/x/src/observability.ts", good)
    head = good.split("const m =")[0]
    bad = [
        head.replace('"mcp_name"]', '"mcp_name", "principal"]'),
        head.replace('"mcp_name"]', '"mcp_name", "mcp_name_raw"]'),
        head.replace('"mcp.tool.name"]', '"mcp.tool.name", "http.request.body"]'),
        head.replace("export const METRIC_LABELS", "export const LABELS"),
        head + 'const b = await c.req.json();',
        head + 'const q = c.req.query("q");',
        head + 'const s = new URL(c.req.url).searchParams;',
        head + 'const t = await c.res.text();',
        head + 'const a = c.req.header("authorization");',
        head + 'const a = c.req.header(name);',
        head + 'const r = c.req.raw;',
    ]
    for n, text in enumerate(bad):
        assert telemetry_violations(f"apps/x/src/observability{n}.ts", text), f"telemetry detector missed case {n}"


def self_test():
    bad = [
        # The real 2026-08-13 findings, in their original shape.
        ("crates/fake/src/a.rs", 'eprintln!("[meeting] saw {bundle} title={title:?}");'),
        ("crates/fake/src/b.rs", 'eprintln!("[selftest] {:?} {:?}", a.level, a.action);'),
        ("crates/fake/src/c.rs", 'println!("transcribed: {text}");'),
    ]
    found = {p for p, _, _ in scan(bad)}
    assert "crates/fake/src/a.rs" in found, "detector missed a window title in a log line"
    assert "crates/fake/src/b.rs" in found, "detector missed a positional Debug-printed action"
    assert "crates/fake/src/c.rs" in found, "detector missed transcribed text in a log line"

    # The bypasses #913 found, each in its own file so one miss cannot hide behind another.
    bypasses = [
        'eprintln!("[cap] {}", title);',
        'eprintln!("[cap] {window_title}");',
        'eprintln!("[cap] {} {}", app, focus.window_title);',
        'eprint!("{msg}");',
        'print!("{}", prompt_text);',
        'dbg!(note_body);',
        'tracing::info!("{query}");',
        'eprintln!("{:?}", a.action);',
        # Review of #914: a method on the content still prints it, and so does pretty Debug.
        'eprintln!("{}", title.trim());',
        'eprintln!("{}", msg.to_string());',
        'eprintln!("{}", body.clone());',
        'eprintln!("{}", text.as_str());',
        'eprintln!("{}", focus.window_title.trim().to_lowercase());',
        'eprintln!("{:#?}", action);',
        'eprintln!("{action:#?}");',
        # A chain keeps the text until a link returns a shape: truncating still prints it.
        'eprintln!("{}", title.chars().take(40).collect::<String>());',
        'eprintln!("{}", title.lines().next().unwrap_or(""));',
        'eprintln!("{}", self.title.as_deref().unwrap_or("-"));',
        'eprintln!("{:?}", a.action.clone());',
        # tracing records its fields' values, and `target:` comes before the format string.
        'tracing::info!(title = %title, "focused");',
        'tracing::warn!(target: "shogun", "{query}");',
    ]
    for n, line in enumerate(bypasses):
        assert scan([(f"crates/fake/src/by{n}.rs", line)]), f"detector missed: {line}"

    # #915: rustfmt wraps a long call, so the arguments sit on the lines after the macro.
    wrapped = [
        'eprintln!(\n    "[cap] {}",\n    focus.window_title\n);',
        'eprintln!(\n    "[cap] {} in {}", // why\n    bundle,\n    title.trim()\n);',
        'tracing::info!(\n    title = %title,\n    "focused"\n);',
        'eprintln!(\n    "[cap] {})",\n    \')\',\n    text\n);',
    ]
    for n, call in enumerate(wrapped):
        assert scan([(f"crates/fake/src/w{n}.rs", call)]), f"detector missed a wrapped call: {call!r}"

    debug_case = [("crates/fake/src/d.rs", 'eprintln!("[selftest] {action:?}");')]
    assert scan(debug_case), "detector missed a Debug-printed action"

    good = [
        # Structural identifiers this codebase logs on purpose.
        ("crates/fake/src/e.rs", 'eprintln!("[shell] panel docked ({anchor}) at {x},{y}");'),
        ("crates/fake/src/f.rs", 'eprintln!("[meeting] saw {bundle} title_len={n}");'),
        # The sanctioned path scrubs, so it is never a hit.
        ("crates/fake/src/g.rs", 'shogun_core::elog!("[ui] {msg}");'),
        # An explicit, justified exemption.
        ("crates/fake/src/h.rs", 'eprintln!("[shell] window `{label}` {msg}"); // log-hygiene-ok: AppKit status strings'),
        # A comment naming the rule must not trip it.
        ("crates/fake/src/i.rs", '// never eprintln! a {title} — CLAUDE.md log rule'),
        # Shapes and ids of content, and function calls, are not the content.
        ("crates/fake/src/k.rs", 'eprintln!("[cap] {title_len} {note_id} {}", text.len());'),
        ("crates/fake/src/l.rs", 'print!("{}", report::render_summary(&result));'),
        ("crates/fake/src/m.rs", 'eprintln!("{} chars from {}", title.chars().count(), bundle);'),
        # A content word inside a string argument, or as escaped braces, is literal text.
        ("crates/fake/src/n.rs", 'eprintln!("{} {{title}}", "title");'),
        # A plain Debug of a non-content value, and of what a method on a content type returns.
        ("crates/fake/src/o.rs", 'eprintln!("{:?}", status);'),
        ("crates/fake/src/u.rs", 'eprintln!("shortcut fired: {:?}", event.state());'),
        # Methods that return a shape of the content, and methods on a non-content receiver.
        ("crates/fake/src/p.rs", 'eprintln!("{} {} {}", text.len(), body.is_empty(), title.chars().count());'),
        ("crates/fake/src/q.rs", 'eprintln!("error: {}", err.to_string());'),
        # A shape anywhere in the chain ends the content.
        ("crates/fake/src/r.rs", 'eprintln!("{} {}", title.trim().len(), msg.split_whitespace().count());'),
        ("crates/fake/src/s.rs", 'eprintln!("{}", text.len());'),
        # A tracing field is judged by its value, not its name.
        ("crates/fake/src/t.rs", 'tracing::info!(title = title.len(), summary = %status, "x");'),
    ]
    hits = scan(good)
    assert not hits, f"detector flagged honest code: {hits}"

    # A wrapped call with shapes only, one whose marker sits inside it, and a call that closes on
    # its own line followed by unrelated content lines.
    good_wrapped = [
        'eprintln!(\n    "[cap] {} chars from {}",\n    title.chars().count(),\n    bundle\n);',
        'eprintln!(\n    "[shell] {}",\n    msg // log-hygiene-ok: AppKit status strings\n);',
        'eprintln!("[cap] done");\nlet title = text.trim();',
        'eprintln!(\n    r#"[cap] ("{}")"#,\n    bundle\n);',
    ]
    for n, call in enumerate(good_wrapped):
        assert not scan([(f"crates/fake/src/gw{n}.rs", call)]), f"detector flagged: {call!r}"

    above = [("crates/fake/src/j.rs", '// log-hygiene-ok: provider status only\neprintln!("{msg}");')]
    assert not scan(above), "marker on the line above was not honoured"
    telemetry_self_test()
    print("check-log-hygiene self-test OK")


def main():
    if "--self-test" in sys.argv:
        self_test()
        return 0
    files = list(repo_rust_files())
    hits = scan(files)
    telemetry = list(repo_telemetry_files())
    telemetry_hits = [v for path, text in telemetry for v in telemetry_violations(path, text)]
    if telemetry_hits:
        print("ops telemetry violation: a metric label or span attribute could carry content.")
        for v in telemetry_hits:
            print(f"  - {v}")
        print(
            "\nLabels are closed sets (plan E37): route template, method, status, Mcp-Method, Mcp-Name. "
            "Never read a body, query or credential in observability.ts, and add a label only by "
            "extending the allowlist in scripts/check-log-hygiene.py, in review."
        )
        return 1
    if hits:
        print("log-rule violation: a bare log line interpolates user-derived content.")
        for path, line, text in hits:
            print(f"  - {path}:{line}: {text}")
        print(
            "\nCaptured text must not reach a log, debug builds included. Either route the line "
            "through `elog!` (shogun_core::log_redact — it scrubs keys, emails and URLs), log a "
            "shape instead of the value (`title_len={n}`), or, if the identifier is structural "
            "despite its name, mark the site `// log-hygiene-ok: <reason>`."
        )
        return 1
    print(
        f"log hygiene OK: no bare log line interpolates user content ({len(files)} files scanned); "
        f"ops telemetry labels within the allowlist ({len(telemetry)} observability modules)."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
