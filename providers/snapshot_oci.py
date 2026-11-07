#!/usr/bin/env python3
"""Assemble provider snapshot OCI layouts offline; publish only on explicit request."""

import argparse
import errno
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tempfile

from permissions import others_can_write


INDEX = "application/vnd.oci.image.index.v1+json"
MANIFEST = "application/vnd.oci.image.manifest.v1+json"
CONFIG = "application/vnd.hyperlight.snapshot.config.v5+json"
MEMORY = "application/vnd.hyperlight.snapshot.memory.v2"
TRANSPORT = "application/vnd.hyperlight.snapshot.transport.v1"
ARCH = "dev.hyperlight.snapshot.arch"
REF_NAME = "org.opencontainers.image.ref.name"
HYPERVISOR = "dev.hyperlight.snapshot.hypervisor"
CPU_VENDOR = "dev.hyperlight.snapshot.cpu.vendor"
# Must match VERSION in cli/src/assets.rs (checked by cli/tests/artifact.rs).
RUNTIME_VERSION = "0.21.0"
JSON_LIMIT = 64 * 1024
LAYER_LIMIT = 4 << 30  # What a snapshot's memory layers may total.
# hyperlight-host 0.19 writes memory as up to 30 data layers (its MAX_SNAPSHOT_LAYERS), plus
# one blob of page tables alone when no data layer holds them, then one transport layer.
MAX_MEMORY_LAYERS = 31


def json_bytes(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True,
                      allow_nan=False).encode()


