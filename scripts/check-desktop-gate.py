#!/usr/bin/env python3
"""Desktop-gate guard (E1-T7): no `cfg(target_os = "macos")` in the default build.

ShogunAgent's default Linux build must carry no module that exists only for the ShogunAI Mac
app. Those modules sit behind the cargo feature `desktop` in `shogun-core` and `shogun-mcp`.
This guard walks every crate's module tree from its roots (lib.rs, main.rs, src/bin/*.rs,
tests/*.rs, examples/*.rs, benches/*.rs, build.rs and the `[[bin]]` / `[[example]]` /
`[[test]]` / `[[bench]]` entries of Cargo.toml) and marks a file as *gated* when

- the `mod` declaration that reaches it carries a `#[cfg(...)]` naming `feature = "desktop"`
  (not under `not(...)`), or sits under such a module,
- the file's own inner attribute `#![cfg(...)]` names `feature = "desktop"`, or
- its Cargo target has `required-features` containing `desktop`.

Every other reachable `.rs` file is compiled in the default feature set. If such a file contains
`target_os = "macos"` outside a `//` comment, the guard fails — unless the file is in ALLOWLIST
below, each entry of which says why that site is not the Mac app's and stays in the server
build. An allowlist entry whose file no longer needs it (gated, missing, or clean) also fails, so
the list stays minimal.

Usage:
    scripts/check-desktop-gate.py             # check the repository
    scripts/check-desktop-gate.py --verbose   # also list gated and unreachable files
    scripts/check-desktop-gate.py --self-test # prove the walker on synthetic crates
"""

from __future__ import annotations

import argparse
import os
import re
import sys
import tempfile
import tomllib
from pathlib import Path

# File (relative to the repository root) -> why its `target_os = "macos"` is not Mac-app code.
ALLOWLIST: dict[str, str] = {
    "crates/shogun-license/src/device.rs": (
        "`hardware_device_id` reads IOPlatformUUID through IOKit on macOS and answers None "
        "elsewhere; the licence token's device binding needs it in every build that verifies "
        "a token the Mac app sends (E3-T4)."
    ),
    "crates/shogun-integrations/src/lib.rs": (
        "declares the Keychain token store and the code-signature reader, both macOS-only "
        "modules; the Keychain store becomes a `TokenStore` trait with a hosted implementation "
        "in E5-T5, which removes this entry."
    ),
    "crates/shogun-integrations/src/oauth_flow.rs": (
        "`open_browser` shells out to `open` on macOS and returns the URL to the caller "
        "elsewhere; the hosted OAuth flow (E5) returns the URL on every platform."
    ),
    "crates/spike-harness/src/cpu.rs": (
        "the `proc_pid_rusage` CPU reader of the SLO harness; `memory-bench` carries the "
        "Linux `/proc` reader so the bench runs on Linux CI."
    ),
    "crates/memory-bench/src/resources.rs": (
        "chooses the platform CPU / RSS reader for the bench's resource section (Linux "
        "`/proc`, macOS `spike_harness::cpu`, none elsewhere)."
    ),
    "crates/shogun-memory/src/embed_onnx.rs": (
        "the ONNX runtime's shared-library file name differs per OS (`.dylib` / `.so`); the "
        "developer-only local embedder loads it dynamically on either."
    ),
}

TARGET_OS = re.compile(r'target_os\s*=\s*"macos"')
DESKTOP_FEATURE = re.compile(r'feature\s*=\s*"desktop"')
MOD_DECL = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;")
INNER_CFG = re.compile(r"^\s*#!\[cfg\((.*)\)\]\s*$", re.S)
PATH_ATTR = re.compile(r'#\[path\s*=\s*"([^"]+)"\]')
SOURCE_DIRS = ("tests", "examples", "benches")


def strip_line_comment(line: str) -> str:
    """Drop a `//` comment. Good enough for this guard: `//` inside a string literal is rare in
    cfg-bearing lines, and a false positive there fails loudly rather than silently."""
    index = line.find("//")
    return line if index < 0 else line[:index]


def mentions_macos(text: str) -> bool:
    return any(TARGET_OS.search(strip_line_comment(line)) for line in text.splitlines())


