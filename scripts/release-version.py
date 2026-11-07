#!/usr/bin/env python3
"""Set or check every committed Lampyr release version reference."""

import argparse
import datetime
from pathlib import Path
import re
import sys
import tomllib


RELEASE_TAG = re.compile(r"v(?P<version>(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*))")
VERSION = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?")
LOCK_ENTRY = re.compile(r'(\[\[package\]\]\nname = "lampyr-cli"\nversion = ")([^"]+)(")')
MANIFEST_VERSION = re.compile(r'(?m)^(version = ")([^"]+)(")')
WALKTHROUGH = re.compile(r"(?m)^(\[ok\] ly )(\S+)$")
UNRELEASED = "## [Prerelease] - Unreleased"


def read(root, relative):
    return (root / relative).read_text(encoding="utf-8")


def write(root, relative, text):
    (root / relative).write_text(text, encoding="utf-8")


def one(pattern, text, relative):
    matches = pattern.findall(text)
    if len(matches) != 1:
        raise ValueError(f"{relative}: expected one version reference, found {len(matches)}")
    match = matches[0]
    return match[1] if isinstance(match, tuple) else match


def versions(root):
    """Return each tracked file's current version."""
    manifest = tomllib.loads(read(root, "cli/Cargo.toml"))["package"]["version"]
    return {
        "VERSION": read(root, "VERSION").strip(),
        "cli/Cargo.toml": manifest,
        "cli/Cargo.lock": one(LOCK_ENTRY, read(root, "cli/Cargo.lock"), "cli/Cargo.lock"),
        "docs/walkthrough.md": one(
            WALKTHROUGH, read(root, "docs/walkthrough.md"), "docs/walkthrough.md"
        ),
    }


def changelog_heading(version):
    return re.compile(rf"(?m)^## \[{re.escape(version)}\] - [0-9]{{4}}-[0-9]{{2}}-[0-9]{{2}}$")


def check(root, tag=None):
    errors = []
    found = versions(root)
    expected = found["VERSION"]
    if tag is not None:
        match = RELEASE_TAG.fullmatch(tag)
        if not match:
            return [f"release tag must look like v0.0.0: {tag}"]
        expected = match["version"]
    if not VERSION.fullmatch(expected):
        errors.append(f"invalid version: {expected}")
    for relative, version in found.items():
        if version != expected:
            errors.append(f"{relative}: found {version}, expected {expected}")
    # A stable version on main must already have its release section; prereleases need not.
    stable = tag is not None or RELEASE_TAG.fullmatch(f"v{expected}")
    if stable and not changelog_heading(expected).search(read(root, "CHANGELOG.md")):
        errors.append(f"CHANGELOG.md: missing '## [{expected}] - YYYY-MM-DD' section")
    return errors


def set_version(root, version, date):
    if not RELEASE_TAG.fullmatch(f"v{version}"):
        raise ValueError(f"release version must look like 0.0.0: {version}")
    write(root, "VERSION", version + "\n")
    for relative, pattern in (
        ("cli/Cargo.toml", MANIFEST_VERSION),
        ("cli/Cargo.lock", LOCK_ENTRY),
        ("docs/walkthrough.md", WALKTHROUGH),
    ):
        text = read(root, relative)
        one(pattern, text, relative)
        write(root, relative, pattern.sub(rf"\g<1>{version}\g<3>" if pattern.groups == 3
                                          else rf"\g<1>{version}", text, count=1))
    changelog = read(root, "CHANGELOG.md")
    if not changelog_heading(version).search(changelog):
        if UNRELEASED not in changelog:
            raise ValueError(f"CHANGELOG.md: missing '{UNRELEASED}' section")
        changelog = changelog.replace(
            UNRELEASED, f"{UNRELEASED}\n\n## [{version}] - {date}", 1
        )
        write(root, "CHANGELOG.md", changelog)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    commands = parser.add_subparsers(dest="command", required=True)
    check_parser = commands.add_parser("check", help="verify version references agree")
    check_parser.add_argument("--tag", help="also require a matching v0.0.0 release tag")
    set_parser = commands.add_parser("set", help="update every version reference")
    set_parser.add_argument("version")
    set_parser.add_argument("--date", default=datetime.date.today().isoformat())
    args = parser.parse_args()
    root = args.root.resolve(strict=True)
    try:
        if args.command == "set":
            set_version(root, args.version, args.date)
        errors = check(root, f"v{args.version}" if args.command == "set" else args.tag)
    except (OSError, KeyError, ValueError) as error:
        errors = [str(error)]
    if errors:
        for error in errors:
            print(error, file=sys.stderr)
        return 1
    print(f"release version {versions(root)['VERSION']} is consistent")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
