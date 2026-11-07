#!/usr/bin/env python3
"""Tests for setting and checking committed release versions."""

from pathlib import Path
import runpy
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
release = runpy.run_path(str(ROOT / "scripts/release-version.py"))
FILES = ("VERSION", "cli/Cargo.toml", "cli/Cargo.lock", "docs/walkthrough.md", "CHANGELOG.md")


class ReleaseVersionTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        for relative in FILES:
            target = self.root / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes((ROOT / relative).read_bytes())

    def tearDown(self):
        self.temporary.cleanup()

    def test_repository_versions_agree(self):
        self.assertEqual(release["check"](ROOT), [])

    def test_set_updates_every_reference_and_satisfies_tag_check(self):
        release["set_version"](self.root, "4.5.6", "2026-01-02")
        self.assertEqual(set(release["versions"](self.root).values()), {"4.5.6"})
        changelog = (self.root / "CHANGELOG.md").read_text()
        self.assertIn("## [Prerelease] - Unreleased\n\n## [4.5.6] - 2026-01-02\n", changelog)
        self.assertEqual(release["check"](self.root, "v4.5.6"), [])
        release["set_version"](self.root, "4.5.6", "2026-01-03")
        self.assertEqual(
            (self.root / "CHANGELOG.md").read_text(), changelog, "set must be idempotent"
        )

    def test_tag_check_reports_every_stale_reference(self):
        errors = release["check"](self.root, "v9.9.9")
        for relative in FILES:
            self.assertTrue(any(error.startswith(relative) for error in errors), relative)

    def test_check_detects_drift_without_a_tag(self):
        current = (self.root / "VERSION").read_text().strip()
        (self.root / "VERSION").write_text(f"{current}.drift\n")
        errors = release["check"](self.root)
        self.assertIn("cli/Cargo.toml", "\n".join(errors))

    def test_stable_version_requires_changelog_without_a_tag(self):
        release["set_version"](self.root, "4.5.6", "2026-01-02")
        changelog = self.root / "CHANGELOG.md"
        changelog.write_text(changelog.read_text().replace("## [4.5.6]", "## [4.5.5]"))
        self.assertEqual(
            release["check"](self.root),
            ["CHANGELOG.md: missing '## [4.5.6] - YYYY-MM-DD' section"],
        )

    def test_tags_and_versions_must_be_plain_semver(self):
        for tag in ("0.1.0", "v0.1", "v0.1.0-rc.1", "v01.0.0", "v0.1.0\n"):
            self.assertEqual(
                release["check"](self.root, tag), [f"release tag must look like v0.0.0: {tag}"]
            )
        with self.assertRaisesRegex(ValueError, "0.0.0"):
            release["set_version"](self.root, "1.0", "2026-01-02")


if __name__ == "__main__":
    unittest.main()
