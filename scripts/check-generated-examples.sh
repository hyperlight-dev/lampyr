#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
ly=${LAMPYR_CLI:-"$repo_root/build/ly"}

if [[ ! -x "$ly" ]]; then
  echo "Lampyr CLI not found at $ly; run 'just build-ly' first" >&2
  exit 1
fi

generated=$(mktemp -d)
trap 'rm -rf "$generated"' EXIT
"$ly" scaffold workspace "$generated" >/dev/null

examples=(
  "nodejs hello-nodejs"
  "python hello-python"
  "go hello-go"
  "rust hello-rust"
  "dotnet hello-dotnet"
)

changed=0
for example in "${examples[@]}"; do
  read -r runtime name <<< "$example"
  (cd "$generated" && "$ly" scaffold service "$name" --runtime "$runtime") >/dev/null

  if ! diff --recursive --unified --new-file --exclude=.lampyr \
    "$repo_root/docs/examples/$name" \
    "$generated/services/$name"; then
    changed=1
  fi
done

for runtime in nodejs python go rust dotnet; do
  name="hello-$runtime-standalone"
  (cd "$generated" && "$ly" scaffold service "$name" --runtime "$runtime" --standalone) >/dev/null
  if ! diff --recursive --unified --new-file --exclude=.lampyr \
    "$repo_root/docs/examples/$name" "$generated/services/$name"; then
    changed=1
  fi
done

if (( changed != 0 )); then
  echo "Generated examples are stale; regenerate docs/examples with build/ly scaffold service." >&2
  exit 1
fi

echo "Generated examples match all supported runtime templates."