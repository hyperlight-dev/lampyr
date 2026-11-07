#!/usr/bin/env bash
# Run `cargo test` with a name filter, failing if the filter selects no test.
# cargo exits 0 when a renamed test no longer matches, which would silently drop coverage.
set -euo pipefail

output=$(mktemp)
trap 'rm -f "$output"' EXIT
cargo test "$@" 2>&1 | tee "$output"
passed=$(sed -nE 's/^test result: ok\. ([0-9]+) passed.*/\1/p' "$output" | awk '{ total += $1 } END { print total + 0 }')
if (( passed == 0 )); then
  echo "cargo test $*: the filter selected no tests" >&2
  exit 1
fi