def json_object(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError("provider JSON contains duplicate keys")
        value[key] = item
    return value


def json_constant(_):
    raise ValueError("provider JSON contains a non-JSON constant")


def unicode_scalars(value):
    if isinstance(value, str):
        value.encode("utf-8")
    elif isinstance(value, dict):
        for key, item in value.items():
            unicode_scalars(key)
            unicode_scalars(item)
    elif isinstance(value, list):
        for item in value:
            unicode_scalars(item)


def read_json(path, expected_hash=None):
    with regular(path).open("rb") as source:
        data = source.read(JSON_LIMIT + 1)
    if len(data) > JSON_LIMIT:
        raise ValueError("provider JSON exceeds 64 KiB")
    if expected_hash is not None and hashlib.sha256(data).hexdigest() != expected_hash:
        raise ValueError("provider JSON digest mismatch")
    value = json.loads(data.decode("utf-8"), object_pairs_hook=json_object,
                       parse_constant=json_constant)
    unicode_scalars(value)
    if not isinstance(value, dict):
        raise ValueError("provider JSON must be an object")
    return value


def regular(path):
    if path.resolve(strict=True) != Path(os.path.abspath(path)) or not path.is_file():
        raise ValueError("provider inputs must be regular files without symlinks")
    return path


def file_hash(path):
    with regular(path).open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def relative_path(value):
    if (not isinstance(value, str) or not value or len(value.encode("utf-8")) > 1024
            or "\\" in value or "\0" in value):
        raise ValueError("unsafe provider file path")
    path = Path(value)
    if path.is_absolute() or any(part in ("", ".", "..", ".git", ".hg", ".svn")
                                 for part in value.split("/")):
        raise ValueError("unsafe provider file path")
    return path


def checked_file_descriptor(item):
    if (not isinstance(item, dict) or set(item) != {"path", "sha256"}
            or not isinstance(item["sha256"], str)
            or not re.fullmatch(r"[0-9a-f]{64}", item["sha256"])):
        raise ValueError("invalid provider file digest")
    relative_path(item["path"])


def checked_file(directory, item, limit):
    checked_file_descriptor(item)
    path = regular(directory / relative_path(item["path"]))
    if path.stat().st_size > limit or file_hash(path) != item["sha256"]:
        raise ValueError("provider file digest or size mismatch")
    return path


def checked_metadata(manifest):
    required = {"protocolVersion", "id", "version", "runtimeVersion", "memoryMiB",
                "rootfs", "program", "operations"}
    optional = {"kernel", "coldCompatibility", "snapshotIndex",
                "snapshotRepository", "environment", "networkHosts", "sourceKinds"}
    if not required <= manifest.keys() or manifest.keys() - required - optional:
        raise ValueError("invalid provider manifest fields")
    if type(manifest["protocolVersion"]) is not int or manifest["protocolVersion"] != 1:
        raise ValueError("unsupported provider protocol version")
    if manifest["runtimeVersion"] != RUNTIME_VERSION:
        raise ValueError("incompatible provider runtime")
    if not isinstance(manifest["id"], str) or not re.fullmatch(r"[A-Za-z0-9-]{1,64}", manifest["id"]):
        raise ValueError("invalid provider identity")
    if not isinstance(manifest["version"], str) or not manifest["version"]:
        raise ValueError("provider artifact version is missing")
    if type(manifest["memoryMiB"]) is not int or not 128 <= manifest["memoryMiB"] <= 4096:
        raise ValueError("provider memory must be 128 to 4096 MiB")
    for field in ("rootfs", "program", "kernel", "snapshotIndex"):
        if field in ("rootfs", "program") or manifest.get(field) is not None:
            checked_file_descriptor(manifest[field])
    cold = manifest.get("coldCompatibility")
    if cold is not None and (
        not isinstance(cold, dict)
        or set(cold) != {"os", "architecture"}
        or any(
            not isinstance(cold[field], str)
            or not re.fullmatch(r"[a-z0-9_]{1,32}", cold[field])
            for field in ("os", "architecture")
        )
    ):
        raise ValueError("provider cold-boot compatibility is invalid")
    repository = manifest.get("snapshotRepository")
    if repository is not None:
        if manifest.get("snapshotIndex") is None:
            raise ValueError("provider snapshot repository requires a snapshot index")
        repository_name(repository)
    operations = manifest["operations"]
    allowed_operations = {"describe", "init", "apply", "inspect", "connect", "delete",
                          "source-create", "source-status", "source-delete"}
    allowed_capabilities = {"workspace-read", "workspace-scaffold", "state", "network",
                            "auth-cache", "docker", "provider-manifests"}
    if (not isinstance(operations, dict) or "describe" not in operations
            or operations.keys() - allowed_operations
            or any(not isinstance(items, list)
                   or any(not isinstance(item, str) or item not in allowed_capabilities for item in items)
                   for items in operations.values())
            or operations["describe"]
            or any("provider-manifests" in items and operation != "apply"
                   for operation, items in operations.items())):
        raise ValueError("invalid provider operations or capabilities")
    environment = manifest.get("environment", {})
    if (not isinstance(environment, dict) or len(environment) > 32
            or any(not re.fullmatch(r"[A-Za-z0-9_]{1,128}", name)
                   or not isinstance(value, str) or len(value.encode("utf-8")) > 4096
                   or "\0" in value for name, value in environment.items())):
        raise ValueError("invalid provider environment variable")
    hosts = manifest.get("networkHosts", [])
    if (not isinstance(hosts, list) or len(hosts) > 64
            or any(not isinstance(host, str) or len(host.encode("utf-8")) > 253
                   or any(not re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?", label)
                          for label in host.split(".")) for host in hosts)):
        raise ValueError("invalid provider network hostname")
    kinds = manifest.get("sourceKinds", [])
    if not isinstance(kinds, list):
        raise ValueError("invalid provider Event Source kinds")
    distinct = set()
    for resource in kinds:
        if (not isinstance(resource, dict) or set(resource) != {"apiVersion", "kind"}
                or not isinstance(resource["apiVersion"], str)
                or not isinstance(resource["kind"], str)):
            raise ValueError("invalid provider Event Source kind")
        group, separator, version = resource["apiVersion"].partition("/")
        kind = resource["kind"]
        safe_knative = group == "eventing.knative.dev" and kind in ("Broker", "Trigger")
        provider_owned = "." in group and not group.endswith((".k8s.io", ".kubernetes.io", ".knative.dev"))
        if (not separator or not (safe_knative or provider_owned)
                or not re.fullmatch(r"[A-Za-z0-9]+", version)
                or not re.fullmatch(r"[A-Za-z][A-Za-z0-9]*", kind)):
            raise ValueError("provider Event Source kind is not safe")
        distinct.add((resource["apiVersion"], kind))
    if len(distinct) > 32 or (operations.keys() & {"source-create", "source-status", "source-delete"} and not distinct):
        raise ValueError("provider source operations require safe Event Source kinds")


def cold_files(directory):
    manifest = read_json(directory / "manifest.json")
    checked_metadata(manifest)
    files = {"manifest.json": directory / "manifest.json"}
    for field, limit in (("rootfs", 2 << 30), ("program", JSON_LIMIT), ("kernel", 64 << 20)):
        item = manifest.get(field)
        if field == "kernel" and item is None:
            continue
        path = checked_file(directory, item, limit)
        if field == "program":
            path.read_text(encoding="utf-8")
        if item["path"] in files:
            raise ValueError("duplicate provider file path")
        files[item["path"]] = path
    return manifest, files


def identity(manifest):
    return {key: value for key, value in manifest.items()
            if key not in ("rootfs", "kernel", "coldCompatibility",
                           "snapshotIndex", "snapshotRepository")}


def descriptor(media_type, content):
    return {
        "mediaType": media_type,
        "digest": "sha256:" + hashlib.sha256(content).hexdigest(),
        "size": len(content),
    }


def write_blob(output, media_type, content):
    item = descriptor(media_type, content)
    (output / "blobs/sha256" / item["digest"][7:]).write_bytes(content)
    return item


def copy_blob(output, source, item):
    destination = output / "blobs/sha256" / item["digest"][7:]
    if destination.exists():
        return item
    shutil.copyfile(source, destination)
    if file_hash(destination) != item["digest"][7:]:
        raise ValueError("snapshot changed during assembly")
    return item


def copy_cold(output, manifest, files):
    for name, source in files.items():
        if name == "manifest.json":
            continue
        destination = output / relative_path(name)
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, destination)
    (output / "manifest.json").write_bytes(json_bytes(manifest) + b"\n")
    cold_files(output)


