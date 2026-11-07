#!/usr/bin/env python3
"""Package Lampyr CLI binaries, platform, and independently loadable providers."""

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import sys
import tarfile
import tempfile
import tomllib

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "providers"))
import snapshot_oci


PLATFORM_STATIC = [
    "LICENSE",
    "k8s/cert-manager-csi-driver.yaml",
    "k8s/fiber-router.yaml",
    "k8s/fiberd-tenant.yaml",
    "k8s/hyperlight-device-plugin.yaml",
    "k8s/serving-controller.yaml",
]
# Linux/amd64 for every platform target, but for ly-dev, which a
# darwin-arm64 platform carries built for macOS: the rest run where a
# snapshot is made, on a Linux KVM host or in a cluster's builder Pod. The
# helper records the facts a fiberd template is sealed with.
PLATFORM_BINARIES = [
    "hluk",
    "hluk-helper",
    "kernel",
    "ly-dev",
]
# What ly deploy seals and pushes fiberd templates with, and makes the
# fleet key with, on the user's machine, so each platform target carries
# its own build: just upstream::template-tools writes them under
# build/template-tools/TARGET from the pinned fiberd, and the platform
# carries them as build/NAME, where a checkout's
# just fiberd-template-tools puts them.
TEMPLATE_TOOLS = ("zygotectl", "grant-issuer")
TEMPLATE_TOOLS_DIRECTORY = "build/template-tools"
# Written by just upstream::controller-inputs; ly cluster install takes each
# fiberd-k8s-controller input an install is not given from them.
CONTROLLER_INPUTS = "build/controller-inputs.json"
CONTROLLER_MANIFEST = "build/fiberd-k8s-controller.yaml"
PINNED_IMAGE = re.compile(r"[^@\s]+@sha256:[0-9a-f]{64}")
MANIFEST_IMAGE = re.compile(r"^\s+(?:- )?image:\s*[\"']?([^\s\"']+)", re.MULTILINE)
LOCAL_REGISTRY = re.compile(r"(?:localhost|127(?:\.\d{1,3}){3}|\[::1\])(?::\d+)?")
PREVIEW_IMAGE = re.compile(r"(ghcr\.io/hyperlight-dev/lampyr/[a-z0-9-]+):preview\b")
MAX_ARCHIVE = 1 << 30
MAX_FILE = 1 << 30
MAX_TOTAL = 4 << 30
MAX_ENTRIES = 100_000
PROVIDER_NAMES = ("kind", "k3d", "azure")
CLI_PLATFORMS = ("linux-amd64", "aarch64-apple-darwin")
PLATFORM_TARGETS = ("linux-amd64", "darwin-arm64")


def regular(path):
    if path.resolve(strict=True) != Path(os.path.abspath(path)) or not path.is_file():
        raise ValueError(f"packaging requires a regular file: {path}")
    return path


def digest(path):
    with regular(path).open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def write_checksum(path):
    checksum = digest(path)
    path.with_name(path.name + ".sha256").write_text(f"{checksum}  {path.name}\n")


def linux_amd64(path):
    with regular(path).open("rb") as source:
        header = source.read(20)
    if header[:6] != b"\x7fELF\x02\x01" or header[18:20] != b"\x3e\x00":
        raise ValueError(f"packaging requires a Linux/amd64 ELF: {path}")


def macos_arm64(path):
    with regular(path).open("rb") as source:
        header = source.read(8)
    if header != b"\xcf\xfa\xed\xfe\x0c\x00\x00\x01":
        raise ValueError(f"packaging requires a macOS/arm64 Mach-O executable: {path}")


def validate_cli_binary(path, platform):
    if platform == "linux-amd64":
        linux_amd64(path)
    elif platform == "aarch64-apple-darwin":
        macos_arm64(path)
    else:
        raise ValueError(f"unsupported CLI platform: {platform}")


