#!/usr/bin/env python3
"""Offline recovery regressions: no Docker daemon, registry, credentials or publication."""

import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock
import urllib.error
import urllib.request

import yaml

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("recovery", ROOT / "scripts/recover-v010-registry.py")
recovery = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(recovery)
SOURCE = "f4b7c3794125ae8169ff4ab65c2f378267f5ca5c"
DIGEST = "sha256:" + "a" * 64
OTHER = "sha256:" + "b" * 64


class Response(io.BytesIO):
    def __init__(self, body, headers=None):
        super().__init__(body)
        self.headers = headers or {}


def http_error(code, body):
    return urllib.error.HTTPError("https://ghcr.io/test", code, "fixture", {}, io.BytesIO(body))


class RegistryTest(unittest.TestCase):
    def registry(self, response=None, error=None):
        registry = object.__new__(recovery.Registry)
        registry.token = "synthetic-test-token"
        registry.open = mock.Mock(side_effect=error, return_value=response)
        return registry

    def test_authentication_failures_are_closed_and_redacted(self):
        for response, error in ((Response(b'{}'), None), (None, http_error(401, b'synthetic secret body'))):
            with self.subTest(error=error), mock.patch.dict(os.environ, {"GITHUB_ACTOR": "fixture", "GITHUB_TOKEN": "synthetic-secret"}), mock.patch.object(recovery.urllib.request, "build_opener") as opener:
                opener.return_value.open.side_effect = error
                opener.return_value.open.return_value = response
                with self.assertRaises(RuntimeError) as caught:
                    recovery.Registry()
                self.assertNotIn("synthetic-secret", str(caught.exception))
                self.assertNotIn("synthetic secret body", str(caught.exception))

    def test_only_explicit_registry_not_found_is_absence(self):
        cases = [
            (401, b'{"errors":[{"code":"MANIFEST_UNKNOWN"}]}'),
            (403, b'{"errors":[{"code":"DENIED"}]}'),
            (404, b'{"errors":[{"code":"MANIFEST_UNKNOWN"},{"code":"DENIED"}]}'),
            (404, b'{}'), (404, b'not json'), (500, b'{}'),
        ]
        for code, body in cases:
            with self.subTest(code=code, body=body):
                registry = self.registry(error=http_error(code, body))
                with self.assertRaises(RuntimeError):
                    registry.get("manifests", "latest", allow_missing=True)
        for code in ("MANIFEST_UNKNOWN", "NAME_UNKNOWN"):
            registry = self.registry(error=http_error(404, json.dumps({"errors": [{"code": code}]}).encode()))
            self.assertIsNone(registry.get("manifests", "latest", allow_missing=True))
        for kind, allow_missing in (("blobs", True), ("manifests", False)):
            with self.subTest(kind=kind), self.assertRaises(RuntimeError):
                self.registry(error=http_error(404, b'{"errors":[{"code":"MANIFEST_UNKNOWN"}]}')).get(kind, DIGEST, allow_missing)

    def test_authentication_uses_fixed_pull_scope(self):
        with mock.patch.dict(os.environ, {"GITHUB_ACTOR": "fixture", "GITHUB_TOKEN": "synthetic-secret"}), mock.patch.object(recovery.urllib.request, "build_opener") as opener:
            opener.return_value.open.return_value = Response(b'{"token":"synthetic-scoped-token"}')
            registry = recovery.Registry()
            self.assertEqual(registry.token, "synthetic-scoped-token")
            request = opener.return_value.open.call_args.args[0]
            self.assertTrue(request.full_url.startswith("https://ghcr.io/token?"))
            self.assertEqual(recovery.urllib.parse.parse_qs(recovery.urllib.parse.urlsplit(request.full_url).query),
                             {"service": ["ghcr.io"], "scope": ["repository:torutesu/shogunagent:pull"]})
            self.assertTrue(request.get_header("Authorization").startswith("Basic "))

    def test_hashes_and_reference_validation(self):
        body = b'{"fixture":true}'
        actual = "sha256:" + hashlib.sha256(body).hexdigest()
        registry = self.registry(Response(body, {"Docker-Content-Digest": actual}))
        self.assertEqual(registry.get("manifests", actual), ({"fixture": True}, actual))
        for reference, claimed in ((DIGEST, actual), ("latest", DIGEST)):
            with self.subTest(reference=reference), self.assertRaises(RuntimeError):
                self.registry(Response(body, {"Docker-Content-Digest": claimed})).get("manifests", reference)
        registry = self.registry()
        for kind, reference in (("other", "latest"), ("manifests", "../token"), ("manifests", "v0.2.0")):
            with self.assertRaises(RuntimeError):
                registry.get(kind, reference)
        registry.open.assert_not_called()

    def test_redirect_strips_auth_cross_host_and_refuses_http(self):
        handler = recovery.SafeRedirect()
        request = urllib.request.Request("https://ghcr.io/blob", headers={"Authorization": "Bearer synthetic"})
        cross = handler.redirect_request(request, None, 302, "", {}, "https://storage.example/blob")
        self.assertFalse(cross.has_header("Authorization"))
        same = handler.redirect_request(request, None, 302, "", {}, "https://ghcr.io/other")
        self.assertTrue(same.has_header("Authorization"))
        with self.assertRaises(RuntimeError):
            handler.redirect_request(request, None, 302, "", {}, "http://ghcr.io/blob")

    def valid_index(self):
        manifests = []
        values = {}
        for arch, digit in (("amd64", "c"), ("arm64", "d")):
            md = "sha256:" + digit * 64
            cd = "sha256:" + ("e" if arch == "amd64" else "f") * 64
            manifests.append({"platform": {"os": "linux", "architecture": arch}, "digest": md})
            values[("manifests", md)] = ({"config": {"digest": cd}}, md)
            values[("blobs", cd)] = ({"os": "linux", "architecture": arch, "config": {"Labels": {"org.opencontainers.image.revision": SOURCE}}}, cd)
        registry = mock.Mock()
        registry.get.side_effect = lambda kind, reference, **kw: values[(kind, reference)]
        return registry, ({"manifests": manifests}, DIGEST), values

    def test_exact_platforms_config_and_revision(self):
        registry, result, _ = self.valid_index()
        self.assertEqual(recovery.check_index(registry, result), DIGEST)
        for mutation in ("missing", "duplicate", "attestation", "wrong-os", "wrong-arch", "wrong-source"):
            with self.subTest(mutation=mutation):
                registry, result, values = self.valid_index()
                manifests = result[0]["manifests"]
                if mutation == "missing":
                    manifests.pop()
                elif mutation == "duplicate":
                    manifests[1] = manifests[0]
                elif mutation == "attestation":
                    manifests.append({"platform": {"os": "unknown", "architecture": "unknown"}, "digest": OTHER})
                else:
                    config = values[("blobs", "sha256:" + "e" * 64)][0]
                    if mutation == "wrong-os":
                        config["os"] = "windows"
                    elif mutation == "wrong-arch":
                        config["architecture"] = "arm64"
                    else:
                        config["config"]["Labels"]["org.opencontainers.image.revision"] = "wrong"
                with self.assertRaises(RuntimeError):
                    recovery.check_index(registry, result)

    def test_preflight_checks_both_and_refuses_any_existing_tag(self):
        for existing in ("v0.1.0", "latest", "both", None):
            with self.subTest(existing=existing):
                registry, valid, _ = self.valid_index()
                underlying = registry.get.side_effect
                def get(kind, reference, **kwargs):
                    if reference in ("v0.1.0", "latest"):
                        return valid if existing in (reference, "both") else None
                    return underlying(kind, reference, **kwargs)
                registry.get.side_effect = get
                output = io.StringIO()
                with contextlib.redirect_stdout(output):
                    if existing:
                        with self.assertRaisesRegex(RuntimeError, "no overwrite"):
                            recovery.preflight(registry)
                    else:
                        recovery.preflight(registry)
                queried = [call.args[1] for call in registry.get.call_args_list if call.args[1] in ("v0.1.0", "latest")]
                self.assertEqual(queried, ["v0.1.0", "latest"])
                if existing:
                    self.assertIn(DIGEST, output.getvalue())
                else:
                    self.assertIn("both v0.1.0 and latest are absent", output.getvalue())

    def test_verify_requires_same_index_for_both_tags(self):
        for mismatch in (False, True):
            registry, result, _ = self.valid_index()
            underlying = registry.get.side_effect
            def get(kind, reference, **kwargs):
                if reference in ("v0.1.0", "latest"):
                    return result[0], OTHER if mismatch and reference == "latest" else DIGEST
                return underlying(kind, reference, **kwargs)
            registry.get.side_effect = get
            with contextlib.redirect_stdout(io.StringIO()):
                if mismatch:
                    with self.assertRaisesRegex(RuntimeError, "different indexes"):
                        recovery.verify(registry)
                else:
                    recovery.verify(registry)


class WorkflowTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workflow = yaml.safe_load((ROOT / ".github/workflows/recover-v010-image.yml").read_text())
        cls.jobs = cls.workflow["jobs"]

    def step(self, job, name):
        return next(step for step in self.jobs[job]["steps"] if step.get("name") == name)

    def shell(self, script, directory, **env):
        return subprocess.run(["bash", "--noprofile", "--norc", "-euo", "pipefail", "-c", script],
                              cwd=directory, env={"PATH": str(directory) + os.pathsep + os.defpath,
                              "RELEASE_SOURCE": SOURCE, "IMAGE_NAME": "ghcr.io/torutesu/shogunagent", "RUNNER_TEMP": str(directory), **env},
                              capture_output=True, text=True)

    def fake_docker(self, directory):
        path = Path(directory) / "docker"
        path.write_text("#!/usr/bin/python3\nimport json, os, sys\n"
                        "with open('docker-calls', 'a') as f: f.write(json.dumps(sys.argv[1:])+'\\n')\n"
                        "if sys.argv[1:3] == ['image','inspect']:\n"
                        "    print({'{{.Architecture}}': os.environ.get('INSPECT_ARCH',''), '{{.Id}}': os.environ.get('INSPECT_ID','')}.get(sys.argv[4], os.environ.get('INSPECT_SOURCE','')))\n"
                        "if sys.argv[1] == 'run': sys.exit(int(os.environ.get('RUN_EXIT','0')))\n")
        path.chmod(0o700)

    def calls(self, directory):
        path = Path(directory) / "docker-calls"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def test_workflow_authority_and_matrix(self):
        triggers = self.workflow.get("on", self.workflow.get(True))
        self.assertEqual(set(triggers), {"workflow_dispatch"})
        self.assertEqual(triggers["workflow_dispatch"]["inputs"]["publish"]["type"], "boolean")
        self.assertIs(triggers["workflow_dispatch"]["inputs"]["publish"]["default"], False)
        self.assertEqual(self.workflow["permissions"], {"contents": "read", "packages": "write"})
        self.assertEqual(self.workflow["concurrency"], {"group": "recover-v010-container", "cancel-in-progress": False})
        self.assertEqual(self.workflow["env"]["RELEASE_SOURCE"], SOURCE)
        self.assertEqual(self.jobs["preflight"]["if"], "github.repository == 'Torutesu/Shogunagent' && github.ref == 'refs/heads/main'")
        self.assertEqual(self.jobs["native"]["needs"], "preflight")
        self.assertEqual(self.jobs["native"]["strategy"]["matrix"]["include"], [
            {"arch": "amd64", "runner": "ubuntu-latest"}, {"arch": "arm64", "runner": "ubuntu-24.04-arm"}])
        self.assertEqual(self.jobs["native"]["runs-on"], "${{ matrix.runner }}")
        self.assertEqual(self.jobs["publish-index"]["needs"], "native")
        self.assertEqual(self.jobs["publish-index"]["if"], "inputs.publish")
        steps = self.jobs["native"]["steps"]
        checkout = next(s for s in steps if s.get("uses", "").startswith("actions/checkout@"))
        self.assertEqual(checkout["with"]["ref"], SOURCE)
        build = next(s for s in steps if s.get("id") == "local-build")["with"]
        self.assertEqual(build["context"], ".")
        self.assertEqual(build["platforms"], "linux/${{ matrix.arch }}")
        self.assertIs(build["provenance"], False)
        self.assertIs(build["sbom"], False)
        self.assertEqual(build["outputs"], "type=docker,name=shogunagent-recovery:local")
        publish_step = next(s for s in steps if s.get("id") == "publish")
        self.assertEqual(publish_step["if"], "inputs.publish")
        publish = publish_step["with"]
        self.assertEqual(publish["outputs"], "type=image,name=ghcr.io/torutesu/shogunagent,push-by-digest=true,name-canonical=true,push=true")
        for key in ("context", "platforms", "provenance", "sbom", "labels"):
            self.assertEqual(build[key], publish[key])
        smoke = self.step("native", "smoke the native image without network access")
        compare = self.step("native", "compare pushed digest with the tested image before recording it")
        self.assertLess(steps.index(smoke), steps.index(publish_step))
        self.assertGreater(steps.index(compare), steps.index(publish_step))
        self.assertEqual(compare["if"], "inputs.publish")
        index_steps = self.jobs["publish-index"]["steps"]
        preflight = self.step("publish-index", "check tag and registry again immediately before publication")
        create = self.step("publish-index", "publish the two verified native images")
        verify = self.step("publish-index", "verify both tags, platforms and source revision")
        self.assertEqual(index_steps.index(create), index_steps.index(preflight) + 1)
        self.assertEqual(index_steps.index(verify), index_steps.index(create) + 1)
        self.assertIn("recover-v010-registry.py preflight", preflight["run"])
        self.assertIn("recover-v010-registry.py verify", verify["run"])
        for step in steps:
            if step.get("uses", "").startswith(("docker/login-action@", "actions/upload-artifact@")):
                self.assertEqual(step["if"], "inputs.publish")
        self.assertFalse(any("setup-qemu" in s.get("uses", "") for s in steps))

    def test_source_and_tag_mismatch_stop(self):
        script = self.step("native", "verify exact build source and release tag")["run"]
        for head, tag in ((SOURCE, SOURCE), ("wrong", SOURCE), (SOURCE, "wrong")):
            with self.subTest(head=head, tag=tag), tempfile.TemporaryDirectory() as tmp:
                git = Path(tmp) / "git"
                git.write_text("#!/usr/bin/python3\nimport os, sys\n"
                               "if sys.argv[1] == 'rev-parse': print(os.environ['HEAD_SHA'] if sys.argv[2] == 'HEAD' else os.environ['TAG_SHA'])\n")
                git.chmod(0o700)
                result = self.shell(script, tmp, HEAD_SHA=head, TAG_SHA=tag)
                self.assertEqual(result.returncode == 0, head == tag == SOURCE)

    def test_local_native_smoke_checks_platform_source_and_network(self):
        script = self.step("native", "smoke the native image without network access")["run"]
        for change in ({}, {"INSPECT_ARCH": "amd64"}, {"INSPECT_SOURCE": "wrong"}, {"RUN_EXIT": "1"}):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as tmp:
                self.fake_docker(tmp)
                env = dict(IMAGE_ARCH="arm64", INSPECT_ARCH="arm64", INSPECT_SOURCE=SOURCE, INSPECT_ID=OTHER)
                env.update(change)
                result = self.shell(script, tmp, **env)
                self.assertEqual(result.returncode == 0, not change, result.stderr)
                receipt = Path(tmp) / "tested-config.digest"
                calls = self.calls(tmp)
                self.assertFalse(any(call[0] == "pull" for call in calls))
                if change:
                    self.assertFalse(receipt.exists())
                else:
                    self.assertEqual(receipt.read_text(), OTHER + "\n")
                    self.assertEqual(calls[-2], ["run", "--rm", "--network", "none", "--read-only", "--entrypoint", "/usr/local/bin/shogun", "shogunagent-recovery:local", "--help"])

    def test_published_image_requires_tested_config_and_successful_smoke(self):
        script = self.step("native", "compare pushed digest with the tested image before recording it")["run"]
        for change in ({}, {"IMAGE_DIGEST": "bad"}, {"INSPECT_ID": DIGEST}, {"RUN_EXIT": "1"}):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as tmp:
                self.fake_docker(tmp)
                (Path(tmp) / "tested-config.digest").write_text(OTHER + "\n")
                env = dict(IMAGE_DIGEST=DIGEST, IMAGE_ARCH="arm64", INSPECT_ID=OTHER)
                env.update(change)
                result = self.shell(script, tmp, **env)
                self.assertEqual(result.returncode == 0, not change, result.stderr)
                receipt = Path(tmp) / "digests/arm64.digest"
                calls = self.calls(tmp)
                if change:
                    self.assertFalse(receipt.exists())
                    if "IMAGE_DIGEST" in change:
                        self.assertEqual(calls, [])
                else:
                    image = "ghcr.io/torutesu/shogunagent@" + DIGEST
                    self.assertEqual(calls[0], ["pull", image])
                    self.assertEqual(calls[-1], ["run", "--rm", "--network", "none", "--read-only", "--entrypoint", "/usr/local/bin/shogun", image, "--help"])
                    self.assertEqual(receipt.read_text(), DIGEST + "\n")

    def test_index_validates_both_before_docker(self):
        script = self.step("publish-index", "publish the two verified native images")["run"]
        for digests in ({}, {"amd64": DIGEST}, {"amd64": DIGEST, "arm64": "bad"}, {"amd64": DIGEST, "arm64": OTHER}):
            with self.subTest(digests=digests), tempfile.TemporaryDirectory() as tmp:
                self.fake_docker(tmp)
                (Path(tmp) / "digests").mkdir()
                for arch, digest in digests.items():
                    (Path(tmp) / "digests" / f"{arch}.digest").write_text(digest + "\n")
                result = self.shell(script, tmp)
                if digests == {"amd64": DIGEST, "arm64": OTHER}:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    name = "ghcr.io/torutesu/shogunagent"
                    self.assertEqual(self.calls(tmp), [["buildx", "imagetools", "create", "--tag", name+":v0.1.0", "--tag", name+":latest", name+"@"+DIGEST, name+"@"+OTHER]])
                else:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(self.calls(tmp), [])



if __name__ == "__main__":
    unittest.main()