def staging_directory(output):
    if output.exists() or output.is_symlink():
        raise ValueError("output must not already exist")
    secure_output_parent(output)
    return tempfile.TemporaryDirectory(prefix=".snapshot-oci-", dir=output.parent)


def secure_output_parent(output):
    parent = output.parent
    missing = []
    existing = parent
    while True:
        try:
            metadata = existing.lstat()
            break
        except FileNotFoundError:
            missing.append(existing)
            if existing.parent == existing:
                raise ValueError("snapshot output has no existing ancestor")
            existing = existing.parent
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        raise ValueError("snapshot output ancestor must be a real directory")
    if os.name == "posix":
        user = os.geteuid()
        child_uid = user
        for ancestor in (existing, *existing.parents):
            metadata = ancestor.lstat()
            if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
                raise ValueError("snapshot output ancestor must be a real directory")
            if metadata.st_uid not in (user, 0):
                raise ValueError("snapshot output ancestor must be owned by the current user or root")
            reason = others_can_write(ancestor, metadata)
            if reason is not None:
                if not metadata.st_mode & 0o1000 or child_uid not in (user, 0):
                    raise ValueError(
                        f"snapshot output ancestor {ancestor} must not be writable by other users: {reason}; "
                        f"run `chmod go-w {ancestor}`"
                    )
            child_uid = metadata.st_uid
    for directory in reversed(missing):
        try:
            directory.mkdir(mode=0o700)
        except FileExistsError:
            pass
        metadata = directory.lstat()
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
            raise ValueError("snapshot output parent must be a real directory")
        if os.name == "posix" and metadata.st_uid != os.geteuid():
            raise ValueError("snapshot output parent must be owned by the current user")
        directory.chmod(0o700)


