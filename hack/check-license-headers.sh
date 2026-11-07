#!/usr/bin/env bash
# Checks that first-party Rust and Go sources start with the SPDX license
# header. Adopted from hyperlight-dev/hyperlight (dev/check-license-headers.sh).
#
#   hack/check-license-headers.sh          report files without the header
#   hack/check-license-headers.sh --fix    add the header to them
#
# docs/ and templates/ are left out: they hold sample code that `ly` copies
# into users' projects, which should not carry this project's header.
set -euo pipefail

# The copyright year is pinned to the project's current copyright year; bump it
# here (and re-run with --fix) when it changes.
YEAR="2026"
SPDX="// SPDX-License-Identifier: Apache-2.0"
COPYRIGHT="// Copyright $YEAR The Hyperlight Authors."

fix=false
case "${1:-}" in
    "") ;;
    --fix) fix=true ;;
    *) echo "usage: $0 [--fix]" >&2; exit 2 ;;
esac

cd "$(git rev-parse --show-toplevel)"

missing=()
while IFS= read -r -d '' file; do
    if grep -qE '@generated|^// Code generated .* DO NOT EDIT\.$' "$file"; then
        continue
    fi
    if [[ "$(sed -n 1p "$file")" == "$SPDX" && "$(sed -n 2p "$file")" == "$COPYRIGHT" ]]; then
        continue
    fi
    if $fix; then
        { printf '%s\n%s\n\n' "$SPDX" "$COPYRIGHT"; cat "$file"; } >"$file.tmp"
        mv "$file.tmp" "$file"
    else
        missing+=("$file")
    fi
done < <(git ls-files -z -- '*.rs' '*.go' ':!:docs/**' ':!:templates/**')

if ((${#missing[@]})); then
    echo "Found ${#missing[@]} files without the license header:"
    printf '  %s\n' "${missing[@]}"
    echo
    echo "Each must start with:"
    printf '%s\n%s\n' "$SPDX" "$COPYRIGHT"
    echo
    echo "Run hack/check-license-headers.sh --fix to add it."
    exit 1
fi
echo "All Rust and Go files have the license header."
