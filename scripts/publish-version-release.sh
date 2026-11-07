#!/usr/bin/env bash
set -euo pipefail

: "${GH_TOKEN:?GH_TOKEN is required}"
: "${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"
: "${GITHUB_SHA:?GITHUB_SHA is required}"
: "${GITHUB_REF:?GITHUB_REF is required}"
: "${IMAGE_PREFIX:?IMAGE_PREFIX is required}"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly repo_root
tag="${GITHUB_REF#refs/tags/}"
[[ "$GITHUB_REF" == "refs/tags/$tag" && "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
    echo "versioned releases require a v0.0.0 tag ref: $GITHUB_REF" >&2
    exit 1
}
version="${tag#v}"
readonly tag version
readonly -a images=(fiberd fiber-router hluk-helper azure-node-setup eventgrid-controller eventgrid-adapter upstream-fiberd fiberd-k8s-controller)
readonly -a assets=(
    "build/dist/lampyr-cli-linux-amd64-${version}.tar.gz"
    "build/dist/lampyr-cli-linux-amd64-${version}.tar.gz.sha256"
    "build/dist/lampyr-cli-aarch64-apple-darwin-${version}.tar.gz"
    "build/dist/lampyr-cli-aarch64-apple-darwin-${version}.tar.gz.sha256"
    "build/dist/lampyr-platform-linux-amd64-${version}.tar.gz"
    "build/dist/lampyr-platform-linux-amd64-${version}.tar.gz.sha256"
    "build/dist/lampyr-platform-darwin-arm64-${version}.tar.gz"
    "build/dist/lampyr-platform-darwin-arm64-${version}.tar.gz.sha256"
    "build/dist/lampyr-provider-kind-${version}.tar.gz"
    "build/dist/lampyr-provider-kind-${version}.tar.gz.sha256"
    "build/dist/lampyr-provider-k3d-${version}.tar.gz"
    "build/dist/lampyr-provider-k3d-${version}.tar.gz.sha256"
    "build/dist/lampyr-provider-azure-${version}.tar.gz"
    "build/dist/lampyr-provider-azure-${version}.tar.gz.sha256"
    build/dist/install-ly.sh
    build/dist/install-ly.sh.sha256
)

cd "$repo_root"
python3 scripts/release-version.py check --tag "$tag"
for asset in "${assets[@]}"; do
    [[ -f "$asset" ]] || {
        echo "missing release asset: $asset" >&2
        exit 1
    }
done

# Release tags are immutable: never replace an existing release.
scripts/require-new-release.sh "$tag"

read -r object_type object_sha < <(
    gh api "repos/${GITHUB_REPOSITORY}/git/ref/tags/${tag}" \
        --jq '.object.type + " " + .object.sha'
)
if [[ "$object_type" == tag ]]; then
    read -r object_type object_sha < <(
        gh api "repos/${GITHUB_REPOSITORY}/git/tags/${object_sha}" \
            --jq '.object.type + " " + .object.sha'
    )
fi
[[ "$object_type" == commit && "$object_sha" == "$GITHUB_SHA" ]] || {
    echo "tag $tag does not point at $GITHUB_SHA" >&2
    exit 1
}

image_notes=()
for image in "${images[@]}"; do
    reference="${IMAGE_PREFIX}/${image}:${version}"
    digest=$(
        docker buildx imagetools inspect "$reference" \
            --format '{{.Manifest.Digest}}'
    )
    [[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]]
    gh attestation verify "oci://${reference}@${digest}" \
        --repo "$GITHUB_REPOSITORY" \
        --signer-workflow "${GITHUB_REPOSITORY}/.github/workflows/release.yml" \
        --source-ref "$GITHUB_REF" \
        --source-digest "$GITHUB_SHA" \
        --deny-self-hosted-runners
    image_notes+=("- ${reference}@${digest}")
done

changes=$(
    awk -v heading="## [${version}] - " '
        index($0, heading) == 1 { found = 1; next }
        found && /^## \[/ { exit }
        found { print }
    ' CHANGELOG.md
)
notes="$(printf '%s\n' \
    "Lampyr ${version} built from ${GITHUB_SHA}." \
    "" \
    "Install with \`scripts/install-ly.sh ${tag}\`." \
    "" \
    "Images:" \
    "${image_notes[@]}" \
    "" \
    "${changes}")"

gh release create "$tag" "${assets[@]}" \
    --repo "$GITHUB_REPOSITORY" \
    --verify-tag \
    --title "Lampyr ${tag}" \
    --notes "$notes"