def names_desktop(cfg_expression: str) -> bool:
    """`feature = "desktop"` outside any `not(...)`."""
    depth_not = 0
    tokens = re.finditer(r'not\s*\(|\(|\)|feature\s*=\s*"desktop"', cfg_expression)
    stack: list[bool] = []
    for match in tokens:
        token = match.group(0)
        if token.startswith("not"):
            stack.append(True)
            depth_not += 1
        elif token == "(":
            stack.append(False)
        elif token == ")":
            if stack and stack.pop():
                depth_not -= 1
        elif depth_not == 0:
            return True
    return False


def attribute_blocks(lines: list[str], start: int) -> tuple[list[str], int]:
    """Collect the outer attributes (`#[...]`, possibly spanning lines) and doc comments that start
    at `start`; return them and the index of the first following line."""
    attributes: list[str] = []
    index = start
    while index < len(lines):
        stripped = lines[index].strip()
        if stripped.startswith("///") or stripped.startswith("//") or stripped == "":
            index += 1
            continue
        if stripped.startswith("#[") or stripped.startswith("#!["):
            text = stripped
            while text.count("[") > text.count("]") or text.count("(") > text.count(")"):
                index += 1
                if index >= len(lines):
                    break
                text += "\n" + lines[index].strip()
            attributes.append(text)
            index += 1
            continue
        break
    return attributes, index


def inner_cfg_gates(source: str) -> bool:
    """Whether the file's own `#![cfg(...)]` names the desktop feature."""
    lines = source.splitlines()
    attributes, _ = attribute_blocks(lines, 0)
    for attribute in attributes:
        match = INNER_CFG.match(attribute)
        if match and names_desktop(match.group(1)):
            return True
    return False


def declared_modules(source: str) -> list[tuple[str, bool, str | None]]:
    """Every `mod name;` with whether its attributes name the desktop feature and its `#[path]`."""
    lines = source.splitlines()
    found: list[tuple[str, bool, str | None]] = []
    index = 0
    while index < len(lines):
        attributes, next_index = attribute_blocks(lines, index)
        if next_index >= len(lines):
            break
        match = MOD_DECL.match(lines[next_index])
        if match:
            gated = False
            explicit_path = None
            for attribute in attributes:
                cfg = re.match(r"#\[cfg\((.*)\)\]$", attribute, re.S)
                if cfg and names_desktop(cfg.group(1)):
                    gated = True
                path = PATH_ATTR.search(attribute)
                if path:
                    explicit_path = path.group(1)
            found.append((match.group(1), gated, explicit_path))
        index = max(next_index + 1, index + 1)
    return found


def child_candidates(parent: Path, name: str, explicit_path: str | None) -> list[Path]:
    if explicit_path is not None:
        return [parent.parent / explicit_path]
    if parent.name in ("lib.rs", "main.rs", "mod.rs") or parent.parent.name in SOURCE_DIRS + ("bin",):
        base = parent.parent
    else:
        base = parent.parent / parent.stem
    return [base / f"{name}.rs", base / name / "mod.rs"]


def cargo_targets(crate: Path) -> list[tuple[Path, bool]]:
    """Roots from Cargo.toml target tables, with whether `required-features` names `desktop`."""
    manifest = crate / "Cargo.toml"
    if not manifest.is_file():
        return []
    with manifest.open("rb") as handle:
        data = tomllib.load(handle)
    roots: list[tuple[Path, bool]] = []
    defaults = {"bin": "src/bin", "example": "examples", "test": "tests", "bench": "benches"}
    for table, directory in defaults.items():
        for entry in data.get(table, []) or []:
            gated = "desktop" in (entry.get("required-features") or [])
            path = entry.get("path")
            if path is None and "name" in entry:
                path = f"{directory}/{entry['name']}.rs"
            if path is not None:
                roots.append((crate / path, gated))
    return roots


