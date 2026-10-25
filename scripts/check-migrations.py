#!/usr/bin/env python3
"""Schema backward-compatibility guard (CLAUDE.md: memory lives for years; never break compat).

Migrations must be additive. A DROP COLUMN / DROP TABLE / RENAME / ALTER COLUMN in a migration
breaks readers of older data and corrupts a memory that is meant to survive for years. This check
scans the refinery migration SQL and fails on any backward-incompatible statement.

It also guards refinery's history (#800): refinery aborts with DivergentVersion when an applied
migration's name or checksum differs from the binary's, so every file that reached main or a
release is frozen. Always checked: filenames match `V<int>__<name>.sql` and no two share a
version. With `--immutable-against REF...` / `--immutable-against-releases` every migration at
those git refs must still exist byte-identical (same blob id) in the working tree; release tags
skip versions above the working tree's max (a hotfix from an older line), explicit refs do not.
No ledger: git history is the ledger. Rules: docs/migrations/README.md.

Run from the repo root:  python3 scripts/check-migrations.py
Freeze check (CI):        python3 scripts/check-migrations.py --immutable-against-releases --immutable-against origin/main
Self-test the detector:   python3 scripts/check-migrations.py --self-test
"""

import argparse
import hashlib
import os
import pathlib
import re
import subprocess
import sys
import tempfile
from unittest.mock import patch

MIGRATIONS_DIR = "crates/shogun-memory/src/migrations"

# A table name as SQLite accepts it: bare, "double", `back` or [bracket] quoted, optionally
# schema-qualified (`"main"."people"`).
_NAME = r'(?:"[^"]+"|`[^`]+`|\[[^\]]+\]|\w+)'
_TABLE = rf"{_NAME}(?:\s*\.\s*{_NAME})?"

# Backward-incompatible statements (case-insensitive). Additive DDL (CREATE, ADD COLUMN) is fine.
BANNED = [
    (re.compile(r"\bDROP\s+TABLE\b", re.IGNORECASE), "DROP TABLE"),
    (re.compile(r"\bDROP\s+COLUMN\b", re.IGNORECASE), "DROP COLUMN"),
    (re.compile(r"\bRENAME\s+COLUMN\b", re.IGNORECASE), "RENAME COLUMN"),
    (re.compile(r"\bRENAME\s+TO\b", re.IGNORECASE), "RENAME TABLE"),
    # `ALTER ... DROP` / `ALTER COLUMN ... TYPE` — a type/shape change breaks old readers.
    (re.compile(r"\bALTER\s+COLUMN\b", re.IGNORECASE), "ALTER COLUMN"),
    # SQLite makes the COLUMN keyword optional (#913): `ALTER TABLE t DROP c` and
    # `ALTER TABLE t RENAME c TO d` are the same changes without it. The lookaheads leave the
    # keyworded forms to the rules above, so one statement is reported once.
    (re.compile(rf"\bALTER\s+TABLE\s+{_TABLE}\s+DROP\b(?!\s+COLUMN\b)", re.IGNORECASE), "DROP COLUMN"),
    (re.compile(rf"\bALTER\s+TABLE\s+{_TABLE}\s+RENAME\b(?!\s+(?:COLUMN|TO)\b)", re.IGNORECASE),
     "RENAME COLUMN"),
]


