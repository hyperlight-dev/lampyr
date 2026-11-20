#!/usr/bin/env python3
"""Run the cluster inputs recipes' shell logic on fixtures."""

import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
UPSTREAM_JUSTFILE = ROOT / "upstream/Justfile"
USE_CLUSTER_INPUTS = ROOT / "scripts/use-cluster-inputs.sh"
DIGEST = "sha256:" + "ab" * 32
CONTROLLER_PLATFORM_DIGEST = "sha256:" + "cd" * 32


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
    def test_release_asset_supports_gnu_and_bsd_checksum_tools(self):
        body = recipe("release-asset")
        payload = b"published release asset"
        checksum = hashlib.sha256(payload).hexdigest()
        for checksum_tool in ("sha256sum", "shasum", None):
            with self.subTest(checksum_tool=checksum_tool), tempfile.TemporaryDirectory() as directory:
                work = Path(directory)
                bin_dir = work / "bin"
                bin_dir.mkdir()
                for command in ("awk", "bash", "basename", "dirname", "install", "mkdir", "mktemp", "rm"):
                    target = shutil.which(command)
                    self.assertIsNotNone(target, f"{command} is required by this test")
                    (bin_dir / command).symlink_to(target)
                source = work / "source"
                source.write_bytes(payload)
                output = work / "output"
                curl = bin_dir / "curl"
                curl.write_text(
                    '#!/bin/bash\nset -euo pipefail\n'
                    'while (($#)); do\n'
                    '  if [[ $1 == --output ]]; then /bin/cp "$SOURCE" "$2"; exit 0; fi\n'
                    '  shift\n'
                    'done\n'
                    'exit 90\n'
                )
                gh = bin_dir / "gh"
                gh.write_text("#!/bin/bash\nexit 0\n")
                commands = [curl, gh]
                if checksum_tool is not None:
                    checksum_command = bin_dir / checksum_tool
                    checksum_command.write_text(
                        f"#!{sys.executable}\n"
                        "import hashlib, json, os, pathlib, sys\n"
                        'expected = ["-a", "256"] if pathlib.Path(sys.argv[0]).name == "shasum" else []\n'
                        "if sys.argv[1:-1] != expected:\n"
                        "    raise SystemExit(64)\n"
                        'pathlib.Path(os.environ["CHECKSUM_ARGS"]).write_text(json.dumps(sys.argv[1:]))\n'
                        'path = pathlib.Path(sys.argv[-1])\n'
                        'print(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path}")\n'
                    )
                    commands.append(checksum_command)
                for command in commands:
                    command.chmod(0o755)
                checksum_args = work / "checksum-args"
                result = bash(
                    body,
                    "Azure/fiberd",
                    "1" * 40,
                    "0.4.0",
                    "fiberd.tar.gz",
                    checksum,
                    str(output),
                    SOURCE=str(source),
                    CHECKSUM_ARGS=str(checksum_args),
                    PATH=str(bin_dir),
                )
                if checksum_tool is None:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("sha256sum or shasum is required", result.stderr)
                    self.assertFalse(output.exists())
                    continue
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(output.read_bytes(), payload)
                args = json.loads(checksum_args.read_text())
                expected_prefix = ["-a", "256"] if checksum_tool == "shasum" else []
                self.assertEqual(args[:-1], expected_prefix)
                self.assertTrue(args[-1].endswith("/fiberd.tar.gz"))

    @unittest.skipIf(os.geteuid() == 0, "root can search an unsearchable output directory")
    def test_release_asset_stops_when_output_parent_cannot_resolve(self):
        body = recipe("release-asset")
        with tempfile.TemporaryDirectory() as directory:
            work = Path(directory)
            locked = work / "locked"
            locked.mkdir()
            locked.chmod(0o600)
            bin_dir = work / "bin"
            bin_dir.mkdir()
            mktemp_args = work / "mktemp-args"
            safe_tmp = work / "safe-tmp"
            for name, script in {
                "mktemp": (
                    '#!/bin/bash\nprintf "%s\\n" "$*" > "$MKTEMP_ARGS"\n'
                    'mkdir -p "$SAFE_TMP"\nprintf "%s\\n" "$SAFE_TMP"\n'
                ),
                "curl": '#!/bin/bash\nexit 0\n',
                "sha256sum": '#!/bin/bash\nprintf "%s  %s\\n" "$CHECKSUM" "$1"\n',
                "gh": '#!/bin/bash\nexit 0\n',
                "install": '#!/bin/bash\nexit 0\n',
            }.items():
                command = bin_dir / name
                command.write_text(script)
                command.chmod(0o755)
            try:
                result = bash(
                    body,
                    "Azure/fiberd",
                    "1" * 40,
                    "0.4.0",
                    "fiberd.tar.gz",
                    "0" * 64,
                    str(locked / "output"),
                    MKTEMP_ARGS=str(mktemp_args),
                    SAFE_TMP=str(safe_tmp),
                    CHECKSUM="0" * 64,
                    PATH=f"{bin_dir}:{os.environ['PATH']}",
                )
            finally:
                locked.chmod(0o700)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(mktemp_args.exists(), "recipe continued after output resolution failed")

    def test_controller_manifest_rewrites_only_the_literal_pinned_image(self):
        body = recipe("controller-manifest")
        pinned = "ghcr.io/azure/fiberd-k8s-controller@sha256:" + "12" * 32
        replacement = "lampyr.local/fiberd-k8s-controller:0123456789ab"
        lookalike = pinned.replace("ghcr.io", "ghcrXio")
        for name, source, expected, count in (
            ("one match", f"image: {pinned}\nimage: {lookalike}\n",
             f"image: {replacement}\nimage: {lookalike}\n", 1),
            ("no matches", f"image: {lookalike}\n", None, 0),
            ("two matches", f"image: {pinned}\nimage: {pinned}\n", None, 2),
        ):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                work = Path(directory)
                pins = work / "pins.env"
                pins.write_text(
                    "CONTROLLER_REPOSITORY=Azure/fiberd-k8s-controller\n"
                    f"CONTROLLER_COMMIT={'1' * 40}\n"
                    "CONTROLLER_VERSION=0.1.0\n"
                    f"CONTROLLER_IMAGE={pinned}\n"
                    f"CONTROLLER_MANIFEST_SHA256={'2' * 64}\n"
                )
                bin_dir = work / "bin"
                bin_dir.mkdir()
                just = bin_dir / "just"
                just.write_text(
                    '#!/bin/bash\nset -euo pipefail\n'
                    'for output; do :; done\n'
                    'cp "$SOURCE_MANIFEST" "$output"\n'
                )
                just.chmod(0o755)
                source_manifest = work / "source.yaml"
                source_manifest.write_text(source)
                output = work / "output.yaml"
                script = body.replace("{{ pins }}", str(pins))
                script = script.replace("{{ project-root }}", str(work))
                result = bash(
                    script,
                    replacement,
                    str(output),
                    SOURCE_MANIFEST=str(source_manifest),
                    PATH=f"{bin_dir}:{os.environ['PATH']}",
                )
                if expected is None:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(f"names its pinned image {count} times", result.stderr)
                    self.assertFalse(output.exists())
                else:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(output.read_text(), expected)

    def test_release_outputs_refuse_symbolic_links(self):
        release_body = recipe("release-asset")
        controller_body = recipe("controller-manifest")
        with tempfile.TemporaryDirectory() as directory:
            work = Path(directory)
            target = work / "target"
            target.write_text("unchanged")
            output = work / "output"
            output.symlink_to(target)
            bin_dir = work / "bin"
            bin_dir.mkdir()
            called = work / "called"
            for name in ("curl", "just"):
                command = bin_dir / name
                command.write_text(
                    '#!/bin/bash\nprintf "%s\\n" "$0" >> "$CALLED"\nexit 90\n'
                )
                command.chmod(0o755)
            pins = work / "pins.env"
            pins.write_text("")
            controller_script = controller_body.replace("{{ pins }}", str(pins))
            controller_script = controller_script.replace("{{ project-root }}", str(work))
            cases = (
                (
                    "release asset",
                    release_body,
                    ("Azure/fiberd", "1" * 40, "0.4.0", "fiberd.tar.gz", "2" * 64, str(output)),
                ),
                (
                    "controller manifest",
                    controller_script,
                    ("lampyr.local/fiberd-k8s-controller:0123456789ab", str(output)),
                ),
            )
            for name, script, args in cases:
                with self.subTest(name=name):
                    result = bash(
                        script,
                        *args,
                        CALLED=str(called),
                        PATH=f"{bin_dir}:{os.environ['PATH']}",
                    )
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("refusing symbolic link output", result.stderr)
                    self.assertEqual(target.read_text(), "unchanged")
                    self.assertFalse(called.exists())

    def test_release_asset_checks_checksum_and_attestation_before_replacement(self):
        body = recipe("release-asset")
        payload = b"published release asset"
        checksum = hashlib.sha256(payload).hexdigest()
        for name, supplied, fail_attestation, success in (
            ("valid", checksum, "", True),
            ("wrong checksum", "0" * 64, "", False),
            ("failed attestation", checksum, "1", False),
        ):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                work = Path(directory)
                source = work / "source"
                source.write_bytes(payload)
                output = work / "output"
                output.write_bytes(b"earlier")
                commands = work / "commands"
                bin_dir = work / "bin"
                bin_dir.mkdir()
                curl = bin_dir / "curl"
                curl.write_text(
                    '#!/bin/bash\nset -euo pipefail\n'
                    'printf "curl %s\\n" "$*" >> "$COMMANDS"\n'
                    'while (($#)); do\n'
                    '  if [[ $1 == --output ]]; then cp "$SOURCE" "$2"; exit 0; fi\n'
                    '  shift\n'
                    'done\n'
                    'exit 90\n'
                )
                gh = bin_dir / "gh"
                gh.write_text(
                    '#!/bin/bash\nset -euo pipefail\n'
                    'printf "gh %s\\n" "$*" >> "$COMMANDS"\n'
                    '[[ -z "$FAIL_ATTESTATION" ]]\n'
                )
                curl.chmod(0o755)
                gh.chmod(0o755)
                result = bash(
                    body,
                    "Azure/fiberd",
                    "1" * 40,
                    "0.4.0",
                    "fiberd.tar.gz",
                    supplied,
                    str(output),
                    SOURCE=str(source),
                    COMMANDS=str(commands),
                    FAIL_ATTESTATION=fail_attestation,
                    PATH=f"{bin_dir}:{os.environ['PATH']}",
                )
                self.assertEqual(result.returncode == 0, success, result.stderr)
                self.assertEqual(output.read_bytes(), payload if success else b"earlier")
                calls = commands.read_text().splitlines()
                self.assertTrue(calls[0].startswith("curl "))
                self.assertEqual(any(call.startswith("gh attestation verify ") for call in calls),
                                 supplied == checksum)

    def test_upstream_images_are_pulled_and_only_the_helper_is_built(self):
        body = recipe("cluster-inputs")
        for fail in ("", "pull"):
            with self.subTest(fail=fail), tempfile.TemporaryDirectory(dir=ROOT) as directory:
                work = Path(directory)
                (work / "bin").mkdir()
                (work / "build/tools").mkdir(parents=True)
                pins = work / "pins.env"
                pins.write_text(
                    "CONTROLLER_COMMIT=" + "1" * 40 + "\n"
                    f"CONTROLLER_IMAGE=ghcr.io/azure/fiberd-k8s-controller@{DIGEST}\n"
                    f"FIBERD_IMAGE=ghcr.io/azure/fiberd@{DIGEST}\n"
                )
                docker = work / "bin/docker"
                docker.write_text(
                    '#!/bin/bash\nset -euo pipefail\n'
                    'printf "%s\\n" "$*" >> "$COMMANDS"\n'
                    'case $1 in\n'
                    'pull) [[ -z "$FAIL_PULL" ]] ;;\n'
                    'tag) ;;\n'
                    'save) touch "$3" ;;\n'
                    'buildx)\n'
                    '  while (($#)); do\n'
                    '    if [[ $1 == --output ]]; then output=$2; break; fi\n'
                    '    shift\n'
                    '  done\n'
                    '  path=${output#type=oci,dest=}; path=${path%,tar=false}\n'
                    '  mkdir -p "$path"\n'
                    '  printf \'{"manifests":[{"digest":"%s"}]}\' "$DIGEST" > "$path/index.json"\n'
                    '  ;;\n'
                    '*) exit 92 ;;\n'
                    'esac\n'
                )
                just = work / "bin/just"
                just.write_text(
                    '#!/bin/bash\nset -euo pipefail\nshift 2\n'
                    'case $1 in\n'
                    'helper::prepare-image)\n'
                    '  mkdir -p "$ROOT/build"\n'
                    '  printf "#!/bin/sh\\n" > "$ROOT/build/hluk-helper" ;;\n'
                    'template-tools-target)\n'
                    '  mkdir -p "$ROOT/build/template-tools/$2"\n'
                    '  for tool in zygotectl grant-issuer; do\n'
                    '    printf "#!/bin/sh\\n" > "$ROOT/build/template-tools/$2/$tool"\n'
                    '  done ;;\n'
                    'providers::install-oras) ;;\n'
                    'controller-manifest) printf "image: %s\\n" "$2" > "$3" ;;\n'
                    '*) exit 93 ;;\n'
                    'esac\n'
                )
                oras = work / "build/tools/oras"
                oras.write_text(
                    '#!/bin/bash\nset -euo pipefail\n'
                    'printf "%s\\n" "$*" >> "$COMMANDS"\n'
                    'if [[ $1 == resolve ]]; then\n'
                    '  printf "%s\\n" "$CONTROLLER_PLATFORM_DIGEST"\n'
                    '  exit 0\n'
                    'fi\n'
                    'for destination; do :; done\n'
                    'path=${destination%:ci}; mkdir -p "$path"\n'
                    'printf \'{"manifests":[{"digest":"%s"}]}\' "$DIGEST" > "$path/index.json"\n'
                )
                for command in (docker, just, oras):
                    command.chmod(0o755)
                script = body
                for placeholder, value in {
                    "repo-root": work, "project-root": work / "upstream",
                    "pins": pins,
                }.items():
                    script = script.replace("{{" + placeholder + "}}", str(value))
                    script = script.replace("{{ " + placeholder + " }}", str(value))
                output = work / "cluster-inputs"
                output.mkdir()
                (output / "inputs.json").write_text('{"earlier":true}')
                commands = work / "commands"
                result = bash(
                    script, str(output), cwd=work, ROOT=str(work), DIGEST=DIGEST,
                    CONTROLLER_PLATFORM_DIGEST=CONTROLLER_PLATFORM_DIGEST,
                    COMMANDS=str(commands), FAIL_PULL=fail,
                    PATH=f"{work / 'bin'}:{os.environ['PATH']}",
                )
                if fail:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(json.loads((output / "inputs.json").read_text()), {"earlier": True})
                    self.assertFalse(any(work.glob(".cluster-inputs.*")))
                    continue
                self.assertEqual(result.returncode, 0, result.stderr)
                inputs = json.loads((output / "inputs.json").read_text())
                self.assertEqual(inputs, {
                    "controllerImage": "lampyr.local/fiberd-k8s-controller:" + "1" * 12,
                    "grantDigest": DIGEST,
                    "helperDigest": DIGEST,
                })
                self.assertTrue((output / "images/fiberd-k8s-controller.tar").is_file())
                calls = commands.read_text().splitlines()
                self.assertEqual(calls[0], (
                    "resolve --platform linux/amd64 "
                    f"ghcr.io/azure/fiberd-k8s-controller@{DIGEST}"
                ))
                # Importers request all platforms. Pull and tag the child
                # manifest, not an index whose other platforms are absent.
                controller = (
                    "ghcr.io/azure/fiberd-k8s-controller@"
                    + CONTROLLER_PLATFORM_DIGEST
                )
                self.assertIn(f"pull --platform linux/amd64 {controller}", calls)
                self.assertIn(
                    f"tag {controller} lampyr.local/fiberd-k8s-controller:" + "1" * 12,
                    calls,
                )
                builds = [line for line in calls if line.startswith("buildx build")]
                self.assertEqual(len(builds), 1)
                self.assertIn("helper/ci.Dockerfile", builds[0])
                self.assertTrue(any(
                    f"cp --platform linux/amd64 --to-oci-layout ghcr.io/azure/fiberd@{DIGEST}" in line
                    for line in calls
                ))

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
        self.assertIn("pwd -P", guard.group(1), "cluster-inputs no longer resolves its output")
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
