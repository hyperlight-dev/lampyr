#!/usr/bin/env python3
"""Fixture tests for verified preview image mirroring."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "mirror-preview-images.sh"
SOURCE = "ghcr.io/hyperlight-dev/lampyr"
IMAGES = (
    "fiberd",
    "fiber-router",
    "azure-node-setup",
    "eventgrid-controller",
    "eventgrid-adapter",
    "upstream-fiberd",
    "fiberd-k8s-controller",
    "hluk-helper",
)
SOURCE_DIGITS = {
    "fiberd": "1",
    "fiber-router": "2",
    "azure-node-setup": "3",
    "eventgrid-controller": "4",
    "eventgrid-adapter": "5",
    "upstream-fiberd": "6",
    "fiberd-k8s-controller": "7",
    "hluk-helper": "8",
}
DESTINATION_DIGITS = {
    "fiberd": "a",
    "fiber-router": "b",
    "azure-node-setup": "c",
    "eventgrid-controller": "d",
    "eventgrid-adapter": "e",
    "upstream-fiberd": "f",
    "fiberd-k8s-controller": "9",
    "hluk-helper": "0",
}
CONTROLLER = f"{SOURCE}/fiberd-k8s-controller@sha256:{'7' * 64}"
MANIFEST = f"""apiVersion: apps/v1
kind: Deployment
spec:
  template:
    spec:
      containers:
        - name: manager
          image: {CONTROLLER}
