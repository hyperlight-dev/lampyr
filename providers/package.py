#!/usr/bin/env python3
"""Package a provider independently of the ly binary."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import stat
import subprocess
import sys

from permissions import others_can_write


def file_digest(path):
    with path.open("rb") as source:
        checksum = hashlib.file_digest(source, "sha256").hexdigest()
    return {"path": path.name, "sha256": checksum}


def native_snapshot_available():
    machine = platform.machine().lower()
    if sys.platform == "darwin" and machine in ("arm64", "aarch64"):
        return True
    if not sys.platform.startswith("linux") or machine not in ("x86_64", "amd64"):
        return False
    try:
        metadata = Path("/dev/kvm").stat()
    except OSError:
        return False
    return stat.S_ISCHR(metadata.st_mode) and os.access(
        "/dev/kvm", os.R_OK | os.W_OK
    )


def remove_snapshot_outputs(output):
    for path in (output / "snapshot.tar", output / "snapshot.tar.gz"):
        path.unlink(missing_ok=True)
    snapshot = output / "snapshot"
    if snapshot.is_symlink() or snapshot.is_file():
        snapshot.unlink()
    elif snapshot.is_dir():
        shutil.rmtree(snapshot)


def restrict_artifact_permissions(output, allow_stale_snapshot_symlinks=False):
    validate_artifact_output(output)
    output.chmod(0o700)

    def raise_walk_error(error):
        raise error

    for root, directories, files in os.walk(
        output, followlinks=False, onerror=raise_walk_error
    ):
        for name in directories:
            path = Path(root) / name
            mode = path.lstat().st_mode
            if (
                allow_stale_snapshot_symlinks
                and path.parent == output
                and path.name in ("snapshot", "snapshot.tar", "snapshot.tar.gz")
                and stat.S_ISLNK(mode)
            ):
                continue
            if stat.S_ISLNK(mode) or not stat.S_ISDIR(mode):
                raise ValueError("provider artifacts may contain only regular files and directories")
            path.chmod(stat.S_IMODE(mode) & ~0o022)
        for name in files:
            path = Path(root) / name
            mode = path.lstat().st_mode
            if (
                allow_stale_snapshot_symlinks
                and path.parent == output
                and path.name in ("snapshot", "snapshot.tar", "snapshot.tar.gz")
                and stat.S_ISLNK(mode)
            ):
                continue
            if stat.S_ISLNK(mode) or not stat.S_ISREG(mode):
                raise ValueError("provider artifacts may contain only regular files and directories")
            path.chmod(stat.S_IMODE(mode) & ~0o022)


def validate_artifact_output(output):
    parent_mode = output.parent.lstat().st_mode
    if stat.S_ISLNK(parent_mode) or not stat.S_ISDIR(parent_mode):
        raise ValueError("provider artifact output parent must be a directory")
    mode = output.lstat().st_mode
    if stat.S_ISLNK(mode) or not stat.S_ISDIR(mode):
        raise ValueError("provider artifact output must be a directory")


def validate_output_file(path):
    try:
        mode = path.lstat().st_mode
    except FileNotFoundError:
        return
    if not stat.S_ISREG(mode):
        raise ValueError(f"provider artifact output {path.name} must be a regular file")


def restrict_artifact_parent_permissions(output):
    parent = output.parent
    metadata = parent.lstat()
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        raise ValueError("provider artifact output parent must be a real directory")
    if os.name == "posix":
        user = os.geteuid()
        if metadata.st_uid != user:
            raise ValueError("provider artifact output parent must be owned by the current user")
        child_uid = metadata.st_uid
        for ancestor in parent.parents:
            metadata = ancestor.lstat()
            if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
                raise ValueError("provider artifact ancestor must be a real directory")
            if metadata.st_uid not in (user, 0):
                raise ValueError("provider artifact ancestor must be owned by the current user or root")
            reason = others_can_write(ancestor, metadata)
            if reason is not None:
                if not metadata.st_mode & 0o1000 or child_uid not in (user, 0):
                    raise ValueError(
                        f"provider artifact ancestor {ancestor} must not be writable by other users: {reason}; "
                        f"run `chmod go-w {ancestor}` or keep the checkout in a directory only you can write"
                    )
            child_uid = metadata.st_uid
        parent.chmod(stat.S_IMODE(parent.lstat().st_mode) & ~0o022)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("project", type=Path)
    parser.add_argument("target", choices=("rootfs", "proof"))
    parser.add_argument("--kernel", type=Path)
    parser.add_argument("--cold-os")
    parser.add_argument("--cold-architecture")
    parser.add_argument("--snapshot-mode", choices=("auto", "variant", "none"), default="auto")
    args = parser.parse_args()
    if (args.cold_os is None) != (args.cold_architecture is None):
        parser.error("--cold-os and --cold-architecture must be specified together")
    project = args.project.resolve(strict=True)
    output = project / "build" / args.target
    validate_artifact_output(output)
    restrict_artifact_parent_permissions(output)
    restrict_artifact_permissions(output, allow_stale_snapshot_symlinks=True)
    for name in ("rootfs.cpio", "provider.py", "kernel", ".manifest.json"):
        validate_output_file(output / name)
    manifest = json.loads((project / "artifact.json").read_text(encoding="utf-8"))
    shutil.copyfile(project / "provider.py", output / "provider.py")
    manifest["rootfs"] = file_digest(output / "rootfs.cpio")
    manifest["program"] = file_digest(output / "provider.py")
    if args.cold_os is None:
        manifest.pop("coldCompatibility", None)
    else:
        manifest["coldCompatibility"] = {
            "os": args.cold_os,
            "architecture": args.cold_architecture,
        }
    manifest.pop("snapshot", None)
    manifest.pop("snapshotCompatibility", None)
    manifest.pop("snapshotIndex", None)
    manifest.pop("snapshotRepository", None)
    remove_snapshot_outputs(output)
    restrict_artifact_permissions(output)
    if args.target == "rootfs" and args.snapshot_mode != "none":
        snapshot_available = native_snapshot_available()
        if args.snapshot_mode == "variant" and not snapshot_available:
            parser.error("native snapshot variants require a supported native hypervisor")
        if snapshot_available:
            snapshot_binary = os.environ.get("LAMPYR_PROVIDER_SNAPSHOT")
            command = (
                [str(Path(snapshot_binary).resolve(strict=True))]
                if snapshot_binary
                else [
                    "cargo",
                    "run",
                    "--quiet",
                    "--locked",
                    "--manifest-path",
                    str(project.parent.parent / "cli" / "Cargo.toml"),
                    "--bin",
                    "provider-snapshot",
                    "--",
                ]
            )
            command.extend([
                "--rootfs",
                str(output / "rootfs.cpio"),
                "--memory-mib",
                str(manifest["memoryMiB"]),
                "--output",
                str(output / "snapshot"),
            ])
            if args.kernel is not None:
                command.extend(["--kernel", str(args.kernel.resolve(strict=True))])
            subprocess.run(command, check=True)
            restrict_artifact_permissions(output)
            snapshot_directory = output / "snapshot"
            manifest["snapshotIndex"] = file_digest(snapshot_directory / "index.json")
            manifest["snapshotIndex"]["path"] = "snapshot/index.json"
        else:
            reason = "a supported native hypervisor is required"
            print(
                f"provider snapshot not generated: {reason}; using cold-boot fallback",
                file=sys.stderr,
            )
    if args.kernel is not None:
        shutil.copyfile(args.kernel, output / "kernel")
        manifest["kernel"] = file_digest(output / "kernel")
    else:
        manifest["kernel"] = None
    temporary = output / ".manifest.json"
    temporary.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    temporary.replace(output / "manifest.json")
    restrict_artifact_permissions(output)
    print(f"packaged {manifest['id']} {manifest['version']}: {output}")


if __name__ == "__main__":
    main()
