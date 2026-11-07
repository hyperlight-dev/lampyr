---
title: ly setup
description: Install and validate the Lampyr platform packages.
---

# Setting up Lampyr

Download `install-ly.sh` from a release. These steps install `v0.1.0`.

```bash
set -euo pipefail

gh auth status --hostname github.com

install -d -m 0700 "$HOME/.cache/lampyr"
gh release download v0.1.0 \
  --repo hyperlight-dev/lampyr \
  --dir "$HOME/.cache/lampyr" \
  --clobber \
  --pattern install-ly.sh \
  --pattern install-ly.sh.sha256

(
  cd "$HOME/.cache/lampyr"
  sha256sum --check install-ly.sh.sha256
)

RELEASE_COMMIT=$(
  gh api "repos/hyperlight-dev/lampyr/commits/v0.1.0" --jq .sha
)
gh attestation verify "$HOME/.cache/lampyr/install-ly.sh" \
  --repo hyperlight-dev/lampyr \
  --signer-workflow hyperlight-dev/lampyr/.github/workflows/release.yml \
  --source-ref refs/tags/v0.1.0 \
  --source-digest "$RELEASE_COMMIT" \
  --deny-self-hosted-runners

chmod 0700 "$HOME/.cache/lampyr/install-ly.sh"

# Install the release whose installer you verified.
"$HOME/.cache/lampyr/install-ly.sh" v0.1.0

# Without an argument, the installer takes the rolling preview-release from main.
# "$HOME/.cache/lampyr/install-ly.sh"

export PATH="$HOME/.local/bin:$PATH"
ly doctor
```

`gh` must be authenticated to `github.com`; it downloads the release assets and
verifies their GitHub attestations.
The host may be Linux/amd64 or macOS/arm64. The installer places `ly` in
`LAMPYR_BIN_DIR` (default `~/.local/bin`) and installs the matching platform
bundle in `LAMPYR_HOME` (default `~/.lampyr`). Downloads use the authenticated
`gh release download` path on both hosts.

The Darwin bundle installs ARM provider cold rootfs files under
`~/.lampyr/platforms/VERSION/providers/PROVIDER/artifact/`.

Before replacing the local installation, the installer:

- resolves the requested tag, including annotated tags, to its exact commit;
- downloads the CLI and platform archives plus their `.sha256` files;
- validates each strict checksum record and archive SHA-256;
- verifies each archive with `gh attestation verify`, restricted to the Lampyr
  release workflow on `main` (or on the `vX.Y.Z` tag for versioned releases),
  the tag's exact source commit, and GitHub-hosted runners; and
- validates archive contents and installs the selected payloads transactionally.

The `preview-release` tag is rolling: each successful publication deletes and
recreates the prerelease and tag, replacing all prior preview assets. There is
no retained preview release to roll back to. Record the exact commit shown by
the release when reproducibility matters.

Repository development is still discovered automatically. To build instead
of installing a release, use the
[source-build instructions](../contributing.md#build-from-source).