def restrict_output_permissions(output):
    metadata = output.lstat()
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        raise ValueError("snapshot output must be a real directory")
    if os.name == "posix" and metadata.st_uid != os.geteuid():
        raise ValueError("snapshot output must be owned by the current user")

    def raise_walk_error(error):
        raise error

    for root, directories, files in os.walk(
        output, followlinks=False, onerror=raise_walk_error
    ):
        for name in directories:
            path = Path(root) / name
            mode = path.lstat().st_mode
            if stat.S_ISLNK(mode) or not stat.S_ISDIR(mode):
                raise ValueError("snapshot outputs may contain only regular files and directories")
            path.chmod(stat.S_IMODE(mode) & ~0o022)
        for name in files:
            path = Path(root) / name
            mode = path.lstat().st_mode
            if stat.S_ISLNK(mode) or not stat.S_ISREG(mode):
                raise ValueError("snapshot outputs may contain only regular files and directories")
            path.chmod(stat.S_IMODE(mode) & ~0o022)
    output.chmod(0o700)


def assemble(artifact, variants, output):
    output = resolved_parent(output)
    manifest, files = cold_files(artifact)
    if len(variants) > 128:
        raise ValueError("at most 128 snapshot variants are supported")
    with staging_directory(output) as temporary:
        staging = Path(temporary) / "artifact"
        (staging / "blobs/sha256").mkdir(parents=True)
        descriptors = []
        seen = set()
        for variant in variants:
            candidate, _ = layout_files(variant)
            if json_bytes(identity(candidate)) != json_bytes(identity(manifest)):
                raise ValueError("snapshot variant logical provider identity differs")
            native_index = variant / candidate["snapshotIndex"]["path"]
            native_layout = native_index.parent
            index = read_json(native_index, candidate["snapshotIndex"]["sha256"])
            if len(descriptors) + len(index["manifests"]) > 128:
                raise ValueError("at most 128 snapshot variants are supported")
            for native in index["manifests"]:
                annotations = native["annotations"]
                key = tuple(annotations[name] for name in (ARCH, HYPERVISOR, CPU_VENDOR, REF_NAME))
                if key in seen:
                    raise ValueError("duplicate snapshot compatibility tuple")
                seen.add(key)
                image = read_json(native_layout / "blobs/sha256" / native["digest"][7:],
                                  native["digest"][7:])
                for item in [native, image["config"], *image["layers"]]:
                    source = native_layout / "blobs/sha256" / item["digest"][7:]
                    copy_blob(staging, source, item)
                item = dict(native)
                item["platform"] = {
                    "os": {
                        "hvf": "darwin",
                        "whp": "windows",
                    }.get(annotations[HYPERVISOR], "linux"),
                    "architecture": {"x86_64": "amd64", "aarch64": "arm64"}.get(
                        annotations[ARCH], annotations[ARCH]),
                }
                descriptors.append(item)
        if len(descriptors) > 128:
            raise ValueError("at most 128 snapshot variants are supported")
        index = json_bytes({
            "schemaVersion": 2, "mediaType": INDEX,
            "manifests": sorted(descriptors, key=lambda item: (
                item["platform"]["os"], item["platform"]["architecture"],
                item["annotations"][HYPERVISOR], item["annotations"][CPU_VENDOR],
                item["annotations"][REF_NAME], item["digest"],
            )),
        })
        if len(index) > JSON_LIMIT:
            raise ValueError("snapshot index exceeds 64 KiB")
        item = write_blob(staging, INDEX, index)
        (staging / "index.json").write_bytes(index)
        (staging / "oci-layout").write_bytes(json_bytes({"imageLayoutVersion": "1.0.0"}))
        manifest["snapshotIndex"] = {"path": "index.json", "sha256": item["digest"][7:]}
        if any(name in ("snapshot-index.json", "index.json", "oci-layout", "blobs") or name.startswith("blobs/") for name in files):
            raise ValueError("cold-boot asset conflicts with OCI layout")
        copy_cold(staging, manifest, files)
        layout_files(staging)
        restrict_output_permissions(staging)
        staging.rename(output)
    return item["digest"]