def check_prepared_source(root, name, manifest):
    """Reject a prepared provider that was not built from this checkout.

    The release records this checkout's commit, so the packaged program and
    declared identity must be the ones in providers/NAME.
    """
    source = root / "providers" / name
    declared = snapshot_oci.read_json(source / "artifact.json")
    snapshot_fields = {
        "snapshotIndex",
        "snapshotRepository",
    }
    package_fields = {"rootfs", "program", "kernel", "coldCompatibility"}
    for key, value in declared.items():
        # Canonical JSON, so true != 1 and 2048.0 != 2048 as they are for the Rust loader.
        if key not in snapshot_fields and (
            key not in manifest or snapshot_oci.json_bytes(manifest[key]) != snapshot_oci.json_bytes(value)
        ):
            raise ValueError(f"prepared {name} provider {key} differs from providers/{name}/artifact.json")
    # package.py only adds file digests and snapshot fields to the declared manifest.
    extra = set(manifest) - set(declared) - snapshot_fields - package_fields
    if extra:
        raise ValueError(f"prepared {name} provider declares {', '.join(sorted(extra))} not in providers/{name}/artifact.json")
    with regular(source / "provider.py").open("rb") as program:
        checksum = hashlib.file_digest(program, "sha256").hexdigest()
    if manifest["program"]["sha256"] != checksum:
        raise ValueError(f"prepared {name} provider program differs from providers/{name}/provider.py")


def provider_files(root, name, artifacts=None):
    directory = artifacts / name if artifacts is not None else root / "providers" / name / "build/rootfs"
    manifest_path = regular(directory / "manifest.json")
    manifest = snapshot_oci.read_json(manifest_path)
    if manifest["id"] != name or manifest["protocolVersion"] != 1:
        raise ValueError(f"incompatible {name} provider")
    if manifest.get("runtimeVersion") != snapshot_oci.RUNTIME_VERSION:
        raise ValueError(f"incompatible {name} provider runtime")
    _, files = snapshot_oci.cold_files(directory)
    if artifacts is not None:
        check_prepared_source(root, name, manifest)
    if manifest.get("snapshotIndex") is not None:
        index = manifest["snapshotIndex"]
        snapshot_oci.checked_file_descriptor(index)
        path = directory / snapshot_oci.relative_path(index["path"])
        if path.name != "index.json":
            raise ValueError("provider snapshot index must name index.json")
        try:
            path.lstat()
        except FileNotFoundError:
            pass
        else:
            _, files = snapshot_oci.layout_files(directory)
            return version_name(manifest["version"]), files
        if not re.fullmatch(r"[0-9a-f]{64}", index["sha256"]):
            raise ValueError("invalid provider snapshot index digest")
    return version_name(manifest["version"]), files


def provider_inputs(root, name, artifacts):
    directory = artifacts / name if artifacts is not None else root / "providers" / name / "build/rootfs"
    manifest = snapshot_oci.read_json(directory / "manifest.json")
    return manifest, provider_files(root, name, artifacts)


def freeze_provider(root, name, artifacts, staging, validated=None):
    manifest, (version, files) = provider_inputs(root, name, artifacts) if validated is None else validated
    if any(path == "LICENSE" or path.startswith("LICENSE/") for path in files):
        raise ValueError("provider asset conflicts with reserved release LICENSE path")
    archive_inputs(files)
    destination = staging / name
    destination.mkdir()
    for relative, source in files.items():
        path = destination / snapshot_oci.relative_path(relative)
        path.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(regular(source), path)
    if snapshot_oci.json_bytes(snapshot_oci.read_json(destination / "manifest.json")) != snapshot_oci.json_bytes(manifest):
        raise ValueError("provider manifest changed during release staging")
    frozen_version, frozen_files = provider_files(root, name, staging)
    if frozen_version != version or frozen_files.keys() != files.keys():
        raise ValueError("provider inputs changed during release staging")
    return frozen_version, frozen_files


def validate_platform_tool(path, platform_target):
    if platform_target == "linux-amd64":
        linux_amd64(path)
    elif platform_target == "darwin-arm64":
        macos_arm64(path)
    else:
        raise ValueError(f"unsupported platform target: {platform_target}")


def platform_base_files(root, version, source_commit, image_tag=None, platform_target="linux-amd64"):
    files = {name: regular(root / name) for name in PLATFORM_STATIC}
    if image_tag is not None:
        for name, path in files.items():
            if name.endswith(".yaml"):
                text = PREVIEW_IMAGE.sub(rf"\g<1>:{image_tag}", path.read_text())
                files[name] = text.encode()
    for name in PLATFORM_BINARIES:
        if name == "ly-dev" and platform_target == "darwin-arm64":
            source = root / "build/ly-dev-darwin-arm64"
            macos_arm64(source)
        else:
            source = root / "build" / name
            linux_amd64(source)
        files[f"build/{name}"] = source
    for name in TEMPLATE_TOOLS:
        source = root / TEMPLATE_TOOLS_DIRECTORY / platform_target / name
        validate_platform_tool(source, platform_target)
        files[f"build/{name}"] = source
    files.update(controller_input_files(root))
    files[".lampyr-platform"] = (version + "\n").encode()
    files[".lampyr-platform-commit"] = (source_commit + "\n").encode()
    return files