def strip_line_comments(text):
    """Drop `-- ...` and `/* ... */` comments, and blank the inside of '…' string literals, so a
    banned keyword in prose or in data doesn't false-positive. Quotes are tracked rather than
    cutting at the first `--` on a line, which let a `'--'` literal hide the statement after it
    (#913). Quoted identifiers ("people", `people`, [people]) are kept, since the rules read table
    names, and an apostrophe inside one does not open a string literal."""
    closers = {"'": "'", '"': '"', "`": "`", "[": "]"}
    out, i, n, close = [], 0, len(text), None
    while i < n:
        c = text[i]
        if close:
            if c == close:
                if close != "]" and i + 1 < n and text[i + 1] == close:  # '' "" `` inside one
                    out.append("  " if close == "'" else close * 2)
                    i += 2
                    continue
                close = None
                out.append(c)
            elif close == "'":
                out.append("\n" if c == "\n" else " ")
            else:
                out.append(c)
            i += 1
        elif c in closers:
            close = closers[c]
            out.append(c)
            i += 1
        elif text.startswith("--", i):
            end = text.find("\n", i)
            i = n if end < 0 else end
        elif text.startswith("/*", i):
            end = text.find("*/", i + 2)
            i = n if end < 0 else end + 2
        else:
            out.append(c)
            i += 1
    return "".join(out)


# A migration may declare a sanctioned exception with a `-- non-additive-ok: <reason>` line,
# but ONLY if the file also carries a rollback plan (CLAUDE.md: unavoidable changes need a
# decision record + rollback plan). Data-preserving table rebuilds (SQLite cannot ALTER a CHECK
# in place: create-copy-drop-rename) are the intended use; the reason line IS the decision record.
EXCEPTION_MARK = re.compile(r"^--\s*non-additive-ok:\s*\S", re.IGNORECASE | re.MULTILINE)
ROLLBACK_MARK = re.compile(r"ロールバック手順|rollback", re.IGNORECASE)

# Already-applied SQL must remain byte-identical: refinery includes comments in its checksum.
# V36 needs a table rebuild to replace UNIQUE(app, text) with UNIQUE(app, person, text).
# Pin the reviewed bytes and keep the decision/rollback outside the immutable migration.
APPLIED_REBUILDS = {
    f"{MIGRATIONS_DIR}/V36__writing_voice_person.sql": (
        "104cb4c63ffc0dfd25cbdba8e960b6188bef4728424631a829942a0d507b1527",
        "docs/migrations/V36-rollback.md",
    ),
}


def sanctioned(text):
    """True when the file declares a non-additive exception AND documents a rollback plan."""
    return bool(EXCEPTION_MARK.search(text)) and bool(ROLLBACK_MARK.search(text))


def scan(files):
    """Yield (path, label, snippet) for every banned statement in the migration SQL."""
    hits = []
    for path, text in files:
        if path in APPLIED_REBUILDS:
            digest, decision = APPLIED_REBUILDS[path]
            decision_path = pathlib.Path(decision)
            if (hashlib.sha256(text.encode("utf-8")).hexdigest() == digest
                    and decision_path.is_file()
                    and sanctioned(decision_path.read_text(encoding="utf-8"))):
                continue
            hits.append((path, "UNREVIEWED REBUILD", "SQL changed or decision/rollback is missing"))
            continue
        if sanctioned(text):
            continue
        code = strip_line_comments(text)
        for regex, label in BANNED:
            for m in regex.finditer(code):
                start = max(0, m.start() - 20)
                hits.append((path, label, code[start : m.end() + 20].strip().replace("\n", " ")))
    return hits


# refinery's own pattern is `^[UV](\d+(?:\.\d+)?)__(\w+)$`; this repo only uses versioned SQL.
FILENAME = re.compile(r"^V(\d+)__(\w+)\.sql$")


def name_problems(names):
    """Malformed filenames and duplicate version numbers among migration basenames."""
    problems, seen = [], {}
    for name in sorted(names):
        m = FILENAME.match(name)
        if not m:
            problems.append(f"{name}: malformed name — refinery needs V<int>__<name>.sql")
            continue
        seen.setdefault(int(m.group(1)), []).append(name)
    top = max(seen, default=0)
    for version, group in sorted(seen.items()):
        if len(group) > 1:
            problems.append(
                f"duplicate version V{version}: {', '.join(group)} — renumber yours above main's max "
                f"(V{top + 1}); two migrations can never share a version"
            )
    return problems


