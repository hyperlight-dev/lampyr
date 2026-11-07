---
name: release-version
description: Prepare a versioned Lampyr release (vX.Y.Z). Updates every committed version reference, checks them, and explains how to tag so the release workflow publishes it. Use when asked to "cut a release", "bump the version", "prepare v0.2.0", "release Lampyr", or update the version files.
---

# Release a Lampyr version

Pushing a `vX.Y.Z` tag runs `.github/workflows/release.yml`. The workflow's
`version` job fails unless every committed version reference already equals
`X.Y.Z`, the CHANGELOG has a `## [X.Y.Z] - YYYY-MM-DD` section, the tagged
commit is on `main`, and no `vX.Y.Z` release exists. This skill prepares that
commit. Tags are plain `vMAJOR.MINOR.PATCH`; pre-release suffixes are rejected.

## Files this updates

`scripts/release-version.py` owns the list. Keep this list and that script in
sync if a new version reference is added:

| File | Reference |
|------|-----------|
| `VERSION` | whole file |
| `cli/Cargo.toml` | `[package] version` (drives `ly version` and archive names) |
| `cli/Cargo.lock` | the `lampyr-cli` package entry |
| `docs/walkthrough.md` | the `[ok] ly X.Y.Z` doctor output |
| `CHANGELOG.md` | inserts `## [X.Y.Z] - <today>` below `## [Prerelease] - Unreleased` |

Do **not** edit image tags. Source keeps `:preview`; the release build stamps
`X.Y.Z` into the CLI's default images (`LAMPYR_IMAGE_TAG`) and the packaged
platform manifests. Provider `artifact.json` versions are independent provider
protocol versions; leave them alone unless the provider itself changed.

## Steps

1. Confirm the version with the user if they did not give one. It must be
   greater than the latest `v*` tag (`git tag --list 'v*' --sort=-v:refname | head -1`).
2. Start from an up-to-date branch off `main`.
3. Update every reference:

   ```bash
   just release-version-set X.Y.Z
   ```

4. Edit `CHANGELOG.md`: the new `## [X.Y.Z]` heading is inserted directly
   under `## [Prerelease] - Unreleased`, so the existing unreleased entries
   now belong to `X.Y.Z`. Review them, and keep the Unreleased heading empty
   for future work. The release notes are taken from this section.
5. Verify exactly what the tag workflow will check, then the related tests:

   ```bash
   just release-version-check tag=vX.Y.Z
   just test-preview-release
   ```

6. Commit (`git commit -S -s`) and open a PR. Do not push tags or trigger
   workflows without the user's explicit permission.
7. After the PR merges, tell the user to tag the merge commit on `main` and
   push it; this starts the release:

   ```bash
   git fetch origin main
   git tag -s vX.Y.Z -m "Lampyr vX.Y.Z" origin/main
   git push origin vX.Y.Z
   ```

## What the tag publishes

Reusing the preview pipeline (same packaging, Kind release gates, and
attestation steps):

- GHCR images `ghcr.io/hyperlight-dev/lampyr/{fiberd,fiber-router,eventgrid-controller,eventgrid-adapter}:X.Y.Z`,
  attested against `refs/tags/vX.Y.Z`.
- A GitHub release `vX.Y.Z` with `lampyr-*-X.Y.Z.tar.gz` archives, checksums,
  and `install-ly.sh`. Install with `scripts/install-ly.sh vX.Y.Z`.

The release is immutable: a failed run must be fixed with a new patch version
rather than by moving the tag.
