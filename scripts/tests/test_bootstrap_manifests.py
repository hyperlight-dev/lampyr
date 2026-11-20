import json
from pathlib import Path
import runpy
import shutil
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
packaging = runpy.run_path(str(ROOT / "scripts/package-platform.py"))
MANIFESTS = {
    "serving-crds.yaml": (
        "knative-v1.23.0",
        "https://github.com/knative/serving/releases/download/knative-v1.23.0/serving-crds.yaml",
        "Apache-2.0",
    ),
    "serving-core.yaml": (
        "knative-v1.23.0",
        "https://github.com/knative/serving/releases/download/knative-v1.23.0/serving-core.yaml",
        "Apache-2.0",
    ),
    "kourier.yaml": (
        "knative-v1.23.0",
        "https://github.com/knative/net-kourier/releases/download/knative-v1.23.0/kourier.yaml",
        "Apache-2.0",
    ),
    "cert-manager.yaml": (
        "v1.21.3",
        "https://github.com/cert-manager/cert-manager/releases/download/v1.21.3/cert-manager.yaml",
        "Apache-2.0",
    ),
}


def prepare_platform(root):
    for relative in packaging["PLATFORM_STATIC"]:
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / relative, path)
    header = bytearray(20)
    header[:6] = b"\x7fELF\x02\x01"
    header[18:20] = b"\x3e\x00"
    for name in packaging["PLATFORM_BINARIES"]:
        path = root / "build" / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(header)
    for name in packaging["TEMPLATE_TOOLS"]:
        path = root / packaging["TEMPLATE_TOOLS_DIRECTORY"] / "linux-amd64" / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(header)
    shutil.copytree(ROOT / "k8s/upstream", root / "k8s/upstream")


class BootstrapManifestPackagingTests(unittest.TestCase):
    def test_platform_archive_contains_verified_bootstrap_manifests_and_sources(self):
        with tempfile.TemporaryDirectory(dir=ROOT) as directory:
            root = Path(directory)
            prepare_platform(root)
            files = packaging["platform_base_files"](root, "1.2.3", "0" * 40)
            archive = root / "platform.tar.gz"
            packaging["archive"](archive, files)
            with tarfile.open(archive, "r:gz") as bundle:
                expected = {
                    *(f"k8s/upstream/{name}" for name in MANIFESTS),
                    "k8s/upstream/sources.json",
                    "upstream/pins.env",
                }
                self.assertTrue(expected.issubset(bundle.getnames()), bundle.getnames())
                for name in expected:
                    self.assertEqual(bundle.extractfile(name).read(), (root / name).read_bytes())
                sources = json.load(bundle.extractfile("k8s/upstream/sources.json"))
                for source in sources["manifests"]:
                    version, url, license_id = MANIFESTS[Path(source["path"]).name]
                    self.assertEqual(
                        (source["version"], source["url"], source["license"]),
                        (version, url, license_id),
                    )
                self.assertIn(
                    b"name: services.serving.knative.dev",
                    bundle.extractfile("k8s/upstream/serving-crds.yaml").read(),
                )
                self.assertIn(
                    b"name: net-kourier-controller",
                    bundle.extractfile("k8s/upstream/kourier.yaml").read(),
                )
                self.assertIn(
                    b"CONTROLLER_COMMIT=",
                    bundle.extractfile("upstream/pins.env").read(),
                )

    def test_package_preparation_rejects_changed_upstream_manifest(self):
        with tempfile.TemporaryDirectory(dir=ROOT) as directory:
            root = Path(directory)
            prepare_platform(root)
            (root / "k8s/upstream/serving-core.yaml").write_text(
                "apiVersion: v1\nkind: Namespace\nmetadata: {name: changed}\n"
            )
            with self.assertRaisesRegex(ValueError, "serving-core.yaml.*SHA-256"):
                packaging["platform_base_files"](root, "1.2.3", "0" * 40)

    def test_package_preparation_rejects_incomplete_or_unsafe_source_inventory(self):
        with tempfile.TemporaryDirectory(dir=ROOT) as directory:
            root = Path(directory)
            prepare_platform(root)
            path = root / "k8s/upstream/sources.json"
            original = json.loads(path.read_bytes())
            licensed = [
                {**entry, "license": "Apache-2.0"}
                for entry in original["manifests"]
            ]
            for entries in (
                licensed[:-1],
                licensed + [licensed[0]],
                [{**licensed[0], "path": "../serving-crds.yaml"}],
                [{key: value for key, value in licensed[0].items() if key != "license"}],
                [{**licensed[0], "license": "MIT"}],
            ):
                with self.subTest(entries=entries):
                    path.write_text(json.dumps({**original, "manifests": entries}))
                    with self.assertRaisesRegex(ValueError, "sources.json"):
                        packaging["platform_base_files"](root, "1.2.3", "0" * 40)

    def test_package_preparation_requires_every_bundled_manifest(self):
        with tempfile.TemporaryDirectory(dir=ROOT) as directory:
            root = Path(directory)
            prepare_platform(root)
            (root / "k8s/upstream/kourier.yaml").unlink()
            with self.assertRaisesRegex((ValueError, FileNotFoundError), "kourier.yaml"):
                packaging["platform_base_files"](root, "1.2.3", "0" * 40)


if __name__ == "__main__":
    unittest.main()