ROLLBACK_DIR = "docs/migrations"
HISTORY_DELETE = re.compile(r"refinery_schema_history\s+WHERE\s+version\s*=\s*(\d+)", re.IGNORECASE)


def rollback_problems(versions, docs):
    """Every V<n> needs docs/migrations/V<n>-rollback.md, and every history-row delete in it must
    name n (#780). Deleting another version's row leaves that version unapplied below the max, so
    refinery's verify_migrations aborts with MissingVersion and the memory DB will not open.

    `versions`: the migration version numbers. `docs`: {version: rollback doc text} for the docs
    that exist.
    """
    problems = []
    for v in sorted(set(versions)):
        text = docs.get(v)
        if text is None:
            problems.append(f"V{v}: no {ROLLBACK_DIR}/V{v}-rollback.md — every migration needs a rollback procedure")
            continue
        wrong = sorted({int(n) for n in HISTORY_DELETE.findall(text)} - {v})
        if wrong:
            listed = ", ".join(str(n) for n in wrong)
            problems.append(
                f"V{v}-rollback.md deletes refinery_schema_history version {listed} — it must delete version {v} "
                f"(another version's row makes refinery abort with MissingVersion)"
            )
    return problems


def rollback_docs(versions):
    docs = {}
    for v in versions:
        path = pathlib.Path(ROLLBACK_DIR) / f"V{v}-rollback.md"
        if path.is_file():
            docs[v] = path.read_text(encoding="utf-8")
    return docs


class GitError(Exception):
    pass


def git(root, *args):
    r = subprocess.run(["git", *args], cwd=root, capture_output=True, text=True)
    if r.returncode != 0:
        raise GitError(f"git {' '.join(args)}: {r.stderr.strip() or 'failed'}")
    return r.stdout


def release_tags(root):
    tags = git(root, "tag", "--list", "v*").split()
    if not tags:
        # Fail closed: a CI checkout without tags must not look like "nothing to compare".
        raise GitError("no v* release tags found — fetch them (git fetch --tags) before checking")
    return tags


def version_of(name):
    m = FILENAME.match(pathlib.PurePosixPath(name).name)
    return int(m.group(1)) if m else None


def tree_max_version(root):
    """Highest V<n> in the working tree at `root` (0 when there is none)."""
    return max((v for p in (pathlib.Path(root) / MIGRATIONS_DIR).glob("*")
                if (v := version_of(p.name)) is not None), default=0)


def frozen_problems(root, ref, ceiling=None):
    """Every migration at `ref` must exist unchanged (same blob id) in the working tree at `root`.

    `ceiling` (release tags only): ignore files at `ref` whose version is above it. A hotfix tagged
    from an older release line legitimately lacks the newer line's migrations; explicit refs (the
    PR base, the previous main) never get a ceiling, so deleting main's newest file still fails.
    """
    try:
        git(root, "rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}")
    except GitError:
        raise GitError(f"cannot resolve ref {ref!r} — refusing to skip the freeze check") from None
    frozen = {}
    for line in git(root, "ls-tree", "-z", ref, "--", f"{MIGRATIONS_DIR}/").split("\0"):
        if line:
            meta, path = line.split("\t", 1)
            v = version_of(path)
            if ceiling is not None and v is not None and v > ceiling:
                continue
            frozen[path] = meta.split()[2]
    here = [p.relative_to(root).as_posix() for p in sorted((pathlib.Path(root) / MIGRATIONS_DIR).glob("*"))
            if p.is_file()]
    current = dict(zip(here, git(root, "hash-object", "--", *here).split())) if here else {}
    by_blob = {blob: path for path, blob in current.items()}
    problems = []
    for path, blob in sorted(frozen.items()):
        name = pathlib.PurePosixPath(path).name
        if current.get(path) == blob:
            continue
        if path in current:
            what = "changed"
        elif blob in by_blob:
            what = f"was renamed to {pathlib.PurePosixPath(by_blob[blob]).name}"
        else:
            what = "was deleted"
        problems.append(
            f"{name} {what} after it was merged/shipped (in {ref}). Migrations are frozen once on main — "
            f"revert it and add a new V<max+1> migration instead."
        )
    return problems


