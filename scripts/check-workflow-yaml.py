#!/usr/bin/env python3
"""Every GitHub Actions workflow parses as YAML and has an `on` trigger and `jobs` (E40).

Also pins the publish workflows' guard rails: they run only on `workflow_dispatch`, and every job
that holds `id-token: write` runs in the `release` environment. And the deploy workflow's
(`deploy.yml`, E48): `workflow_dispatch` only, a read-only token, a `confirm` input that defaults to
a dry run, every job that reads a secret runs in the environment the run chose (so that
environment's reviewers must approve it), and every `wrangler deploy` without `--dry-run` sits behind
the explicit deploy mode. `--self-test` checks the deploy rules against fixed samples. Needs PyYAML.
"""

from __future__ import annotations

import pathlib
import sys

import yaml

ROOT = pathlib.Path(__file__).resolve().parent.parent
WORKFLOWS = ROOT / ".github" / "workflows"

DEPLOY_WORKFLOWS = {"deploy.yml"}
CHOSEN_ENVIRONMENT = "${{ inputs.environment }}"


def triggers(doc: dict) -> dict:
    # PyYAML reads the bare key `on` as the boolean True.
    on = doc.get("on", doc.get(True))
    return on if isinstance(on, dict) else {name: None for name in (on if isinstance(on, list) else [on])}


def deploy_problems(name: str, doc: dict) -> list[str]:
    """The guard rails of a Worker deploy workflow."""
    out: list[str] = []
    on = triggers(doc)
    if set(on) != {"workflow_dispatch"}:
        out.append(f"{name}: a deploy workflow runs only on workflow_dispatch, found {sorted(map(str, on))}")
    dispatch = on.get("workflow_dispatch") if isinstance(on.get("workflow_dispatch"), dict) else {}
    inputs = (dispatch or {}).get("inputs") or {}
    for required in ("component", "environment", "confirm"):
        if required not in inputs:
            out.append(f"{name}: needs the `{required}` input")
    env_input = inputs.get("environment") or {}
    if sorted(env_input.get("options") or []) != ["production", "staging"]:
        out.append(f"{name}: `environment` must be a choice of staging and production")
    confirm = inputs.get("confirm") or {}
    if confirm.get("default", "") not in ("", None):
        out.append(f"{name}: `confirm` must default to empty (a dry run)")
    top = doc.get("permissions") or {}
    if not isinstance(top, dict) or any(v != "read" for v in top.values()):
        out.append(f"{name}: workflow permissions must be read-only")
    for job_name, job in (doc.get("jobs") or {}).items():
        text = yaml.safe_dump(job)
        env = job.get("environment")
        env_name = env.get("name") if isinstance(env, dict) else env
        if "secrets." in text and env_name != CHOSEN_ENVIRONMENT:
            out.append(f"{name}: job {job_name} reads a secret outside `environment: {CHOSEN_ENVIRONMENT}`")
        perms = job.get("permissions") or {}
        if isinstance(perms, dict) and any(v == "write" for v in perms.values()):
            out.append(f"{name}: job {job_name} asks for a write permission")
        for step in job.get("steps") or []:
            run = str(step.get("run") or "")
            if "wrangler deploy" in run and "--dry-run" not in run:
                gate = str(step.get("if") or "")
                if "== 'deploy'" not in gate or env_name != CHOSEN_ENVIRONMENT:
                    out.append(f"{name}: job {job_name} runs `wrangler deploy` without --dry-run outside the explicit deploy mode")
    return out


SAMPLE_OK = """
on:
  workflow_dispatch:
    inputs:
      component: {type: choice, options: [relay, router]}
      environment: {type: choice, options: [staging, production]}
      confirm: {type: string, default: ""}
permissions: {contents: read}
jobs:
  deploy:
    environment: ${{ inputs.environment }}
    steps:
      - run: wrangler deploy --dry-run
        if: steps.t.outputs.mode != 'deploy'
      - run: wrangler deploy
        if: steps.t.outputs.mode == 'deploy'
        env: {CLOUDFLARE_API_TOKEN: "${{ secrets.CLOUDFLARE_API_TOKEN }}"}
"""


def self_test() -> int:
    ok = yaml.safe_load(SAMPLE_OK)
    assert deploy_problems("deploy.yml", ok) == [], deploy_problems("deploy.yml", ok)
    cases = {
        "push trigger": lambda d: d[True].update({"push": {"branches": ["main"]}}),
        "confirm default": lambda d: d[True]["workflow_dispatch"]["inputs"]["confirm"].update({"default": "deploy-relay-production"}),
        "secret outside the environment": lambda d: d["jobs"]["deploy"].pop("environment"),
        "ungated deploy": lambda d: d["jobs"]["deploy"]["steps"][1].pop("if"),
        "write permission": lambda d: d.update({"permissions": {"contents": "write"}}),
        "no confirm input": lambda d: d[True]["workflow_dispatch"]["inputs"].pop("confirm"),
    }
    for label, mutate in cases.items():
        doc = yaml.safe_load(SAMPLE_OK)
        mutate(doc)
        assert deploy_problems("deploy.yml", doc), f"self-test: {label} was not caught"
    print(f"self-test ok  {len(cases)} deploy-workflow violations caught")
    return 0


def main() -> int:
    if "--self-test" in sys.argv[1:]:
        return self_test()
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
        if path.name in DEPLOY_WORKFLOWS:
            problems.extend(deploy_problems(path.name, doc))
    missing = sorted(name for name in DEPLOY_WORKFLOWS if not (WORKFLOWS / name).exists())
    for name in missing:
        problems.append(f"{name}: the manual deploy workflow is missing")
    for line in problems:
        print(f"FAIL {line}", file=sys.stderr)
    if not problems:
        print(f"ok  {len(files)} workflows parse")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
