#!/usr/bin/env bash
# Fail unless the GitHub release for TAG is confirmed absent.
set -euo pipefail

: "${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"
(( $# == 1 )) || { echo "usage: require-new-release.sh TAG" >&2; exit 2; }
tag="$1"

if output=$(gh api "repos/${GITHUB_REPOSITORY}/releases/tags/${tag}" --silent 2>&1); then
    echo "release ${tag} already exists" >&2
    exit 1
fi
# Any lookup failure other than a confirmed 404 (auth, rate limit, outage) is fatal.
if [[ "$output" != *"(HTTP 404)"* ]]; then
    printf 'could not confirm release %s is absent: %s\n' "$tag" "$output" >&2
    exit 1
fi