def checked_blob(directory, item, media_type, limit, verified=None):
    checked_annotations(item)
    if item["mediaType"] != media_type or not re.fullmatch(r"sha256:[0-9a-f]{64}", item["digest"]):
        raise ValueError("unsupported snapshot OCI descriptor")
    if type(item["size"]) is not int or not 0 <= item["size"] <= limit:
        raise ValueError("snapshot OCI descriptor exceeds size limit")
    path = regular(directory / "blobs/sha256" / item["digest"][7:])
    stat = path.stat()
    if stat.st_size != item["size"]:
        raise ValueError("snapshot OCI descriptor digest or size mismatch")
    key = (path, item["digest"], stat.st_dev, stat.st_ino, stat.st_size,
           stat.st_mtime_ns, stat.st_ctime_ns)
    if verified is None or key not in verified:
        if file_hash(path) != item["digest"][7:]:
            raise ValueError("provider file digest or size mismatch")
        if verified is not None:
            verified.add(key)
    return path


def checked_annotations(item):
    annotations = item.get("annotations", {})
    if not isinstance(annotations, dict) or not all(
        isinstance(value, str) for value in annotations.values()
    ):
        raise ValueError("snapshot descriptor annotations must be strings")
    return annotations


def layout_files(directory):
    """Validate all variants, not just the current host's selected variant."""
    manifest, files = cold_files(directory)
    index_path = checked_file(directory, manifest["snapshotIndex"], JSON_LIMIT)
    index = read_json(index_path, manifest["snapshotIndex"]["sha256"])
    if (type(index.get("schemaVersion")) is not int or index["schemaVersion"] != 2
            or index.get("mediaType") != INDEX
            or not isinstance(index.get("manifests"), list) or len(index["manifests"]) > 128):
        raise ValueError("unsupported snapshot OCI index")
    if index_path.name != "index.json":
        raise ValueError("native snapshot index must name index.json")
    layout = index_path.parent
    verified = set()
    marker = layout / "oci-layout"
    if read_json(marker) != {"imageLayoutVersion": "1.0.0"}:
        raise ValueError("unsupported OCI layout")
    files[marker.relative_to(directory).as_posix()] = marker
    files[index_path.relative_to(directory).as_posix()] = index_path
    index_blob = layout / "blobs/sha256" / manifest["snapshotIndex"]["sha256"]
    if index_blob.exists() or index_blob.is_symlink():
        path = checked_blob(layout, {
            "mediaType": INDEX, "digest": "sha256:" + manifest["snapshotIndex"]["sha256"],
            "size": index_path.stat().st_size,
        }, INDEX, JSON_LIMIT)
        files[path.relative_to(directory).as_posix()] = path
    for item in index["manifests"]:
        annotations = checked_annotations(item)
        if not all(isinstance(value, str) and value for value in (
            annotations.get(ARCH), annotations.get(HYPERVISOR), annotations.get(CPU_VENDOR),
            annotations.get(REF_NAME),
        )):
            raise ValueError("snapshot variant has incomplete compatibility")
        path = checked_blob(layout, item, MANIFEST, JSON_LIMIT, verified)
        files[path.relative_to(directory).as_posix()] = path
        image = read_json(path, item["digest"][7:])
        if (type(image.get("schemaVersion")) is not int or image["schemaVersion"] != 2
                or image.get("mediaType") != MANIFEST
                or image.get("artifactType") != CONFIG
                or not isinstance(image.get("layers"), list)
                or not 2 <= len(image["layers"]) <= MAX_MEMORY_LAYERS + 1):
            raise ValueError("unsupported snapshot OCI manifest")
        config = checked_blob(layout, image["config"], CONFIG, JSON_LIMIT, verified)
        configuration = read_json(config, image["config"]["digest"][7:])
        if any(configuration.get(field) != annotations.get(annotation) for field, annotation in (
            ("arch", ARCH), ("hypervisor", HYPERVISOR), ("cpu_vendor", CPU_VENDOR),
        )):
            raise ValueError("snapshot config does not match compatibility annotations")
        # The layers' kinds and declared total are checked before any memory layer is hashed.
        *memory_layers, transport_layer = image["layers"]
        if (not all(isinstance(layer, dict) for layer in image["layers"])
                or any(layer.get("mediaType") != MEMORY for layer in memory_layers)
                or transport_layer.get("mediaType") != TRANSPORT):
            raise ValueError("unsupported snapshot OCI manifest")
        sizes = [layer.get("size") for layer in memory_layers]
        if any(type(size) is not int or size < 0 for size in sizes):
            raise ValueError("unsupported snapshot OCI descriptor")
        if sum(sizes) > LAYER_LIMIT:
            raise ValueError("snapshot memory exceeds its size limit")
        memory = [checked_blob(layout, layer, MEMORY, LAYER_LIMIT, verified)
                  for layer in memory_layers]
        transport = checked_blob(layout, transport_layer, TRANSPORT, 2 << 20, verified)
        for path in (config, *memory, transport):
            files[path.relative_to(directory).as_posix()] = path
    return manifest, files