def controller_input_files(root):
    """The controller inputs the platform installs by default, if any were written.

    A release writes them from the images it published; a checkout packaged
    without them makes a platform that needs every input given.
    """
    inputs = root / CONTROLLER_INPUTS
    try:
        inputs.lstat()
    except FileNotFoundError:
        return {}
    document = json.loads(regular(inputs).read_text())
    if (not isinstance(document, dict)
            or set(document) != {"controllerManifest", "grantImage", "helperImage"}):
        raise ValueError(f"{CONTROLLER_INPUTS} must give controllerManifest, grantImage and helperImage")
    if document["controllerManifest"] != CONTROLLER_MANIFEST:
        raise ValueError(f"{CONTROLLER_INPUTS} must name {CONTROLLER_MANIFEST}")
    for field in ("grantImage", "helperImage"):
        if not isinstance(document[field], str) or not PINNED_IMAGE.fullmatch(document[field]):
            raise ValueError(f"{CONTROLLER_INPUTS} {field} must be pinned by a sha256 digest")
    manifest = regular(root / CONTROLLER_MANIFEST)
    # just use-cluster-inputs writes inputs a registry on this host serves,
    # which no other machine can pull, so a platform must not default to them.
    images = [document["grantImage"], document["helperImage"]]
    images += MANIFEST_IMAGE.findall(manifest.read_text())
    for image in images:
        if LOCAL_REGISTRY.fullmatch(image.split("/", 1)[0]):
            raise ValueError(
                f"{CONTROLLER_INPUTS} names {image} on a local registry; remove "
                f"{CONTROLLER_INPUTS} or write published inputs with "
                "just upstream::controller-inputs before packaging"
            )
    return {CONTROLLER_INPUTS: inputs, CONTROLLER_MANIFEST: manifest}


def commit_name(value):
    if not re.fullmatch(r"[0-9a-f]{40}", value):
        raise ValueError("invalid source commit")
    return value


def image_tag_name(value):
    if not re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}", value):
        raise ValueError("invalid image tag")
    return value


def version_name(value):
    if not isinstance(value, str) or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][A-Za-z0-9.-]+)?", value):
        raise ValueError("invalid package version")
    return value


def archive_inputs(files):
    if len(files) > MAX_ENTRIES:
        raise ValueError("platform archive has too many entries")
    total = 0
    for name, source in files.items():
        snapshot_oci.relative_path(name)
        if re.fullmatch(r"[A-Za-z0-9._/@+=-]+", name) is None:
            raise ValueError("platform archive has an unsafe installer path")
        size = len(source) if isinstance(source, bytes) else regular(source).stat().st_size
        total += size
        if size > MAX_FILE or total > MAX_TOTAL:
            raise ValueError("platform archive exceeds installer file or total size limits")


def archive(output, files):
    archive_inputs(files)
    with tempfile.NamedTemporaryFile(
        dir=output.parent, prefix=".package-", delete=False
    ) as temporary:
        staging = Path(temporary.name)
        try:
            with gzip.GzipFile(
                filename="", fileobj=temporary, mode="wb", mtime=0
            ) as compressed:
                with tarfile.open(fileobj=compressed, mode="w", dereference=True) as bundle:
                    written_total = 0
                    for name, source in sorted(files.items()):
                        if isinstance(source, bytes):
                            info = tarfile.TarInfo(name)
                            info.size = len(source)
                            info.mode = 0o644
                            written_total += info.size
                            if info.size > MAX_FILE or written_total > MAX_TOTAL:
                                raise ValueError("platform archive exceeds installer file or total size limits")
                            bundle.addfile(info, io.BytesIO(source))
                        else:
                            regular(source)
                            info = bundle.gettarinfo(str(source), arcname=name)
                            info.mode = 0o755 if info.mode & 0o111 else 0o644
                            info.uid = info.gid = info.mtime = 0
                            info.uname = info.gname = ""
                            written_total += info.size
                            if info.size > MAX_FILE or written_total > MAX_TOTAL:
                                raise ValueError("platform archive exceeds installer file or total size limits")
                            with source.open("rb") as content:
                                bundle.addfile(info, content)
            temporary.flush()
            if temporary.tell() > MAX_ARCHIVE:
                raise ValueError("platform archive exceeds installer compressed size limit")
            staging.replace(output)
        finally:
            staging.unlink(missing_ok=True)
    write_checksum(output)
    print(output)


