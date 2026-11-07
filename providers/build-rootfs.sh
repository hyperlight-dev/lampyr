#!/usr/bin/env bash
set -euo pipefail

project=$(cd -- "$(dirname -- "$1")" && pwd)
target=${2:-rootfs}
guest_platform=${3:-linux/amd64}
case "$target" in
  rootfs|proof) ;;
  *) echo "unsupported provider build target: $target" >&2; exit 1 ;;
esac
case "$guest_platform" in
  linux/amd64|linux/arm64) ;;
  *) echo "unsupported provider guest platform: $guest_platform" >&2; exit 1 ;;
esac
output="$project/build/$target"
mkdir -p "$output"
stage=$(mktemp -d "$output/.export.XXXXXX")
container=
cleanup() {
  local status=$?
  if [[ -n "$container" ]]; then
    docker rm "$container" >/dev/null || status=1
  fi
  rm -r -- "$stage" || status=1
  exit "$status"
}
trap cleanup EXIT

image=${LAMPYR_PROVIDER_PREBUILT_IMAGE:-"lampyr-provider-$(basename "$project")-$target-${guest_platform#linux/}:0.1.0"}
if [[ -z "${LAMPYR_PROVIDER_PREBUILT_IMAGE:-}" ]]; then
  docker buildx build --load --platform "$guest_platform" --target "$target" \
    --build-context "platform=$project/../../k8s" \
    --build-context "common=$project/../common" \
    --build-context "kind=$project/../kind" \
    --tag "$image" "$project"
fi
container=$(docker create --platform "$guest_platform" --entrypoint / "$image")
mkdir "$stage/rootfs"
docker export "$container" | tar --extract --file - --directory "$stage/rootfs" \
  --exclude='./dev/*' --exclude='dev/*' \
  --exclude='./proc/*' --exclude='proc/*' \
  --exclude='./sys/*' --exclude='sys/*' \
  --exclude='.dockerenv' --exclude='etc/hostname' \
  --exclude='etc/hosts' --exclude='etc/resolv.conf' --exclude='etc/mtab'
(
  cd "$stage/rootfs"
  find . -print0 | LC_ALL=C sort -z | cpio --null --create --format=newc --quiet
) > "$stage/rootfs.cpio"
mv -- "$stage/rootfs.cpio" "$output/rootfs.cpio"
printf 'built %s\n' "$output/rootfs.cpio"
