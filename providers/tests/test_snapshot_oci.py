import contextlib
import errno
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import runpy
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest import mock


PROVIDERS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(PROVIDERS))
import snapshot_oci as oci

spec = importlib.util.spec_from_file_location("provider_package", PROVIDERS / "package.py")
package = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package)
platform_package = runpy.run_path(str(PROVIDERS.parent / "scripts/package-platform.py"))


class SnapshotTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.intel = self.variant("intel", "GenuineIntel")
        self.amd = self.variant("amd", "AuthenticAMD")
        self.arm = self.variant(
            "arm", "0x61", architecture="arm64", hypervisor="hvf"
        )
        self.output = self.root / "assembled"

    def variant(self, name, vendor, architecture="amd64", hypervisor="kvm", memory_layers=2,
                sizes=None, layers=None):
        """A provider artifact whose snapshot has `memory_layers` memory layers, declared with
        `sizes` instead of their own sizes when given; `layers` rewrites its manifest's layers."""
        directory = self.root / name
        directory.mkdir()
        manifest = {
            "id": "test", "version": "1.2.3", "protocolVersion": 1,
            "runtimeVersion": oci.RUNTIME_VERSION, "memoryMiB": 256, "operations": {"describe": []},
        }
        for field, filename in (("rootfs", "rootfs.cpio"), ("program", "provider.py"), ("kernel", "kernel")):
            path = directory / filename
            path.write_bytes(field.encode())
            manifest[field] = package.file_digest(path)
        snapshot = directory / "snapshot"
        snapshot.mkdir()
        (snapshot / "blobs/sha256").mkdir(parents=True)
        native_arch = {"amd64": "x86_64", "arm64": "aarch64"}[architecture]
        config = oci.write_blob(snapshot, oci.CONFIG, oci.json_bytes({
            "arch": native_arch, "hypervisor": hypervisor, "cpu_vendor": vendor,
        }))
        # Memory in layers, as hyperlight-host 0.19 writes it: two by default, so more than one
        # memory layer is exercised, then transport.
        memory = [oci.write_blob(snapshot, oci.MEMORY, vendor.encode())]
        memory += [oci.write_blob(snapshot, oci.MEMORY, f"{vendor} memory layer {i}".encode())
                   for i in range(1, memory_layers)]
        for layer, size in zip(memory, sizes or []):
            layer["size"] = size
        transport = oci.write_blob(snapshot, oci.TRANSPORT, b"transport")
        image = oci.write_blob(snapshot, oci.MANIFEST, oci.json_bytes({
            "schemaVersion": 2, "mediaType": oci.MANIFEST, "artifactType": oci.CONFIG,
            "config": config, "layers": (layers or list)([*memory, transport]),
        }))
        image["annotations"] = {
            oci.ARCH: native_arch, oci.HYPERVISOR: hypervisor, oci.CPU_VENDOR: vendor,
            oci.REF_NAME: f"{oci.RUNTIME_VERSION}-test-snapshot-key",
        }
        (snapshot / "index.json").write_bytes(oci.json_bytes({
            "schemaVersion": 2, "mediaType": oci.INDEX, "manifests": [image],
        }))
        (snapshot / "oci-layout").write_text('{"imageLayoutVersion":"1.0.0"}')
        manifest["snapshotIndex"] = package.file_digest(snapshot / "index.json")
        manifest["snapshotIndex"]["path"] = "snapshot/index.json"
        (directory / "manifest.json").write_text(json.dumps(manifest))
        return directory

    def provider_source(self, root, name, prepared):
        """Writes providers/NAME as the checkout the prepared artifact was built from."""
        manifest = oci.read_json(prepared / "manifest.json")
        source = root / "providers" / name
        source.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(prepared / manifest["program"]["path"], source / "provider.py")
        declared = {key: manifest[key] for key in
                    ("id", "version", "protocolVersion", "runtimeVersion", "memoryMiB", "operations")}
        (source / "artifact.json").write_text(json.dumps(declared))

    def assemble(self, variants=None):
        return oci.assemble(self.intel, [self.intel, self.amd, self.arm] if variants is None else variants, self.output)

    def test_multi_platform_index_is_reproducible_and_digest_pinned(self):
        digest = self.assemble()
        second = self.root / "second"
        self.assertEqual(digest, oci.assemble(self.intel, [self.arm, self.amd, self.intel], second))
        files = lambda root: {p.relative_to(root): p.read_bytes() for p in root.rglob("*") if p.is_file()}
        self.assertEqual(files(self.output), files(second))
        index = oci.read_json(self.output / "index.json")
        self.assertEqual(len(index["manifests"]), 3)
        self.assertEqual({d["platform"]["architecture"] for d in index["manifests"]}, {"amd64", "arm64"})
        self.assertIn(
            ("darwin", "arm64"),
            {
                (descriptor["platform"]["os"], descriptor["platform"]["architecture"])
                for descriptor in index["manifests"]
            },
        )
        manifest, checked = oci.layout_files(self.output)
        self.assertNotIn("snapshot", manifest)
        self.assertEqual(manifest["snapshotIndex"]["sha256"], digest[7:])
        self.assertEqual((self.output / "blobs/sha256" / digest[7:]).read_bytes(), (self.output / "index.json").read_bytes())
        self.assertIn("rootfs.cpio", checked)
        self.assertIn("kernel", checked)
        self.assertNotIn("snapshot-compatibility.json", checked)
        for item in index["manifests"]:
            image = oci.read_json(self.output / "blobs/sha256" / item["digest"][7:])
            self.assertEqual(image["artifactType"], oci.CONFIG)
            config = oci.read_json(self.output / "blobs/sha256" / image["config"]["digest"][7:])
            self.assertEqual(config["cpu_vendor"], item["annotations"][oci.CPU_VENDOR])
            self.assertEqual((self.output / "blobs/sha256" / image["layers"][0]["digest"][7:]).read_bytes(),
                             config["cpu_vendor"].encode())
            self.assertEqual((self.output / "blobs/sha256" / image["layers"][1]["digest"][7:]).read_bytes(),
                             f"{config['cpu_vendor']} memory layer 1".encode())
            self.assertEqual((self.output / "blobs/sha256" / image["layers"][-1]["digest"][7:]).read_bytes(), b"transport")

    def test_published_index_can_bind_to_platform_specific_cold_artifact(self):
        digest = self.assemble()
        bound = self.root / "bound-arm"
        oci.bind(
            self.arm,
            self.output,
            "ghcr.io/hyperlight-dev/lampyr/provider-test-snapshots",
            bound,
        )
        manifest, files = oci.cold_files(bound)
        self.assertEqual(
            manifest["snapshotRepository"],
            "ghcr.io/hyperlight-dev/lampyr/provider-test-snapshots",
        )
        self.assertEqual(manifest["snapshotIndex"]["sha256"], digest[7:])
        self.assertEqual(manifest["snapshotIndex"]["path"], "index.json")
        self.assertEqual(
            files["rootfs.cpio"].read_bytes(),
            (self.arm / "rootfs.cpio").read_bytes(),
        )
        self.assertTrue((bound / "index.json").is_file())
        bound_manifest, bound_files = oci.layout_files(bound)
        self.assertEqual(bound_manifest["snapshotIndex"]["sha256"], digest[7:])
        self.assertTrue(any(name.startswith("blobs/") for name in bound_files))

    def test_binding_shares_blobs_rather_than_copying_them(self):
        """The release binds every provider's layout to its darwin artifact on one runner; a
        copy of each blob ran it out of disk, so a bound blob is the layout's own file."""
        self.assemble()
        bound = self.root / "bound-shared"
        oci.bind(
            self.arm,
            self.output,
            "ghcr.io/hyperlight-dev/lampyr/provider-test-snapshots",
            bound,
        )
        _, layout = oci.layout_files(self.output)
        _, bound_files = oci.layout_files(bound)
        blobs = [name for name in bound_files if name.startswith("blobs/")]
        self.assertTrue(blobs)
        for name in blobs:
            self.assertTrue(
                os.path.samefile(bound_files[name], layout[name]),
                f"{name} was copied",
            )

    def test_binding_copies_blobs_across_filesystems_and_keeps_other_errors(self):
        """Where the layout and the artifact are on different filesystems a link fails with
        EXDEV, and the blob is copied instead; any other failure, such as a full disk, is the
        bind's failure rather than a reason to copy."""
        from unittest import mock

        self.assemble()
        _, layout = oci.layout_files(self.output)
        before = {name: path.read_bytes() for name, path in layout.items()}

        def cross_device(source, destination):
            raise OSError(errno.EXDEV, os.strerror(errno.EXDEV))

        bound = self.root / "bound-copied"
        with mock.patch.object(oci.os, "link", side_effect=cross_device):
            oci.bind(
                self.arm,
                self.output,
                "ghcr.io/hyperlight-dev/lampyr/provider-test-snapshots",
                bound,
            )
        _, bound_files = oci.layout_files(bound)
        blobs = [name for name in bound_files if name.startswith("blobs/")]
        self.assertTrue(blobs)
        for name in blobs:
            self.assertFalse(os.path.samefile(bound_files[name], layout[name]), f"{name} was linked")
            self.assertEqual(bound_files[name].read_bytes(), layout[name].read_bytes())
        self.assertEqual({name: path.read_bytes() for name, path in layout.items()}, before)

        def full_disk(source, destination):
            raise OSError(errno.ENOSPC, os.strerror(errno.ENOSPC))

        refused = self.root / "bound-full"
        with mock.patch.object(oci.os, "link", side_effect=full_disk), \
                mock.patch.object(oci.shutil, "copyfile", wraps=shutil.copyfile) as copied:
            with self.assertRaises(OSError) as raised:
                oci.bind(
                    self.arm,
                    self.output,
                    "ghcr.io/hyperlight-dev/lampyr/provider-test-snapshots",
                    refused,
                )
        self.assertEqual(raised.exception.errno, errno.ENOSPC)
        self.assertFalse(
            any(str(call.args[0]).startswith(str(self.output / "blobs")) for call in copied.call_args_list),
            "a blob was copied after a link failed for want of space",
        )
        self.assertFalse(refused.exists())
        self.assertEqual({name: path.read_bytes() for name, path in layout.items()}, before)

    def test_a_cold_file_named_as_a_layout_blob_cannot_bind(self):
        """A bound blob is the layout's own file, so a cold file that took its path would be
        written into the layout: the bind is refused before anything is linked, and the
        layout is left as it was."""
        self.assemble()
        _, layout = oci.layout_files(self.output)
        blob = next(name for name in layout if name.startswith("blobs/"))
        before = {name: path.read_bytes() for name, path in layout.items()}
        cold = self.root / "cold-collides"
        shutil.copytree(self.arm, cold)
        manifest = oci.read_json(cold / "manifest.json")
        (cold / blob).parent.mkdir(parents=True)
        (cold / manifest["kernel"]["path"]).rename(cold / blob)
        manifest["kernel"]["path"] = blob
        (cold / "manifest.json").write_text(json.dumps(manifest))
        with self.assertRaisesRegex(ValueError, "collides with the snapshot layout"):
            oci.bind(
                cold,
                self.output,
                "ghcr.io/hyperlight-dev/lampyr/provider-test-snapshots",
                self.root / "bound-collides",
            )
        self.assertFalse((self.root / "bound-collides").exists())
        self.assertEqual({name: path.read_bytes() for name, path in layout.items()}, before)

    def test_assembly_and_binding_secure_outputs_under_group_umask(self):
        self.output = self.root / "assembled-parent/assembled"
        bound = self.root / "bound-parent/bound"
        previous = os.umask(0o002)
        try:
            self.assemble()
            oci.bind(
                self.arm,
                self.output,
                "ghcr.io/hyperlight-dev/lampyr/provider-test-snapshots",
                bound,
            )
        finally:
            os.umask(previous)

        for output in (self.output, bound):
            self.assertEqual(output.parent.lstat().st_mode & 0o077, 0)
            for path in (output, *output.rglob("*")):
                self.assertFalse(path.is_symlink(), path)
                self.assertEqual(path.lstat().st_mode & 0o022, 0, path)

    def test_assembly_rejects_an_unsafe_existing_output_ancestor(self):
        parent = self.root / "shared"
        parent.mkdir()
        parent.chmod(0o777)
        output = parent / "assembled"

        with self.assertRaisesRegex(ValueError, "writable by other users"):
            oci.assemble(self.intel, [self.intel], output)

        self.assertFalse(output.exists())
        self.assertEqual(list(parent.glob(".snapshot-oci-*")), [])

    def test_empty_index_keeps_cold_assets(self):
        self.assemble([])
        manifest, files = oci.layout_files(self.output)
        self.assertEqual(oci.read_json(self.output / "index.json")["manifests"], [])
        self.assertIn("rootfs.cpio", files)
        self.assertIn("kernel", files)
        self.assertNotIn("snapshot", manifest)

    def test_rejects_mismatched_logical_identity_but_allows_platform_cold_assets(self):
        original = (self.amd / "manifest.json").read_bytes()
        for field in ("version", "runtimeVersion", "memoryMiB", "program", "operations"):
            with self.subTest(field=field):
                manifest = json.loads(original)
                if field == "program":
                    path = self.amd / manifest[field]["path"]
                    previous = path.read_bytes()
                    path.write_bytes(b"other")
                    manifest[field] = package.file_digest(path)
                else:
                    manifest[field] = 128 if field == "memoryMiB" else ({} if field == "operations" else "other")
                (self.amd / "manifest.json").write_text(json.dumps(manifest))
                error = {"runtimeVersion": "incompatible provider runtime",
                         "operations": "invalid provider operations"}.get(field, "differ")
                with self.assertRaisesRegex(ValueError, error):
                    self.assemble()
                self.assertFalse(self.output.exists())
                self.assertEqual(list(self.root.glob(".snapshot-oci-*")), [])
                if field == "program":
                    path.write_bytes(previous)
                (self.amd / "manifest.json").write_bytes(original)

        for field in ("rootfs", "kernel"):
            with self.subTest(field=field):
                manifest = json.loads(original)
                path = self.amd / manifest[field]["path"]
                previous = path.read_bytes()
                path.write_bytes(b"other")
                manifest[field] = package.file_digest(path)
                manifest["coldCompatibility"] = {
                    "os": "macos",
                    "architecture": "aarch64",
                }
                (self.amd / "manifest.json").write_text(json.dumps(manifest))
                self.assemble()
                self.assertTrue(self.output.is_dir())
                shutil.rmtree(self.output)
                path.write_bytes(previous)
                (self.amd / "manifest.json").write_bytes(original)

    def test_rejects_corruption_duplicate_compatibility_and_limits(self):
        with self.assertRaisesRegex(ValueError, "duplicate"):
            self.assemble([self.intel, self.intel])
        with self.assertRaisesRegex(ValueError, "128"):
            self.assemble([self.intel] * 129)
        next((self.amd / "snapshot/blobs/sha256").iterdir()).write_bytes(b"modified")
        with self.assertRaisesRegex(ValueError, "digest"):
            self.assemble()
        index = oci.read_json(self.intel / "snapshot/index.json")
        index["manifests"][0]["annotations"].pop(oci.CPU_VENDOR)
        (self.intel / "snapshot/index.json").write_bytes(oci.json_bytes(index))
        manifest = oci.read_json(self.intel / "manifest.json")
        manifest["snapshotIndex"]["sha256"] = oci.file_hash(self.intel / "snapshot/index.json")
        (self.intel / "manifest.json").write_text(json.dumps(manifest))
        with self.assertRaisesRegex(ValueError, "compatibility"):
            self.assemble([self.intel])
        self.assertFalse(self.output.exists())

    def test_rejects_symlink_traversal_and_output_overwrite(self):
        manifest = oci.read_json(self.intel / "manifest.json")
        manifest["rootfs"]["path"] = "../outside"
        (self.intel / "manifest.json").write_text(json.dumps(manifest))
        with self.assertRaisesRegex(ValueError, "unsafe"):
            self.assemble([])
        manifest["rootfs"]["path"] = "rootfs.cpio"
        (self.intel / "manifest.json").write_text(json.dumps(manifest))
        path = self.intel / "rootfs.cpio"
        path.unlink()
        path.symlink_to(self.amd / "rootfs.cpio")
        with self.assertRaisesRegex(ValueError, "regular"):
            self.assemble([])
        path.unlink()
        path.write_bytes(b"rootfs")
        self.output.mkdir()
        with self.assertRaisesRegex(ValueError, "already exist"):
            self.assemble([])

    def fake_oras(self, arguments, timeout=None, capture=False):
        self.calls.append(arguments)
        if "--descriptor" in arguments:
            digest = arguments[-1].rsplit("@", 1)[1]
            blob = self.output / "blobs/sha256" / digest[7:]
            if not blob.exists() or digest in getattr(self, "missing", ()):
                raise ValueError("ORAS publication failed")
            return json.dumps({"digest": digest, "size": blob.stat().st_size}).encode()
        if arguments[0] == "manifest":
            Path(arguments[-1]).write_bytes((self.output / "index.json").read_bytes())
        return None

    def test_explicit_publish_preserves_digest_and_uses_standard_auth(self):
        digest = self.assemble()
        self.calls = []
        remote = self.root / "remote"
        with mock.patch.object(oci, "oras", side_effect=self.fake_oras):
            reference = oci.publish(self.output, "registry.example/team/snapshots", "release-1", remote)
        self.assertEqual(reference, f"registry.example/team/snapshots@{digest}")
        self.assertEqual(self.calls[0][:2], ["cp", "--from-oci-layout"])
        self.assertTrue(self.calls[0][2].endswith("@" + digest))
        self.assertEqual(self.calls[0][-1], "registry.example/team/snapshots:release-1")
        self.assertEqual(self.calls[1][2], reference)
        self.assertEqual(
            set(p.name for p in remote.iterdir()),
            {
                "manifest.json",
                "rootfs.cpio",
                "kernel",
                "provider.py",
                "index.json",
                "oci-layout",
                "blobs",
            },
        )
        published = oci.read_json(remote / "manifest.json")
        self.assertEqual(
            published.pop("snapshotRepository"),
            "registry.example/team/snapshots",
        )
        self.assertEqual(published, oci.read_json(self.output / "manifest.json"))
        oci.layout_files(remote)
        with mock.patch.object(oci.subprocess, "run") as run:
            oci.oras(["manifest", "fetch", reference])
        self.assertEqual(run.call_args.args[0], ["oras", "manifest", "fetch", reference])
        self.assertEqual(run.call_args.kwargs["stderr"], subprocess.DEVNULL)
        self.assertNotIn("env", run.call_args.kwargs)

    def test_publish_fits_one_frozen_memory_copy_budget(self):
        digest = self.assemble()
        self.calls = []
        remote = self.root / "remote"
        memory = {}
        for item in oci.read_json(self.output / "index.json")["manifests"]:
            image = oci.read_json(self.output / "blobs/sha256" / item["digest"][7:])
            for layer in image["layers"]:
                if layer["mediaType"] == oci.MEMORY:
                    memory[layer["digest"][7:]] = layer["size"]
        remaining = sum(memory.values())
        copyfile = shutil.copyfile

        def bounded_copy(source, destination, **kwargs):
            nonlocal remaining
            size = memory.get(Path(source).name, 0)
            if size > remaining:
                raise OSError(errno.ENOSPC, "No space left on device", str(destination))
            remaining -= size
            return copyfile(source, destination, **kwargs)

        previous = os.umask(0o002)
        try:
            with mock.patch.object(shutil, "copyfile", side_effect=bounded_copy), \
                 mock.patch.object(oci, "oras", side_effect=self.fake_oras):
                reference = oci.publish(
                    self.output, "registry.example/team/snapshots", "v1", remote
                )
        finally:
            os.umask(previous)

        self.assertEqual(reference, f"registry.example/team/snapshots@{digest}")
        self.assertEqual(remaining, 0)
        _, source_files = oci.layout_files(self.output)
        manifest, published_files = oci.layout_files(remote)
        self.assertEqual(manifest["snapshotRepository"], "registry.example/team/snapshots")
        for name, source in source_files.items():
            if name != "manifest.json":
                self.assertEqual(source.read_bytes(), published_files[name].read_bytes())
        for path in (remote, *remote.rglob("*")):
            self.assertFalse(path.is_symlink(), path)
            self.assertEqual(path.lstat().st_mode & 0o022, 0, path)
        self.assertEqual(list(self.root.glob(".snapshot-oci-*")), [])

    def test_package_versions_must_be_strings(self):
        for value in (123, None, ["1.2.3"], "1.2"):
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, "invalid package version"):
                platform_package["version_name"](value)
        self.assertEqual(platform_package["version_name"]("1.2.3-preview.1"), "1.2.3-preview.1")

    def test_cli_accepts_inputs_reached_through_a_symlinked_directory(self):
        link = self.root.parent / (self.root.name + "-link")
        link.symlink_to(self.root, target_is_directory=True)
        self.addCleanup(link.unlink)
        subprocess.run([
            sys.executable, str(PROVIDERS / "snapshot_oci.py"), "assemble",
            "--artifact", str(link / "intel"), "--variant", str(link / "intel"),
            "--output", str(link / "assembled"),
        ], check=True, capture_output=True)
        oci.layout_files(self.output)
        # The output itself is still never followed through a symlink.
        dangling = self.root / "dangling-output"
        dangling.symlink_to(self.root / "elsewhere")
        failed = subprocess.run([
            sys.executable, str(PROVIDERS / "snapshot_oci.py"), "assemble",
            "--artifact", str(link / "intel"), "--variant", str(link / "intel"),
            "--output", str(link / "dangling-output"),
        ], capture_output=True)
        self.assertNotEqual(failed.returncode, 0)
        self.assertIn(b"already exist", failed.stderr)
        self.assertFalse((self.root / "elsewhere").exists())

    def test_output_parents_resolve_parent_traversal_after_symlinks(self):
        physical = self.root / "physical/checkout"
        physical.mkdir(parents=True)
        (self.root / "logical").mkdir()
        (self.root / "logical/checkout-link").symlink_to(physical, target_is_directory=True)
        resolved = oci.resolved_parent(self.root / "logical/checkout-link/../dist")
        self.assertEqual(resolved, (self.root / "physical").resolve() / "dist")
        # A trailing separator must not turn a symlinked last component into its target.
        dangling = self.root / "logical/dangling"
        dangling.symlink_to(self.root / "elsewhere")
        self.assertEqual(oci.resolved_parent(str(dangling) + os.sep),
                         (self.root / "logical").resolve() / "dangling")
        self.assertEqual(oci.resolved_parent(self.root / "logical/missing/.."),
                         (self.root / "logical").resolve())

    def test_publish_checks_every_published_manifest_and_blob(self):
        digest = self.assemble()
        index = oci.read_json(self.output / "blobs/sha256" / digest[7:])
        children = [item["digest"] for item in index["manifests"]]
        image = oci.read_json(self.output / "blobs/sha256" / children[0][7:])
        blobs = [image["config"]["digest"], *(layer["digest"] for layer in image["layers"])]
        self.calls = []
        remote = self.root / "remote"
        with mock.patch.object(oci, "oras", side_effect=self.fake_oras):
            oci.publish(self.output, "registry.example/team/snapshots", "release-1", remote)
        checked = {call[-1].rsplit("@", 1)[1] for call in self.calls if "--descriptor" in call}
        self.assertTrue({*children, *blobs} <= checked)
        for missing in (children[0], blobs[-1]):
            shutil.rmtree(remote)
            self.missing = {missing}
            self.calls = []
            with self.subTest(missing=missing), mock.patch.object(oci, "oras", side_effect=self.fake_oras):
                with self.assertRaisesRegex(ValueError, "ORAS publication failed"):
                    oci.publish(self.output, "registry.example/team/snapshots", "release-1", remote)
            self.assertFalse(remote.exists())
            remote.mkdir()
        self.missing = set()
        for reply in (b"not json", json.dumps({"digest": blobs[-1], "size": 1}).encode()):
            shutil.rmtree(remote)
            def wrong(arguments, timeout=None, capture=False, reply=reply):
                if "--descriptor" in arguments and arguments[-1].endswith(blobs[-1]):
                    return reply
                return self.fake_oras(arguments, timeout, capture)
            with self.subTest(reply=reply), mock.patch.object(oci, "oras", side_effect=wrong):
                with self.assertRaisesRegex(ValueError, "unreadable descriptor|missing or differs"):
                    oci.publish(self.output, "registry.example/team/snapshots", "release-1", remote)
            self.assertFalse(remote.exists())
            remote.mkdir()

    def test_oras_timeout_scales_and_diagnostics_are_opt_in(self):
        self.assemble()
        self.calls = []
        remote = self.root / "remote"
        timeouts = []
        def record(arguments, timeout=oci.ORAS_TIMEOUT, capture=False):
            timeouts.append((arguments[0], timeout))
            return self.fake_oras(arguments, timeout, capture)
        with mock.patch.object(oci, "ORAS_MIN_RATE", 1), mock.patch.object(oci, "ORAS_TIMEOUT", 1), \
                mock.patch.object(oci, "oras", side_effect=record):
            oci.publish(self.output, "registry.example/team/snapshots", "release-1", remote)
        timeouts = dict(timeouts)
        # One second per byte of the frozen graph (the index blob is added during staging).
        self.assertGreater(timeouts["cp"], 1)
        self.assertGreaterEqual(timeouts["cp"],
                                sum(path.stat().st_size for path in (self.output / "blobs/sha256").iterdir()))
        for verbose, stderr in (("1", None), ("", subprocess.DEVNULL)):
            with self.subTest(verbose=verbose), mock.patch.dict(os.environ, {"LAMPYR_ORAS_VERBOSE": verbose}), \
                    mock.patch.object(oci.subprocess, "run") as run:
                oci.oras(["manifest", "fetch", "registry.example/repo@sha256:" + "a" * 64])
            self.assertIs(run.call_args.kwargs["stderr"], stderr)
        with mock.patch.object(oci.subprocess, "run", side_effect=subprocess.TimeoutExpired(["oras"], 5)):
            with self.assertRaisesRegex(ValueError, "timed out after 5 seconds"):
                oci.oras(["cp"], timeout=5)

    def test_publish_errors_never_emit_remote_artifact_or_credentials(self):
        self.assemble()
        remote = self.root / "remote"
        with mock.patch.object(oci.subprocess, "run", side_effect=subprocess.CalledProcessError(
            1, ["oras"], stderr="password=secret",
        )):
            with self.assertRaisesRegex(ValueError, "ORAS publication failed") as raised:
                oci.publish(self.output, "registry.example/team/snapshots", "v1", remote)
        self.assertNotIn("secret", str(raised.exception))
        self.assertFalse(remote.exists())
        with mock.patch.object(oci, "oras") as run:
            with self.assertRaisesRegex(FileNotFoundError, "published-index"):
                oci.publish(self.output, "registry.example/team/snapshots", "v1", remote)
        self.assertFalse(remote.exists())
        self.assertEqual(list(self.root.glob(".snapshot-oci-*")), [])
        for repository in ("https://registry.example/repo", "user:secret@registry.example/repo",
                           "registry.example/repo:tag", "registry.example/repo@sha256:abc", "repo", "registry.example/../repo"):
            with self.subTest(repository=repository), mock.patch.object(oci, "oras") as run:
                with self.assertRaises(ValueError):
                    oci.publish(self.output, repository, "v1", remote)
                run.assert_not_called()
        with mock.patch.object(oci, "oras") as run:
            with self.assertRaisesRegex(ValueError, "tag"):
                oci.publish(self.output, "registry.example/repo", "bad tag", remote)
            remote.mkdir()
            with self.assertRaisesRegex(ValueError, "already exist"):
                oci.publish(self.output, "registry.example/repo", "v1", remote)
            run.assert_not_called()

    def test_published_digest_mismatch_and_corrupt_input_fail_closed(self):
        self.assemble()
        remote = self.root / "remote"
        def mismatch(arguments, timeout=None, capture=False):
            if arguments[0] == "manifest":
                Path(arguments[-1]).write_bytes(b"{}")
        with mock.patch.object(oci, "oras", side_effect=mismatch):
            with self.assertRaisesRegex(ValueError, "digest mismatch"):
                oci.publish(self.output, "registry.example/team/snapshots", "v1", remote)
        self.assertFalse(remote.exists())
        manifest = oci.read_json(self.output / "index.json")["manifests"][0]
        (self.output / "blobs/sha256" / manifest["digest"][7:]).write_bytes(b"{}")
        with mock.patch.object(oci, "oras") as run:
            with self.assertRaisesRegex(ValueError, "digest"):
                oci.publish(self.output, "registry.example/team/snapshots", "v1", remote)
            run.assert_not_called()

    def test_release_archive_keeps_local_blobs_or_remote_pin_and_cold_fallback(self):
        digest = self.assemble()
        directory = self.root / "release/providers/test/build/rootfs"
        directory.parent.mkdir(parents=True)
        shutil.copytree(self.output, directory)
        version, files = platform_package["provider_files"](self.root / "release", "test")
        self.assertEqual(version, "1.2.3")
        self.assertIn("index.json", files)
        self.assertIn("blobs/sha256/" + digest[7:], files)
        self.assertTrue(any(name.startswith("blobs/") for name in files))
        archive = self.root / "release.tar.gz"
        with contextlib.redirect_stdout(io.StringIO()):
            platform_package["archive"](archive, files)
        with tarfile.open(archive) as bundled:
            self.assertIn("index.json", bundled.getnames())
            self.assertIn("rootfs.cpio", bundled.getnames())
            self.assertEqual(bundled.extractfile("blobs/sha256/" + digest[7:]).read(),
                             (directory / "index.json").read_bytes())
        (directory / "index.json").unlink()
        _, files = platform_package["provider_files"](self.root / "release", "test")
        self.assertEqual(set(files), {"manifest.json", "rootfs.cpio", "kernel", "provider.py"})
        self.assertIn("snapshotIndex", json.loads(files["manifest.json"].read_bytes()))

    def test_cold_asset_limits_match_loader_and_program_is_utf8(self):
        for field, limit in (("rootfs", 2 << 30), ("program", oci.JSON_LIMIT),
                             ("kernel", 64 << 20)):
            with self.subTest(field=field):
                manifest = oci.read_json(self.intel / "manifest.json")
                path = self.intel / manifest[field]["path"]
                original = path.read_bytes()
                with path.open("wb") as content:
                    content.truncate(limit + 1)
                with self.assertRaisesRegex(ValueError, "size mismatch"):
                    oci.assemble(self.intel, [], self.output)
                self.assertFalse(self.output.exists())
                path.write_bytes(original)
        program = self.intel / "provider.py"
        program.write_bytes(b"x" * oci.JSON_LIMIT)
        manifest["program"] = package.file_digest(program)
        (self.intel / "manifest.json").write_text(json.dumps(manifest))
        oci.cold_files(self.intel)
        program.write_bytes(b"\xff")
        manifest["program"] = package.file_digest(program)
        (self.intel / "manifest.json").write_text(json.dumps(manifest))
        with self.assertRaises(UnicodeDecodeError):
            oci.cold_files(self.intel)

    def test_variant_index_replacement_after_validation_is_rejected(self):
        validate = oci.layout_files
        def replace(directory):
            result = validate(directory)
            if directory == self.intel:
                (self.intel / "snapshot/index.json").write_bytes(
                    oci.json_bytes({"schemaVersion": 2, "mediaType": oci.INDEX, "manifests": []}))
            return result
        with mock.patch.object(oci, "layout_files", side_effect=replace):
            with self.assertRaisesRegex(ValueError, "digest mismatch"):
                self.assemble([self.intel])
        self.assertFalse(self.output.exists())

    def test_publication_rejects_replaced_manifest_and_index_before_transport(self):
        self.assemble()
        original_manifest = (self.output / "manifest.json").read_bytes()
        original_index = (self.output / "index.json").read_bytes()
        remote = self.root / "remote"
        validate = oci.layout_files
        for field in ("manifest", "index"):
            with self.subTest(field=field):
                validations = 0
                def replace(directory):
                    nonlocal validations
                    result = validate(directory)
                    if directory == self.output:
                        validations += 1
                        if validations == 1:
                            if field == "manifest":
                                manifest = json.loads(original_manifest)
                                manifest["version"] = "9.9.9"
                                (directory / "manifest.json").write_text(json.dumps(manifest))
                            else:
                                (directory / "index.json").write_bytes(b"{}")
                    return result
                with mock.patch.object(oci, "layout_files", side_effect=replace), \
                     mock.patch.object(oci, "oras") as transport:
                    with self.assertRaisesRegex(ValueError, "changed|mismatch"):
                        oci.publish(self.output, "registry.example/repo", "v1", remote)
                    transport.assert_not_called()
                self.assertFalse(remote.exists())
                (self.output / "manifest.json").write_bytes(original_manifest)
                (self.output / "index.json").write_bytes(original_index)

    def test_json_reader_rejects_non_utf8_duplicate_keys_and_constants(self):
        path = self.root / "invalid.json"
        cases = [
            b'{"schemaVersion":2,"schemaVersion":2}',
            b'{"config":{"arch":"x86_64","arch":"x86_64"}}',
            b'{"value":NaN}', b'{"value":Infinity}', b'{"value":-Infinity}',
            '{"schemaVersion":2}'.encode("utf-16"),
            '{"schemaVersion":2}'.encode("utf-32"),
            b'{"extra":"\\ud800"}', b'{"\\udfff":"value"}',
            b'{"nested":[{"value":"\\udc00"}]}',
        ]
        for content in cases:
            with self.subTest(content=content):
                path.write_bytes(content)
                with self.assertRaises(ValueError):
                    oci.read_json(path, oci.file_hash(path))
        path.write_bytes(b'{"schemaVersion":2,"label":"\\u00e9t\\u00e9"}')
        self.assertEqual(oci.read_json(path)["label"], "\u00e9t\u00e9")
        path.write_bytes(b'{"label":"\\ud83d\\ude00"}')
        self.assertEqual(oci.read_json(path)["label"], "\U0001f600")

    def test_native_surrogate_annotations_fail_before_output(self):
        self.assemble()
        directory = self.root / "surrogate"
        shutil.copytree(self.output, directory)
        index = oci.read_json(directory / "index.json")
        index["manifests"][0]["annotations"]["extra"] = "\ud800"
        (directory / "index.json").write_bytes(oci.json_bytes(index))
        manifest = oci.read_json(directory / "manifest.json")
        manifest["snapshotIndex"]["sha256"] = oci.file_hash(directory / "index.json")
        (directory / "manifest.json").write_bytes(oci.json_bytes(manifest))
        with mock.patch.object(oci, "oras") as transport:
            with self.assertRaises(UnicodeEncodeError):
                oci.publish(directory, "registry.example/repo", "v1", self.root / "remote")
            transport.assert_not_called()
        with self.assertRaises(UnicodeEncodeError):
            oci.assemble(self.intel, [directory], self.root / "invalid-output")

    def test_provider_metadata_matches_loader_types_and_constraints(self):
        original = (self.intel / "manifest.json").read_bytes()
        cases = [
            ("memoryMiB", 256.0), ("memoryMiB", True), ("memoryMiB", 127),
            ("memoryMiB", 4097), ("protocolVersion", 1.0), ("protocolVersion", 2),
            ("id", "unsafe_id"), ("id", 7), ("version", ""), ("version", 1),
            ("runtimeVersion", "0.0.0"), ("unknownField", "extra"),
            ("environment", {"BAD-NAME": "value"}), ("environment", {"X": 1}),
            ("networkHosts", ["UPPER.example"]), ("networkHosts", [False]),
            ("sourceKinds", [{"apiVersion": "apps/v1", "kind": "Deployment"}]),
            ("operations", {"describe": ["state"]}),
            ("operations", {"describe": [], "health": []}),
            ("operations", {"describe": [], "init": ["provider-manifests"]}),
            ("operations", {"describe": [], "apply": ["unknown"]}),
            ("operations", {"describe": [], "source-create": []}),
        ]
        for field, value in cases:
            with self.subTest(field=field, value=value):
                manifest = json.loads(original)
                manifest[field] = value
                (self.intel / "manifest.json").write_bytes(oci.json_bytes(manifest))
                with self.assertRaises(ValueError):
                    oci.assemble(self.intel, [], self.output)
                self.assertFalse(self.output.exists())
                with mock.patch.object(oci, "oras") as transport:
                    with self.assertRaises(ValueError):
                        oci.publish(self.intel, "registry.example/repo", "v1", self.root / "remote")
                    transport.assert_not_called()
        (self.intel / "manifest.json").write_bytes(original)
        for memory in (128, 4096):
            manifest = json.loads(original)
            manifest["memoryMiB"] = memory
            oci.checked_metadata(manifest)
        for name in ("kind", "k3d", "azure"):
            manifest = oci.read_json(PROVIDERS / name / "artifact.json")
            fixture = json.loads(original)
            fixture.update(manifest)
            oci.checked_metadata(fixture)

    def test_combined_platform_limit_is_preflighted_before_provider_staging(self):
        root = self.root / "release"
        (root / "cli").mkdir(parents=True)
        (root / "cli/Cargo.toml").write_text('[package]\nversion = "1.2.3"\n')
        binary = root / "ly"
        binary.write_bytes(b"fixture")
        first = self.root / "first"
        second = self.root / "second"
        first.write_bytes(b"123456")
        second.write_bytes(b"123456")
        globals_ = platform_package["main"].__globals__
        def provider(root, name, artifacts=None):
            return "1.2.3", {"rootfs.cpio": first if name == "kind" else second}
        freeze = mock.Mock()
        overrides = {"linux_amd64": mock.Mock(), "platform_base_files": mock.Mock(return_value={"LICENSE": b"x"}),
                     "provider_inputs": lambda root, name, artifacts: ({}, provider(root, name, artifacts)),
                     "freeze_provider": freeze,
                     "MAX_TOTAL": 12}
        with mock.patch.dict(globals_, overrides), \
             mock.patch.object(sys, "argv", ["package-platform.py", "--root", str(root),
                 "--binary", str(binary), "--output", str(self.root / "dist"),
                 "--source-commit", "0" * 40]):
            platform_package["archive_inputs"]({"rootfs.cpio": first})
            platform_package["archive_inputs"]({"rootfs.cpio": second})
            with self.assertRaisesRegex(ValueError, "total size"):
                platform_package["main"]()
        freeze.assert_not_called()
        self.assertFalse((self.root / "dist").exists())

    def test_failed_repackaging_invalidates_alias_completion_marker(self):
        root = self.root / "release"
        (root / "cli").mkdir(parents=True)
        (root / "cli/Cargo.toml").write_text('[package]\nversion = "1.2.3"\n')
        destination = self.root / "dist"
        destination.mkdir()
        marker = destination / ".provider-versions.json"
        marker.write_bytes(oci.json_bytes({
            "platform": "1.2.3", "providers": {name: "1.2.3" for name in ("kind", "k3d", "azure")},
        }))
        globals_ = platform_package["main"].__globals__
        inputs = mock.Mock(return_value=({}, ("1.2.3", {"rootfs.cpio": b"fixture"})))
        frozen = mock.Mock(return_value=("1.2.3", {"rootfs.cpio": b"fixture"}))
        calls = []
        def fail_archive(output, files):
            self.assertFalse(marker.exists())
            calls.append(output)
            if len(calls) == 2:
                raise ValueError("archive write failed")
            output.write_bytes(b"new archive")
        overrides = {"linux_amd64": mock.Mock(),
                     "platform_base_files": mock.Mock(return_value={}),
                     "provider_inputs": inputs, "freeze_provider": frozen, "archive": fail_archive}
        with mock.patch.dict(globals_, overrides), \
             mock.patch.object(sys, "argv", ["package-platform.py", "--root", str(root),
                 "--binary", str(root / "ly"), "--output", str(destination),
                 "--source-commit", "0" * 40]):
            with self.assertRaisesRegex(ValueError, "archive write failed"):
                platform_package["main"]()
        self.assertEqual(len(calls), 2)
        self.assertFalse(marker.exists())
        aliases = mock.Mock()
        with mock.patch.dict(globals_, {"release_aliases": aliases}), \
             mock.patch.object(sys, "argv", ["package-platform.py", "--root", str(root),
                 "--output", str(destination), "--aliases", "preview"]):
            with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()) as stderr:
                platform_package["main"]()
        self.assertIn("package before creating aliases", stderr.getvalue())
        aliases.assert_not_called()

    def test_native_json_and_schema_types_are_rejected_before_output_or_transport(self):
        self.assemble()
        for position in ("index", "manifest"):
            for problem in ("float", "duplicate", "constant", "utf16"):
                with self.subTest(position=position, problem=problem):
                    directory = self.root / (position + problem)
                    shutil.copytree(self.output, directory)
                    index = oci.read_json(directory / "index.json")
                    if position == "index":
                        value = index
                    else:
                        native = index["manifests"][0]
                        value = oci.read_json(directory / "blobs/sha256" / native["digest"][7:])
                    content = oci.json_bytes(value)
                    if problem == "float":
                        content = content.replace(b'"schemaVersion":2', b'"schemaVersion":2.0')
                    elif problem == "duplicate":
                        content = b'{"schemaVersion":2,' + content[1:]
                    elif problem == "constant":
                        content = b'{"extra":NaN,' + content[1:]
                    else:
                        content = content.decode("utf-8").encode("utf-16")
                    if position == "manifest":
                        item = oci.write_blob(directory, oci.MANIFEST, content)
                        item["annotations"] = native["annotations"]
                        index["manifests"][0] = item
                        content = oci.json_bytes(index)
                    (directory / "index.json").write_bytes(content)
                    manifest = oci.read_json(directory / "manifest.json")
                    manifest["snapshotIndex"]["sha256"] = oci.file_hash(directory / "index.json")
                    (directory / "manifest.json").write_bytes(oci.json_bytes(manifest))
                    output = self.root / (position + problem + "-assembled")
                    with self.assertRaises(ValueError):
                        oci.assemble(self.intel, [directory], output)
                    self.assertFalse(output.exists())
                    remote = self.root / (position + problem + "-remote")
                    with mock.patch.object(oci, "oras") as transport:
                        with self.assertRaises(ValueError):
                            oci.publish(directory, "registry.example/repo", "v1", remote)
                        transport.assert_not_called()
                    self.assertFalse(remote.exists())
                    release = self.root / "release/providers/test/build/rootfs"
                    shutil.copytree(directory, release, dirs_exist_ok=True)
                    with self.assertRaises(ValueError):
                        platform_package["provider_files"](self.root / "release", "test")

    def test_type_changing_manifest_mutations_fail_before_publication_and_release(self):
        self.assemble()
        validate = oci.layout_files
        original = (self.output / "manifest.json").read_bytes()
        def replace(directory):
            result = validate(directory)
            if directory == self.output:
                manifest = oci.read_json(directory / "manifest.json")
                manifest["memoryMiB"] = float(manifest["memoryMiB"])
                (directory / "manifest.json").write_bytes(oci.json_bytes(manifest))
            return result
        with mock.patch.object(oci, "layout_files", side_effect=replace), \
             mock.patch.object(oci, "oras") as transport:
            with self.assertRaisesRegex(ValueError, "manifest changed|provider memory"):
                oci.publish(self.output, "registry.example/repo", "v1", self.root / "remote")
            transport.assert_not_called()
        self.assertFalse((self.root / "remote").exists())
        (self.output / "manifest.json").write_bytes(original)
        prepared = self.root / "prepared/test"
        prepared.parent.mkdir()
        shutil.copytree(self.output, prepared)
        self.provider_source(self.root, "test", prepared)
        validate_provider = platform_package["provider_files"]
        def replace_provider(root, name, artifacts=None):
            result = validate_provider(root, name, artifacts)
            if artifacts == prepared.parent:
                manifest = oci.read_json(prepared / "manifest.json")
                manifest["memoryMiB"] = float(manifest["memoryMiB"])
                (prepared / "manifest.json").write_bytes(oci.json_bytes(manifest))
            return result
        globals_ = platform_package["freeze_provider"].__globals__
        staging = self.root / "frozen"
        staging.mkdir()
        with mock.patch.dict(globals_, {"provider_files": replace_provider}):
            with self.assertRaisesRegex(ValueError, "manifest changed|provider memory"):
                platform_package["freeze_provider"](self.root, "test", prepared.parent, staging)

    def test_publication_validates_source_graph_only_once(self):
        self.assemble()
        validate = oci.layout_files
        with mock.patch.object(oci, "layout_files", wraps=validate) as validation, \
             mock.patch.object(oci, "oras", side_effect=ValueError("offline stop")):
            with self.assertRaisesRegex(ValueError, "offline stop"):
                oci.publish(self.output, "registry.example/repo", "v1", self.root / "remote")
        self.assertEqual(sum(call.args[0] == self.output for call in validation.call_args_list), 1)
        self.assertEqual(len(validation.call_args_list), 2)

    def test_frozen_index_replacement_after_staged_validation_is_rejected(self):
        self.assemble()
        validate = oci.layout_files
        def replace(directory):
            result = validate(directory)
            if directory != self.output:
                (directory / "index.json").write_bytes(b"{}")
            return result
        with mock.patch.object(oci, "layout_files", side_effect=replace), \
             mock.patch.object(oci, "oras") as transport:
            with self.assertRaisesRegex(ValueError, "index changed"):
                oci.publish(self.output, "registry.example/repo", "v1", self.root / "remote")
            transport.assert_not_called()

    def test_registry_only_index_filename_and_repository_separators(self):
        self.assemble()
        directory = self.root / "release/providers/test/build/rootfs"
        directory.parent.mkdir(parents=True)
        shutil.copytree(self.output, directory)
        (directory / "index.json").unlink()
        manifest = oci.read_json(directory / "manifest.json")
        manifest["snapshotIndex"]["path"] = "snapshot/index.txt"
        (directory / "manifest.json").write_text(json.dumps(manifest))
        with self.assertRaisesRegex(ValueError, "index.json"):
            platform_package["provider_files"](self.root / "release", "test")
        for repository in ("registry.example/team/provider--snapshots",
                           "registry.example/team/provider__snapshots"):
            self.assertEqual(oci.repository_name(repository), repository)
        for repository in ("registry.example/team/provider___snapshots",
                           "registry.example/team/provider..snapshots"):
            with self.assertRaises(ValueError):
                oci.repository_name(repository)

    def test_registry_hosts_accept_hyphen_runs_and_ipv6_without_credentials(self):
        for repository in ("registry--prod.example/team/snapshots",
                           "[2001:db8::1]:5000/team/snapshots",
                           "[::1]/snapshots"):
            self.assertEqual(oci.repository_name(repository), repository)
        for repository in ("[2001:::1]:5000/team/snapshots",
                           "[2001:db8::1]:70000/team/snapshots",
                           "registry.example:0/team/snapshots",
                           "-registry.example/team/snapshots",
                           "registry-.example/team/snapshots",
                           "user:secret@[::1]/team/snapshots"):
            with self.subTest(repository=repository), self.assertRaises(ValueError):
                oci.repository_name(repository)

    def test_all_native_descriptor_annotations_must_be_strings(self):
        for location in ("variant", "config", "memory", "transport"):
            for value in (42, None, {"nested": "value"}):
                with self.subTest(location=location, value=value):
                    directory = self.root / (location + str(value is None) + str(isinstance(value, dict)))
                    shutil.copytree(self.intel, directory)
                    layout = directory / "snapshot"
                    index = oci.read_json(layout / "index.json")
                    native = index["manifests"][0]
                    image = oci.read_json(layout / "blobs/sha256" / native["digest"][7:])
                    if location == "variant":
                        native["annotations"]["extra"] = value
                    else:
                        item = image["config"] if location == "config" else image["layers"][
                            0 if location == "memory" else -1]
                        item["annotations"] = {"extra": value}
                        updated = oci.write_blob(layout, oci.MANIFEST, oci.json_bytes(image))
                        updated["annotations"] = native["annotations"]
                        index["manifests"] = [updated]
                    (layout / "index.json").write_bytes(oci.json_bytes(index))
                    manifest = oci.read_json(directory / "manifest.json")
                    manifest["snapshotIndex"]["sha256"] = oci.file_hash(layout / "index.json")
                    (directory / "manifest.json").write_text(json.dumps(manifest))
                    with self.assertRaisesRegex(ValueError, "annotations must be strings"):
                        oci.assemble(self.intel, [directory], self.output)
                    self.assertFalse(self.output.exists())
                    with mock.patch.object(oci, "oras") as transport:
                        with self.assertRaisesRegex(ValueError, "annotations must be strings"):
                            oci.publish(directory, "registry.example/repo", "v1", self.output)
                        transport.assert_not_called()

    def test_memory_takes_up_to_31_layers(self):
        # hyperlight-host 0.19's 30 data layers and one blob of page tables alone.
        oci.assemble(self.variant("layers-31", "GenuineIntel", memory_layers=31),
                     [self.root / "layers-31"], self.output)
        with self.assertRaisesRegex(ValueError, "unsupported snapshot OCI manifest"):
            oci.assemble(self.variant("layers-32", "GenuineIntel", memory_layers=32),
                         [self.root / "layers-32"], self.root / "assembled-32")

    def assert_refused_before_hashing(self, directory, output, error):
        """Assemble `directory`, which must fail with `error` before any memory layer is hashed."""
        with mock.patch.object(oci, "file_hash", wraps=oci.file_hash) as hashed:
            with self.assertRaises(ValueError) as refused:
                oci.assemble(directory, [directory], output)
        # Only the memory layers' contents start with the vendor.
        self.assertFalse(any(Path(call.args[0]).read_bytes().startswith(b"GenuineIntel")
                             for call in hashed.call_args_list),
                         f"{directory.name}: a memory layer was hashed")
        self.assertRegex(str(refused.exception), error)

    def test_declared_memory_total_is_checked_before_any_layer_is_hashed(self):
        # The first layer is valid, so a check made only as each layer is hashed would hash it.
        first = len(b"GenuineIntel")
        for i, (sizes, error) in enumerate((
            ([first, oci.LAYER_LIMIT], "memory exceeds its size limit"),
            ([first, "1"], "unsupported snapshot OCI descriptor"),
        )):
            directory = self.variant(f"sizes-{i}", "GenuineIntel", sizes=sizes)
            self.assert_refused_before_hashing(directory, self.root / f"assembled-sizes-{i}", error)

    def test_layer_kinds_are_checked_before_any_layer_is_hashed(self):
        # Each but the last has a valid memory layer first, which a check made only as each
        # layer is hashed would hash.
        for name, layers in (
            ("transport-between", lambda l: [l[0], l[-1], l[1]]),
            ("no-transport", lambda l: l[:-1] + [l[0]]),
            ("not-a-descriptor", lambda l: l[:-1] + ["transport"]),
            ("memory-not-a-descriptor", lambda l: [7, *l[1:]]),
        ):
            directory = self.variant(name, "GenuineIntel", layers=layers)
            self.assert_refused_before_hashing(directory, self.root / f"assembled-{name}",
                                               "unsupported snapshot OCI manifest")

    def test_aggregate_variant_limit_precedes_next_blob_copy(self):
        index_path = self.intel / "snapshot/index.json"
        index = oci.read_json(index_path)
        native = index["manifests"][0]
        variants = []
        for i in range(128):
            item = dict(native)
            item["annotations"] = dict(native["annotations"])
            item["annotations"][oci.REF_NAME] = f"{oci.RUNTIME_VERSION}-{i}-test-key"
            variants.append(item)
        index["manifests"] = variants
        index_path.write_bytes(oci.json_bytes(index))
        manifest = oci.read_json(self.intel / "manifest.json")
        manifest["snapshotIndex"]["sha256"] = oci.file_hash(index_path)
        (self.intel / "manifest.json").write_text(json.dumps(manifest))
        with mock.patch.object(oci, "copy_blob", wraps=oci.copy_blob) as copy:
            with self.assertRaisesRegex(ValueError, "128"):
                self.assemble([self.intel, self.amd])
            # Each variant's manifest, config, two memory layers and transport.
            self.assertEqual(copy.call_count, 128 * 5)
            self.assertTrue(all(call.args[1].is_relative_to(self.intel)
                                for call in copy.call_args_list))
        self.assertFalse(self.output.exists())

    def test_paths_match_loader_length_and_reserved_components(self):
        for path in (".git/rootfs.cpio", "nested/.hg/rootfs.cpio", ".svn/kernel",
                     "a" * 1025, "\u00e9" * 513):
            with self.subTest(path=path), self.assertRaisesRegex(ValueError, "unsafe"):
                oci.relative_path(path)
        self.assertEqual(oci.relative_path("a" * 1024), Path("a" * 1024))
        self.assertEqual(oci.relative_path("nested/rootfs.cpio"), Path("nested/rootfs.cpio"))

    def test_archive_names_match_installer_character_and_component_rules(self):
        output = self.root / "names.tar.gz"
        for name in ("cold assets/rootfs.cpio", "a\nb", ".git/rootfs.cpio",
                     "a/../b", "/absolute", "a//b", "\u96ea", "a" * 1025):
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, "unsafe"):
                platform_package["archive"](output, {name: b"fixture"})
            self.assertFalse(output.exists())
        with contextlib.redirect_stdout(io.StringIO()):
            platform_package["archive"](output, {"providers/test/cold_assets/rootfs.cpio": b"fixture"})
        with tarfile.open(output) as archive:
            self.assertEqual(archive.getnames(), ["providers/test/cold_assets/rootfs.cpio"])

    def test_release_staging_rejects_modified_pinned_inputs(self):
        self.assemble()
        prepared = self.root / "prepared/test"
        prepared.parent.mkdir()
        shutil.copytree(self.output, prepared)
        self.provider_source(self.root, "test", prepared)
        validate = platform_package["provider_files"]
        globals_ = platform_package["freeze_provider"].__globals__
        manifest = oci.read_json(prepared / "manifest.json")
        native = oci.read_json(prepared / "index.json")["manifests"][0]
        image = oci.read_json(prepared / "blobs/sha256" / native["digest"][7:])
        memory = prepared / "blobs/sha256" / image["layers"][0]["digest"][7:]
        original = memory.read_bytes()
        for name in ("memory", "manifest"):
            staging = self.root / ("freeze-" + name)
            staging.mkdir()
            def replace(root, provider, artifacts=None):
                result = validate(root, provider, artifacts)
                if artifacts == prepared.parent:
                    if name == "memory":
                        memory.write_bytes(b"modified memory")
                    else:
                        changed = dict(manifest, version="9.9.9")
                        (prepared / "manifest.json").write_text(json.dumps(changed))
                return result
            with mock.patch.dict(globals_, {"provider_files": replace}):
                with self.assertRaisesRegex(ValueError, "mismatch|changed"):
                    platform_package["freeze_provider"](self.root, "test", prepared.parent, staging)
            memory.write_bytes(original)
            (prepared / "manifest.json").write_text(json.dumps(manifest))

    def test_frozen_provider_bytes_are_reused_for_both_release_archives(self):
        self.assemble()
        prepared = self.root / "prepared/test"
        prepared.parent.mkdir()
        shutil.copytree(self.output, prepared)
        self.provider_source(self.root, "test", prepared)
        staging = self.root / "frozen"
        staging.mkdir()
        _, files = platform_package["freeze_provider"](self.root, "test", prepared.parent, staging)
        pinned = {name: path.read_bytes() for name, path in files.items()}
        for path in prepared.rglob("*"):
            if path.is_file():
                path.write_bytes(b"changed after freeze")
        with contextlib.redirect_stdout(io.StringIO()):
            platform_package["archive"](self.root / "platform.tar.gz", {
                "providers/test/artifact/" + name: path for name, path in files.items()
            })
            platform_package["archive"](self.root / "provider.tar.gz", files)
        with tarfile.open(self.root / "platform.tar.gz") as platform, \
             tarfile.open(self.root / "provider.tar.gz") as provider:
            for name, content in pinned.items():
                self.assertEqual(provider.extractfile(name).read(), content)
                self.assertEqual(platform.extractfile("providers/test/artifact/" + name).read(), content)

    def test_registry_only_release_preserves_preflight_manifest_pin(self):
        self.assemble()
        prepared = self.root / "prepared/test"
        prepared.parent.mkdir()
        shutil.copytree(self.output, prepared)
        self.provider_source(self.root, "test", prepared)
        (prepared / "index.json").unlink()
        validated = platform_package["provider_inputs"](self.root, "test", prepared.parent)
        manifest, (_, files) = validated
        platform_package["archive_inputs"](files)
        changed = json.loads(oci.json_bytes(manifest))
        changed["snapshotIndex"]["sha256"] = "0" * 64
        (prepared / "manifest.json").write_bytes(oci.json_bytes(changed))
        staging = self.root / "frozen"
        staging.mkdir()
        with self.assertRaisesRegex(ValueError, "manifest changed"):
            platform_package["freeze_provider"](self.root, "test", prepared.parent, staging, validated)
        self.assertEqual(manifest["snapshotIndex"]["sha256"],
                         oci.read_json(self.output / "manifest.json")["snapshotIndex"]["sha256"])

    def test_registry_only_index_requires_missing_not_invalid_lookup(self):
        self.assemble()
        prepared = self.root / "prepared/test"
        prepared.parent.mkdir()
        shutil.copytree(self.output, prepared)
        manifest = oci.read_json(prepared / "manifest.json")
        (prepared / "rootfs.cpio").rename(prepared / "snapshot")
        manifest["rootfs"]["path"] = "snapshot"
        manifest["snapshotIndex"]["path"] = "snapshot/index.json"
        (prepared / "manifest.json").write_bytes(oci.json_bytes(manifest))
        self.provider_source(self.root, "test", prepared)
        with self.assertRaises(NotADirectoryError):
            platform_package["provider_files"](self.root, "test", prepared.parent)
        manifest["snapshotIndex"]["path"] = "missing/index.json"
        (prepared / "manifest.json").write_bytes(oci.json_bytes(manifest))
        _, files = platform_package["provider_files"](self.root, "test", prepared.parent)
        self.assertIn("snapshot", files)

    def test_release_preflights_installer_file_limit_before_any_staging_copy(self):
        self.assemble()
        prepared = self.root / "prepared/test"
        prepared.parent.mkdir()
        shutil.copytree(self.output, prepared)
        self.provider_source(self.root, "test", prepared)
        version, files = platform_package["provider_files"](self.root, "test", prepared.parent)
        memory = self.root / "large-memory"
        limit = platform_package["MAX_FILE"]
        with memory.open("wb") as source:
            source.truncate(limit)
        platform_package["archive_inputs"]({"memory": memory})
        with memory.open("r+b") as source:
            source.truncate(limit + 1)
        files["memory"] = memory
        staging = self.root / "frozen"
        staging.mkdir()
        globals_ = platform_package["freeze_provider"].__globals__
        with mock.patch.dict(globals_, {"provider_files": mock.Mock(return_value=(version, files))}), \
             mock.patch.object(shutil, "copyfile") as copy:
            with self.assertRaisesRegex(ValueError, "installer file or total size"):
                platform_package["freeze_provider"](self.root, "test", prepared.parent, staging)
            copy.assert_not_called()
        self.assertEqual(list(staging.iterdir()), [])

    def test_file_descriptors_reject_unknown_fields_on_every_output_path(self):
        for field in ("rootfs", "program", "kernel", "snapshotIndex"):
            with self.subTest(field=field):
                manifest = oci.read_json(self.intel / "manifest.json")
                original = oci.json_bytes(manifest)
                manifest[field]["size"] = 1
                (self.intel / "manifest.json").write_bytes(oci.json_bytes(manifest))
                with self.assertRaisesRegex(ValueError, "invalid provider file digest"):
                    self.assemble([self.intel])
                self.assertFalse(self.output.exists())
                with mock.patch.object(oci, "oras") as transport:
                    with self.assertRaisesRegex(ValueError, "invalid provider file digest"):
                        oci.publish(self.intel, "registry.example/test", "v1", self.output)
                    transport.assert_not_called()
                release = self.root / "providers/test/build/rootfs"
                shutil.copytree(self.intel, release, dirs_exist_ok=True)
                with self.assertRaisesRegex(ValueError, "invalid provider file digest"):
                    platform_package["provider_files"](self.root, "test")
                (self.intel / "manifest.json").write_bytes(original)

    def test_registry_only_file_descriptor_rejects_unknown_fields(self):
        self.assemble()
        prepared = self.root / "prepared/test"
        prepared.parent.mkdir()
        shutil.copytree(self.output, prepared)
        self.provider_source(self.root, "test", prepared)
        (prepared / "index.json").unlink()
        manifest = oci.read_json(prepared / "manifest.json")
        manifest["snapshotIndex"]["size"] = 1
        (prepared / "manifest.json").write_bytes(oci.json_bytes(manifest))
        with self.assertRaisesRegex(ValueError, "invalid provider file digest"):
            platform_package["provider_files"](self.root, "test", prepared.parent)

    def test_reserved_license_asset_is_rejected_before_release_copy(self):
        self.assemble()
        prepared = self.root / "prepared/test"
        prepared.parent.mkdir()
        shutil.copytree(self.output, prepared)
        manifest = oci.read_json(prepared / "manifest.json")
        (prepared / "rootfs.cpio").rename(prepared / "LICENSE")
        manifest["rootfs"]["path"] = "LICENSE"
        (prepared / "manifest.json").write_bytes(oci.json_bytes(manifest))
        self.provider_source(self.root, "test", prepared)
        staging = self.root / "frozen"
        staging.mkdir()
        with mock.patch.object(shutil, "copyfile") as copy:
            with self.assertRaisesRegex(ValueError, "reserved release LICENSE"):
                platform_package["freeze_provider"](self.root, "test", prepared.parent, staging)
            copy.assert_not_called()
        self.assertEqual(list(staging.iterdir()), [])

    def test_shared_blobs_are_hashed_and_copied_once_per_distinct_file(self):
        index_path = self.intel / "snapshot/index.json"
        index = oci.read_json(index_path)
        native = index["manifests"][0]
        index["manifests"] = []
        for i in range(128):
            item = dict(native)
            item["annotations"] = dict(native["annotations"])
            item["annotations"][oci.REF_NAME] = f"{oci.RUNTIME_VERSION}-{i}-test-key"
            index["manifests"].append(item)
        index_path.write_bytes(oci.json_bytes(index))
        manifest = oci.read_json(self.intel / "manifest.json")
        manifest["snapshotIndex"]["sha256"] = oci.file_hash(index_path)
        (self.intel / "manifest.json").write_bytes(oci.json_bytes(manifest))
        image = oci.read_json(self.intel / "snapshot/blobs/sha256" / native["digest"][7:])
        memory = self.intel / "snapshot/blobs/sha256" / image["layers"][0]["digest"][7:]
        with mock.patch.object(oci, "file_hash", wraps=oci.file_hash) as hash_file:
            oci.layout_files(self.intel)
            self.assertEqual(sum(call.args[0] == memory for call in hash_file.call_args_list), 1)
        with mock.patch.object(shutil, "copyfile", wraps=shutil.copyfile) as copy:
            self.assemble([self.intel])
            self.assertEqual(sum(call.args[0] == memory for call in copy.call_args_list), 1)
        other = self.root / "distinct"
        shutil.copytree(self.intel, other)
        other_memory = other / memory.relative_to(self.intel)
        other_memory.write_bytes(b"corrupt")
        with self.assertRaisesRegex(ValueError, "size mismatch"):
            oci.layout_files(other)
        verified = set()
        oci.checked_blob(self.intel / "snapshot", image["layers"][0], oci.MEMORY, oci.LAYER_LIMIT, verified)
        changed = b"x" * memory.stat().st_size
        memory.write_bytes(changed)
        with self.assertRaisesRegex(ValueError, "digest"):
            oci.checked_blob(self.intel / "snapshot", image["layers"][0], oci.MEMORY, oci.LAYER_LIMIT, verified)

    def test_prepared_runtime_and_remote_cold_assets_match_loader(self):
        self.assemble()
        prepared = self.root / "prepared/test"
        prepared.parent.mkdir()
        shutil.copytree(self.output, prepared)
        self.provider_source(self.root, "test", prepared)
        for local in (True, False):
            if not local:
                (prepared / "index.json").unlink()
            manifest = oci.read_json(prepared / "manifest.json")
            original = dict(manifest)
            manifest["runtimeVersion"] = "0.18.0"
            (prepared / "manifest.json").write_text(json.dumps(manifest))
            with self.subTest(local=local), self.assertRaisesRegex(ValueError, "runtime"):
                platform_package["provider_files"](self.root, "test", prepared.parent)
            (prepared / "manifest.json").write_text(json.dumps(original))
        for content in (b"x" * (oci.JSON_LIMIT + 1), b"\xff"):
            manifest = oci.read_json(prepared / "manifest.json")
            path = prepared / "provider.py"
            path.write_bytes(content)
            manifest["program"] = package.file_digest(path)
            (prepared / "manifest.json").write_text(json.dumps(manifest))
            with self.assertRaises((ValueError, UnicodeDecodeError)):
                platform_package["provider_files"](self.root, "test", prepared.parent)

    def test_prepared_artifacts_must_match_the_checkout_being_released(self):
        self.assemble()
        prepared = self.root / "prepared/test"
        prepared.parent.mkdir()
        shutil.copytree(self.output, prepared)
        self.provider_source(self.root, "test", prepared)
        platform_package["provider_files"](self.root, "test", prepared.parent)
        source = self.root / "providers/test"
        original = (source / "provider.py").read_bytes()
        (source / "provider.py").write_bytes(original + b"# changed after the artifact was built\n")
        with self.assertRaisesRegex(ValueError, "program differs"):
            platform_package["provider_files"](self.root, "test", prepared.parent)
        (source / "provider.py").write_bytes(original)
        declared = oci.read_json(source / "artifact.json")
        declared["memoryMiB"] += 1
        (source / "artifact.json").write_text(json.dumps(declared))
        with self.assertRaisesRegex(ValueError, "memoryMiB differs"):
            platform_package["provider_files"](self.root, "test", prepared.parent)
        self.provider_source(self.root, "test", prepared)
        manifest = oci.read_json(prepared / "manifest.json")
        manifest["sourceKinds"] = [{"apiVersion": "example.dev/v1", "kind": "Example"}]
        (prepared / "manifest.json").write_text(json.dumps(manifest))
        with self.assertRaisesRegex(ValueError, "declares sourceKinds"):
            platform_package["provider_files"](self.root, "test", prepared.parent)
        manifest.pop("sourceKinds")
        original_manifest = oci.json_bytes(manifest)
        for key, value in (("protocolVersion", True), ("memoryMiB", 256.0)):
            (prepared / "manifest.json").write_bytes(original_manifest)
            self.provider_source(self.root, "test", prepared)
            manifest = oci.read_json(prepared / "manifest.json")
            manifest.pop("sourceKinds", None)
            manifest[key] = value
            (prepared / "manifest.json").write_text(json.dumps(manifest))
            error = "protocol version" if key == "protocolVersion" else "provider memory"
            with self.subTest(key=key), self.assertRaisesRegex(ValueError, error):
                platform_package["provider_files"](self.root, "test", prepared.parent)

    def test_hardlinked_layout_index_is_archived_as_two_regular_files(self):
        digest = self.assemble()
        blob = self.output / "blobs/sha256" / digest[7:]
        blob.unlink()
        os.link(self.output / "index.json", blob)
        _, files = oci.layout_files(self.output)
        output = self.root / "provider.tar.gz"
        with contextlib.redirect_stdout(io.StringIO()):
            platform_package["archive"](output, files)
        with tarfile.open(output) as archive:
            for name in ("index.json", "blobs/sha256/" + digest[7:]):
                self.assertTrue(archive.getmember(name).isfile())
                self.assertFalse(archive.getmember(name).islnk())
                self.assertEqual(archive.extractfile(name).read(), (self.output / "index.json").read_bytes())

    def test_archives_reject_installer_limits_without_partial_output(self):
        output = self.root / "limited.tar.gz"
        path = self.root / "oversized"
        with path.open("wb") as source:
            source.truncate((1 << 30) + 1)
        with self.assertRaisesRegex(ValueError, "installer"):
            platform_package["archive"](output, {"memory": path})
        self.assertFalse(output.exists())
        with path.open("wb") as source:
            source.truncate(900 << 20)
        with self.assertRaisesRegex(ValueError, "total"):
            platform_package["archive"](output, {str(i): path for i in range(5)})
        self.assertFalse(output.exists())
        # Same limits at a small scale exercise boundary success without huge writes.
        globals_ = platform_package["archive"].__globals__
        with mock.patch.dict(globals_, {"MAX_FILE": 4, "MAX_TOTAL": 8}):
            with contextlib.redirect_stdout(io.StringIO()):
                platform_package["archive"](output, {"first": b"1234", "second": b"5678"})
        with tarfile.open(output) as archive:
            self.assertEqual(sum(item.size for item in archive), 8)
        output.unlink()
        output.with_name(output.name + ".sha256").unlink()
        with mock.patch.dict(globals_, {"MAX_ARCHIVE": 1}):
            with self.assertRaisesRegex(ValueError, "compressed"):
                platform_package["archive"](output, {"memory": b"abc"})
        self.assertFalse(output.exists())
        self.assertEqual(list(self.root.glob(".package-*")), [])

    def test_provider_build_modes_are_literal_arguments(self):
        tools = self.root / "mode-tools"
        tools.mkdir()
        log = self.root / "mode-args"
        for name in ("python3",):
            path = tools / name
            path.write_text(
                "#!" + sys.executable + "\n"
                "import json, os, sys\n"
                "with open(os.environ['MODE_ARGS'], 'a') as output:\n"
                "    output.write(json.dumps(sys.argv) + '\\n')\n"
            )
            path.chmod(0o755)
        environment = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ["PATH"],
                           MODE_ARGS=str(log))
        literal = 'none$(printf BAD) `printf BAD` $HOME'
        executable = shutil.which("just")
        for provider, arguments in (("kind", [literal]), ("azure", ["rootfs", literal])):
            log.unlink(missing_ok=True)
            dry = subprocess.run([executable, "--justfile", str(PROVIDERS.parent / "Justfile"),
                                  "--dry-run", provider + "::build", *arguments],
                                 check=True, capture_output=True)
            command = next(line for line in dry.stderr.decode().splitlines()
                           if "/package.py" in line)
            self.assertNotIn(literal, command)
            subprocess.run(["/bin/bash", "-c", command, "build", *arguments],
                           check=True, capture_output=True, env=environment)
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            self.assertTrue(any("--snapshot-mode" in call and literal in call for call in calls))

    def test_release_recipes_pass_literal_artifact_arguments(self):
        tools = self.root / "recipe-tools"
        tools.mkdir()
        log = self.root / "recipe-args"
        stub = (
            "#!" + sys.executable + "\n"
            "import json, os, sys\n"
            "with open(os.environ['RECIPE_ARGS'], 'a') as output:\n"
            "    output.write(json.dumps([*sys.argv, 'cwd=' + os.getcwd()]) + '\\n')\n"
        )
        for tool in ("cargo", "python3", "just"):
            path = tools / tool
            path.write_text(stub)
            path.chmod(0o755)
        environment = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ["PATH"],
                           RECIPE_ARGS=str(log))
        executable = shutil.which("just")
        literal = str(self.root / 'artifacts $(touch BAD) `touch BAD2` $HOME')
        for recipe, arguments in (("cli::package", ["", literal]),
                                  ("cli::package-preview", [literal]),
                                  ("cli::package-release", ["1.2.3", literal]),
                                  ("package-release", ["v1.2.3", literal])):
            with self.subTest(recipe=recipe):
                log.unlink(missing_ok=True)
                subprocess.run([executable, "--justfile", str(PROVIDERS.parent / "Justfile"),
                                recipe, *arguments], check=True, capture_output=True, env=environment)
                calls = [json.loads(line) for line in log.read_text().splitlines()]
                self.assertTrue(any(literal in call for call in calls))
                self.assertFalse((PROVIDERS.parent / "BAD").exists())
                self.assertFalse((PROVIDERS.parent / "BAD2").exists())
        # A relative prepared path stays relative to the invocation directory.
        log.unlink(missing_ok=True)
        result = subprocess.run([executable, "--justfile", str(PROVIDERS.parent / "Justfile"),
                                 "package-release", "v1.2.3", "./prepared"],
                                check=True, capture_output=True, env=environment, cwd=self.root)
        calls = [json.loads(line) for line in log.read_text().splitlines()]
        self.assertTrue(any("./prepared" in call and "cwd=" + str(self.root) in call for call in calls))

    def test_package_none_does_not_boot_guest_and_variant_records_native_host(self):
        project = self.root / "project"
        output = project / "build/rootfs"
        shutil.copytree(self.intel, output)
        shutil.copyfile(self.intel / "provider.py", project / "provider.py")
        manifest = oci.read_json(self.intel / "manifest.json")
        (project / "artifact.json").write_text(json.dumps(manifest))
        for mode in ("none", "variant"):
            output.parent.chmod(output.parent.stat().st_mode | 0o022)
            for path in (output, *output.rglob("*")):
                path.chmod(path.stat().st_mode | 0o022)
            if mode == "variant":
                stale_snapshot = self.root / "stale-snapshot"
                stale_snapshot.mkdir()
                (stale_snapshot / "sentinel").write_text("keep")
                (output / "snapshot").symlink_to(stale_snapshot, target_is_directory=True)

            copyfile = package.shutil.copyfile

            def capture(*args, **kwargs):
                self.assertFalse((output / "snapshot").is_symlink())
                # Stands in for the snapshot tool, so copy with the real
                # copyfile, not the patched one that checks package.py's copies.
                shutil.copytree(self.intel / "snapshot", output / "snapshot", dirs_exist_ok=True,
                                copy_function=lambda source, destination: copyfile(source, destination))
                for path in (output / "snapshot", *(output / "snapshot").rglob("*")):
                    path.chmod(path.stat().st_mode | 0o022)

            file_digest = package.file_digest

            def check_permissions_before_copy(source, destination, **kwargs):
                for path in (output, *output.rglob("*")):
                    if path.is_symlink():
                        continue
                    self.assertEqual(path.lstat().st_mode & 0o022, 0, path)
                return copyfile(source, destination, **kwargs)

            def check_permissions_before_digest(path):
                for entry in (output, *output.rglob("*")):
                    if entry.is_symlink():
                        continue
                    self.assertEqual(entry.lstat().st_mode & 0o022, 0, entry)
                return file_digest(path)

            with mock.patch.object(sys, "argv", ["package.py", str(project), "rootfs",
                                               "--snapshot-mode", mode, "--kernel", str(self.intel / "kernel")]):
                with mock.patch.object(package, "native_snapshot_available", return_value=True), \
                     mock.patch.object(package.subprocess, "run", side_effect=capture) as run, \
                     mock.patch.object(package.shutil, "copyfile", side_effect=check_permissions_before_copy), \
                     mock.patch.object(package, "file_digest", side_effect=check_permissions_before_digest), \
                     contextlib.redirect_stdout(io.StringIO()):
                    package.main()
            self.assertEqual(output.parent.lstat().st_mode & 0o022, 0)
            for path in (output, *output.rglob("*")):
                self.assertEqual(path.lstat().st_mode & 0o022, 0, path)
            result = oci.read_json(output / "manifest.json")
            if mode == "variant":
                self.assertEqual((stale_snapshot / "sentinel").read_text(), "keep")
            if mode == "none":
                run.assert_not_called()
                self.assertNotIn("snapshot", result)
                self.assertNotIn("snapshotIndex", result)
            else:
                self.assertIn("--locked", run.call_args.args[0])
                self.assertIn("snapshotIndex", result)
                oci.layout_files(output)

    def test_provider_permission_scan_reports_walk_errors(self):
        output = self.root / "artifact"
        output.mkdir()

        def failed_walk(_path, **kwargs):
            if kwargs.get("onerror") is not None:
                kwargs["onerror"](PermissionError("permission scan failed"))
            return iter(())

        with mock.patch.object(package.os, "walk", side_effect=failed_walk):
            with self.assertRaisesRegex(PermissionError, "permission scan failed"):
                package.restrict_artifact_permissions(output)

    def test_provider_permission_scan_rejects_symlink_before_changing_target(self):
        target = self.root / "artifact-target"
        target.mkdir()
        file = target / "provider.py"
        file.write_text("provider")
        file.chmod(0o666)
        output = self.root / "artifact"
        output.symlink_to(target, target_is_directory=True)

        with self.assertRaisesRegex(ValueError, "output must be a directory"):
            package.restrict_artifact_permissions(output)

        self.assertEqual(file.stat().st_mode & 0o022, 0o022)

    def test_package_rejects_symlink_output_before_mutating_target(self):
        project = self.root / "project"
        output_parent = project / "build"
        output_parent.mkdir(parents=True)
        target = self.root / "target"
        target.mkdir()
        (target / "rootfs.cpio").write_bytes(b"rootfs")
        snapshot = target / "snapshot"
        snapshot.mkdir()
        (snapshot / "sentinel").write_text("keep")
        (project / "provider.py").write_text("provider")
        (project / "artifact.json").write_text(
            json.dumps({"id": "test", "version": "1.0.0", "memoryMiB": 128})
        )
        (output_parent / "proof").symlink_to(target, target_is_directory=True)

        with mock.patch.object(sys, "argv", ["package.py", str(project), "proof"]):
            with self.assertRaisesRegex(ValueError, "output must be a directory"):
                package.main()

        self.assertFalse((target / "provider.py").exists())
        self.assertEqual((snapshot / "sentinel").read_text(), "keep")

    def test_package_rejects_symlink_file_before_mutating_target(self):
        project = self.root / "project"
        output = project / "build" / "proof"
        output.mkdir(parents=True)
        (output / "rootfs.cpio").write_bytes(b"rootfs")
        (project / "provider.py").write_text("provider")
        (project / "artifact.json").write_text(
            json.dumps({"id": "test", "version": "1.0.0", "memoryMiB": 128})
        )
        target = self.root / "external-provider.py"
        target.write_text("external")
        (output / "provider.py").symlink_to(target)

        with mock.patch.object(sys, "argv", ["package.py", str(project), "proof"]):
            with self.assertRaises(ValueError):
                package.main()

        self.assertEqual(target.read_text(), "external")

    def test_package_rejects_symlink_build_directory_before_mutating_target(self):
        project = self.root / "project"
        project.mkdir()
        target = self.root / "external-build"
        output = target / "proof"
        output.mkdir(parents=True)
        (output / "rootfs.cpio").write_bytes(b"rootfs")
        (project / "provider.py").write_text("provider")
        (project / "artifact.json").write_text(
            json.dumps({"id": "test", "version": "1.0.0", "memoryMiB": 128})
        )
        (project / "build").symlink_to(target, target_is_directory=True)

        with mock.patch.object(sys, "argv", ["package.py", str(project), "proof"]):
            with self.assertRaises(ValueError):
                package.main()

        self.assertFalse((output / "provider.py").exists())

    def release_checkout(self, root):
        """Writes the platform files package-platform.py takes from a checkout, and returns the
        darwin-arm64 ly-dev it packages."""
        for relative in platform_package["PLATFORM_STATIC"]:
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"fixture")
        (root / "cli").mkdir()
        (root / "cli/Cargo.toml").write_text('[package]\nversion = "1.2.3"\n')
        header = bytearray(20)
        header[:6] = b"\x7fELF\x02\x01"
        header[18:20] = b"\x3e\x00"
        for name in [*platform_package["PLATFORM_BINARIES"], "ly"]:
            path = root / "build" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(header)
        supervisor = b"\xcf\xfa\xed\xfe\x0c\x00\x00\x01" + bytes(24)
        (root / "build/ly-dev-darwin-arm64").write_bytes(supervisor)
        # Each platform target carries its own template tools.
        tools = root / platform_package["TEMPLATE_TOOLS_DIRECTORY"]
        for target, tool_header in (
            ("linux-amd64", header),
            ("darwin-arm64", supervisor),
        ):
            for name in platform_package["TEMPLATE_TOOLS"]:
                path = tools / target / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(tool_header)
                path.chmod(0o755)
        return supervisor

    def prepared_providers(self, root, prepared, repository=None):
        """Copies the assembled artifact to PREPARED/NAME for each maintained provider, naming
        `repository` as snapshot publication does, and writes the checkout each came from."""
        for name in ("kind", "k3d", "azure"):
            directory = prepared / name
            shutil.copytree(self.output, directory)
            manifest = oci.read_json(directory / "manifest.json")
            manifest["id"] = name
            if repository is not None:
                manifest["snapshotRepository"] = f"{repository}/provider-{name}-snapshots"
            (directory / "manifest.json").write_text(json.dumps(manifest))
            self.provider_source(root, name, directory)

    def test_release_cli_uses_prepared_artifacts_without_rebuilding(self):
        self.assemble()
        root = self.root / "repository"
        prepared = self.root / "prepared"
        self.prepared_providers(root, prepared)
        supervisor = self.release_checkout(root)
        destination = self.root / "dist"
        result = subprocess.run([
            sys.executable, str(PROVIDERS.parent / "scripts/package-platform.py"),
            "--root", str(root), "--binary", str(root / "build/ly"),
            "--output", str(destination), "--source-commit", "0" * 40,
            "--provider-artifacts", str(prepared),
        ], check=True, capture_output=True)
        self.assertEqual(result.stderr, b"")
        for name in ("kind", "k3d", "azure"):
            with tarfile.open(destination / f"lampyr-provider-{name}-1.2.3.tar.gz") as archive:
                self.assertIn("snapshotIndex", json.load(archive.extractfile("manifest.json")))
                self.assertIn("rootfs.cpio", archive.getnames())
                self.assertIn("index.json", archive.getnames())
        subprocess.run([
            sys.executable, str(PROVIDERS.parent / "scripts/package-platform.py"),
            "--root", str(root), "--output", str(destination),
            "--source-commit", "0" * 40, "--provider-artifacts", str(prepared),
            "--platform-only", "--platform-target", "darwin-arm64",
        ], check=True, capture_output=True)
        for target, magic in (("linux-amd64", b"\x7fELF"), ("darwin-arm64", b"\xcf\xfa\xed\xfe")):
            with tarfile.open(destination / f"lampyr-platform-{target}-1.2.3.tar.gz") as archive:
                names = archive.getnames()
                for name in ("kind", "k3d", "azure"):
                    self.assertIn(f"providers/{name}/artifact/rootfs.cpio", names)
                    self.assertIn(f"providers/{name}/artifact/manifest.json", names)
                if target == "darwin-arm64":
                    self.assertEqual(archive.extractfile("build/ly-dev").read(), supervisor)
                # The helper is Linux's in both; the host tools the target's.
                self.assertEqual(archive.extractfile("build/hluk-helper").read(4), b"\x7fELF")
                for name in ("zygotectl", "grant-issuer"):
                    member = archive.getmember(f"build/{name}")
                    self.assertEqual(member.mode, 0o755, name)
                    self.assertEqual(archive.extractfile(member).read(4), magic, (target, name))
        # Alias creation must not need the prepared inputs, even if the option is still passed.
        shutil.rmtree(prepared)
        subprocess.run([
            sys.executable, str(PROVIDERS.parent / "scripts/package-platform.py"),
            "--root", str(root), "--output", str(destination), "--aliases", "preview",
            "--provider-artifacts", str(prepared),
        ], check=True, capture_output=True)
        for name in ("kind", "k3d", "azure"):
            self.assertEqual(
                (destination / f"lampyr-provider-{name}-preview.tar.gz").read_bytes(),
                (destination / f"lampyr-provider-{name}-1.2.3.tar.gz").read_bytes(),
            )
        marker = destination / ".provider-versions.json"
        packaged = oci.read_json(marker)
        self.assertEqual(packaged["platform"], "1.2.3")
        for content, error in (
            ('{"kind":"9.9.9"}', b"invalid packaged provider versions"),
            (oci.json_bytes({**packaged, "platform": "1.2.2"}).decode(), b"packaged for version '1.2.2'"),
        ):
            marker.write_text(content)
            failed = subprocess.run([
                sys.executable, str(PROVIDERS.parent / "scripts/package-platform.py"),
                "--root", str(root), "--output", str(destination), "--aliases", "preview",
            ], capture_output=True)
            self.assertNotEqual(failed.returncode, 0)
            self.assertIn(error, failed.stderr)

    def release_step(self, name):
        """The run script of release.yml's step NAME, read without a YAML parser, which the
        hosts that run these tests needn't have."""
        lines = (PROVIDERS.parent / ".github/workflows/release.yml").read_text().splitlines()
        start = lines.index(f"      - name: {name}")
        self.assertEqual(lines[start + 1], "        run: |")
        script = []
        for line in lines[start + 2:]:
            if line and not line.startswith(" " * 10):
                break
            script.append(line[10:])
        return "\n".join(script) + "\n"

    def test_release_ships_providers_registry_only(self):
        self.assemble()
        root = self.root / "repository"
        work = self.root / "work"
        for platform in ("linux", "darwin"):
            self.prepared_providers(root, work / f"build/providers-{platform}", "registry.example/team")
        self.release_checkout(root)
        published = {name: oci.read_json(work / "build/providers-linux" / name / "manifest.json")
                     for name in ("kind", "k3d", "azure")}
        script = self.release_step("Drop local snapshot layouts from the provider artifacts")
        subprocess.run(["bash", "--noprofile", "--norc", "-c", script], cwd=work, check=True,
                       capture_output=True)
        for platform in ("linux", "darwin"):
            for name in ("kind", "k3d", "azure"):
                artifact = work / f"build/providers-{platform}" / name
                for layout in ("blobs", "index.json", "oci-layout"):
                    self.assertFalse((artifact / layout).exists(), (platform, name, layout))
                # The pins ly pulls the host's variant by are kept as published.
                self.assertEqual(oci.read_json(artifact / "manifest.json"), published[name])
        destination = self.root / "dist"
        packaging = [sys.executable, str(PROVIDERS.parent / "scripts/package-platform.py"),
                     "--root", str(root), "--output", str(destination), "--source-commit", "0" * 40]
        subprocess.run([*packaging, "--binary", str(root / "build/ly"),
                        "--provider-artifacts", str(work / "build/providers-linux")],
                       check=True, capture_output=True)
        subprocess.run([*packaging, "--provider-artifacts", str(work / "build/providers-darwin"),
                        "--platform-only", "--platform-target", "darwin-arm64"],
                       check=True, capture_output=True)
        archives = [destination / f"lampyr-provider-{name}-1.2.3.tar.gz" for name in published]
        archives += [destination / f"lampyr-platform-{target}-1.2.3.tar.gz"
                     for target in ("linux-amd64", "darwin-arm64")]
        for path in archives:
            with tarfile.open(path) as archive:
                names = archive.getnames()
                prefixes = [""] if path.name.startswith("lampyr-provider-") else [
                    f"providers/{name}/artifact/" for name in published]
                for prefix in prefixes:
                    self.assertIn(prefix + "rootfs.cpio", names)
                    manifest = json.load(archive.extractfile(prefix + "manifest.json"))
                    self.assertIn("snapshotIndex", manifest)
                    self.assertIn("snapshotRepository", manifest)
                    self.assertNotIn(prefix + "index.json", names)
                    self.assertNotIn(prefix + "oci-layout", names)
                    self.assertFalse(any(member.startswith(prefix + "blobs/") for member in names))

    def test_release_refuses_to_drop_a_layout_no_registry_serves(self):
        self.assemble()
        root = self.root / "repository"
        work = self.root / "work"
        self.prepared_providers(root, work / "build/providers-linux", "registry.example/team")
        self.prepared_providers(root, work / "build/providers-darwin")
        script = self.release_step("Drop local snapshot layouts from the provider artifacts")
        result = subprocess.run(["bash", "--noprofile", "--norc", "-c", script], cwd=work,
                                capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("build/providers-darwin/kind does not pin a published snapshot index",
                      result.stderr)
        # No layout goes until every artifact is checked: a darwin artifact with no
        # registry has no other source for its snapshots.
        for platform in ("linux", "darwin"):
            for name in ("kind", "k3d", "azure"):
                oci.layout_files(work / f"build/providers-{platform}" / name)

    def test_build_recipes_handle_paths_and_redact_transport_output(self):
        variant = self.root / "variant with spaces"
        shutil.copytree(self.intel, variant)
        layout = self.root / "layout with spaces"
        recipe = ["just", "--justfile", str(PROVIDERS.parent / "Justfile")]
        for provider in ("kind", "azure"):
            dry_run = subprocess.run([*recipe, "--dry-run", f"{provider}::snapshot"],
                                     check=True, capture_output=True)
            self.assertNotIn(b"build-rootfs.sh", dry_run.stderr)
            self.assertIn(b"--snapshot-mode variant", dry_run.stderr)
        subprocess.run([*recipe, "providers::snapshot-assemble", str(self.intel),
                        str(layout), str(variant)], check=True, capture_output=True)
        oci.layout_files(layout)
        tools = self.root / "tools"
        tools.mkdir()
        transport = tools / "oras"
        transport.write_text(
            "#!" + sys.executable + "\n"
            "import json, os, pathlib, sys\n"
            "with open(os.environ['TRANSPORT_LOG'], 'a') as log:\n"
            "    log.write(json.dumps([sys.argv[1:], os.environ.get('DOCKER_CONFIG')]) + '\\n')\n"
            "print('private registry diagnostic', file=sys.stderr)\n"
            "if '--descriptor' in sys.argv:\n"
            "    digest = sys.argv[-1].rsplit('@', 1)[1]\n"
            "    blob = pathlib.Path(os.environ['TEST_INDEX']).parent / 'blobs/sha256' / digest[7:]\n"
            "    print(json.dumps({'digest': digest, 'size': blob.stat().st_size}))\n"
            "    sys.exit(0)\n"
            "print('private registry diagnostic')\n"
            "if sys.argv[1:3] == ['manifest', 'fetch']:\n"
            "    pathlib.Path(sys.argv[-1]).write_bytes(pathlib.Path(os.environ['TEST_INDEX']).read_bytes())\n"
        )
        transport.chmod(0o755)
        log = self.root / "transport.log"
        docker_config = self.root / "docker"
        remote = self.root / "remote with spaces"
        environment = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ["PATH"],
                           DOCKER_CONFIG=str(docker_config), TRANSPORT_LOG=str(log),
                           TEST_INDEX=str(layout / "index.json"))
        result = subprocess.run([
            *recipe, "providers::snapshot-publish", str(layout),
            "registry.example/team/snapshots", "v1", str(remote),
        ], check=True, capture_output=True, env=environment)
        self.assertNotIn(b"private registry diagnostic", result.stdout + result.stderr)
        digest = oci.read_json(remote / "manifest.json")["snapshotIndex"]["sha256"]
        self.assertEqual(result.stdout.strip(), f"registry.example/team/snapshots@sha256:{digest}".encode())
        calls = [json.loads(line) for line in log.read_text().splitlines()]
        self.assertEqual([call[0][:2] for call in calls[:2]], [["cp", "--from-oci-layout"], ["manifest", "fetch"]])
        self.assertTrue(calls[2:] and all("--descriptor" in call[0] for call in calls[2:]))
        self.assertTrue(all(call[1] == str(docker_config) for call in calls))
        self.assertTrue(all("password" not in argument for call in calls for argument in call[0]))

    def test_real_oras_copies_multi_platform_graph_offline(self):
        for name, variants in (("multi", [self.intel, self.amd, self.arm]), ("empty", [])):
            with self.subTest(name=name):
                output = self.root / name
                digest = oci.assemble(self.intel, variants, output)
                copied = self.root / (name + "-oras-copy")
                catalog = self.root / (name + "-catalog")
                shutil.copytree(output, catalog)
                (catalog / "index.json").write_bytes(oci.json_bytes({
                    "schemaVersion": 2, "mediaType": oci.INDEX,
                    "manifests": [oci.descriptor(oci.INDEX, (output / "index.json").read_bytes())],
                }))
                subprocess.run(["oras", "cp", "--from-oci-layout", "--to-oci-layout",
                                f"{catalog}@{digest}", f"{copied}:test"],
                               check=True, stdout=subprocess.DEVNULL)
                self.assertEqual((copied / "blobs/sha256" / digest[7:]).read_bytes(),
                                 (output / "index.json").read_bytes())
                for path in (output / "blobs/sha256").iterdir():
                    self.assertEqual(path.read_bytes(), (copied / "blobs/sha256" / path.name).read_bytes())


if __name__ == "__main__":
    unittest.main()