"""
COMMIT = "0123456789abcdef0123456789abcdef01234567"
SOURCE_TAG = f"preview-{COMMIT}"

FAKE = r"""#!/usr/bin/env bash
set -euo pipefail
{ printf '%s' "${0##*/}"; printf '\t%s' "$@"; printf '\n'; } >> "$FAKE_LOG"
if [[ ${0##*/} == gh ]]; then
    case "$1 $2" in
        "auth status") exit 0 ;;
        "api repos/hyperlight-dev/lampyr/commits/preview-release")
            printf '%s\n' "$COMMIT"; exit 0 ;;
        "attestation verify") exit 0 ;;
        "release download")
            while (($#)); do [[ $1 == --dir ]] && dir=$2; shift; done
            root=$(mktemp -d "${FAKE_LOG%/*}/archive.XXXXXX")
            mkdir -p "$root/build"
            printf '%s' "$FAKE_MANIFEST" > "$root/build/fiberd-k8s-controller.yaml"
            tar --create --gzip --file "$dir/lampyr-platform-linux-amd64-preview.tar.gz" \
                --directory "$root" build/fiberd-k8s-controller.yaml
            rm -rf "$root"
            exit 0 ;;
    esac
    exit 90
fi
[[ "$1 $2" == "buildx version" ]] && exit 0
if [[ "$1 $2 $3" == "buildx imagetools inspect" ]]; then
    ref=$4; image=${ref##*/}; image=${image%%:*}
    case "$image" in
        fiberd) digits=1a ;;
        fiber-router) digits=2b ;;
        azure-node-setup) digits=3c ;;
        eventgrid-controller) digits=4d ;;
        eventgrid-adapter) digits=5e ;;
        upstream-fiberd) digits=6f ;;
        fiberd-k8s-controller) digits=79 ;;
        hluk-helper) digits=80 ;;
    esac
    [[ $ref == ghcr.io/hyperlight-dev/lampyr/* ]] && digit=${digits:0:1} || digit=${digits:1:1}
    printf 'sha256:'; printf "${digit}%.0s" {1..64}; printf '\n'; exit 0
fi
if [[ "$1 $2 $3" == "buildx imagetools create" ]]; then
    image=${7##*/}; image=${image%%:*}
    [[ ${FAIL_IMAGE:-} == "$image" ]] && exit 31 || exit 0
fi
exit 91
"""


class PreviewMirrorTest(unittest.TestCase):
    def setUp(self):
        (ROOT / "build").mkdir(exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(
            prefix="preview-mirror-test-", dir=ROOT / "build")
        self.root = Path(self.temporary.name)
        script = self.root / "scripts" / SCRIPT.name
        script.parent.mkdir()
        script.write_bytes(SCRIPT.read_bytes())
        self.script = script
        self.fake_bin = self.root / "bin"
        self.fake_bin.mkdir()
        for name in ("gh", "docker"):
            command = self.fake_bin / name
            command.write_text(FAKE)
            command.chmod(0o755)
        self.log = self.root / "commands"

    def tearDown(self):
        self.temporary.cleanup()

    def run_script(self, destination="registry.example.com/team/lampyr", **extra):
        env = os.environ | {
            "PATH": f"{self.fake_bin}{os.pathsep}{os.environ['PATH']}",
            "FAKE_LOG": str(self.log),
            "COMMIT": COMMIT,
            "FAKE_MANIFEST": MANIFEST,
        } | extra
        return subprocess.run(
            ["bash", str(self.script), destination],
            cwd=self.root, env=env, text=True, capture_output=True, check=False,
        )

    def calls(self, command, prefix=()):
        if not self.log.exists():
            return []
        rows = (line.split("\t") for line in self.log.read_text().splitlines())
        return [
            row[1:] for row in rows
            if row[0] == command and row[1 : 1 + len(prefix)] == list(prefix)
        ]

    def test_fixed_sources_and_attestation_policy(self):
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        inspects = self.calls("docker", ("buildx", "imagetools", "inspect"))
        self.assertEqual(
            [call[3] for call in inspects[:len(IMAGES)]],
            [f"{SOURCE}/{image}:{SOURCE_TAG}" for image in IMAGES])
        self.assertEqual(
            self.calls("gh", ("api",)),
            [["api", "repos/hyperlight-dev/lampyr/commits/preview-release",
              "--jq", ".sha"]])
        policy = [
            "--repo", "hyperlight-dev/lampyr",
            "--signer-workflow", "hyperlight-dev/lampyr/.github/workflows/release.yml",
            "--source-ref", "refs/heads/main",
            "--source-digest", COMMIT,
            "--deny-self-hosted-runners"]
        verified = self.calls("gh", ("attestation", "verify"))
        self.assertEqual(
            verified[:-1],
            [
                ["attestation", "verify",
                 f"oci://{SOURCE}/{image}:{SOURCE_TAG}@sha256:{SOURCE_DIGITS[image] * 64}",
                 *policy]
                for image in IMAGES])
        # The platform archive carrying the controller manifest is verified
        # under the same policy.
        self.assertEqual(verified[-1][3:], policy)
        self.assertTrue(
            verified[-1][2].endswith("/lampyr-platform-linux-amd64-preview.tar.gz"))
        self.assertEqual(
            [call[:6] for call in self.calls("gh", ("release", "download"))],
            [["release", "download", "preview-release",
              "--repo", "hyperlight-dev/lampyr", "--pattern"]])
        creates = self.calls("docker", ("buildx", "imagetools", "create"))
        self.assertTrue(all(call[3:5] == ["--platform", "linux/amd64"] for call in creates))
        self.assertEqual(
            [call[7] for call in creates],
            [f"{SOURCE}/{image}:{SOURCE_TAG}@sha256:{SOURCE_DIGITS[image] * 64}"
             for image in IMAGES])

    def test_invalid_destinations_run_no_commands(self):
        invalid = (
            "", "https://registry.example.com/team", "registry.example.com/Team",
            "registry.example.com/team:preview", "registry.example.com/team/",
            "registry.example.com/team name", "registry.example.com/team@sha256:abc",
            "registry.example.com/team//lampyr")
        for destination in invalid:
            with self.subTest(destination=destination):
                result = self.run_script(destination)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("invalid destination prefix", result.stderr)
        self.assertFalse(self.log.exists())

    def test_copy_failure_writes_no_configs(self):
        result = self.run_script(FAIL_IMAGE="eventgrid-controller")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("failed to mirror", result.stderr)
        self.assertEqual(len(self.calls("docker", ("buildx", "imagetools", "create"))), 4)
        self.assertFalse((self.root / "build" / "preview-kind.yaml").exists())
        self.assertFalse((self.root / "build" / "preview-azure.yaml").exists())
        self.assertFalse(
            (self.root / "build" / "preview-fiberd-k8s-controller.yaml").exists())

    def test_a_manifest_pulling_an_unmirrored_image_copies_nothing(self):
        for image in (
            f"{SOURCE}/fiberd-k8s-controller@sha256:{'9' * 64}",
            f"quay.io/other/controller@sha256:{'7' * 64}",
        ):
            with self.subTest(image=image):
                result = self.run_script(FAKE_MANIFEST=MANIFEST.replace(CONTROLLER, image))
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("not a verified preview image", result.stderr)
                self.assertEqual(self.calls("docker", ("buildx", "imagetools", "create")), [])
                self.log.unlink()

    def test_destination_digests_and_yaml_order(self):
        destination = "localhost:5000/mirrors/lampyr"
        result = self.run_script(destination)
        self.assertEqual(result.returncode, 0, result.stderr)
        refs = {
            image: f"{destination}/{image}:preview@sha256:{DESTINATION_DIGITS[image] * 64}"
            for image in IMAGES}
        inspects = self.calls("docker", ("buildx", "imagetools", "inspect"))
        self.assertEqual(
            [call[3] for call in inspects[len(IMAGES):]],
            [f"{destination}/{image}:preview" for image in IMAGES])
        manifest = self.root / "build" / "preview-fiberd-k8s-controller.yaml"
        self.assertEqual(
            manifest.read_text(),
            MANIFEST.replace(CONTROLLER, refs["fiberd-k8s-controller"]))
        controller = (
            f"controllerManifest: {manifest}\n"
            f"grantImage: {refs['upstream-fiberd']}\n"
            f"helperImage: {refs['hluk-helper']}\n")
        self.assertEqual(
            (self.root / "build" / "preview-kind.yaml").read_text(),
            f"fiberdImage: {refs['fiberd']}\nrouterImage: {refs['fiber-router']}\n"
            + controller)
        self.assertEqual(
            (self.root / "build" / "preview-azure.yaml").read_text(),
            f"fiberdImage: {refs['fiberd']}\nrouterImage: {refs['fiber-router']}\n"
            f"nodeSetupImage: {refs['azure-node-setup']}\n"
            f"eventGridControllerImage: {refs['eventgrid-controller']}\n"
            f"eventGridAdapterImage: {refs['eventgrid-adapter']}\n"
            + controller)
        self.assertEqual(
            [path.name for path in (self.root / "build").iterdir()
             if path.name.startswith(".preview")], [])


if __name__ == "__main__":
    unittest.main()
