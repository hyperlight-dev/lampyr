#!/usr/bin/env bash
set -euo pipefail

readonly SOURCE_PREFIX="ghcr.io/hyperlight-dev/lampyr"
readonly REPOSITORY="hyperlight-dev/lampyr"
readonly SIGNER_WORKFLOW="${REPOSITORY}/.github/workflows/release.yml"
readonly SOURCE_REF="refs/heads/main"
readonly PREVIEW_RELEASE="preview-release"
# The last three are the platform's default controller inputs: the fiberd
# image grant Pods run, the controller the manifest below runs, and the
# helper image.
readonly -a IMAGES=(
    fiberd
    fiber-router
    azure-node-setup
    eventgrid-controller
    eventgrid-adapter
    upstream-fiberd
    fiberd-k8s-controller
    hluk-helper
)
# The preview platform carries fiberd-k8s-controller's manifest, which
# pulls the controller image by digest.
readonly PLATFORM_ARCHIVE="lampyr-platform-linux-amd64-preview.tar.gz"
readonly CONTROLLER_MANIFEST="build/fiberd-k8s-controller.yaml"

die() {
    echo "preview mirror: $*" >&2
    exit 1
}

valid_destination() {
    local destination="$1"
    local registry='[a-z0-9]+([.-][a-z0-9]+)*(:[0-9]+)?'
    local component='[a-z0-9]+(([._]|__|-+)[a-z0-9]+)*'

    [[ "$destination" =~ ^${registry}(/${component})*$ ]]
}

inspect_digest() {
    local reference="$1"
    local digest

    if ! digest=$(
        docker buildx imagetools inspect "$reference" \
            --format '{{.Manifest.Digest}}'
    ); then
        die "failed to inspect digest for ${reference}"
    fi
    [[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]] ||
        die "invalid digest returned for ${reference}: ${digest}"
    printf '%s' "$digest"
}

