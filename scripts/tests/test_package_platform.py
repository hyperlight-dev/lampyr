import contextlib
import hashlib
import io
import json
from pathlib import Path
import runpy
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import Mock, patch


packaging = runpy.run_path(
    str(Path(__file__).resolve().parents[1] / "package-platform.py")
)
provider_packaging = runpy.run_path(
    str(Path(__file__).resolve().parents[2] / "providers" / "package.py")
)


MACHO_ARM64 = b"\xcf\xfa\xed\xfe\x0c\x00\x00\x01" + bytes(24)


def write_template_tools(root, linux):
    for target, header in (("linux-amd64", linux), ("darwin-arm64", MACHO_ARM64)):
        for name in packaging["TEMPLATE_TOOLS"]:
            path = root / packaging["TEMPLATE_TOOLS_DIRECTORY"] / target / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(header)


class PackagingTests(unittest.TestCase):
    def test_macos_cli_archive_requires_arm64_macho_and_creates_alias(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "dist"
            output.mkdir()
            binary = root / "ly"
            binary.write_bytes(b"\xcf\xfa\xed\xfe\x0c\x00\x00\x01" + bytes(24))
            binary.chmod(0o777)
            license_file = root / "LICENSE"
            license_file.write_text("license")
            license_file.chmod(0o666)

            packaging["validate_cli_binary"](binary, "aarch64-apple-darwin")
            with self.assertRaisesRegex(ValueError, "macOS/arm64"):
                packaging["validate_cli_binary"](license_file, "aarch64-apple-darwin")

            archive = output / "lampyr-cli-aarch64-apple-darwin-1.2.3.tar.gz"
            packaging["archive"](
                archive,
                {"ly": binary, "LICENSE": license_file},
            )
            packaging["cli_alias"](
                output,
                "preview",
                "1.2.3",
                "aarch64-apple-darwin",
            )

            with tarfile.open(archive, "r:gz") as bundle:
                self.assertEqual(
                    [member.name for member in bundle.getmembers()],
                    ["LICENSE", "ly"],
                )
                self.assertEqual(bundle.getmember("ly").mode, 0o755)
                self.assertEqual(bundle.getmember("LICENSE").mode, 0o644)
            alias = output / "lampyr-cli-aarch64-apple-darwin-preview.tar.gz"
            self.assertEqual(alias.read_bytes(), archive.read_bytes())
            self.assertEqual(
                (output / f"{alias.name}.sha256").read_text(),
                f"{hashlib.sha256(alias.read_bytes()).hexdigest()}  {alias.name}\n",
            )

    def test_darwin_platform_alias_matches_versioned_bundle(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            archive = output / "lampyr-platform-darwin-arm64-1.2.3.tar.gz"
            archive.write_bytes(b"darwin platform")
            packaging["platform_alias"](
                output,
                "preview",
                "1.2.3",
                "darwin-arm64",
            )
            alias = output / "lampyr-platform-darwin-arm64-preview.tar.gz"
            self.assertEqual(alias.read_bytes(), archive.read_bytes())
            self.assertEqual(
                alias.with_name(alias.name + ".sha256").read_text(),
                f"{hashlib.sha256(alias.read_bytes()).hexdigest()}  {alias.name}\n",
            )

    def test_darwin_platform_packages_and_validates_the_native_supervisor(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "cli").mkdir()
            (root / "cli/Cargo.toml").write_text('[package]\nversion = "1.2.3"\n')
            for relative in packaging["PLATFORM_STATIC"]:
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"content")
            linux = bytearray(20)
            linux[:6] = b"\x7fELF\x02\x01"
            linux[18:20] = b"\x3e\x00"
            for name in packaging["PLATFORM_BINARIES"]:
                path = root / "build" / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(linux)
            native = root / "build/ly-dev-darwin-arm64"
            macho = b"\xcf\xfa\xed\xfe\x0c\x00\x00\x01" + bytes(24)
            native.write_bytes(macho)
            native.chmod(0o755)
            write_template_tools(root, linux)
            for name in packaging["PROVIDER_NAMES"]:
                directory = root / "providers" / name / "build/rootfs"
                directory.mkdir(parents=True)
                manifest = {
                    "id": name, "protocolVersion": 1, "version": "0.1.0",
                    "runtimeVersion": packaging["snapshot_oci"].RUNTIME_VERSION,
                    "memoryMiB": 256, "operations": {"describe": []},
                }
                (root / "providers" / name / "artifact.json").write_text(json.dumps(manifest))
                (root / "providers" / name / "provider.py").write_bytes(b"program")
                for field, filename in [("rootfs", "rootfs.cpio"), ("program", "provider.py")]:
                    content = field.encode()
                    (directory / filename).write_bytes(content)
                    manifest[field] = {
                        "path": filename, "sha256": hashlib.sha256(content).hexdigest(),
                    }
                (directory / "manifest.json").write_text(json.dumps(manifest))
            output = root / "dist"
            arguments = [
                "package-platform.py", "--root", str(root), "--output", str(output),
                "--source-commit", "0" * 40, "--platform-only",
                "--platform-target", "darwin-arm64",
            ]
            with patch.object(sys, "argv", arguments):
                packaging["main"]()
            with tarfile.open(output / "lampyr-platform-darwin-arm64-1.2.3.tar.gz") as bundle:
                self.assertEqual(bundle.extractfile("build/ly-dev").read(), macho)
                self.assertEqual(bundle.getmember("build/ly-dev").mode, 0o755)
                self.assertEqual(bundle.extractfile("build/hluk").read(), linux)
            native.write_bytes(linux)
            with patch.object(sys, "argv", arguments):
                with self.assertRaisesRegex(ValueError, "macOS/arm64"):
                    packaging["main"]()

    def test_provider_packaging_without_kvm_uses_cold_boot_and_removes_stale_snapshot(
        self,
    ):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            project = root / "providers/kind"
            output = project / "build/rootfs"
            output.mkdir(parents=True)
            (project / "provider.py").write_text("print('provider')\n")
            (output / "rootfs.cpio").write_bytes(b"rootfs")
            kernel = root / "kernel"
            kernel.write_bytes(b"kernel")
            (output / "snapshot.tar.gz").write_bytes(b"stale snapshot")
            (output / "snapshot").mkdir()
            (output / "snapshot/stale").write_bytes(b"stale")
            (project / "artifact.json").write_text(
                json.dumps(
                    {
                        "id": "kind",
                        "version": "0.1.0",
                        "memoryMiB": 128,
                        "snapshot": {"path": "snapshot.tar.gz", "sha256": "stale"},
                        "snapshotCompatibility": {"cpuVendor": "stale"},
                    }
                )
            )
            stderr = io.StringIO()
            native_snapshot_available = Mock(return_value=False)
            with (
                patch.object(
                    sys,
                    "argv",
                    ["package.py", str(project), "rootfs", "--kernel", str(kernel)],
                ),
                patch.dict(
                    provider_packaging["main"].__globals__,
                    {"native_snapshot_available": native_snapshot_available},
                ),
                contextlib.redirect_stderr(stderr),
            ):
                provider_packaging["main"]()

            native_snapshot_available.assert_called_once_with()
            manifest = json.loads((output / "manifest.json").read_text())
            self.assertEqual((output / "kernel").read_bytes(), b"kernel")
            self.assertEqual(
                manifest["kernel"],
                {"path": "kernel", "sha256": hashlib.sha256(b"kernel").hexdigest()},
            )
            self.assertNotIn("snapshot", manifest)
            self.assertNotIn("snapshotCompatibility", manifest)
            self.assertFalse((output / "snapshot.tar.gz").exists())
            self.assertFalse((output / "snapshot").exists())
            self.assertIn("cold-boot fallback", stderr.getvalue())

    def test_platform_archive_excludes_image_owned_build_inputs(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for relative in packaging["PLATFORM_STATIC"]:
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"content")
            header = bytearray(20)
            header[:6] = b"\x7fELF\x02\x01"
            header[18:20] = b"\x3e\x00"
            for name in packaging["PLATFORM_BINARIES"]:
                path = root / "build" / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(header)
            write_template_tools(root, header)

            source_commit = "0123456789abcdef0123456789abcdef01234567"
            files = packaging["platform_base_files"](root, "1.2.3", source_commit)

            self.assertEqual(
                set(files),
                {
                    ".lampyr-platform",
                    ".lampyr-platform-commit",
                    "LICENSE",
                    "build/grant-issuer",
                    "build/hluk",
                    "build/hluk-helper",
                    "build/kernel",
                    "build/ly-dev",
                    "build/zygotectl",
                    "k8s/cert-manager-csi-driver.yaml",
                    "k8s/fiber-router.yaml",
                    "k8s/fiberd-tenant.yaml",
                    "k8s/hyperlight-device-plugin.yaml",
                    "k8s/serving-controller.yaml",
                },
            )
            for excluded in (
                "build/serving-controller",
                "build/fiber-router",
                "fiberd/ci.Dockerfile",
                "activator/ci.Dockerfile",
            ):
                self.assertNotIn(excluded, files)
            self.assertEqual(files[".lampyr-platform-commit"], f"{source_commit}\n".encode())
            with self.assertRaisesRegex(ValueError, "source commit"):
                packaging["commit_name"]("preview")

    def test_platform_archive_carries_template_tools_for_its_target(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for relative in packaging["PLATFORM_STATIC"]:
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"content")
            header = bytearray(20)
            header[:6] = b"\x7fELF\x02\x01"
            header[18:20] = b"\x3e\x00"
            (root / "build").mkdir()
            for name in packaging["PLATFORM_BINARIES"]:
                (root / "build" / name).write_bytes(header)
            # A darwin-arm64 platform carries ly-dev built for macOS.
            (root / "build/ly-dev-darwin-arm64").write_bytes(MACHO_ARM64)
            write_template_tools(root, header)
            commit = "0123456789abcdef0123456789abcdef01234567"
            tools = root / packaging["TEMPLATE_TOOLS_DIRECTORY"]
            for target in ("linux-amd64", "darwin-arm64"):
                with self.subTest(target=target):
                    files = packaging["platform_base_files"](
                        root, "1.2.3", commit, None, target)
                    for name in ("zygotectl", "grant-issuer"):
                        self.assertEqual(files[f"build/{name}"], tools / target / name)
                    # The helper runs where snapshots are made, always Linux.
                    self.assertEqual(files["build/hluk-helper"], root / "build/hluk-helper")
            # A tool built for the other target is refused.
            (tools / "darwin-arm64/zygotectl").write_bytes(header)
            with self.assertRaisesRegex(ValueError, "macOS/arm64"):
                packaging["platform_base_files"](root, "1.2.3", commit, None, "darwin-arm64")
            (tools / "linux-amd64/grant-issuer").write_bytes(MACHO_ARM64)
            with self.assertRaisesRegex(ValueError, "Linux/amd64"):
                packaging["platform_base_files"](root, "1.2.3", commit)
            (tools / "linux-amd64/grant-issuer").unlink()
            with self.assertRaises(FileNotFoundError):
                packaging["platform_base_files"](root, "1.2.3", commit)

    def test_platform_archive_carries_written_controller_inputs(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for relative in packaging["PLATFORM_STATIC"]:
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"content")
            header = bytearray(20)
            header[:6] = b"\x7fELF\x02\x01"
            header[18:20] = b"\x3e\x00"
            (root / "build").mkdir()
            for name in packaging["PLATFORM_BINARIES"]:
                (root / "build" / name).write_bytes(header)
            write_template_tools(root, header)
            commit = "0123456789abcdef0123456789abcdef01234567"
            digest = "sha256:" + "ab" * 32
            inputs = root / "build/controller-inputs.json"
            manifest = root / "build/fiberd-k8s-controller.yaml"
            manifest.write_text("kind: Deployment\n")
            valid = {
                "controllerManifest": "build/fiberd-k8s-controller.yaml",
                "grantImage": f"ghcr.io/hyperlight-dev/lampyr/upstream-fiberd@{digest}",
                "helperImage": f"ghcr.io/hyperlight-dev/lampyr/hluk-helper@{digest}",
            }
            inputs.write_text(json.dumps(valid))

            files = packaging["platform_base_files"](root, "1.2.3", commit)

            self.assertEqual(files["build/controller-inputs.json"], inputs)
            self.assertEqual(files["build/fiberd-k8s-controller.yaml"], manifest)
            for invalid, message in (
                ({**valid, "grantImage": "ghcr.io/team/fiberd:v1"}, "grantImage must be pinned"),
                ({**valid, "controllerManifest": "../controller.yaml"}, "must name"),
                ({**valid, "zygoteRegistries": ["ghcr.io/team"]}, "must give"),
            ):
                with self.subTest(message=message):
                    inputs.write_text(json.dumps(invalid))
                    with self.assertRaisesRegex(ValueError, message):
                        packaging["platform_base_files"](root, "1.2.3", commit)
            for name, document, text in (
                ("helperImage", {**valid, "helperImage": f"localhost:5002/hluk-helper@{digest}"},
                 "kind: Deployment\n"),
                ("grantImage", {**valid, "grantImage": f"127.0.0.1:5500/upstream-fiberd@{digest}"},
                 "kind: Deployment\n"),
                ("manifest", valid,
                 f"spec:\n  containers:\n    - image: localhost:5002/fiberd-k8s-controller@{digest}\n"),
                ("quoted manifest", valid, f'      image: "[::1]:5000/controller@{digest}"\n'),
            ):
                with self.subTest(local=name):
                    inputs.write_text(json.dumps(document))
                    manifest.write_text(text)
                    with self.assertRaisesRegex(ValueError, "local registry"):
                        packaging["platform_base_files"](root, "1.2.3", commit)
            inputs.write_text(json.dumps(valid))
            manifest.write_text(f"        image: ghcr.io/hyperlight-dev/lampyr/controller@{digest}\n")
            packaging["platform_base_files"](root, "1.2.3", commit)
            manifest.unlink()
            with self.assertRaises(FileNotFoundError):
                packaging["platform_base_files"](root, "1.2.3", commit)

    def test_preview_aliases_match_versioned_assets_and_name_checksums(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            assets = {
                "lampyr-cli-linux-amd64-1.2.3.tar.gz": b"cli archive",
                "lampyr-platform-linux-amd64-1.2.3.tar.gz": b"platform archive",
                "lampyr-provider-kind-4.5.6.tar.gz": b"kind archive",
                "lampyr-provider-k3d-0.1.0.tar.gz": b"k3d archive",
                "lampyr-provider-azure-7.8.9.tar.gz": b"azure archive",
            }
            for name, content in assets.items():
                (output / name).write_bytes(content)

            packaging["release_aliases"](
                output, "preview", "1.2.3", {"kind": "4.5.6", "k3d": "0.1.0", "azure": "7.8.9"}
            )

            aliases = {
                "lampyr-cli-linux-amd64-preview.tar.gz": (
                    "lampyr-cli-linux-amd64-1.2.3.tar.gz"
                ),
                "lampyr-platform-linux-amd64-preview.tar.gz": (
                    "lampyr-platform-linux-amd64-1.2.3.tar.gz"
                ),
                "lampyr-provider-kind-preview.tar.gz": (
                    "lampyr-provider-kind-4.5.6.tar.gz"
                ),
                "lampyr-provider-k3d-preview.tar.gz": (
                    "lampyr-provider-k3d-0.1.0.tar.gz"
                ),
                "lampyr-provider-azure-preview.tar.gz": (
                    "lampyr-provider-azure-7.8.9.tar.gz"
                ),
            }
            for alias_name, versioned_name in aliases.items():
                alias = output / alias_name
                self.assertEqual(
                    alias.read_bytes(), (output / versioned_name).read_bytes()
                )
                checksum = alias.with_name(alias.name + ".sha256")
                self.assertEqual(
                    checksum.read_text(),
                    f"{hashlib.sha256(alias.read_bytes()).hexdigest()}  {alias_name}\n",
                )
            for name, content in assets.items():
                self.assertEqual((output / name).read_bytes(), content)
            self.assertEqual(
                sorted(path.name for path in output.glob("*preview*")),
                sorted(
                    name
                    for alias_name in aliases
                    for name in (alias_name, alias_name + ".sha256")
                ),
            )

    def test_release_aliases_use_one_version_suffix(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            for name in (
                "lampyr-cli-linux-amd64-1.2.3.tar.gz",
                "lampyr-platform-linux-amd64-1.2.3.tar.gz",
                "lampyr-provider-kind-4.5.6.tar.gz",
                "lampyr-provider-azure-7.8.9.tar.gz",
            ):
                (output / name).write_bytes(name.encode())
                (output / f"{name}.sha256").write_text("original\n")

            packaging["release_aliases"](
                output, "1.2.3", "1.2.3", {"kind": "4.5.6", "azure": "7.8.9"}
            )

            self.assertEqual(
                (output / "lampyr-cli-linux-amd64-1.2.3.tar.gz.sha256").read_text(),
                "original\n",
            )
            self.assertEqual(
                (output / "lampyr-provider-kind-1.2.3.tar.gz").read_bytes(),
                b"lampyr-provider-kind-4.5.6.tar.gz",
            )
            self.assertTrue((output / "lampyr-provider-azure-1.2.3.tar.gz.sha256").is_file())

    def test_image_tag_replaces_preview_manifest_images(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for relative in packaging["PLATFORM_STATIC"]:
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(
                    "image: ghcr.io/hyperlight-dev/lampyr/fiberd:preview\n"
                    "other: example.com/thing:preview\n"
                )
            header = bytearray(20)
            header[:6] = b"\x7fELF\x02\x01"
            header[18:20] = b"\x3e\x00"
            for name in packaging["PLATFORM_BINARIES"]:
                path = root / "build" / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(header)
            write_template_tools(root, header)
            commit = "0123456789abcdef0123456789abcdef01234567"

            files = packaging["platform_base_files"](root, "1.2.3", commit, "1.2.3")

            self.assertEqual(
                files["k8s/fiberd-tenant.yaml"],
                b"image: ghcr.io/hyperlight-dev/lampyr/fiberd:1.2.3\n"
                b"other: example.com/thing:preview\n",
            )
            self.assertEqual(files["LICENSE"], root / "LICENSE")
            untagged = packaging["platform_base_files"](root, "1.2.3", commit)
            self.assertEqual(untagged["k8s/fiberd-tenant.yaml"], root / "k8s/fiberd-tenant.yaml")
            with self.assertRaisesRegex(ValueError, "image tag"):
                packaging["image_tag_name"]("bad tag")

    def test_provider_rejects_modified_content_and_parent_paths(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            directory = root / "providers/kind/build/rootfs"
            directory.mkdir(parents=True)
            manifest = {"id": "kind", "protocolVersion": 1, "version": "0.1.0",
                        "runtimeVersion": "0.21.0", "memoryMiB": 256,
                        "operations": {"describe": []}}
            for field, name in [("rootfs", "rootfs.cpio"), ("program", "provider.py")]:
                content = field.encode()
                (directory / name).write_bytes(content)
                manifest[field] = {
                    "path": name,
                    "sha256": hashlib.sha256(content).hexdigest(),
                }
            manifest_path = directory / "manifest.json"
            manifest_path.write_text(json.dumps(manifest))
            version, files = packaging["provider_files"](root, "kind")
            self.assertEqual(version, "0.1.0")
            self.assertEqual(files["provider.py"], directory / "provider.py")
            manifest["snapshot"] = {"path": "snapshot.tar.gz", "sha256": "0" * 64}
            manifest_path.write_text(json.dumps(manifest))
            with self.assertRaisesRegex(ValueError, "manifest fields"):
                packaging["provider_files"](root, "kind")
            manifest.pop("snapshot")
            manifest_path.write_text(json.dumps(manifest))
            (directory / "rootfs.cpio").write_bytes(b"modified")
            with self.assertRaisesRegex(ValueError, "digest"):
                packaging["provider_files"](root, "kind")
            (directory / "rootfs.cpio").write_bytes(b"rootfs")
            manifest["program"]["path"] = "../outside"
            manifest_path.write_text(json.dumps(manifest))
            with self.assertRaisesRegex(ValueError, "unsafe"):
                packaging["provider_files"](root, "kind")

    def test_archive_preserves_executables_and_rejects_links_without_partial_output(
        self,
    ):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "binary"
            binary.write_bytes(b"executable")
            binary.chmod(0o755)
            (root / "nested").mkdir()
            output = root / "nested/../platform.tar.gz"
            packaging["archive"](
                output, {"build/binary": binary, ".lampyr-platform": b"version\n"}
            )
            with tarfile.open(output) as archive:
                self.assertEqual(
                    archive.extractfile("build/binary").read(), b"executable"
                )
                self.assertEqual(archive.getmember("build/binary").mode, 0o755)
            self.assertEqual(
                output.with_name(output.name + ".sha256").read_text().split()[0],
                hashlib.sha256(output.read_bytes()).hexdigest(),
            )
            link = root / "link"
            link.symlink_to(binary)
            rejected = root / "rejected.tar.gz"
            with self.assertRaisesRegex(ValueError, "regular file"):
                packaging["archive"](rejected, {"build/binary": link})
            self.assertFalse(rejected.exists())
            self.assertEqual(list(root.glob(".package-*")), [])


if __name__ == "__main__":
    unittest.main()