def crate_roots(crate: Path) -> list[tuple[Path, bool]]:
    roots: list[tuple[Path, bool]] = []
    declared = cargo_targets(crate)
    declared_paths = {path.resolve() for path, _ in declared}
    roots.extend(declared)
    for name in ("lib.rs", "main.rs"):
        candidate = crate / "src" / name
        if candidate.is_file():
            roots.append((candidate, False))
    build_script = crate / "build.rs"
    if build_script.is_file():
        roots.append((build_script, False))
    for candidate in sorted((crate / "src" / "bin").glob("*.rs")):
        if candidate.resolve() not in declared_paths:
            roots.append((candidate, False))
    for directory in SOURCE_DIRS:
        folder = crate / directory
        if not folder.is_dir():
            continue
        for candidate in sorted(folder.glob("*.rs")):
            if candidate.resolve() not in declared_paths:
                roots.append((candidate, False))
        for candidate in sorted(folder.glob("*/main.rs")):
            if candidate.resolve() not in declared_paths:
                roots.append((candidate, False))
    return roots


def walk(crate: Path) -> dict[Path, bool]:
    """Every reachable `.rs` file -> True when some path reaches it without the desktop gate."""
    reached: dict[Path, bool] = {}
    queue = [(path.resolve(), gated) for path, gated in crate_roots(crate) if path.is_file()]
    while queue:
        path, gated = queue.pop()
        source = path.read_text(encoding="utf-8", errors="replace")
        gated = gated or inner_cfg_gates(source)
        in_default = not gated
        if path in reached:
            if reached[path] or not in_default:
                continue
            reached[path] = True
        else:
            reached[path] = in_default
        for name, child_gated, explicit_path in declared_modules(source):
            for candidate in child_candidates(path, name, explicit_path):
                if candidate.is_file():
                    queue.append((candidate.resolve(), gated or child_gated))
                    break
    return reached


def check(root: Path, allowlist: dict[str, str], verbose: bool = False) -> list[str]:
    """Return the violations (empty when the tree passes)."""
    root = root.resolve()
    crates = sorted(p for p in (root / "crates").iterdir() if (p / "Cargo.toml").is_file())
    reached: dict[Path, bool] = {}
    for crate in crates:
        for path, in_default in walk(crate).items():
            reached[path] = reached.get(path, False) or in_default
    problems: list[str] = []
    allowed_used: set[str] = set()
    for path in sorted(reached):
        relative = path.relative_to(root).as_posix()
        source = path.read_text(encoding="utf-8", errors="replace")
        if not mentions_macos(source):
            continue
        if not reached[path]:
            if verbose:
                print(f"gated:   {relative}")
            continue
        if relative in allowlist:
            allowed_used.add(relative)
            if verbose:
                print(f"allowed: {relative}")
            continue
        problems.append(
            f'{relative}: `target_os = "macos"` in a file the default build compiles — put the '
            'module behind `#[cfg(feature = "desktop")]` or move the site into one that is'
        )
    for relative in sorted(set(allowlist) - allowed_used):
        path = root / relative
        if not path.is_file():
            why = "the file does not exist"
        elif path.resolve() not in reached:
            why = "no crate reaches the file"
        elif not reached[path.resolve()]:
            why = "the file is already behind the desktop feature"
        else:
            why = 'the file no longer contains `target_os = "macos"`'
        problems.append(f"{relative}: stale ALLOWLIST entry — {why}; remove it")
    if verbose:
        for crate in crates:
            for path in sorted(crate.rglob("*.rs")):
                if path.resolve() not in reached:
                    print(f"unreached: {path.relative_to(root).as_posix()}")
    return problems


# --- self-test ---------------------------------------------------------------------------------

MACOS_SITE = '#[cfg(target_os = "macos")]\npub fn mac() {}\n#[cfg(not(target_os = "macos"))]\npub fn mac() {}\n'


def write(root: Path, relative: str, text: str) -> None:
    path = root / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def synthetic_crate(root: Path, manifest_extra: str = "") -> None:
    write(root, "crates/demo/Cargo.toml", '[package]\nname = "demo"\nversion = "0.1.0"\n' + manifest_extra)
    write(
        root,
        "crates/demo/src/lib.rs",
        "pub mod clean;\n#[cfg(feature = \"desktop\")]\npub mod mac;\npub mod nested;\n"
        "#[cfg(all(feature = \"db\", feature = \"desktop\"))]\npub mod glue;\n",
    )
    write(root, "crates/demo/src/clean.rs", '// a comment may say target_os = "macos"\npub fn f() {}\n')
    write(root, "crates/demo/src/mac.rs", MACOS_SITE)
    write(root, "crates/demo/src/glue/mod.rs", "pub mod inner;\n")
    write(root, "crates/demo/src/glue/inner.rs", MACOS_SITE)
    write(root, "crates/demo/src/nested.rs", "#[cfg(feature = \"desktop\")]\nmod deep;\npub mod open;\n")
    write(root, "crates/demo/src/nested/deep.rs", MACOS_SITE)
    write(root, "crates/demo/src/nested/open.rs", "pub fn g() {}\n")
    write(root, "crates/demo/tests/gated.rs", '//! doc\n#![cfg(all(feature = "net", feature = "desktop"))]\n' + MACOS_SITE)


