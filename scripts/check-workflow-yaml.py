#!/usr/bin/env python3
"""Every GitHub Actions workflow parses as YAML and has an `on` trigger and `jobs` (E40).

Also pins the publish workflows' guard rails: they run only on `workflow_dispatch`, and every job
that holds `id-token: write` runs in the `release` environment. Needs PyYAML.
"""

from __future__ import annotations

import pathlib
import sys

import yaml

ROOT = pathlib.Path(__file__).resolve().parent.parent
WORKFLOWS = ROOT / ".github" / "workflows"


def triggers(doc: dict) -> dict:
    # PyYAML reads the bare key `on` as the boolean True.
    on = doc.get("on", doc.get(True))
    return on if isinstance(on, dict) else {name: None for name in (on if isinstance(on, list) else [on])}


def main() -> int:
    problems: list[str] = []
    files = sorted(WORKFLOWS.glob("*.y*ml"))
    for path in files:
        try:
            doc = yaml.safe_load(path.read_text())
        except yaml.YAMLError as e:
            problems.append(f"{path.name}: {e}")
            continue
        if not isinstance(doc, dict) or not isinstance(doc.get("jobs"), dict) or not triggers(doc):
            problems.append(f"{path.name}: needs `on` and `jobs`")
            continue
        if path.name.startswith("publish-"):
            if set(triggers(doc)) != {"workflow_dispatch"}:
                problems.append(f"{path.name}: a publish workflow runs only on workflow_dispatch, found {sorted(map(str, triggers(doc)))}")
            for name, job in doc["jobs"].items():
                perms = job.get("permissions") or {}
                if perms.get("id-token") == "write":
                    env = job.get("environment")
                    env_name = env.get("name") if isinstance(env, dict) else env
                    if env_name != "release":
                        problems.append(f"{path.name}: job {name} holds id-token: write outside the release environment")
            top = doc.get("permissions") or {}
            if top.get("id-token") == "write":
                problems.append(f"{path.name}: id-token: write at workflow level; grant it to the publish job only")
    for line in problems:
        print(f"FAIL {line}", file=sys.stderr)
    if not problems:
        print(f"ok  {len(files)} workflows parse")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
