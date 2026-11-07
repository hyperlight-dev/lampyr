#!/usr/bin/env python3
"""Validate the committed preview image contexts and prepared Linux/amd64 binaries."""

import argparse
from dataclasses import dataclass
from pathlib import Path
import re
import shlex
from typing import Callable, Iterable


PINNED_IMAGE = re.compile(r".+@sha256:[0-9a-f]{64}")


@dataclass(frozen=True)
class Image:
    name: str
    context: str
    dockerfile: str
    sources: tuple[str, ...]
    binaries: tuple[str, ...]


IMAGES = (
    Image(
        "fiberd",
        ".",
        "fiberd/ci.Dockerfile",
        (),
        ("build/serving-controller",),
    ),
    Image(
        "fiber-router",
        ".",
        "activator/ci.Dockerfile",
        (),
        ("build/fiber-router",),
    ),
    Image(
        "hluk-helper",
        ".",
        "helper/ci.Dockerfile",
        (),
        ("build/hluk-helper",),
    ),
    Image(
        "fiberd-k8s-controller",
        ".",
        "upstream/controller.Dockerfile",
        (),
        ("build/fiberd-k8s-controller",),
    ),
    Image(
        "azure-node-setup",
        "providers/azure/node-setup",
        "providers/azure/node-setup/Dockerfile",
        ("providers/azure/node-setup/node-setup.sh",),
        (),
    ),
    Image(
        "eventgrid-controller",
        "providers/azure",
        "providers/azure/eventgrid/ci.Dockerfile",
        (),
        ("providers/azure/build/eventgrid-controller",),
    ),
    Image(
        "eventgrid-adapter",
        "providers/azure",
        "providers/azure/eventgrid/ci.Dockerfile",
        (),
        ("providers/azure/build/eventgrid-adapter",),
    ),
)


def dockerfile_base_errors(contents: str, dockerfile: str) -> list[str]:
    errors = []
    stages = set()
    logical_lines = contents.replace("\\\n", " ").splitlines()
    for line_number, line in enumerate(logical_lines, 1):
        tokens = shlex.split(line, comments=True)
        if not tokens or tokens[0].upper() != "FROM":
            continue
        index = 1
        while index < len(tokens) and tokens[index].startswith("--"):
            index += 1
        if index >= len(tokens):
            errors.append(f"{dockerfile}:{line_number}: FROM has no base image")
            continue
        base = tokens[index]
        if base != "scratch" and base not in stages and not PINNED_IMAGE.fullmatch(base):
            errors.append(
                f"{dockerfile}:{line_number}: base image is not pinned by sha256 digest: {base}"
            )
        if index + 2 < len(tokens) and tokens[index + 1].upper() == "AS":
            stages.add(tokens[index + 2])
    return errors


def required_path_errors(
    root: Path,
    paths: Iterable[str],
    *,
    is_file: Callable[[Path], bool] = Path.is_file,
) -> list[str]:
    return [
        f"required image context file is missing: {relative}"
        for relative in paths
        if not is_file(root / relative)
    ]


def linux_amd64_error(path: Path, relative: str) -> str | None:
    with path.open("rb") as binary:
        header = binary.read(20)
    if header[:6] != b"\x7fELF\x02\x01" or header[18:20] != b"\x3e\x00":
        return f"prepared image binary is not Linux/amd64 ELF: {relative}"
    return None


def validate_image(root: Path, image: Image, require_binaries: bool) -> list[str]:
    errors = []
    context = root / image.context
    dockerfile = root / image.dockerfile
    if not context.is_dir():
        errors.append(f"image context directory is missing: {image.context}")
    if not dockerfile.is_file():
        errors.append(f"image Dockerfile is missing: {image.dockerfile}")
    else:
        try:
            dockerfile.relative_to(context)
        except ValueError:
            errors.append(
                f"image Dockerfile is outside its context: {image.dockerfile} ({image.context})"
            )
        errors.extend(dockerfile_base_errors(dockerfile.read_text(), image.dockerfile))
    errors.extend(required_path_errors(root, image.sources))
    if require_binaries:
        missing = required_path_errors(root, image.binaries)
        errors.extend(missing)
        missing_paths = {
            error.rsplit(": ", 1)[-1]
            for error in missing
        }
        for relative in image.binaries:
            if relative in missing_paths:
                continue
            architecture_error = linux_amd64_error(root / relative, relative)
            if architecture_error:
                errors.append(architecture_error)
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument(
        "--image",
        action="append",
        choices=[image.name for image in IMAGES],
        help="limit validation to one image; may be repeated",
    )
    parser.add_argument(
        "--require-binaries",
        action="store_true",
        help="also require prepared Linux/amd64 binaries",
    )
    args = parser.parse_args()

    root = args.root.resolve(strict=True)
    selected = set(args.image or ())
    images = [image for image in IMAGES if not selected or image.name in selected]
    errors = []
    for image in images:
        errors.extend(validate_image(root, image, args.require_binaries))
    if errors:
        for error in errors:
            print(error)
        return 1
    print("checked preview images: " + ", ".join(image.name for image in images))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