def release_aliases(output, suffix, version, provider_versions):
    """Copy versioned archives to one uniform release-asset suffix."""
    assets = {
        f"lampyr-cli-linux-amd64-{suffix}.tar.gz": (
            f"lampyr-cli-linux-amd64-{version}.tar.gz"
        ),
        f"lampyr-platform-linux-amd64-{suffix}.tar.gz": (
            f"lampyr-platform-linux-amd64-{version}.tar.gz"
        ),
        **{
            f"lampyr-provider-{name}-{suffix}.tar.gz": (
                f"lampyr-provider-{name}-{provider_version}.tar.gz"
            )
            for name, provider_version in provider_versions.items()
        },
    }
    sources = {
        alias_name: regular(output / versioned_name)
        for alias_name, versioned_name in assets.items()
    }
    for alias_name, source in sorted(sources.items()):
        if alias_name == source.name:
            continue
        alias = output / alias_name
        with tempfile.NamedTemporaryFile(
            dir=output, prefix=".package-", delete=False
        ) as temporary:
            staging = Path(temporary.name)
            try:
                with source.open("rb") as content:
                    shutil.copyfileobj(content, temporary)
                temporary.flush()
                staging.replace(alias)
            finally:
                staging.unlink(missing_ok=True)
        write_checksum(alias)
        print(alias)


def cli_alias(output, suffix, version, platform):
    source = regular(output / f"lampyr-cli-{platform}-{version}.tar.gz")
    alias = output / f"lampyr-cli-{platform}-{suffix}.tar.gz"
    if alias == source:
        return
    with tempfile.NamedTemporaryFile(
        dir=output, prefix=".package-", delete=False
    ) as temporary:
        staging = Path(temporary.name)
        try:
            with source.open("rb") as content:
                shutil.copyfileobj(content, temporary)
            temporary.flush()
            staging.replace(alias)
        finally:
            staging.unlink(missing_ok=True)
    write_checksum(alias)
    print(alias)


