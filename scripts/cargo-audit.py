#!/usr/bin/env python3
"""Audit normally, except the verified re-export-only webpki-roots wrapper.

RUSTSEC-2026-0381 flags 0.26.11 even when its only dependency is patched 1.x.
Never put this exception in audit.toml: every invocation revalidates Cargo.lock.
Advisory: https://rustsec.org/advisories/RUSTSEC-2026-0381.html
"""
from pathlib import Path
import re
import subprocess
import sys

try:
    import tomllib
except ImportError:
    try:
        # CI on older Python supplies the hash-pinned official wheel in RUNNER_TEMP.
        import tomli as tomllib
    except ImportError:
        tomllib = None

REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"
WRAPPER_CHECKSUM = "521bc38abb08001b01866da9f51eb7c5d647a19260e00054a8c7fd5f9e57f7a9"
ADVISORY = "RUSTSEC-2026-0381"
ROOT = Path(__file__).resolve().parent.parent


def wrapper_exception(lock: dict) -> bool:
    """Unknown, ambiguous, non-registry or unpatched roots always deny exception."""
    packages = lock.get("package")
    if not isinstance(packages, list) or any(not isinstance(p, dict) for p in packages):
        return False
    roots = [p for p in packages if p.get("name") == "webpki-roots"]
    wrappers = [p for p in roots if p.get("version") == "0.26.11"]
    if len(wrappers) != 1:
        return False
    wrapper = wrappers[0]
    if wrapper.get("source") != REGISTRY or wrapper.get("checksum") != WRAPPER_CHECKSUM:
        return False
    patched = [p for p in roots if p is not wrapper]
    if not patched:
        return False
    versions = set()
    for package in patched:
        version = package.get("version")
        checksum = package.get("checksum")
        if not isinstance(version, str) or not re.fullmatch(r"1\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version):
            return False
        if tuple(map(int, version.split("."))) < (1, 0, 10):
            return False
        if package.get("source") != REGISTRY or not isinstance(checksum, str) or not re.fullmatch(r"[0-9a-f]{64}", checksum):
            return False
        if version in versions:
            return False
        versions.add(version)
    dependencies = wrapper.get("dependencies")
    return (isinstance(dependencies, list) and len(dependencies) == 1
            and isinstance(dependencies[0], str)
            and dependencies[0] in {f"webpki-roots {version}" for version in versions})


def main() -> int:
    if len(sys.argv) != 1:
        print("cargo-audit.py takes no arguments; audit scope is the repository Cargo.lock", file=sys.stderr)
        return 2
    permitted = False
    reason = f"neither stdlib tomllib nor official tomli available in Python {sys.version.split()[0]} ({sys.executable}); lock verification unavailable"
    if tomllib is not None:
        try:
            with (ROOT / "Cargo.lock").open("rb") as source:
                permitted = wrapper_exception(tomllib.load(source))
            reason = "Cargo.lock does not satisfy the exact verified wrapper and patched-roots conditions"
        except (OSError, ValueError, TypeError) as error:
            reason = f"Cargo.lock could not be verified ({type(error).__name__})"
    command = ["cargo", "audit"]
    if permitted:
        print(f"{ADVISORY}: invocation-only exception for official webpki-roots 0.26.11 "
              "re-export wrapper; its sole roots dependency and all other roots versions are patched stable 1.x. "
              "All other cargo audit failures remain fatal.", flush=True)
        command += ["--ignore", ADVISORY]
    else:
        print(f"cargo audit: running without wrapper exception: {reason}", flush=True)
    return subprocess.call(command, cwd=ROOT)


if __name__ == "__main__":
    result = main()
    sys.exit(result if result >= 0 else 128 - result)