def expect(condition: bool, message: str) -> None:
    if not condition:
        sys.exit(f"self-test failed: {message}")


def self_test() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        synthetic_crate(root)
        expect(check(root, {}) == [], "a fully gated tree passes")

        # A plain module with the cfg fails, and the message names the file.
        write(root, "crates/demo/src/nested/open.rs", MACOS_SITE)
        problems = check(root, {})
        expect(len(problems) == 1 and "nested/open.rs" in problems[0], f"ungated site is caught: {problems}")
        # Allowlisting it passes; a stale allowlist entry fails.
        expect(check(root, {"crates/demo/src/nested/open.rs": "why"}) == [], "allowlist admits the site")
        write(root, "crates/demo/src/nested/open.rs", "pub fn g() {}\n")
        problems = check(root, {"crates/demo/src/nested/open.rs": "why"})
        expect(len(problems) == 1 and "stale ALLOWLIST" in problems[0], f"stale entry is caught: {problems}")

        # `not(feature = "desktop")` is not a gate.
        write(root, "crates/demo/tests/negated.rs", '#![cfg(not(feature = "desktop"))]\n' + MACOS_SITE)
        problems = check(root, {})
        expect(len(problems) == 1 and "negated.rs" in problems[0], f"negated cfg is not a gate: {problems}")
        (root / "crates/demo/tests/negated.rs").unlink()

        # An example compiles by default unless its Cargo target requires the feature.
        write(root, "crates/demo/examples/probe.rs", MACOS_SITE + "fn main() {}\n")
        problems = check(root, {})
        expect(len(problems) == 1 and "examples/probe.rs" in problems[0], f"ungated example is caught: {problems}")
        synthetic_crate(
            root,
            '[[example]]\nname = "probe"\npath = "examples/probe.rs"\nrequired-features = ["desktop"]\n',
        )
        expect(check(root, {}) == [], "required-features gates the example")

        # A `#[path]` module is followed, and `cfg!(target_os = "macos")` counts too.
        write(root, "crates/demo/src/lib.rs", '#[path = "elsewhere/thing.rs"]\nmod thing;\n')
        write(root, "crates/demo/src/elsewhere/thing.rs", 'pub fn h() -> bool { cfg!(target_os = "macos") }\n')
        problems = check(root, {})
        expect(len(problems) == 1 and "elsewhere/thing.rs" in problems[0], f"#[path] module is walked: {problems}")

        # The cfg expression parser.
        expect(names_desktop('feature = "desktop"'), "bare feature")
        expect(names_desktop('all(feature = "db", feature = "desktop")'), "inside all()")
        expect(not names_desktop('not(feature = "desktop")'), "negated")
        expect(not names_desktop('all(feature = "db", not(feature = "desktop"))'), "negated inside all()")
        expect(names_desktop('any(not(feature = "db"), feature = "desktop")'), "sibling of a not()")
    print("check-desktop-gate self-test: OK")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--self-test", action="store_true", help="prove the walker on synthetic crates")
    parser.add_argument("--root", default=None, help="repository root (default: the parent of scripts/)")
    parser.add_argument("--verbose", action="store_true", help="list gated, allowed and unreached files")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return 0
    root = Path(args.root) if args.root else Path(__file__).resolve().parent.parent
    problems = check(root, ALLOWLIST, verbose=args.verbose)
    if problems:
        print("check-desktop-gate: FAIL", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1
    print(f"check-desktop-gate: OK ({len(ALLOWLIST)} allowlisted files, all still needed)")
    return 0


if __name__ == "__main__":
    os.environ.setdefault("PYTHONDONTWRITEBYTECODE", "1")
    sys.exit(main())
