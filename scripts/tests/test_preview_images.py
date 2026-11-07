#!/usr/bin/env python3
"""Focused tests for preview image context validation."""

from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import check_preview_images

ROOT = Path(__file__).resolve().parents[2]


class PreviewImageChecksTest(unittest.TestCase):
    def test_unpinned_base_image_is_rejected(self):
        errors = check_preview_images.dockerfile_base_errors(
            "FROM ubuntu:24.04\n", "Dockerfile"
        )
        self.assertEqual(len(errors), 1)
        self.assertIn("not pinned", errors[0])

    def test_pinned_base_and_named_stage_are_accepted(self):
        digest = "a" * 64
        contents = (
            f"FROM ubuntu:24.04@sha256:{digest} AS build\n"
            "FROM build AS final\n"
        )
        self.assertEqual(
            check_preview_images.dockerfile_base_errors(contents, "Dockerfile"), []
        )

    def test_missing_context_file_is_rejected(self):
        errors = check_preview_images.required_path_errors(
            Path("/repo"),
            ("build/serving-controller",),
            is_file=lambda _path: False,
        )
        self.assertEqual(
            errors, ["required image context file is missing: build/serving-controller"]
        )

    def test_shipped_runtime_images_use_ghcr_preview_references(self):
        expected = {
            "cli/Justfile": (
                "ghcr.io/hyperlight-dev/lampyr/fiberd:preview",
                "ghcr.io/hyperlight-dev/lampyr/fiber-router:preview",
            ),
            # Release builds stamp LAMPYR_IMAGE_TAG; everything else defaults to preview.
            "cli/src/cluster.rs": (
                'const IMAGE_PREFIX: &str = "ghcr.io/hyperlight-dev/lampyr";',
                'option_env!("LAMPYR_IMAGE_TAG")',
                'None => "preview",',
                'format!("{IMAGE_PREFIX}/fiberd:{IMAGE_TAG}")',
                'format!("{IMAGE_PREFIX}/fiber-router:{IMAGE_TAG}")',
            ),
            "k8s/serving-controller.yaml": (
                "ghcr.io/hyperlight-dev/lampyr/fiberd:preview",
            ),
            "k8s/fiber-router.yaml": (
                "ghcr.io/hyperlight-dev/lampyr/fiber-router:preview",
            ),
            "helper/Justfile": (
                "ghcr.io/hyperlight-dev/lampyr/hluk-helper:preview",
            ),
            "providers/azure/Justfile": (
                "for binary in eventgrid-controller eventgrid-adapter",
                "ghcr.io/hyperlight-dev/lampyr/$binary:preview",
            ),
            "providers/azure/eventgrid/eventgrid-source.yaml": (
                "ghcr.io/hyperlight-dev/lampyr/eventgrid-controller:preview",
                "ghcr.io/hyperlight-dev/lampyr/eventgrid-adapter:preview",
            ),
        }
        for relative, references in expected.items():
            contents = (ROOT / relative).read_text(encoding="utf-8")
            with self.subTest(path=relative):
                for reference in references:
                    self.assertIn(reference, contents)
                for image in (
                    "fiberd",
                    "fiber-router",
                    "hluk-helper",
                    "eventgrid-controller",
                    "eventgrid-adapter",
                ):
                    self.assertNotIn(f"lampyr.local/{image}", contents)


if __name__ == "__main__":
    unittest.main()