(( $# == 1 )) ||
    die "usage: mirror-preview-images.sh DESTINATION"
readonly destination="$1"
valid_destination "$destination" ||
    die "invalid destination prefix: ${destination}"

command -v gh >/dev/null 2>&1 ||
    die "gh is required"
gh auth status --hostname github.com >/dev/null 2>&1 ||
    die "gh must be authenticated to github.com"
command -v docker >/dev/null 2>&1 ||
    die "docker is required"
docker buildx version >/dev/null 2>&1 ||
    die "Docker Buildx is required"

source_commit=$(
    gh api "repos/${REPOSITORY}/commits/${PREVIEW_RELEASE}" --jq '.sha'
) || die "failed to resolve ${PREVIEW_RELEASE}"
[[ "$source_commit" =~ ^[0-9a-f]{40}$ ]] ||
    die "invalid source commit returned for ${PREVIEW_RELEASE}"
readonly source_tag="preview-${source_commit}"

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
readonly repo_root
readonly build_dir="${repo_root}/build"
mkdir -p "$build_dir"
kind_tmp=
azure_tmp=
manifest_tmp=
work=$(mktemp -d "${build_dir}/.preview-mirror.XXXXXX")

cleanup() {
    [[ -z "${kind_tmp:-}" ]] || rm -f -- "$kind_tmp"
    [[ -z "${azure_tmp:-}" ]] || rm -f -- "$azure_tmp"
    [[ -z "${manifest_tmp:-}" ]] || rm -f -- "$manifest_tmp"
    rm -rf -- "$work"
}
trap cleanup EXIT

declare -a source_references=()
declare -a source_digests=()
declare -a destination_references=()

for image in "${IMAGES[@]}"; do
    source="${SOURCE_PREFIX}/${image}:${source_tag}"
    digest=$(inspect_digest "$source")
    source_references+=("$source")
    source_digests+=("$digest")

    if ! gh attestation verify "oci://${source}@${digest}" \
        --repo "$REPOSITORY" \
        --signer-workflow "$SIGNER_WORKFLOW" \
        --source-ref "$SOURCE_REF" \
        --source-digest "$source_commit" \
        --deny-self-hosted-runners >/dev/null; then
        die "attestation verification failed for ${source}@${digest}"
    fi
done

gh release download "$PREVIEW_RELEASE" \
    --repo "$REPOSITORY" \
    --pattern "$PLATFORM_ARCHIVE" \
    --dir "$work" ||
    die "failed to download ${PLATFORM_ARCHIVE}"
if ! gh attestation verify "${work}/${PLATFORM_ARCHIVE}" \
    --repo "$REPOSITORY" \
    --signer-workflow "$SIGNER_WORKFLOW" \
    --source-ref "$SOURCE_REF" \
    --source-digest "$source_commit" \
    --deny-self-hosted-runners >/dev/null; then
    die "attestation verification failed for ${PLATFORM_ARCHIVE}"
fi
tar --extract --gzip --file "${work}/${PLATFORM_ARCHIVE}" \
    --directory "$work" "$CONTROLLER_MANIFEST" ||
    die "${PLATFORM_ARCHIVE} has no ${CONTROLLER_MANIFEST}"
manifest=$(<"${work}/${CONTROLLER_MANIFEST}")

# Every image the manifest pulls must be one mirrored here, by the digest
# verified above, so the mirrored manifest pulls nothing from the source.
declare -a manifest_images=()
mapfile -t manifest_images < <(
    sed -nE 's/^ +(- )?image: ([^[:space:]]+)$/\2/p' \
        "${work}/${CONTROLLER_MANIFEST}" | sort -u
)
(( ${#manifest_images[@]} > 0 )) ||
    die "${CONTROLLER_MANIFEST} pulls no image"
declare -a manifest_indexes=()
for reference in "${manifest_images[@]}"; do
    found=
    for index in "${!IMAGES[@]}"; do
        if [[ "$reference" == "${SOURCE_PREFIX}/${IMAGES[$index]}@${source_digests[$index]}" ]]; then
            found=$index
        fi
    done
    [[ -n "$found" ]] ||
        die "${CONTROLLER_MANIFEST} pulls ${reference}, which is not a verified preview image"
    manifest_indexes+=("$found")
done

for index in "${!IMAGES[@]}"; do
    image="${IMAGES[$index]}"
    source="${source_references[$index]}@${source_digests[$index]}"
    destination_tag="${destination}/${image}:preview"

    if ! docker buildx imagetools create \
        --platform linux/amd64 \
        --tag "$destination_tag" \
        "$source"; then
        die "failed to mirror ${source_references[$index]} to ${destination_tag}"
    fi

    destination_digest=$(inspect_digest "$destination_tag")
    destination_references+=("${destination_tag}@${destination_digest}")
done

readonly mirrored_manifest="${build_dir}/preview-fiberd-k8s-controller.yaml"
kind_tmp="${build_dir}/.preview-kind.yaml.$$"
azure_tmp="${build_dir}/.preview-azure.yaml.$$"
manifest_tmp="${build_dir}/.preview-fiberd-k8s-controller.yaml.$$"

for index in "${manifest_indexes[@]}"; do
    source="${SOURCE_PREFIX}/${IMAGES[$index]}@${source_digests[$index]}"
    manifest=${manifest//"$source"/"${destination_references[$index]}"}
done
printf '%s\n' "$manifest" > "$manifest_tmp"

# The mirrored reference of the image IMAGES names $1.
mirrored() {
    local index
    for index in "${!IMAGES[@]}"; do
        if [[ ${IMAGES[$index]} == "$1" ]]; then
            printf '%s\n' "${destination_references[$index]}"
            return
        fi
    done
    die "IMAGES has no ${1}"
}
# Assigned first: a failure inside $(...) as an argument would not stop
# the script.
fiberd_reference=$(mirrored fiberd)
router_reference=$(mirrored fiber-router)
node_setup_reference=$(mirrored azure-node-setup)
eventgrid_controller_reference=$(mirrored eventgrid-controller)
eventgrid_adapter_reference=$(mirrored eventgrid-adapter)
grant_reference=$(mirrored upstream-fiberd)
helper_reference=$(mirrored hluk-helper)

# ly cluster install resolves a relative controllerManifest against the
# --provider-config file that names it, and these inputs are meant to be
# copied into one anywhere, so the path is absolute.
controller_inputs() {
    printf 'controllerManifest: %s\n' "$mirrored_manifest"
    printf 'grantImage: %s\n' "$grant_reference"
    printf 'helperImage: %s\n' "$helper_reference"
}
{
    printf 'fiberdImage: %s\n' "$fiberd_reference"
    printf 'routerImage: %s\n' "$router_reference"
    controller_inputs
} > "$kind_tmp"
{
    printf 'fiberdImage: %s\n' "$fiberd_reference"
    printf 'routerImage: %s\n' "$router_reference"
    printf 'nodeSetupImage: %s\n' "$node_setup_reference"
    printf 'eventGridControllerImage: %s\n' "$eventgrid_controller_reference"
    printf 'eventGridAdapterImage: %s\n' "$eventgrid_adapter_reference"
    controller_inputs
} > "$azure_tmp"

mv -f -- "$manifest_tmp" "$mirrored_manifest"
manifest_tmp=
mv -f -- "$kind_tmp" "${build_dir}/preview-kind.yaml"
kind_tmp=
mv -f -- "$azure_tmp" "${build_dir}/preview-azure.yaml"
azure_tmp=

printf 'Wrote %s, %s and %s\n' \
    "${build_dir}/preview-kind.yaml" \
    "${build_dir}/preview-azure.yaml" \
    "$mirrored_manifest"
