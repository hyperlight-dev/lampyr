#!/usr/bin/env python3
"""High-value policy tests for rolling preview and versioned publication."""

from collections import Counter
from contextlib import redirect_stdout, redirect_stderr
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import tomllib
import unittest
from unittest.mock import Mock, patch
import urllib.error
import yaml

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github/workflows/release.yml"
CI_WORKFLOW = ROOT / ".github/workflows/ci.yml"
DEPENDABOT = ROOT / ".github/dependabot.yml"
PUBLISH_SCRIPT = ROOT / "scripts/publish-preview-release.sh"
KIND_INTEGRATION = ROOT / "scripts/test-kind-integration.sh"
VERSION_SCRIPT = ROOT / "scripts/publish-version-release.sh"
TOOLCHAIN = ROOT / "cli/rust-toolchain.toml"
CLI_JUSTFILE = ROOT / "cli/Justfile"
SUFFIX = "${{ needs.version.outputs.suffix }}"
RELEASE_REFS = "github.ref == 'refs/heads/main' || startsWith(github.ref, 'refs/tags/v')"
SHA_PIN = re.compile(r"^[^@\s]+@[0-9a-f]{40}$")
# The gates import their sibling fiberd_grants, as python3 scripts/... finds it.
sys.path.insert(0, str(ROOT / "scripts"))
PINS = {
    "docker/setup-buildx-action": "f87e5991a6d7451dcb8d9637bfbc97413f497069",
    "docker/setup-qemu-action": "99012661954931238ded8c8b007157a8430204e1",
    "docker/login-action": "81c6f93d4bc991e510b342a00b419b3bb4efc5c6",
    "docker/metadata-action": "dc802804100637a589fabce1cb79ff13a1411302",
    "docker/build-push-action": "c3c9e263c25d99ce0380d002d59b67737d91b0dc",
    "actions/attest-build-provenance": "4d101475d8b20a2381f78447822ac1eab6504dd8",
}
MATRIX = {
    "fiberd": ("prepare-fiberd-image", ".", "fiberd/ci.Dockerfile", "", ""),
    "fiber-router": ("prepare-fiber-router-image", ".", "activator/ci.Dockerfile", "", ""),
    "hluk-helper": ("helper::prepare-image", ".", "helper/ci.Dockerfile", "", ""),
    "azure-node-setup": ("azure::prepare-node-setup-image", "providers/azure/node-setup", "providers/azure/node-setup/Dockerfile", "", ""),
    "eventgrid-controller": ("azure::prepare-eventgrid-controller-image", "providers/azure", "providers/azure/eventgrid/ci.Dockerfile", "", "BINARY=eventgrid-controller"),
    "eventgrid-adapter": ("azure::prepare-eventgrid-adapter-image", "providers/azure", "providers/azure/eventgrid/ci.Dockerfile", "", "BINARY=eventgrid-adapter"),
    # Built from upstream/pins.env's commits: fiberd's own Dockerfile from its
    # checkout, and the controller's manager in Lampyr's.
    "upstream-fiberd": ("upstream::prepare-fiberd-image", "build/upstream/fiberd", "build/upstream/fiberd/docker/fiberd/Dockerfile", "fiberd", ""),
    "fiberd-k8s-controller": ("upstream::prepare-controller-image", ".", "upstream/controller.Dockerfile", "", ""),
}
PINS_FILE = ROOT / "upstream/pins.env"
HOSTED_ARCHIVE_KINDS = (
    "cli-linux-amd64",
    "platform-linux-amd64",
    "platform-darwin-arm64",
    "provider-kind",
    "provider-k3d",
    "provider-azure",
)
MACOS_ARCHIVE_KIND = "cli-aarch64-apple-darwin"
ARCHIVE_KINDS = (*HOSTED_ARCHIVE_KINDS, MACOS_ARCHIVE_KIND)


def archives(suffix):
    return tuple(f"lampyr-{kind}-{suffix}.tar.gz" for kind in ARCHIVE_KINDS)


def assets(suffix):
    return {
        name
        for artifact in (*archives(suffix), "install-ly.sh")
        for name in (artifact, f"{artifact}.sha256")
    }


def hosted_assets(suffix):
    return {
        name
        for artifact in (
            *(f"lampyr-{kind}-{suffix}.tar.gz" for kind in HOSTED_ARCHIVE_KINDS),
            "install-ly.sh",
        )
        for name in (artifact, f"{artifact}.sha256")
    }


HOSTED_ATTESTED = (
    *(f"lampyr-{kind}-{SUFFIX}.tar.gz" for kind in HOSTED_ARCHIVE_KINDS),
    "install-ly.sh",
)
MACOS_ATTESTED = f"lampyr-{MACOS_ARCHIVE_KIND}-{SUFFIX}.tar.gz"
COMMON_VERIFY = (
    '--repo "$GITHUB_REPOSITORY"',
    '--signer-workflow "${GITHUB_REPOSITORY}/.github/workflows/release.yml"',
    '--source-digest "$GITHUB_SHA"',
    "--deny-self-hosted-runners",
)
VERIFY = (*COMMON_VERIFY, '--source-ref "$GITHUB_REF"')
PREVIEW_VERIFY = (*COMMON_VERIFY, '--source-ref "refs/heads/main"')


def load(path):
    return yaml.safe_load(path.read_text(encoding="utf-8"))


def named(job, name):
    return next(step for step in job["steps"] if step.get("name") == name)


class PreviewWorkflowPolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = WORKFLOW.read_text(encoding="utf-8")
        cls.workflow = load(WORKFLOW)
        cls.ci = load(CI_WORKFLOW)
        cls.jobs = cls.workflow["jobs"]
        cls.images = cls.jobs["publish-images"]
        cls.macos = cls.jobs["macos-cli-package"]
        cls.publish = cls.jobs["publish"]
        cls.publisher = PUBLISH_SCRIPT.read_text(encoding="utf-8")
        cls.kind_integration = KIND_INTEGRATION.read_text(encoding="utf-8")
        cls.versioned = VERSION_SCRIPT.read_text(encoding="utf-8")
        cls.dependabot = load(DEPENDABOT)
        cls.toolchain = tomllib.loads(TOOLCHAIN.read_text(encoding="utf-8"))["toolchain"]
        cls.cli_justfile = CLI_JUSTFILE.read_text(encoding="utf-8")

    def assert_verify(self, script, fragments=VERIFY):
        for fragment in fragments:
            self.assertIn(fragment, script)

    def test_rust_target_caches_do_not_accumulate_old_build_inputs(self):
        caches = [
            (job_name, step["with"])
            for job_name, job in self.ci["jobs"].items()
            for step in job["steps"]
            if step.get("uses", "").startswith("actions/cache@")
            and any(
                path.startswith("~/.cargo/") or path.endswith("/target")
                for path in step.get("with", {}).get("path", "").splitlines()
            )
        ]
        self.assertEqual({job_name for job_name, _ in caches}, {
            "runtime-assets", "rust-cli", "hluk-helper", "cluster-inputs", "azure-guest",
            "kind-provider-integration", "k3d-integration", "k3d-provider-integration",
            "unit", "local-runtime", "local-contracts", "kind-runtime-integration",
            "kind-cluster-lifecycle", "knative-portability", "kind-existing-cluster",
            "new-developer-workflow", "walkthrough",
        }, "Rust build caching must remain enabled in each existing job")
        for job_name, cache in caches:
            with self.subTest(job=job_name):
                required_paths = {
                    "~/.cargo/registry/index", "~/.cargo/registry/cache",
                    "~/.cargo/git/db",
                }
                # cluster-inputs builds only the helper with Cargo.
                if job_name != "cluster-inputs":
                    required_paths.add("cli/target")
                if job_name in {"hluk-helper", "cluster-inputs"}:
                    required_paths.add("helper/target")
                else:
                    required_paths.add("providers/runtime/build/elfloader_hyperlight-x86_64")
                if job_name not in {
                    "azure-guest", "kind-provider-integration", "k3d-provider-integration",
                    "cluster-inputs",
                }:
                    required_paths.update({"build/hluk", "build/kernel"})
                self.assertTrue(
                    required_paths.issubset(cache["path"].splitlines()),
                    "Keep cached build inputs while bounding stale artifacts",
                )
                self.assertFalse(
                    cache.get("restore-keys"),
                    "Fallback keys import stale artifacts from older build inputs",
                )
                self.assertNotRegex(
                    cache["key"], r"^(cargo|helper)-\$\{\{",
                    "Use a new namespace so inflated legacy caches are not restored",
                )
                self.assertIn("${{ runner.os }}", cache["key"])
                self.assertIn("${{ runner.arch }}", cache["key"])
                hashes = re.findall(r"hashFiles\((.*?)\)", cache["key"])
                inputs = {
                    path
                    for expression in hashes
                    for path in re.findall(r"'([^']+)'", expression)
                }
                paths = cache["path"].splitlines()
                projects = [
                    project for project in ("cli", "helper") if f"{project}/target" in paths
                ]
                self.assertTrue(projects, "A Rust cache holds a project's target")
                for project in projects:
                    for filename in ("Cargo.lock", "Cargo.toml", "rust-toolchain.toml"):
                        self.assertIn(
                            f"{project}/{filename}", inputs,
                            "Cache generations must track dependencies and toolchains",
                        )
        keys = dict(caches)
        self.assertNotEqual(
            keys["cluster-inputs"]["key"].split("-$")[0],
            keys["hluk-helper"]["key"].split("-$")[0],
            "Jobs that cache different paths must not share a cache key",
        )

    def test_triggers_guards_concurrency_and_permissions(self):
        events = self.workflow.get("on", self.workflow.get(True))
        self.assertIn("workflow_dispatch", events)
        self.assertEqual(events["push"]["branches"], ["main"])
        self.assertEqual(events["push"]["tags"], ["v*"])
        self.assertEqual(self.workflow["concurrency"], {
            "group": "${{ startsWith(github.ref, 'refs/tags/') && github.ref || 'preview-release' }}",
            "cancel-in-progress": False,
        })
        self.assertEqual(self.workflow["permissions"], {"contents": "read"})
        for name in ("version", "packages"):
            self.assertNotIn("permissions", self.jobs[name])
            self.assertNotIn("if", self.jobs[name])
        self.assertEqual(
            self.jobs["packages"]["needs"],
            ["version", "publish-provider-snapshots", "publish-images"],
        )
        self.assertEqual(self.macos["needs"], ["version"])
        self.assertEqual(
            self.macos["permissions"],
            {
                "contents": "read",
                "packages": "read",
                "id-token": "write",
                "attestations": "write",
            },
        )
        self.assertNotIn("release-gates", self.jobs)
        self.assertEqual(self.jobs["verify-ci"]["permissions"], {"actions": "read"})
        self.assertEqual(self.jobs["verify-ci"]["if"], RELEASE_REFS)
        self.assertEqual(self.images["if"], RELEASE_REFS)
        self.assertEqual(self.images["needs"], ["version", "verify-ci"])
        self.assertEqual(self.images["permissions"], {
            "contents": "read", "packages": "write",
            "id-token": "write", "attestations": "write",
        })
        self.assertEqual(self.publish["if"], RELEASE_REFS)
        self.assertEqual(set(self.publish["needs"]), {
            "version", "packages", "macos-cli-package", "publish-images"
        })
        self.assertEqual(self.publish["permissions"], {
            "contents": "write", "packages": "write",
            "id-token": "write", "attestations": "write",
        })

    def test_publication_waits_for_successful_ci_on_the_same_commit(self):
        wait = named(self.jobs["verify-ci"], "Wait for CI on this commit")
        self.assertIn(
            "actions/workflows/ci.yml/runs?head_sha=${GITHUB_SHA}&event=push",
            wait["run"],
        )
        self.assertNotIn("env.GITHUB_SHA", wait["run"])
        self.assertIn("head_sha=${GITHUB_SHA}&event=push", wait["run"])
        self.assertIn('[[ "$conclusion" != "success" ]]', wait["run"])
        self.assertIn("Timed out waiting for CI on $GITHUB_SHA", wait["run"])

    def test_actions_are_pinned_and_checkouts_drop_credentials(self):
        counts = Counter()
        for job in self.jobs.values():
            for step in (item for item in job["steps"] if "uses" in item):
                self.assertRegex(step["uses"], SHA_PIN)
                action, sha = step["uses"].rsplit("@", 1)
                if action in PINS:
                    self.assertEqual(sha, PINS[action])
                    counts[action] += 1
                if action == "actions/checkout":
                    self.assertIs(step.get("with", {}).get("persist-credentials"), False)
        expected = Counter({action: 1 for action in PINS})
        expected["docker/setup-buildx-action"] = 4
        expected["docker/setup-qemu-action"] = 1
        expected["docker/login-action"] = 3
        expected["actions/attest-build-provenance"] = 12
        self.assertEqual(counts, expected)

    def test_macos_cli_is_tested_and_packaged_on_native_runner(self):
        ci_job = self.ci["jobs"]["macos-cli"]
        self.assertEqual(
            ci_job["runs-on"],
            ["self-hosted", "macos", "arm64", "hvf"],
        )
        self.assertEqual(
            named(ci_job, "Check and test the macOS CLI")["run"],
            "just cli::test-macos",
        )
        self.assertEqual(
            named(ci_job, "Build the macOS CLI")["run"],
            "just cli::build-macos",
        )
        shared_setup = next(
            step
            for step in ci_job["steps"]
            if step.get("uses", "").startswith("hyperlight-dev/ci-setup-workflow@")
        )
        self.assertEqual(shared_setup["with"]["additional-toolchain-directory"], "cli")
        self.assertEqual(shared_setup["with"]["just-version"], "1.43")
        self.assertEqual(
            self.macos["runs-on"],
            ["self-hosted", "macos", "arm64", "hvf"],
        )
        self.assertEqual(
            named(self.macos, "Check and test the macOS CLI")["run"],
            "just cli::test-macos",
        )
        self.assertEqual(
            named(self.macos, "Package the macOS CLI")["run"],
            'just cli::package-macos "${{ needs.version.outputs.suffix }}"',
        )
        self.assertEqual(
            named(self.macos, "Package the macOS CLI")["env"]["LAMPYR_IMAGE_TAG"],
            "${{ needs.version.outputs.suffix }}",
        )
        shared_release_setup = next(
            step
            for step in self.macos["steps"]
            if step.get("uses", "").startswith("hyperlight-dev/ci-setup-workflow@")
        )
        self.assertEqual(
            shared_release_setup["with"]["additional-toolchain-directory"],
            "cli",
        )
        self.assertEqual(shared_release_setup["with"]["just-version"], "1.43")
        self.assertEqual(
            self.toolchain,
            {
                "channel": "1.91.0",
                "profile": "minimal",
                "components": ["clippy", "rustfmt"],
            },
        )
        self.assertIn(
            'codesign --force --sign - --entitlements "{{project-root}}/macos-entitlements.plist" "{{project-root}}/target/release/ly"',
            self.cli_justfile,
        )
        self.assertIn(
            'codesign --force --sign - --entitlements "{{project-root}}/macos-entitlements.plist" "{{project-root}}/target/debug/provider-snapshot"',
            self.cli_justfile,
        )
        upload = named(self.macos, "Upload macOS CLI for publication")
        listed = {Path(line.strip()).name for line in upload["with"]["path"].splitlines()}
        suffix = "${{ needs.version.outputs.suffix }}"
        self.assertEqual(
            listed,
            {
                f"lampyr-cli-aarch64-apple-darwin-{suffix}.tar.gz",
                f"lampyr-cli-aarch64-apple-darwin-{suffix}.tar.gz.sha256",
            },
        )
        attest = named(self.macos, "Attest macOS CLI archive")
        self.assertEqual(
            attest["with"]["subject-path"],
            f"build/dist/{MACOS_ATTESTED}",
        )

    def test_image_matrix_and_buildkit_policy(self):
        entries = self.images["strategy"]["matrix"]["include"]
        actual = {
            entry["image"]: tuple(entry[key] for key in
                                  ("prepare", "context", "dockerfile", "target", "build_args"))
            for entry in entries
        }
        self.assertEqual(actual, MATRIX)
        self.assertTrue(all(entry["title"] and entry["description"] for entry in entries))
        self.assertEqual(
            named(self.images, "Prepare ${{ matrix.image }} image context")["run"],
            "just ${{ matrix.prepare }}",
        )
        metadata = next(step for step in self.images["steps"] if step.get("id") == "meta")
        build = next(step for step in self.images["steps"] if step.get("id") == "build")
        self.assertEqual(metadata["with"]["images"], "${{ env.IMAGE_PREFIX }}/${{ matrix.image }}")
        self.assertEqual(
            metadata["with"]["tags"], "type=raw,value=${{ needs.version.outputs.image-tag }}"
        )
        self.assertIn("matrix.title", metadata["with"]["labels"])
        self.assertIn("matrix.description", metadata["with"]["labels"])
        self.assertIn(
            "version=${{ needs.version.outputs.image-tag }}", metadata["with"]["labels"]
        )
        self.assertEqual(
            tuple(build["with"][key] for key in ("context", "file", "target", "build-args")),
            ("${{ matrix.context }}", "${{ matrix.dockerfile }}", "${{ matrix.target }}",
             "${{ steps.fiberd-version.outputs.build-args || matrix.build_args }}"),
        )
        # upstream-fiberd alone is built as go.mod's fiberd version, which
        # fiberd -version prints.
        version = next(
            step for step in self.images["steps"] if step.get("id") == "fiberd-version"
        )
        self.assertEqual(version["if"], "matrix.image == 'upstream-fiberd'")
        self.assertIn("just upstream::fiberd-version", version["run"])
        self.assertEqual(MATRIX["upstream-fiberd"][4], "")
        self.assertEqual(build["with"]["platforms"], "linux/amd64")
        self.assertEqual(
            tuple(build["with"][key] for key in ("push", "provenance", "sbom")),
            (True, "mode=max", True),
        )

    def test_image_attestation_is_immediately_verified(self):
        index = next(i for i, step in enumerate(self.images["steps"])
                     if step.get("id") == "build")
        attest, verify = self.images["steps"][index + 1:index + 3]
        self.assertIn("attest-build-provenance@", attest["uses"])
        self.assertEqual(attest["with"]["subject-digest"], "${{ steps.build.outputs.digest }}")
        self.assertIs(attest["with"]["push-to-registry"], True)
        self.assertEqual(
            verify["env"]["IMAGE"],
            "${{ env.IMAGE_PREFIX }}/${{ matrix.image }}:${{ needs.version.outputs.image-tag }}",
        )
        self.assertEqual(verify["env"]["IMAGE_DIGEST"], "${{ steps.build.outputs.digest }}")
        self.assertIn('gh attestation verify "oci://${IMAGE}@${IMAGE_DIGEST}"', verify["run"])
        self.assert_verify(verify["run"])

    def test_release_gates_run_in_existing_kind_ci(self):
        kind_job = self.ci["jobs"]["kind-runtime-integration"]
        self.assertEqual(kind_job["timeout-minutes"], 25)
        release_shards = [
            entry["name"]
            for entry in kind_job["strategy"]["matrix"]["include"]
            if entry["release_gates"] == "1"
        ]
        self.assertEqual(release_shards, ["release-gates"])
        self.assertEqual(
            named(kind_job, "Run Kind runtime examples")["run"],
            "just test-kind-runtime-integration-prebuilt",
        )
        evidence = named(kind_job, "Upload release gate evidence")
        self.assertEqual(evidence["if"], "always()")
        self.assertEqual(evidence["with"]["path"], "build/release-results/")
        self.assertIn('LAMPYR_CI_RELEASE_GATES:-0', self.kind_integration)
        self.assertEqual(self.kind_integration.count("scripts/release-gate.py"), 2)
        self.assertEqual(self.kind_integration.count("scripts/disruption-gate.py"), 1)
        latency_command, reliability_command = self.kind_integration.split(
            "scripts/release-gate.py"
        )[1:]
        self.assertIn('--requests 100 ', latency_command)
        self.assertIn('--concurrency 1 ', latency_command)
        self.assertIn('--requests 300 ', reliability_command)
        self.assertIn('--concurrency 4 ', reliability_command)
        self.assertIn('--p95-ms 0 ', reliability_command)
        self.assertNotIn('--requests 10000', self.kind_integration)
        self.assertIn('--placement-timeout-seconds 15 ', self.kind_integration)
        justfile = (ROOT / "Justfile").read_text(encoding="utf-8")
        self.assertIn(
            'release-reliability-gate service url requests="10000" concurrency="16":',
            justfile,
        )
        self.assertIn('--max-fibers 100 "$project/services/$service"', self.kind_integration)
        self.assertLess(
            self.kind_integration.index('--max-fibers 100'),
            self.kind_integration.index("scripts/release-gate.py"),
        )
        reliability = self.kind_integration.index('"$release_results/reliability.json"')
        restore_capacity = self.kind_integration.index(
            '--max-fibers 4 "$project/services/$service"',
            self.kind_integration.index('--max-fibers 100'),
        )
        disruption = self.kind_integration.index("scripts/disruption-gate.py")
        self.assertLess(reliability, restore_capacity)
        self.assertLess(restore_capacity, disruption)
        self.assertNotIn("release-gate.py", self.text)
        self.assertNotIn("ttl" + ".sh", self.text)

    def test_ci_passed_includes_native_macos(self):
        self.assertIn("macos-cli", self.ci["jobs"]["ci-passed"]["needs"])

    def test_long_runtime_suites_are_sharded_without_losing_examples(self):
        standard = {
            "hello-nodejs", "hello-python", "hello-go",
            "hello-rust", "hello-dotnet",
        }
        standalone = {
            "hello-nodejs-standalone", "hello-python-standalone",
            "hello-go-standalone", "hello-rust-standalone",
            "hello-dotnet-standalone",
        }
        expected = standard | standalone
        jobs = self.ci["jobs"]
        local = jobs["local-runtime"]["strategy"]["matrix"]["include"]
        kind = jobs["kind-runtime-integration"]["strategy"]["matrix"]["include"]
        k3d = jobs["k3d-integration"]["strategy"]["matrix"]["include"]

        def selected(entries):
            return [
                example
                for entry in entries
                for example in entry["examples"].split()
            ]

        self.assertCountEqual(selected(local), expected)
        self.assertCountEqual(
            selected(
                entry
                for entry in kind
                if entry["suite"] == "runtime"
                and entry["release_gates"] == "0"
                and entry.get("eventing_only", "0") == "0"
            ),
            expected,
        )
        self.assertTrue(
            all(
                len(entry["examples"].split()) <= 2
                for entry in kind
                if entry["suite"] == "runtime"
                and entry["release_gates"] == "0"
                and entry.get("eventing_only", "0") == "0"
            )
        )
        k3d_runtime = [entry for entry in k3d if entry["lifecycle_only"] == "0"]
        self.assertCountEqual(selected(k3d_runtime), standard)
        self.assertTrue(
            all(len(entry["examples"].split()) == 1 for entry in k3d_runtime)
        )
        self.assertEqual(
            [entry["name"] for entry in k3d if entry["lifecycle_only"] == "1"],
            ["lifecycle"],
        )
        self.assertEqual(
            named(jobs["k3d-integration"], "Deploy and test every runtime example on k3d")[
                "env"
            ]["LAMPYR_CI_RUNTIME_ONLY"],
            "${{ matrix.runtime_only }}",
        )
        self.assertEqual(
            [entry["name"] for entry in kind if entry["suite"] == "modes"],
            ["fresh-mode", "reuse-mode"],
        )
        node = next(entry for entry in kind if entry["name"] == "node")
        self.assertEqual(node["eventing"], "0")
        eventing = next(entry for entry in kind if entry["name"] == "node-eventing")
        self.assertEqual(eventing["examples"], "hello-nodejs")
        self.assertEqual(eventing["eventing"], "1")
        self.assertEqual(eventing["eventing_only"], "1")
        self.assertEqual(
            [entry["name"] for entry in local if entry["contracts"] == "1"],
            [],
        )
        self.assertEqual(
            named(jobs["local-contracts"], "Run VM handler contracts")["run"],
            "python3 tests/contracts/test_contract.py --ly build/ly --vm",
        )
        lifecycle = jobs["kind-cluster-lifecycle"]["strategy"]["matrix"]["include"]
        self.assertEqual(
            {entry["suite"] for entry in lifecycle},
            {"lifecycle", "eventing-lifecycle"},
        )
        lifecycle_steps = jobs["kind-cluster-lifecycle"]["steps"]
        artifact_cache = next(
            step for step in lifecycle_steps
            if step.get("id") == "kind-provider-artifact"
        )
        self.assertIn("providers/kind/build/rootfs", artifact_cache["with"]["path"])
        self.assertEqual(
            named(jobs["kind-cluster-lifecycle"], "Prepare the Kind provider artifact")[
                "if"
            ],
            "steps.kind-provider-artifact.outputs.cache-hit != 'true'",
        )
        self.assertNotIn("needs", jobs["local-runtime"])
        self.assertNotIn(
            "Test the walkthrough",
            [step.get("name") for step in jobs["new-developer-workflow"]["steps"]],
        )
        self.assertEqual(
            named(jobs["walkthrough"], "Test the walkthrough")["run"],
            "just test-walkthrough",
        )
        walkthrough_steps = [
            step.get("name")
            for step in jobs["walkthrough"]["steps"]
        ]
        self.assertLess(
            walkthrough_steps.index("Build the CLI and container inputs"),
            walkthrough_steps.index("Warm the Fiberd build cache"),
        )

    def test_arm64_provider_builds_are_parallel_and_cached(self):
        job = self.ci["jobs"]["provider-arm64-guests"]
        self.assertEqual(job["strategy"]["matrix"]["provider"], ["kind", "k3d", "azure"])
        self.assertEqual(job["timeout-minutes"], 25)
        build = named(job, "Build macOS ARM64 provider rootfs")
        self.assertIn("matrix.provider", build["with"]["cache-from"])
        self.assertIs(build["with"]["load"], True)
        package = named(job, "Package macOS ARM64 provider guest")
        self.assertIn("providers::build-platform-artifact", package["run"])
        self.assertIn("matrix.provider", package["env"]["LAMPYR_PROVIDER_PREBUILT_IMAGE"])
        download = named(
            self.ci["jobs"]["macos-provider-snapshots"],
            "Download macOS ARM64 provider guests",
        )
        self.assertIs(download["with"]["merge-multiple"], True)
        self.assertIn("*", download["with"]["pattern"])
        for provider in ("kind", "k3d"):
            dockerfile = (
                ROOT / "providers" / provider / "Dockerfile"
            ).read_text(encoding="utf-8")
            self.assertIn("FROM --platform=$BUILDPLATFORM golang:", dockerfile)
            self.assertIn('GOOS="$TARGETOS" GOARCH="$TARGETARCH"', dockerfile)

    def test_checkout_cluster_workflow_uses_a_separate_workspace(self):
        self.assertFalse((ROOT / "lampyr.workspace.yaml").exists())
        justfile = (ROOT / "Justfile").read_text(encoding="utf-8")
        self.assertIn('env_var_or_default("LAMPYR_WORKSPACE", repo-root / "build/workspace")', justfile)
        self.assertIn('build/ly scaffold workspace "{{workspace}}"', justfile)
        for recipe in ["cluster-install", "cluster-install-k3d", "cluster-install-azure"]:
            with self.subTest(recipe=recipe):
                dry_run = subprocess.run(
                    ["just", "--dry-run", recipe, *(["provider.yaml"] if recipe.endswith("azure") else [])],
                    cwd=ROOT,
                    env=dict(os.environ, LAMPYR_WORKSPACE=str(ROOT / "build/workspace")),
                    check=True,
                    text=True,
                    capture_output=True,
                )
                self.assertIn("scaffold workspace", dry_run.stderr)
                self.assertIn(f'--workspace "{ROOT / "build/workspace"}"', dry_run.stderr)
        with tempfile.TemporaryDirectory() as directory:
            workspace = str(Path(directory) / "customer workspace")
            for recipe, arguments in [
                ("cluster-install", []),
                ("cluster-install-k3d", []),
                ("cluster-install-azure", ["provider.yaml"]),
                ("cluster-health", []),
                ("cluster-uninstall", []),
                ("cluster-health-azure", []),
                ("cluster-uninstall-azure", []),
            ]:
                with self.subTest(recipe=recipe, workspace=workspace):
                    dry_run = subprocess.run(
                        ["just", "--dry-run", recipe, *arguments],
                        cwd=ROOT,
                        env=dict(os.environ, LAMPYR_WORKSPACE=workspace),
                        check=True,
                        text=True,
                        capture_output=True,
                    )
                    self.assertIn(f'--workspace "{workspace}"', dry_run.stderr)
        job = self.ci["jobs"]["new-developer-workflow"]
        self.assertEqual(job["env"]["LAMPYR_WORKSPACE"], "${{ github.workspace }}/build/workspace")
        self.assertIn('"LAMPYR_WORKSPACE=$LAMPYR_WORKSPACE"', named(job, "Join the kvm group")["run"])
        apps = named(job, "Run, build and deploy the demo apps")["run"]
        self.assertIn('cd "$LAMPYR_WORKSPACE"', apps)
        self.assertIn('"$repo/build/ly" scaffold service "$app"', apps)
        cleanup = named(job, "Call the deployed apps through Kourier")["run"]
        self.assertIn('build/ly delete "$LAMPYR_WORKSPACE/services/demo-app"', cleanup)
        self.assertIn('--workspace "$LAMPYR_WORKSPACE"', cleanup)

    def test_new_developer_timeout_leaves_diagnostic_headroom(self):
        job = self.ci["jobs"]["new-developer-workflow"]
        diagnostics = named(job, "Collect diagnostics")["timeout-minutes"]
        longest_step = max(
            step.get("timeout-minutes", 0)
            for step in job["steps"]
        )
        self.assertGreater(
            job["timeout-minutes"],
            longest_step + diagnostics,
        )

    def test_kind_provider_artifact_cache_includes_manifest_inputs(self):
        cache = next(
            step
            for step in self.ci["jobs"]["kind-cluster-lifecycle"]["steps"]
            if step.get("id") == "kind-provider-artifact"
        )
        self.assertIn("providers/kind/artifact.json", cache["with"]["key"])

    def test_runtime_example_selector_rejects_unknowns_and_preserves_order(self):
        script = """
          source scripts/runtime-examples.sh
          examples+=("${standalone_examples[@]}")
          select_runtime_examples
          printf '%s\\n' "${examples[@]}"
        """
        environment = dict(os.environ, LAMPYR_CI_EXAMPLES="hello-rust hello-nodejs")
        selected = subprocess.run(
            ["bash", "-c", script],
            cwd=ROOT,
            env=environment,
            check=True,
            text=True,
            capture_output=True,
        )
        self.assertEqual(
            [line.split("|", 1)[0] for line in selected.stdout.splitlines()],
            ["hello-nodejs", "hello-rust"],
        )
        environment["LAMPYR_CI_EXAMPLES"] = "missing-runtime"
        rejected = subprocess.run(
            ["bash", "-c", script],
            cwd=ROOT,
            env=environment,
            text=True,
            capture_output=True,
        )
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn("Unknown LAMPYR_CI_EXAMPLES", rejected.stderr)

    def test_nonempty_line_count_handles_empty_and_single_records(self):
        result = subprocess.run(
            [
                "bash",
                "-c",
                """
                source scripts/runtime-examples.sh
                printf '%s\\n' "$(nonempty_line_count '')"
                printf '%s\\n' "$(nonempty_line_count 'one')"
                printf '%s\\n' "$(nonempty_line_count $'one\\ntwo')"
                """,
            ],
            cwd=ROOT,
            text=True,
            capture_output=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), ["0", "1", "2"])

    def test_runtime_suite_disconnects_registry_before_uninstall(self):
        runtime_cleanup = self.kind_integration.split(
            'if [[ "$suite" == "runtime" || "$suite" == "modes" ]]; then', 1
        )[1].split("current_service=", 1)[0]
        disconnect = runtime_cleanup.index(
            'docker network disconnect "$provider_network" "$registry"'
        )
        remove_registry = runtime_cleanup.index('docker rm -f "$registry"')
        uninstall = runtime_cleanup.index(
            '"$repo_root/build/ly" cluster uninstall --provider kind'
        )
        self.assertLess(disconnect, remove_registry)
        self.assertLess(remove_registry, uninstall)

    def test_failed_cleanup_disconnects_registry_before_uninstall(self):
        cleanup = self.kind_integration.split("cleanup() {", 1)[1].split(
            "\n}\ntrap cleanup EXIT", 1
        )[0]
        disconnect = cleanup.index('docker network disconnect "$provider_network" "$registry"')
        uninstall = cleanup.index('"$repo_root/build/ly" cluster uninstall')
        remove_registry = cleanup.index('docker rm -f "$registry"')
        self.assertLess(disconnect, uninstall)
        self.assertLess(uninstall, remove_registry)

    def test_archive_attestations_and_verification(self):
        subjects = {
            step.get("with", {}).get("subject-path")
            for step in self.publish["steps"]
            if "attest-build-provenance@" in step.get("uses", "")
        }
        self.assertEqual(
            subjects,
            {f"build/dist/{artifact}" for artifact in HOSTED_ATTESTED},
        )
        verify = named(self.publish, "Verify artifact provenance")["run"]
        for artifact in (
            *(f"lampyr-{kind}-\"$SUFFIX\".tar.gz" for kind in HOSTED_ARCHIVE_KINDS),
            "install-ly.sh",
        ):
            self.assertIn(f"build/dist/{artifact}", verify)
        self.assert_verify(verify)
        macos_verify = named(self.publish, "Verify macOS artifact provenance")["run"]
        self.assertIn(
            'build/dist/lampyr-cli-aarch64-apple-darwin-"$SUFFIX".tar.gz',
            macos_verify,
        )
        self.assert_verify(
            macos_verify,
            tuple(fragment for fragment in VERIFY if fragment != "--deny-self-hosted-runners"),
        )
        self.assertNotIn("--deny-self-hosted-runners", macos_verify)

    def test_version_job_gates_tags_before_any_build(self):
        check = named(self.jobs["version"], "Check release versions")["run"]
        checkout = self.jobs["version"]["steps"][0]
        self.assertEqual(checkout["with"]["fetch-depth"], 0)
        self.assertIn(
            'scripts/release-version.py check --tag "$GITHUB_REF_NAME"', check
        )
        self.assertIn('git merge-base --is-ancestor "$GITHUB_SHA" origin/main', check)
        self.assertIn('scripts/require-new-release.sh "$GITHUB_REF_NAME"', check)
        guard = named(self.images, "Require an unpublished version")
        self.assertEqual(guard["if"], "startsWith(github.ref, 'refs/tags/v')")
        self.assertEqual(guard["run"], 'scripts/require-new-release.sh "$GITHUB_REF_NAME"')
        names = [step.get("name") for step in self.images["steps"]]
        self.assertLess(names.index("Require an unpublished version"),
                        names.index("Build and push image"))
        self.assertIn("python3 scripts/release-version.py check\n", check)
        self.assertIn("suffix=preview", check)
        package = named(self.jobs["packages"], "Package release archives")["run"]
        self.assertIn('just package-release "$GITHUB_REF_NAME"', package)
        self.assertIn("just package-preview", package)
        upload = named(self.jobs["packages"], "Upload packages for publication")
        listed = {Path(line.strip()).name for line in upload["with"]["path"].splitlines()}
        self.assertEqual(listed, hosted_assets(SUFFIX))

    def test_versioned_release_is_immutable_and_verified(self):
        self.assertEqual(
            named(self.publish, "Publish version"),
            {
                "name": "Publish version",
                "if": "startsWith(github.ref, 'refs/tags/v')",
                "env": {"GH_TOKEN": "${{ github.token }}"},
                "run": "scripts/publish-version-release.sh",
            },
        )
        self.assertEqual(
            named(self.publish, "Publish preview")["if"], "github.ref == 'refs/heads/main'"
        )
        release = self.versioned
        listed = {Path(line.strip().strip('"')).name for line in release.splitlines()
                  if line.strip().strip('"').startswith("build/dist/")}
        self.assertEqual(listed, assets("${version}"))
        self.assertIn('scripts/release-version.py check --tag "$tag"', release)
        self.assertIn('scripts/require-new-release.sh "$tag"', release)
        guard = (ROOT / "scripts/require-new-release.sh").read_text(encoding="utf-8")
        self.assertIn('"$output" != *"(HTTP 404)"*', guard)
        self.assertNotIn("gh release view", guard)
        self.assertIn('"$object_sha" == "$GITHUB_SHA"', release)
        self.assertIn('reference="${IMAGE_PREFIX}/${image}:${version}"', release)
        self.assertIn(f"images=({' '.join(MATRIX)})", release)
        self.assertIn("--verify-tag", release)
        self.assertNotIn("--prerelease", release)
        for forbidden in ("release delete", "imagetools create", "--method DELETE"):
            self.assertNotIn(forbidden, release)
        self.assert_verify(release)

    def test_release_contains_only_stable_assets(self):
        self.assertEqual(
            named(self.publish, "Publish preview")["run"],
            "scripts/publish-preview-release.sh",
        )
        release = self.publisher
        listed = {Path(line.strip()).name for line in release.splitlines()
                  if line.strip().startswith("build/dist/")}
        self.assertEqual(listed, assets("preview"))
        self.assertIn('--target "$GITHUB_SHA"', release)
        self.assertIn("--prerelease", release)
        self.assertIn('gh release upload "$PREVIEW_TAG" "${assets[@]}"', release)
        for image in MATRIX:
            self.assertIn(f"${{IMAGE_PREFIX}}/{image}:preview-${{GITHUB_SHA}}", release)
        self.assertIn("Canonical images use preview-<full-commit> tags", release)
        self.assertIn("build/dist/install-ly.sh", self.text)
        self.assertNotIn(":latest", self.text)
        self.assertNotIn("value=latest", self.text)

    def test_release_precedes_alias_promotion_and_safe_pruning(self):
        names = [step.get("name") for step in self.publish["steps"]]
        setup_index = names.index("Set up Docker Buildx")
        login_index = names.index("Log in to GHCR")
        publish_index = names.index("Publish preview")
        self.assertLess(setup_index, login_index)
        self.assertLess(login_index, publish_index)
        promote = self.publisher
        self.assertLess(
            promote.index('gh release create "$PREVIEW_TAG"'),
            promote.index("docker buildx imagetools create"),
        )
        for image in MATRIX:
            self.assertRegex(promote, rf"(?m)^\s+{image}$")
        self.assertIn('candidate="${IMAGE_PREFIX}/${image}:preview-${GITHUB_SHA}"', promote)
        self.assertIn('alias="${IMAGE_PREFIX}/${image}:preview"', promote)
        self.assertIn('[[ "$promoted_digest" == "$digest" ]]', promote)
        self.assertIn("docker buildx imagetools create", promote)
        self.assert_verify(promote, PREVIEW_VERIFY)
        prune = self.publisher
        self.assertLess(
            prune.index("docker buildx imagetools create"),
            prune.index('current="preview-${GITHUB_SHA}"'),
        )
        for image in MATRIX:
            self.assertRegex(prune, rf"(?m)^\s+{image}$")
        self.assertIn('current="preview-${GITHUB_SHA}"', prune)
        self.assertIn("date --utc --date='30 days ago'", prune)
        self.assertIn("lampyr%2F${image}", prune)
        self.assertIn("--paginate --slurp", prune)
        self.assertIn("select(.created_at < $cutoff)", prune)
        self.assertIn("select(($tags | length) == 1)", prune)
        self.assertIn('$tags[0] | test("^preview-[0-9a-f]{40}$")', prune)
        self.assertIn("select($tags[0] != $current)", prune)
        self.assertIn("Delete only stale candidate-only versions", prune)
        self.assertIn("gh api --method DELETE", prune)

    def test_dependabot_covers_actions_and_docker(self):
        updates = {item["package-ecosystem"]: item for item in self.dependabot["updates"]}
        self.assertIn("github-actions", updates)
        self.assertEqual(set(updates["docker"]["directories"]), {
            "/activator", "/fiberd", "/helper", "/providers/azure/eventgrid",
            "/providers/azure/node-setup", "/upstream",
        })

    def test_upstream_pins_are_full_commits_and_match_go_mod(self):
        pins = dict(
            line.split("=", 1)
            for line in PINS_FILE.read_text(encoding="utf-8").splitlines()
            if line and not line.startswith("#")
        )
        self.assertEqual(set(pins), {
            "FIBERD_REPOSITORY", "FIBERD_COMMIT",
            "CONTROLLER_REPOSITORY", "CONTROLLER_COMMIT",
        })
        self.assertEqual(pins["FIBERD_REPOSITORY"], "Azure/fiberd")
        self.assertEqual(pins["CONTROLLER_REPOSITORY"], "Azure/fiberd-k8s-controller")
        for name in ("FIBERD_COMMIT", "CONTROLLER_COMMIT"):
            self.assertRegex(pins[name], r"^[0-9a-f]{40}$")
        # The router's fiberd client must speak the grant image's protocol.
        go_mod = (ROOT / "go.mod").read_text(encoding="utf-8")
        version = re.search(r"^\s*github\.com/Azure/fiberd (\S+)$", go_mod, re.M)
        self.assertIsNotNone(version)
        self.assertEqual(version.group(1).rsplit("-", 1)[1], pins["FIBERD_COMMIT"][:12])
        # upstream-fiberd is built as that version.
        self.assertIn(
            """@awk '$1 == "github.com/Azure/fiberd" {print $2; found=1} END {exit !found}' "{{repo-root}}/go.mod""",
            (ROOT / "upstream/Justfile").read_text(encoding="utf-8"),
        )

    def test_upstream_images_describe_the_pinned_source(self):
        metadata = next(step for step in self.images["steps"] if step.get("id") == "meta")
        self.assertIn(
            "org.opencontainers.image.description="
            "${{ steps.upstream.outputs.description || matrix.description }}",
            metadata["with"]["labels"],
        )
        upstream = named(self.images, "Read the upstream source")
        names = [step.get("name") for step in self.images["steps"]]
        self.assertLess(names.index("Read the upstream source"), names.index("Extract image metadata"))
        pins = dict(
            line.split("=", 1)
            for line in (ROOT / "upstream/pins.env").read_text(encoding="utf-8").splitlines()
            if line and not line.startswith("#")
        )
        expected = {
            "upstream-fiberd": f"{pins['FIBERD_REPOSITORY']}@{pins['FIBERD_COMMIT']}",
            "fiberd-k8s-controller": f"{pins['CONTROLLER_REPOSITORY']}@{pins['CONTROLLER_COMMIT']}",
        }
        for image, source in expected.items():
            with self.subTest(image=image), tempfile.TemporaryDirectory() as work:
                output = Path(work) / "output"
                subprocess.run(
                    ["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", upstream["run"]],
                    cwd=ROOT, check=True, capture_output=True, text=True,
                    env={**os.environ, "IMAGE": image, "GITHUB_OUTPUT": str(output)},
                )
                description = output.read_text().removeprefix("description=").strip()
                self.assertIn(source, description)

    def test_packages_pin_the_controller_inputs_to_the_published_images(self):
        images = self.images["steps"]
        names = [step.get("name") for step in images]
        self.assertLess(names.index("Verify image"), names.index("Record image digest"))
        record = named(self.images, "Record image digest")
        self.assertEqual(record["env"]["IMAGE_DIGEST"], "${{ steps.build.outputs.digest }}")
        upload = named(self.images, "Upload image digest")
        self.assertEqual(upload["with"]["name"],
                         "image-digest-${{ matrix.image }}-${{ github.sha }}")
        packages = self.jobs["packages"]["steps"]
        names = [step.get("name") for step in packages]
        download = named(self.jobs["packages"], "Download published image digests")
        self.assertEqual(download["with"]["pattern"], "image-digest-*-${{ github.sha }}")
        self.assertIs(download["with"]["merge-multiple"], True)
        write = named(self.jobs["packages"], "Write the default controller inputs")["run"]
        self.assertIn("just upstream::controller-inputs", write)
        images = ("fiberd-k8s-controller", "upstream-fiberd", "hluk-helper")
        for image in images:
            self.assertIn(f"$(image {image})", write)
            self.assertIn(image, MATRIX)
        # Run the step as Actions does, with a just that records its
        # arguments: a bad digest must stop it before just runs.
        digest = "sha256:" + "a" * 64
        for bad in (None, *images):
            with self.subTest(bad=bad), tempfile.TemporaryDirectory() as work:
                work = Path(work)
                (work / "build/image-digests").mkdir(parents=True)
                for image in images:
                    (work / "build/image-digests" / image).write_text(
                        "sha256:nope" if image == bad else digest
                    )
                (work / "bin").mkdir()
                just = work / "bin/just"
                just.write_text('#!/bin/sh\nprintf "%s\\n" "$@" > "$JUST_LOG"\n')
                just.chmod(0o755)
                log = work / "just.log"
                result = subprocess.run(
                    ["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", write],
                    cwd=work, capture_output=True, text=True,
                    env={**os.environ, "PATH": f"{work / 'bin'}:{os.environ['PATH']}",
                         "IMAGE_PREFIX": "ghcr.io/team", "JUST_LOG": str(log)},
                )
                if bad is None:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(log.read_text().splitlines(), [
                        "upstream::controller-inputs",
                        *(f"ghcr.io/team/{image}@{digest}" for image in images),
                    ])
                else:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(f"bad digest for {bad}", result.stderr)
                    self.assertFalse(log.exists(), "just ran with a bad digest")
        self.assertLess(names.index("Download published image digests"),
                        names.index("Write the default controller inputs"))
        self.assertLess(names.index("Write the default controller inputs"),
                        names.index("Package release archives"))

    def test_cluster_suites_install_the_built_controller_inputs(self):
        jobs = self.ci["jobs"]
        build = jobs["cluster-inputs"]
        self.assertEqual(
            named(build, "Build the cluster suites' controller inputs")["run"],
            "just upstream::cluster-inputs build/cluster-inputs",
        )
        # The cache is warmed for the VERSION the inputs build fiberd as.
        warm = named(build, "Warm the stock fiberd build cache")
        self.assertEqual(
            warm["with"]["build-args"], "VERSION=${{ steps.fiberd-version.outputs.version }}"
        )
        self.assertIn(
            "just upstream::fiberd-version", named(build, "Read the fiberd version")["run"]
        )
        upload = named(build, "Upload the cluster suites' controller inputs")
        self.assertEqual(upload["with"]["name"], "cluster-inputs-${{ github.sha }}")
        self.assertEqual(upload["with"]["path"], "build/cluster-inputs")
        self.assertIn("cluster-inputs", jobs["ci-passed"]["needs"])
        suites = {
            "k3d-integration": "Deploy and test every runtime example on k3d",
            "kind-runtime-integration": "Run Kind runtime examples",
            "kind-cluster-lifecycle": "Run Kind provider and existing-cluster lifecycle",
            "knative-portability": "Verify identical Knative Service on Pods and Fibers",
            "kind-existing-cluster": "Install and use Lampyr on an independently created Kind cluster",
        }
        for job_name, suite in suites.items():
            with self.subTest(job=job_name):
                job = jobs[job_name]
                self.assertEqual(job["needs"], ["cluster-inputs"])
                names = [step.get("name") for step in job["steps"]]
                download = named(job, "Download the cluster suites' controller inputs")
                self.assertEqual(download["with"]["name"], "cluster-inputs-${{ github.sha }}")
                self.assertEqual(download["with"]["path"], "build/cluster-inputs")
                self.assertLess(
                    names.index("Download the cluster suites' controller inputs"),
                    names.index(suite),
                )
                env = named(job, suite)["env"]
                self.assertEqual(
                    env["LAMPYR_CLUSTER_INPUTS"],
                    "${{ github.workspace }}/build/cluster-inputs",
                )
                for published in ("LAMPYR_CONTROLLER_MANIFEST", "LAMPYR_GRANT_IMAGE",
                                  "LAMPYR_HELPER_IMAGE", "LAMPYR_FIBERD_SRC"):
                    self.assertNotIn(published, env)

    def test_documented_installs_take_the_built_controller_inputs_as_defaults(self):
        jobs = self.ci["jobs"]
        # Each serves the inputs on the registryPort its install uses.
        for job_name, port, first in (
            ("new-developer-workflow", "5002", "Install a local Kind cluster"),
            ("walkthrough", "5500", "Test the walkthrough"),
        ):
            with self.subTest(job=job_name):
                job = jobs[job_name]
                self.assertEqual(job["needs"], ["cluster-inputs"])
                names = [step.get("name") for step in job["steps"]]
                download = named(job, "Download the cluster suites' controller inputs")
                self.assertEqual(download["with"]["name"], "cluster-inputs-${{ github.sha }}")
                self.assertEqual(download["with"]["path"], "build/cluster-inputs")
                use = named(job, "Use the cluster suites' controller inputs")
                self.assertEqual(use["run"], f"just use-cluster-inputs {port}")
                self.assertEqual(
                    use["env"]["LAMPYR_CLUSTER_INPUTS"],
                    "${{ github.workspace }}/build/cluster-inputs",
                )
                self.assertLess(
                    names.index("Download the cluster suites' controller inputs"),
                    names.index("Use the cluster suites' controller inputs"),
                )
                self.assertLess(
                    names.index("Use the cluster suites' controller inputs"),
                    names.index(first),
                )
        walkthrough = (ROOT / "docs/walkthrough.md").read_text(encoding="utf-8")
        self.assertIn("registry_port=5500", walkthrough)
        self.assertIn("ly build services/app", walkthrough)
        self.assertIn(
            '"$repo/build/ly" build "$service"',
            named(jobs["new-developer-workflow"], "Run, build and deploy the demo apps")["run"],
        )


class DisruptionGateTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location(
            "disruption_gate", ROOT / "scripts/disruption-gate.py"
        )
        cls.gate = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.gate)

    def run_gate(self, options, placement, elapsed, passes):
        grant = {
            "apiVersion": "fiberd.io/v1alpha1",
            "kind": "CapacityGrant",
            "metadata": {"name": "revision", "namespace": "default", "uid": "grant"},
            "spec": {"podTemplate": {"spec": {"containers": [
                {"name": "fiberd", "image": "ghcr.io/example/fiberd@sha256:" + "1" * 64},
            ]}}},
        }
        refused = self.gate.subprocess.CompletedProcess(
            ["kubectl"], 1, stdout="",
            stderr="image registry.invalid/... is not under a repository listed in "
            "GrantPolicy default spec.images",
        )
        rendered = io.StringIO()
        with (
            patch("sys.argv", ["disruption-gate.py", "--service", "hello",
                               "--url", "https://hello.localhost", *options]),
            patch.object(self.gate, "output", side_effect=[
                json.dumps({"status": {"traffic": [
                    {"revisionName": "revision", "percent": 100},
                ]}}),
                json.dumps({
                    "metadata": {"uid": "pod-uid"},
                    "spec": {"nodeName": "kind-worker"},
                    "status": {"containerStatuses": [
                        {"name": "fiberd", "containerID": "containerd://exited-id"},
                    ]},
                }),
                "agent-id",
            ]),
            patch.object(self.gate.fiberd_grants, "grant", return_value=grant),
            patch.object(self.gate.fiberd_grants, "home_pod",
                         return_value=self.gate.fiberd_grants.Home("home", "pod-uid")),
            patch.object(self.gate, "run", return_value=refused) as run,
            patch.object(self.gate, "open_websocket", return_value=Mock()),
            patch.object(self.gate, "active_fibers", side_effect=[1, 0, 0]),
            patch.object(self.gate, "fiber_epoch", side_effect=[1, 2]),
            patch.object(self.gate, "wait_request", return_value=(200, "ok")),
            patch.object(self.gate, "request", side_effect=[placement, (200, "ok")]) as request,
            patch.object(self.gate.time, "perf_counter", side_effect=[0, elapsed]),
            redirect_stdout(rendered),
        ):
            if passes:
                self.gate.main()
            else:
                with self.assertRaisesRegex(SystemExit, "disruption gate failed"):
                    self.gate.main()
        run.assert_any_call(
            "kubectl", "--context", "kind-lampyr", "--namespace", "default",
            "exec", "pod/home", "--container", "fiberd", "--",
            "sh", "-c", "kill -TERM 1",
        )
        probe = [
            call for call in run.call_args_list if "--dry-run=server" in call.args
        ]
        self.assertEqual(len(probe), 1)
        self.assertEqual(
            json.loads(probe[0].kwargs["input_text"])["spec"]["podTemplate"]["spec"]
            ["containers"][0]["image"],
            self.gate.POLICY_PROBE_IMAGE,
        )
        timeout = 15 if options else 5
        self.assertEqual(request.call_args_list[0].kwargs["timeout"], timeout)
        task = ("docker", "exec", "kind-worker", "ctr",
                "--namespace", "k8s.io", "tasks")
        run.assert_any_call(*task, "pause", "agent-id")
        run.assert_any_call(*task, "resume", "agent-id")
        result = json.loads(rendered.getvalue())
        self.assertEqual(result["placementTimeoutSeconds"], timeout)
        self.assertEqual(result["placementFailureFast"], passes)
        self.assertTrue(result["agentRestart"])
        self.assertTrue(result["placementRecovery"])
        self.assertTrue(result["policyViolationRejected"])
        self.assertTrue(result["controlPlaneOutageServing"])
        self.assertEqual(result["activeFibersAfter"], 0)

    def test_placement_deadlines_preserve_strict_default(self):
        for options, elapsed, passes in [
            ([], 4.9, True),
            ([], 5, False),
            (["--placement-timeout-seconds", "15"], 10, True),
            (["--placement-timeout-seconds", "15"], 15, False),
        ]:
            with self.subTest(options=options, elapsed=elapsed):
                self.run_gate(options, (503, ""), elapsed, passes)

    def test_ci_still_requires_explicit_service_unavailable(self):
        for placement in [
            (200, "ok"),
            (502, "bad gateway"),
            TimeoutError("timed out"),
            urllib.error.HTTPError("https://hello.localhost", 503, "unavailable", {}, None),
        ]:
            with self.subTest(placement=placement):
                self.run_gate(
                    ["--placement-timeout-seconds", "15"],
                    placement,
                    3,
                    isinstance(placement, urllib.error.HTTPError),
                )

    def test_invalid_deadline_fails_before_cluster_access(self):
        for value in ["0", "-1", "invalid"]:
            with (
                self.subTest(value=value),
                patch("sys.argv", ["disruption-gate.py", "--service", "hello",
                                   "--url", "https://hello.localhost",
                                   "--placement-timeout-seconds", value]),
                patch.object(self.gate, "output") as output,
                redirect_stderr(io.StringIO()),
            ):
                with self.assertRaises(SystemExit) as error:
                    self.gate.main()
                self.assertEqual(error.exception.code, 2)
                output.assert_not_called()

    def test_only_a_policy_refusal_counts_as_rejection(self):
        grant = {
            "apiVersion": "fiberd.io/v1alpha1",
            "kind": "CapacityGrant",
            "metadata": {"name": "revision", "namespace": "default"},
            "spec": {"podTemplate": {"spec": {"containers": [
                {"name": "fiberd", "image": "ghcr.io/example/fiberd@sha256:" + "1" * 64},
            ]}}},
        }
        image_rule = ("image registry.invalid/lampyr-policy-probe/fiberd@sha256:... "
                      "is not under a repository listed in GrantPolicy default spec.images")
        for code, stderr, rejected in [
            (1, f"denied: ... {image_rule}", True),
            # The fallback message, for when the expression's is unusable.
            (1, "the fiberd container image is not under a repository listed in "
                "GrantPolicy default spec.images", True),
            # Another GrantPolicy rule says nothing about the image rule.
            (1, "spec.helperImage is not under a repository listed in "
                "GrantPolicy default spec.helperImages", False),
            (1, "a podTemplate env name must match ^FIBERD_TENANT_[A-Z0-9_]+$", False),
            # Refused for another reason, such as a missing CRD.
            (1, "error: the server doesn't have a resource type", False),
            (0, image_rule, False),
        ]:
            with (
                self.subTest(code=code, stderr=stderr),
                patch.object(self.gate, "run", return_value=self.gate.subprocess.CompletedProcess(
                    ["kubectl"], code, stdout="", stderr=stderr,
                )),
            ):
                self.assertEqual(
                    self.gate.policy_violation_rejected("kind-test", grant), rejected
                )

    def test_restart_waits_for_fiberd_to_raise_its_epoch(self):
        with (
            patch.object(self.gate, "fiber_epoch", side_effect=[
                self.gate.subprocess.CalledProcessError(1, ["kubectl"]), 3, 4,
            ]),
            patch.object(self.gate.time, "sleep"),
        ):
            self.assertEqual(self.gate.wait_epoch("kind-test", "home", 3), 4)

    def test_paused_agent_resumes_on_unexpected_probe_error(self):
        pod = {
            "metadata": {"uid": "pod-uid"},
            "spec": {"nodeName": "kind-worker"},
            "status": {"containerStatuses": [
                {"name": "fiberd", "containerID": "containerd://agent-id"},
            ]},
        }
        with (
            patch.object(self.gate, "output", side_effect=[json.dumps(pod), "agent-id"]) as output,
            patch.object(self.gate, "run") as run,
        ):
            with self.assertRaisesRegex(RuntimeError, "probe failed"):
                with self.gate.paused_agent("kind-test", "home"):
                    raise RuntimeError("probe failed")
        task = ("docker", "exec", "kind-worker", "ctr",
                "--namespace", "k8s.io", "tasks")
        self.assertEqual(
            [call.args for call in run.call_args_list],
            [(*task, "pause", "agent-id"), (*task, "resume", "agent-id")],
        )
        output.assert_any_call(
            "docker", "exec", "kind-worker", "crictl", "ps",
            "--state", "running", "--name", "^fiberd$",
            "--label", "io.kubernetes.pod.uid=pod-uid", "--quiet",
        )

    def test_paused_agent_requires_one_running_container(self):
        pod = {
            "metadata": {"uid": "pod-uid"},
            "spec": {"nodeName": "kind-worker"},
        }
        for containers in ["", "agent-id\nanother-id"]:
            with (
                self.subTest(containers=containers),
                patch.object(self.gate, "output", side_effect=[json.dumps(pod), containers]),
                patch.object(self.gate, "run") as run,
            ):
                with self.assertRaisesRegex(RuntimeError, "expected one running fiberd"):
                    with self.gate.paused_agent("kind-test", "home"):
                        self.fail("ambiguous container must not execute the probe")
                run.assert_not_called()

    def test_command_failure_includes_stderr(self):
        with patch.object(
            self.gate.subprocess, "run",
            return_value=self.gate.subprocess.CompletedProcess(
                ["ctr"], 1, stdout="", stderr="container not found",
            ),
        ):
            with self.assertRaisesRegex(RuntimeError, "container not found"):
                self.gate.run("ctr")


class ReleaseGateTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location(
            "release_gate", ROOT / "scripts/release-gate.py"
        )
        cls.gate = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.gate)

    def test_active_revision_uses_resolved_traffic(self):
        service = {"status": {"traffic": [
            {"revisionName": "demo-00001", "percent": 50, "tag": "blue"},
            {"revisionName": "demo-00001", "percent": 50, "tag": "green"},
        ]}}
        self.assertEqual(self.gate.active_revision(service), "demo-00001")
        service["status"]["traffic"].append(
            {"revisionName": "demo-00002", "percent": 10}
        )
        with self.assertRaisesRegex(RuntimeError, "exactly one"):
            self.gate.active_revision(service)

    def test_idle_needs_no_busy_fiber_no_service_pod_and_a_ready_grant(self):
        started = [{"event": "create"}, {"event": "resume"}, {"event": "release"}]
        with (
            patch.object(self.gate.fiberd_grants, "router_fibers_busy", side_effect=[2, 0, 0, 0]),
            patch.object(self.gate, "service_pods", return_value=0),
            patch.object(self.gate.fiberd_grants, "grant_settled", side_effect=[False, True]),
            patch.object(self.gate.fiberd_grants, "audit_records", return_value=started) as audit,
            patch.object(self.gate.time, "sleep"),
        ):
            self.assertEqual(
                self.gate.wait_idle("kind-test", "hello", "revision", "home"),
                {"activeFibers": 0, "totalFibersStarted": 2},
            )
        # The spool is read once, when the workload is idle.
        audit.assert_called_once_with("kind-test", "home")

if __name__ == "__main__":
    unittest.main()