def repository_name(value):
    # Explicit host and repository only; no schemes, userinfo, tags or digests.
    server, separator, name = value.partition("/")
    if not separator:
        raise ValueError("repository requires a registry host and repository")
    if server.startswith("["):
        match = re.fullmatch(r"\[([0-9A-Fa-f:]+)\](?::([0-9]{1,5}))?", server)
        if match is None:
            raise ValueError("invalid bracketed registry host")
        ipaddress.IPv6Address(match[1])
        port = match[2]
    else:
        label = r"[a-z0-9](?:[a-z0-9-]*[a-z0-9])?"
        match = re.fullmatch(rf"({label}(?:[.]{label})*)(?::([0-9]{{1,5}}))?", server)
        if match is None or any(len(label) > 63 for label in match[1].split(".")):
            raise ValueError("invalid registry host")
        port = match[2]
    if port is not None and not 1 <= int(port) <= 65535:
        raise ValueError("invalid registry port")
    component = r"[a-z0-9]+(?:(?:[.]|_{1,2}|-+)[a-z0-9]+)*"
    if len(value) > 255 or not re.fullmatch(rf"{component}(?:/{component})*", name):
        raise ValueError("repository must be HOST/REPOSITORY without credentials, tag or digest")
    return value


ORAS_TIMEOUT = 600
ORAS_MIN_RATE = 1 << 20  # Bytes per second assumed for large copies.


def oras(arguments, timeout=ORAS_TIMEOUT, capture=False):
    # Registry/helper diagnostics can contain credentials, so they are shown only on request.
    verbose = os.environ.get("LAMPYR_ORAS_VERBOSE") == "1"
    try:
        result = subprocess.run(["oras", *arguments], check=True,
                                stdout=subprocess.PIPE if capture else subprocess.DEVNULL,
                                stderr=None if verbose else subprocess.DEVNULL, timeout=timeout)
    except FileNotFoundError:
        raise ValueError("publication requires ORAS 1.2+ on PATH") from None
    except subprocess.TimeoutExpired:
        raise ValueError(f"ORAS {arguments[0]} timed out after {timeout} seconds") from None
    except subprocess.CalledProcessError:
        raise ValueError("ORAS publication failed; check registry access and Docker credential "
                         "configuration, or set LAMPYR_ORAS_VERBOSE=1 to show ORAS diagnostics") from None
    return result.stdout if capture else None