def platform_alias(output, suffix, version, platform):
    source = regular(output / f"lampyr-platform-{platform}-{version}.tar.gz")
    alias = output / f"lampyr-platform-{platform}-{suffix}.tar.gz"
    if alias == source:
        return
    with tempfile.NamedTemporaryFile(
        dir=output, prefix=".package-", delete=False
    ) as temporary:
        staging = Path(temporary.name)
        try:
            with source.open("rb") as content:
                shutil.copyfileobj(content, temporary)
            temporary.flush()
            staging.replace(alias)
        finally:
            staging.unlink(missing_ok=True)
    write_checksum(alias)
    print(alias)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--cli-only", action="store_true")
    parser.add_argument("--platform-only", action="store_true")
    parser.add_argument(
        "--cli-platform",
        choices=CLI_PLATFORMS,
        default="linux-amd64",
    )
    parser.add_argument(
        "--platform-target",
        choices=PLATFORM_TARGETS,
        default="linux-amd64",
    )
    parser.add_argument(
        "--aliases",
        metavar="SUFFIX",
        help="copy existing versioned archives to lampyr-*-SUFFIX.tar.gz",
    )
    parser.add_argument(
        "--image-tag",
        help="replace :preview platform manifest image tags with this tag",
    )
    parser.add_argument("--source-commit")
    parser.add_argument(
        "--provider-artifacts", type=Path,
        help="use prepared kind/, k3d/ and azure/ artifacts instead of provider build/rootfs directories",
    )
    args = parser.parse_args()
    if args.cli_only and args.platform_only:
        parser.error("--cli-only and --platform-only cannot be combined")
    if not args.platform_only and args.platform_target != "linux-amd64":
        parser.error("--platform-target requires --platform-only")
    root = args.root.resolve(strict=True)
    # Input checks reject symlinked paths; resolve the directories given here once so a checkout or
    # output reached through a symlinked directory is still accepted.
    args.output = snapshot_oci.resolved_parent(args.output)
    if args.binary is not None:
        args.binary = Path(os.path.realpath(args.binary))
    version = version_name(
        tomllib.loads((root / "cli/Cargo.toml").read_text())["package"]["version"]
    )
    if args.aliases is not None:
        suffix = image_tag_name(args.aliases)
        if args.cli_only:
            cli_alias(args.output, suffix, version, args.cli_platform)
            return
        if args.platform_only:
            platform_alias(args.output, suffix, version, args.platform_target)
            return
        try:
            packaged = snapshot_oci.read_json(args.output / ".provider-versions.json")
        except FileNotFoundError:
            parser.error(f"{args.output} has no packaged provider versions; package before creating aliases")
        providers = packaged.get("providers")
        if (set(packaged) != {"platform", "providers"} or not isinstance(providers, dict)
                or set(providers) != set(PROVIDER_NAMES)):
            raise ValueError("invalid packaged provider versions")
        # The CLI and platform archive names come from this checkout's version, so they must be the
        # ones the marker's packaging run produced.
        if packaged["platform"] != version:
            parser.error(f"{args.output} was packaged for version {packaged['platform']!r}, not {version}; "
                         "package this checkout before creating aliases")
        providers = {name: version_name(value) for name, value in providers.items()}
        release_aliases(args.output, suffix, version, providers)
        return
    if args.cli_only:
        if args.binary is None:
            parser.error("--binary is required when packaging a CLI")
        binary = Path(os.path.realpath(args.binary))
        validate_cli_binary(binary, args.cli_platform)
        args.output.mkdir(parents=True, exist_ok=True)
        archive(
            args.output / f"lampyr-cli-{args.cli_platform}-{version}.tar.gz",
            {"ly": binary, "LICENSE": root / "LICENSE"},
        )
        return
    # Alias creation above never reads prepared artifacts, so resolve them only when packaging.
    if args.provider_artifacts is not None:
        args.provider_artifacts = args.provider_artifacts.resolve(strict=True)
    if args.source_commit is None:
        parser.error("--source-commit is required unless --aliases is set")
    source_commit = commit_name(args.source_commit)
    image_tag = None if args.image_tag is None else image_tag_name(args.image_tag)
    binary = None
    if not args.platform_only:
        if args.binary is None:
            parser.error("--binary is required unless --aliases or --platform-only is set")
        binary = args.binary.absolute()
        linux_amd64(binary)
    files = platform_base_files(root, version, source_commit, image_tag, args.platform_target)
    inputs = {name: provider_inputs(root, name, args.provider_artifacts) for name in PROVIDER_NAMES}
    combined = dict(files)
    for name, (_, (_, artifact)) in inputs.items():
        combined.update({f"providers/{name}/artifact/{path}": source
                         for path, source in artifact.items()})
    archive_inputs(combined)
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".providers-", dir=args.output) as temporary:
        staging = Path(temporary)
        providers = {
            name: freeze_provider(root, name, args.provider_artifacts, staging, inputs[name])
            for name in PROVIDER_NAMES
        }
        for name, (_, artifact) in providers.items():
            files.update(
                {
                    f"providers/{name}/artifact/{path}": source
                    for path, source in artifact.items()
                }
            )
        if not args.platform_only:
            (args.output / ".provider-versions.json").unlink(missing_ok=True)
        if not args.platform_only:
            archive(
                args.output / f"lampyr-cli-linux-amd64-{version}.tar.gz",
                {"ly": binary, "LICENSE": root / "LICENSE"},
            )
        archive(
            args.output / f"lampyr-platform-{args.platform_target}-{version}.tar.gz",
            files,
        )
        if args.platform_only:
            return
        for name, (provider_version, artifact) in providers.items():
            archive(
                args.output / f"lampyr-provider-{name}-{provider_version}.tar.gz",
                {**artifact, "LICENSE": root / "LICENSE"},
            )
        with tempfile.NamedTemporaryFile(dir=args.output, prefix=".versions-", delete=False) as temporary:
            metadata = Path(temporary.name)
            try:
                temporary.write(snapshot_oci.json_bytes({
                    "platform": version,
                    "providers": {
                        name: provider_version for name, (provider_version, _) in providers.items()
                    },
                }))
                temporary.flush()
                metadata.replace(args.output / ".provider-versions.json")
            finally:
                metadata.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