def freeze_check(root, releases, explicit):
    """(refs checked, problems). Release tags get the working tree's max version as a ceiling."""
    refs, problems = [], []
    if releases:
        ceiling = tree_max_version(root)
        for tag in release_tags(root):
            refs.append(tag)
            problems += frozen_problems(root, tag, ceiling)
    for ref in explicit:
        refs.append(ref)
        problems += frozen_problems(root, ref)
    return refs, problems


def _freeze_self_test():
    env = dict(os.environ, GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1",
               GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@t", GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@t")
    with tempfile.TemporaryDirectory() as tmp:
        root = pathlib.Path(tmp)
        mig = root / MIGRATIONS_DIR
        mig.mkdir(parents=True)

        def sh(*args):
            subprocess.run(["git", *args], cwd=root, env=env, check=True, capture_output=True)

        def check(ref="main"):
            return frozen_problems(root, ref)

        sh("init", "-q", "-b", "main")
        (mig / "V1__init.sql").write_text("CREATE TABLE a (id INTEGER);\n")
        (mig / "V2__b.sql").write_text("CREATE TABLE b (id INTEGER);\n")
        sh("add", "-A")
        sh("commit", "-q", "-m", "base")
        sh("tag", "v0.1.0")
        assert check() == [] and check("v0.1.0") == [], "unchanged tree must pass"
        assert release_tags(root) == ["v0.1.0"]

        (mig / "V3__c.sql").write_text("CREATE TABLE c (id INTEGER);\n")
        assert check() == [], "a new migration appended above the max must pass"

        (mig / "V2__b.sql").write_text("CREATE TABLE b (id INTEGER); -- tweak\n")
        found = check()
        assert len(found) == 1 and "V2__b.sql changed" in found[0] and "(in main)" in found[0], found
        sh("checkout", "-q", "--", ".")

        (mig / "V2__b.sql").rename(mig / "V2__renamed.sql")
        found = check()
        assert len(found) == 1 and "renamed to V2__renamed.sql" in found[0], found
        (mig / "V2__renamed.sql").rename(mig / "V2__b.sql")

        (mig / "V1__init.sql").unlink()
        found = check()
        assert len(found) == 1 and "V1__init.sql was deleted" in found[0], found
        sh("checkout", "-q", "--", ".")
        assert check() == []

        # Hotfix from an older release line: v0.1.0 (V1-V2) → v0.1.0.1, while v0.2.0 shipped V3.
        sh("add", "-A")
        sh("commit", "-q", "-m", "V3")
        sh("tag", "v0.2.0")
        sh("checkout", "-q", "-b", "hotfix", "v0.1.0")
        assert not (mig / "V3__c.sql").exists()
        refs, found = freeze_check(root, True, [])
        assert refs == ["v0.1.0", "v0.2.0"] and found == [], f"older-line hotfix must pass: {found}"
        assert frozen_problems(root, "v0.2.0"), "without the ceiling V3 reads as deleted"
        (mig / "V2__b.sql").write_text("CREATE TABLE b (id INTEGER); -- hotfix tweak\n")
        _, found = freeze_check(root, True, [])
        assert len(found) == 2 and all("V2__b.sql changed" in p for p in found), \
            f"editing a shared migration on the hotfix line must fail against every release: {found}"
        sh("checkout", "-q", "--", ".")
        sh("checkout", "-q", "main")
        # Deleting main's newest migration: the release ceiling drops with it, the explicit ref does not.
        (mig / "V3__c.sql").unlink()
        _, found = freeze_check(root, True, ["main"])
        assert len(found) == 1 and "V3__c.sql was deleted" in found[0] and "(in main)" in found[0], found
        sh("checkout", "-q", "--", ".")
        assert freeze_check(root, True, ["main"])[1] == []

        for bad in ("no-such-ref", "HEAD~5"):
            try:
                check(bad)
            except GitError:
                pass
            else:
                raise AssertionError(f"unresolvable ref {bad} must be a hard error")
        sh("tag", "-d", "v0.1.0", "v0.2.0")
        try:
            release_tags(root)
        except GitError:
            pass
        else:
            raise AssertionError("zero release tags must be a hard error")


