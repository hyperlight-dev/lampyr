#!/usr/bin/env bash
set -euo pipefail

: "${GH_TOKEN:?GH_TOKEN is required}"
: "${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"
: "${GITHUB_SHA:?GITHUB_SHA is required}"
: "${IMAGE_PREFIX:?IMAGE_PREFIX is required}"
: "${PREVIEW_TAG:?PREVIEW_TAG is required}"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly repo_root
readonly -a images=(
    fiberd
    fiber-router
    hluk-helper
    azure-node-setup
    eventgrid-controller
    eventgrid-adapter
    upstream-fiberd
    fiberd-k8s-controller
)
readonly -a assets=(
    build/dist/lampyr-cli-linux-amd64-preview.tar.gz
    build/dist/lampyr-cli-linux-amd64-preview.tar.gz.sha256
    build/dist/lampyr-cli-aarch64-apple-darwin-preview.tar.gz
    build/dist/lampyr-cli-aarch64-apple-darwin-preview.tar.gz.sha256
    build/dist/lampyr-platform-linux-amd64-preview.tar.gz
    build/dist/lampyr-platform-linux-amd64-preview.tar.gz.sha256
    build/dist/lampyr-platform-darwin-arm64-preview.tar.gz
    build/dist/lampyr-platform-darwin-arm64-preview.tar.gz.sha256
    build/dist/lampyr-provider-kind-preview.tar.gz
    build/dist/lampyr-provider-kind-preview.tar.gz.sha256
    build/dist/lampyr-provider-k3d-preview.tar.gz
    build/dist/lampyr-provider-k3d-preview.tar.gz.sha256
    build/dist/lampyr-provider-azure-preview.tar.gz
    build/dist/lampyr-provider-azure-preview.tar.gz.sha256
    build/dist/install-ly.sh
    build/dist/install-ly.sh.sha256
)

cd "$repo_root"
for asset in "${assets[@]}"; do
    [[ -f "$asset" ]] || {
        echo "missing preview asset: $asset" >&2
        exit 1
    }
done

declare -A candidate_digests=()
for image in "${images[@]}"; do
    candidate="${IMAGE_PREFIX}/${image}:preview-${GITHUB_SHA}"
    digest=$(
        docker buildx imagetools inspect "$candidate" \
            --format '{{.Manifest.Digest}}'
    )
    [[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]]
    candidate_digests["$image"]="$digest"
done

if gh release view "$PREVIEW_TAG" --repo "$GITHUB_REPOSITORY" >/dev/null 2>&1; then
    gh release delete "$PREVIEW_TAG" \
        --repo "$GITHUB_REPOSITORY" \
        --cleanup-tag \
        --yes
elif gh api "repos/${GITHUB_REPOSITORY}/git/ref/tags/${PREVIEW_TAG}" >/dev/null 2>&1; then
    gh api \
        --method DELETE \
        "repos/${GITHUB_REPOSITORY}/git/refs/tags/${PREVIEW_TAG}"
fi

gh release create "$PREVIEW_TAG" \
    --repo "$GITHUB_REPOSITORY" \
    --target "$GITHUB_SHA" \
    --title "Lampyr preview" \
    --notes "$(printf '%s\n' \
        "Preview built from ${GITHUB_SHA}." \
        "" \
        "Canonical images use preview-<full-commit> tags:" \
        "- ${IMAGE_PREFIX}/fiberd:preview-${GITHUB_SHA}" \
        "- ${IMAGE_PREFIX}/fiber-router:preview-${GITHUB_SHA}" \
        "- ${IMAGE_PREFIX}/hluk-helper:preview-${GITHUB_SHA}" \
        "- ${IMAGE_PREFIX}/azure-node-setup:preview-${GITHUB_SHA}" \
        "- ${IMAGE_PREFIX}/eventgrid-controller:preview-${GITHUB_SHA}" \
        "- ${IMAGE_PREFIX}/eventgrid-adapter:preview-${GITHUB_SHA}" \
        "- ${IMAGE_PREFIX}/upstream-fiberd:preview-${GITHUB_SHA}" \
        "- ${IMAGE_PREFIX}/fiberd-k8s-controller:preview-${GITHUB_SHA}" \
        "" \
        "The :preview tags are post-release convenience aliases.")" \
    --prerelease

tag_commit=$(
    gh api "repos/${GITHUB_REPOSITORY}/git/ref/tags/${PREVIEW_TAG}" \
        --jq '.object.sha'
)
[[ "$tag_commit" == "$GITHUB_SHA" ]]
gh release upload "$PREVIEW_TAG" "${assets[@]}" --repo "$GITHUB_REPOSITORY"

for image in "${images[@]}"; do
    candidate="${IMAGE_PREFIX}/${image}:preview-${GITHUB_SHA}"
    alias="${IMAGE_PREFIX}/${image}:preview"
    digest="${candidate_digests[$image]}"
    docker buildx imagetools create --tag "$alias" "${candidate}@${digest}"
    promoted_digest=$(
        docker buildx imagetools inspect "$alias" \
            --format '{{.Manifest.Digest}}'
    )
    [[ "$promoted_digest" == "$digest" ]]
    gh attestation verify "oci://${alias}@${promoted_digest}" \
        --repo "$GITHUB_REPOSITORY" \
        --signer-workflow "${GITHUB_REPOSITORY}/.github/workflows/release.yml" \
        --source-ref "refs/heads/main" \
        --source-digest "$GITHUB_SHA" \
        --deny-self-hosted-runners
done

current="preview-${GITHUB_SHA}"
cutoff=$(date --utc --date='30 days ago' '+%Y-%m-%dT%H:%M:%SZ')
for image in "${images[@]}"; do
    package="lampyr%2F${image}"
    version_ids=$(
        gh api --paginate --slurp \
            "/orgs/hyperlight-dev/packages/container/${package}/versions?per_page=100" \
            | jq -r --arg current "$current" --arg cutoff "$cutoff" '
                .[][] |
                (.metadata.container.tags // []) as $tags |
                # Delete only stale candidate-only versions; preserve untagged,
                # current, multiply-tagged, and versioned release images.
                select(.created_at < $cutoff) |
                select(($tags | length) == 1) |
                select($tags[0] | test("^preview-[0-9a-f]{40}$")) |
                select($tags[0] != $current) |
                .id
              '
    )
    while IFS= read -r version_id; do
        [[ -z "$version_id" ]] || gh api --method DELETE \
            "/orgs/hyperlight-dev/packages/container/${package}/versions/${version_id}"
    done <<<"$version_ids"
done