def check_published_graph(layout, repository):
    """Confirm every manifest and blob of the frozen graph is in the registry, without downloading it."""
    index = read_json(layout / "blobs/sha256" / read_json(layout / "index.json")["manifests"][0]["digest"][7:])
    expected = []
    for item in index["manifests"]:
        expected.append(("manifest", item))
        image = read_json(layout / "blobs/sha256" / item["digest"][7:])
        expected.extend(("blob", blob) for blob in (image["config"], *image["layers"]))
    for kind, item in expected:
        output = oras([kind, "fetch", "--descriptor", f"{repository}@{item['digest']}"], capture=True)
        try:
            remote = json.loads(output)
        except (TypeError, ValueError):
            raise ValueError(f"published snapshot {kind} {item['digest']} has an unreadable descriptor") from None
        if not isinstance(remote, dict) or remote.get("digest") != item["digest"] or remote.get("size") != item["size"]:
            raise ValueError(f"published snapshot {kind} {item['digest']} is missing or differs")


def publish(artifact, repository, tag, output):
    output = resolved_parent(output)
    repository_name(repository)
    if not re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}", tag):
        raise ValueError("invalid publication tag")
    manifest, files = layout_files(artifact)
    index_path = artifact / manifest["snapshotIndex"]["path"]
    digest = "sha256:" + manifest["snapshotIndex"]["sha256"]
    if index_path.name != "index.json" or index_path.parent != artifact:
        raise ValueError("publication requires an assembled OCI layout")
    if read_json(artifact / "oci-layout") != {"imageLayoutVersion": "1.0.0"}:
        raise ValueError("unsupported OCI layout")
    with staging_directory(output) as temporary:
        temporary = Path(temporary)
        # Freeze verified inputs before any network operation.
        layout = temporary / "layout"
        layout.mkdir()
        for name, source in files.items():
            path = layout / name
            path.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, path)
        shutil.copyfile(artifact / "oci-layout", layout / "oci-layout")
        shutil.copyfile(artifact / "index.json", layout / "index.json")
        (layout / "blobs/sha256").mkdir(parents=True, exist_ok=True)
        frozen_manifest, _ = layout_files(layout)
        if json_bytes(frozen_manifest) != json_bytes(manifest):
            raise ValueError("provider manifest changed during publication staging")
        frozen_index = (layout / "index.json").read_bytes()
        item = descriptor(INDEX, frozen_index)
        if item["digest"] != digest:
            raise ValueError("snapshot index changed during publication staging")
        write_blob(layout, INDEX, frozen_index)
        checked_blob(layout, item, INDEX, JSON_LIMIT)
        # ORAS resolves the index as an index (not an opaque blob) via this catalog.
        (layout / "index.json").write_bytes(json_bytes({
            "schemaVersion": 2, "mediaType": INDEX, "manifests": [item],
        }))
        reference = f"{repository}@{digest}"
        size = sum(path.stat().st_size for path in (layout / "blobs/sha256").iterdir())
        oras(["cp", "--from-oci-layout", f"{layout}@{digest}", f"{repository}:{tag}"],
             timeout=max(ORAS_TIMEOUT, size // ORAS_MIN_RATE))
        fetched = temporary / "published-index.json"
        oras(["manifest", "fetch", reference, "--output", str(fetched)])
        if file_hash(fetched) != digest[7:]:
            raise ValueError("published snapshot index digest mismatch")
        check_published_graph(layout, repository)
        # Restore the native index after the temporary ORAS catalog has served
        # its purpose, then emit the complete local provider artifact.
        (layout / "index.json").write_bytes(frozen_index)
        frozen_manifest["snapshotRepository"] = repository
        (layout / "manifest.json").write_bytes(json_bytes(frozen_manifest) + b"\n")
        layout_files(layout)
        restrict_output_permissions(layout)
        layout.rename(output)
    return reference


def link_blob(source, destination):
    """Gives `destination` the content of blob `source`, a hard link where both are on one
    filesystem. A blob is named by its digest and never written again, so the two names can
    share one file: binding every provider's snapshot layout to the darwin artifacts by copy
    held each blob twice and ran the release runner out of disk."""
    try:
        os.link(source, destination)
    except OSError as error:
        if error.errno not in (errno.EXDEV, errno.EPERM, errno.EMLINK):
            raise
        shutil.copyfile(source, destination)


def bind(artifact, index_artifact, repository, output):
    """Attach a complete published OCI layout to another platform's cold artifact."""
    output = resolved_parent(output)
    repository_name(repository)
    manifest, files = cold_files(artifact)
    index_manifest, index_files = layout_files(index_artifact)
    if json_bytes(identity(manifest)) != json_bytes(identity(index_manifest)):
        raise ValueError("provider logical identity differs from snapshot index")
    # The layout's blobs are linked, not copied, so a cold file written over
    # one would write into the layout itself. Refuse any cold path the
    # layout's own files take before anything is linked.
    for name in files:
        path = relative_path(name)
        if path.parts[0] == "blobs" or path.as_posix() in ("oci-layout", "index.json"):
            raise ValueError(f"cold provider file {name!r} collides with the snapshot layout")
    with staging_directory(output) as temporary:
        staging = Path(temporary) / "artifact"
        for name, source in index_files.items():
            if name not in ("oci-layout", "index.json") and not name.startswith("blobs/"):
                continue
            destination = staging / relative_path(name)
            destination.parent.mkdir(parents=True, exist_ok=True)
            if name.startswith("blobs/"):
                link_blob(source, destination)
            else:
                shutil.copyfile(source, destination)
        remote_manifest = dict(manifest)
        remote_manifest["snapshotIndex"] = {
            "path": "index.json",
            "sha256": index_manifest["snapshotIndex"]["sha256"],
        }
        remote_manifest["snapshotRepository"] = repository
        copy_cold(staging, remote_manifest, files)
        layout_files(staging)
        restrict_output_permissions(staging)
        staging.rename(output)


def resolved_parent(path):
    """Resolves symlinked parent directories but keeps the last component, so an output that is
    itself a symlink is still rejected rather than followed."""
    # realpath (not abspath) resolves `..` after the symlinks before it, as the shell does.
    path = os.fspath(path).rstrip(os.sep) or os.sep
    if os.path.basename(path) in ("", os.curdir, os.pardir):
        # Nothing to keep: `.`, `..` and `/` name existing directories, which outputs reject.
        return Path(os.path.realpath(path))
    return Path(os.path.realpath(os.path.dirname(path) or os.curdir)) / os.path.basename(path)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    assembly = commands.add_parser("assemble", help="assemble a local provider artifact and OCI layout")
    assembly.add_argument("--artifact", type=Path, required=True)
    assembly.add_argument("--variant", type=Path, action="append", default=[])
    assembly.add_argument("--output", type=Path, required=True)
    publication = commands.add_parser(
        "publish", help="explicitly publish and emit a provider with its local OCI layout"
    )
    publication.add_argument("--artifact", type=Path, required=True)
    publication.add_argument("--repository", required=True)
    publication.add_argument("--tag", required=True)
    publication.add_argument("--output", type=Path, required=True)
    binding = commands.add_parser(
        "bind", help="attach an assembled OCI layout to another platform artifact"
    )
    binding.add_argument("--artifact", type=Path, required=True)
    binding.add_argument("--index-artifact", type=Path, required=True)
    binding.add_argument("--repository", required=True)
    binding.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    # Input checks reject symlinks inside provider artifacts; resolve the paths given here once so a
    # checkout reached through a symlinked directory is still accepted.
    args.artifact = Path(os.path.realpath(args.artifact))
    args.output = resolved_parent(args.output)
    if args.command == "assemble":
        args.variant = [Path(os.path.realpath(variant)) for variant in args.variant]
    elif args.command == "bind":
        args.index_artifact = Path(os.path.realpath(args.index_artifact))
    try:
        if args.command == "assemble":
            result = assemble(args.artifact, args.variant, args.output)
        elif args.command == "publish":
            result = publish(args.artifact, args.repository, args.tag, args.output)
        else:
            bind(args.artifact, args.index_artifact, args.repository, args.output)
            result = None
    except (ValueError, OSError, KeyError, TypeError) as error:
        parser.exit(1, f"provider snapshot OCI: {error}\n")
    if result is not None:
        print(result)


if __name__ == "__main__":
    main()