def self_test():
    assert name_problems(["V1__a.sql", "V2__b.sql", "V10__c.sql"]) == []
    dup = name_problems(["V64__a.sql", "V64__b.sql", "V65__c.sql"])
    assert len(dup) == 1 and "V64" in dup[0] and "V66" in dup[0], dup
    assert len(name_problems(["V064__a.sql", "V64__b.sql"])) == 1, "V064 and V64 are the same version"
    for bad in ("v1__a.sql", "V1_a.sql", "V1__a.SQL", "V1.5__a.sql", "V1__a-b.sql", "README.md", "U1__a.sql"):
        assert name_problems([bad]), f"{bad} must be rejected"
    _freeze_self_test()
    ok_doc = "DELETE FROM refinery_schema_history WHERE version = 3;"
    assert rollback_problems([3], {3: ok_doc}) == []
    assert rollback_problems([3], {3: "no history step (pure docs)"}) == []
    missing = rollback_problems([3, 4], {3: ok_doc})
    assert len(missing) == 1 and "V4-rollback.md" in missing[0], missing
    off_by = rollback_problems([46], {46: "DELETE FROM refinery_schema_history\n  WHERE version=42;"})
    assert len(off_by) == 1 and "version 42" in off_by[0] and "version 46" in off_by[0], off_by
    clean = [
        ("V1__init.sql", "CREATE TABLE people (id INTEGER PRIMARY KEY);"),
        ("V3__add.sql", "ALTER TABLE people ADD COLUMN nickname TEXT; -- DROP COLUMN in a comment is fine"),
    ]
    dirty = [
        ("V4__bad.sql", "ALTER TABLE people DROP COLUMN confidence;"),
    ]
    assert scan(clean) == [], "additive DDL and commented keywords must pass"
    found = scan(dirty)
    assert len(found) == 1 and found[0][1] == "DROP COLUMN", "must catch a real DROP COLUMN"
    # The bypasses #913 found: SQLite's optional COLUMN keyword, and a '--' inside a literal.
    for sql, label in [
        ("ALTER TABLE people DROP confidence;", "DROP COLUMN"),
        ('ALTER TABLE "people" DROP "confidence";', "DROP COLUMN"),
        ("alter table people rename confidence to certainty;", "RENAME COLUMN"),
        ("ALTER TABLE people RENAME COLUMN confidence TO certainty;", "RENAME COLUMN"),
        ("INSERT INTO t VALUES ('--'); DROP TABLE people;", "DROP TABLE"),
        ("INSERT INTO t VALUES ('it''s -- fine'); DROP TABLE people;", "DROP TABLE"),
        ("/* DROP TABLE people */ CREATE TABLE x (id INTEGER); DROP TABLE people;", "DROP TABLE"),
        # Review of #914: schema-qualified and spaced names, and an apostrophe in a quoted name.
        ('ALTER TABLE "main"."people" DROP confidence;', "DROP COLUMN"),
        ('ALTER TABLE "my people" DROP confidence;', "DROP COLUMN"),
        ("ALTER TABLE [my people] RENAME confidence TO certainty;", "RENAME COLUMN"),
        ("CREATE TABLE `it's` (id INTEGER); DROP TABLE people;", "DROP TABLE"),
        ("CREATE TABLE [it's] (id INTEGER); DROP TABLE people;", "DROP TABLE"),
    ]:
        labels = [hit[1] for hit in scan([("V5__bad.sql", sql)])]
        assert labels == [label], f"must catch {label} once in: {sql} (got {labels})"
    for sql in [
        "/* DROP TABLE people */ CREATE TABLE x (id INTEGER);",
        "INSERT INTO notes VALUES ('ALTER TABLE t DROP c -- in a note');",
        "ALTER TABLE people ADD COLUMN dropped_at INTEGER;",
    ]:
        assert scan([("V6__ok.sql", sql)]) == [], f"must pass: {sql}"
    rebuild = "-- non-additive-ok: CHECK constraint rebuild\n-- ロールバック手順: ...\nDROP TABLE t;"
    assert scan([("V9__rebuild.sql", rebuild)]) == [], "a sanctioned rebuild with a rollback plan must pass"
    no_rollback = "-- non-additive-ok: CHECK constraint rebuild\nDROP TABLE t;"
    assert len(scan([("V9__bad.sql", no_rollback)])) == 1, "the exception mark alone (no rollback plan) must NOT pass"
    for path in APPLIED_REBUILDS:
        text = pathlib.Path(path).read_text(encoding="utf-8")
        assert scan([(path, text)]) == [], "reviewed immutable rebuild must pass"
        assert scan([(path, text + "\nDROP TABLE people;")]), "changed rebuild must fail"
        assert scan([("V99__unreviewed.sql", text)]), "approval must not transfer to another file"
        with patch.object(pathlib.Path, "is_file", return_value=False):
            assert scan([(path, text)]), "missing decision must fail"
        with patch.object(pathlib.Path, "read_text", return_value="-- non-additive-ok: rebuild"):
            assert scan([(path, text)]), "decision without rollback must fail"
    print("self-test OK: additive DDL, names/duplicates, rollback docs, and the frozen-migration check "
          "(append/edit/rename/delete/bad ref/older-line hotfix).")


