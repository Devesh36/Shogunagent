#!/usr/bin/env python3
"""Exercise release image-name generation without Docker, credentials or publishing."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest

import yaml


ROOT = Path(__file__).resolve().parent.parent


class ReleaseImageTest(unittest.TestCase):
    def test_owner_case_and_build_tags(self):
        workflow = yaml.safe_load((ROOT / ".github/workflows/release.yml").read_text())
        steps = workflow["jobs"]["image"]["steps"]
        metadata = next((step for step in steps if step.get("id") == "image-name"), None)
        self.assertIsNotNone(metadata, "release needs a normalized image-name step")
        build = next(step for step in steps if step.get("uses", "").startswith("docker/build-push-action@"))
        for owner, expected in (("Torutesu", "torutesu"), ("Devesh36", "devesh36"), ("lower-case", "lower-case")):
            with self.subTest(owner=owner), tempfile.TemporaryDirectory() as directory:
                output = Path(directory) / "output"
                subprocess.run(
                    ["bash", "--noprofile", "--norc", "-euo", "pipefail", "-c", metadata["run"]],
                    env={"PATH": os.defpath, "GITHUB_REPOSITORY_OWNER": owner, "GITHUB_OUTPUT": str(output)},
                    check=True,
                )
                self.assertEqual(output.read_text(), f"name=ghcr.io/{expected}/shogunagent\n")
                name = output.read_text().strip().removeprefix("name=")
                tags = build["with"]["tags"].replace("${{ steps.image-name.outputs.name }}", name)
                tags = tags.replace("${{ github.ref_name }}", "v0.1.0").splitlines()
                self.assertEqual(tags, [f"ghcr.io/{expected}/shogunagent:v0.1.0", f"ghcr.io/{expected}/shogunagent:latest"])
        # Normalizing names must not enable publishing on manual runs.
        self.assertEqual(build["with"]["push"], "${{ startsWith(github.ref, 'refs/tags/v') }}")


if __name__ == "__main__":
    unittest.main()
