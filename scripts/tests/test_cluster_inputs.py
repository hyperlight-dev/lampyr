#!/usr/bin/env python3
"""Run the cluster inputs recipes' shell logic on fixtures."""

import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
UPSTREAM_JUSTFILE = ROOT / "upstream/Justfile"
USE_CLUSTER_INPUTS = ROOT / "scripts/use-cluster-inputs.sh"
DIGEST = "sha256:" + "ab" * 32


def recipe(name):
    text = UPSTREAM_JUSTFILE.read_text(encoding="utf-8")
    match = re.search(rf"^{re.escape(name)}\b.*:.*\n((?:    .*\n|\n)+)", text, re.M)
    if match is None:
        raise AssertionError(f"upstream/Justfile has no {name} recipe")
    return match.group(1)


def bash(script, *args, cwd=None, **env):
    return subprocess.run(
        ["bash", "--noprofile", "--norc", "-euo", "pipefail", "-c", script, "bash", *args],
        capture_output=True, text=True, env={**os.environ, **env}, cwd=cwd,
    )


class ClusterInputsTests(unittest.TestCase):
    def test_digest_takes_only_one_sha256_manifest(self):
        body = recipe("cluster-inputs")
        program = re.search(r"jq -er '([^']*select\(length == 1\)[^']*)'", body)
        self.assertIsNotNone(program, "cluster-inputs no longer reads the index digest with jq")
        manifest = {"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": DIGEST}
        for name, index, expected in (
            ("one", {"manifests": [manifest]}, DIGEST),
            ("two", {"manifests": [manifest, manifest]}, None),
            ("none", {"manifests": []}, None),
            ("not sha256", {"manifests": [{**manifest, "digest": "sha512:" + "ab" * 64}]}, None),
            ("short", {"manifests": [{**manifest, "digest": "sha256:abc"}]}, None),
        ):
            with self.subTest(index=name), tempfile.TemporaryDirectory() as work:
                path = Path(work) / "index.json"
                path.write_text(json.dumps(index))
                result = subprocess.run(
                    ["jq", "-er", program.group(1), str(path)], capture_output=True, text=True,
                )
                if expected is None:
                    self.assertNotEqual(result.returncode, 0, result.stdout)
                else:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout.strip(), expected)

    def test_output_guard_refuses_what_is_not_earlier_inputs(self):
        body = recipe("cluster-inputs")
        # From the recipe's first use of its argument, so that its path
        # handling runs before the guard as it does in the recipe.
        guard = re.search(r"^ +([^\n]*\"\$1\".*?^ +fi;) \\$", body, re.M | re.S)
        self.assertIsNotNone(guard, "cluster-inputs no longer guards its output")
        self.assertIn("realpath", guard.group(1), "cluster-inputs no longer resolves its output")
        script = re.sub(r" \\$", "", guard.group(1), flags=re.M)
        with tempfile.TemporaryDirectory() as work:
            work = Path(work)
            (work / "earlier").mkdir()
            (work / "earlier/inputs.json").write_text("{}")
            (work / "empty").mkdir()
            (work / "build").mkdir()
            (work / "build/ly").write_text("")
            (work / "file").write_text("")
            (work / "link").symlink_to(work / "earlier")
            (work / "relative-link").symlink_to("earlier")
            (work / "parent-link").symlink_to(work)
            # A directory ls can't list would otherwise pass for an empty one.
            (work / "unreadable").mkdir()
            (work / "unreadable/ly").write_text("")
            (work / "unreadable").chmod(0o300)
            cases = [
                ("missing", False), ("earlier", False), ("empty", False),
                ("earlier/", False), ("parent-link/earlier", False),
                ("build", True), ("file", True), ("link", True), ("link/", True),
                ("relative-link", True), ("link/.", True), ("earlier/..", True),
            ]
            if os.geteuid() != 0:
                cases.append(("unreadable", True))
            try:
                for name, refused in cases:
                    for output, cwd in ((os.path.join(work, name), None), (name, work)):
                        with self.subTest(output=output):
                            result = bash(script, output, cwd=cwd)
                            if refused:
                                self.assertNotEqual(result.returncode, 0)
                                self.assertIn("refusing to replace", result.stderr)
                            else:
                                self.assertEqual(result.returncode, 0, result.stderr)
            finally:
                (work / "unreadable").chmod(0o700)
            self.assertTrue((work / "build/ly").exists())
            self.assertTrue((work / "earlier/inputs.json").exists())

    def test_use_cluster_inputs_takes_only_a_canonical_port(self):
        # Each is refused before the script runs Docker.
        for port in ("0", "05002", "65536", "99999", "", "5002x", "-1"):
            with self.subTest(port=port):
                result = subprocess.run(
                    ["bash", str(USE_CLUSTER_INPUTS), port],
                    capture_output=True, text=True, env={**os.environ, "PATH": "/usr/bin:/bin"},
                )
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn("1 to 65535", result.stderr)
        for port in ("1", "5002", "65535"):
            with self.subTest(port=port):
                env = {**os.environ, "LAMPYR_CLUSTER_INPUTS": ""}
                result = subprocess.run(
                    ["bash", str(USE_CLUSTER_INPUTS), port], capture_output=True, text=True, env=env,
                )
                self.assertIn("set LAMPYR_CLUSTER_INPUTS", result.stderr)

    def test_use_cluster_inputs_rewrites_only_the_controller_image_lines(self):
        text = USE_CLUSTER_INPUTS.read_text(encoding="utf-8")
        rewrite = re.search(
            r"^(grep -Eq \"\^ \+\(- \)\?image: .*?^mv -- \"\$manifest\.tmp\" \"\$manifest\")$",
            text, re.M | re.S,
        )
        self.assertIsNotNone(rewrite, "use-cluster-inputs.sh no longer rewrites the manifest")
        controller = "lampyr.local/fiberd-k8s-controller:0123456789ab"
        pulled = f"localhost:5002/fiberd-k8s-controller@{DIGEST}"
        source = (
            "spec:\n"
            "  containers:\n"
            f"    - image: {controller}\n"
            "  initContainers:\n"
            "    - name: setup\n"
            f"      image: {controller}\n"
            f"      image: {controller}-other\n"
            f"      image: lampyrXlocal/fiberd-k8s-controller:0123456789ab\n"
            f"# image: {controller}\n"
        )
        expected = (
            "spec:\n"
            "  containers:\n"
            f"    - image: {pulled}\n"
            "  initContainers:\n"
            "    - name: setup\n"
            f"      image: {pulled}\n"
            f"      image: {controller}-other\n"
            f"      image: lampyrXlocal/fiberd-k8s-controller:0123456789ab\n"
            f"# image: {controller}\n"
        )
        for name, manifest, rewritten in (
            ("runs it", source, expected),
            ("does not run it", "spec:\n  containers:\n    - image: ghcr.io/team/other:v1\n", None),
        ):
            with self.subTest(manifest=name), tempfile.TemporaryDirectory() as work:
                work = Path(work)
                (work / "fiberd-k8s-controller.yaml").write_text(manifest)
                out = work / "out.yaml"
                result = bash(rewrite.group(1), controller=controller, pulled=pulled,
                              cluster_inputs=str(work), manifest=str(out))
                if rewritten is None:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("does not run", result.stderr)
                    self.assertFalse(out.exists())
                else:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(out.read_text(), rewritten)


if __name__ == "__main__":
    unittest.main()