def migration_files():
    root = pathlib.Path(MIGRATIONS_DIR)
    if not root.exists():
        return []
    return [(p.as_posix(), p.read_text(encoding="utf-8", errors="replace")) for p in sorted(root.glob("*.sql"))]


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--self-test", action="store_true")
    ap.add_argument("--immutable-against", nargs="+", action="extend", default=[], metavar="REF",
                    help="every migration at REF must be unchanged in the working tree")
    ap.add_argument("--immutable-against-releases", action="store_true",
                    help="same, against every local v* tag (zero tags is an error), ignoring tag files "
                         "above the working tree's highest version")
    args = ap.parse_args()
    if args.self_test:
        self_test()
        return 0
    root = pathlib.Path(MIGRATIONS_DIR)
    # refinery silently skips a non-matching file, so a typo'd name would never run; dotfiles
    # (.DS_Store) are ignored.
    names = [p.name for p in root.iterdir() if p.is_file() and not p.name.startswith(".")] if root.exists() else []
    problems = name_problems(names)
    versions = [v for n in names if (v := version_of(n)) is not None]
    problems += rollback_problems(versions, rollback_docs(versions))
    try:
        refs, frozen = freeze_check(".", args.immutable_against_releases, args.immutable_against)
        problems += frozen
    except GitError as e:
        print(f"migration freeze check error: {e}")
        return 2
    if problems:
        print("migration history violation (refinery would refuse to open the memory DB, #800 / #780):")
        for p in problems:
            print(f"  - {p}")
        print("\nRules: docs/migrations/README.md")
        return 1
    if refs:
        print(f"migrations frozen OK against {len(refs)} ref(s): {' '.join(refs)}")
    files = migration_files()
    hits = scan(files)
    if hits:
        print("backward-compat violation: a migration contains a non-additive statement.")
        for path, label, snippet in hits:
            print(f"  - {path}: {label}  …{snippet}…")
        print("\nMigrations must be additive (memory lives for years). If a change is unavoidable, it needs a decision record + rollback plan (CLAUDE.md).")
        return 1
    print(f"migrations OK: {len(files)} file(s), additive or explicitly reviewed rebuilds.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
