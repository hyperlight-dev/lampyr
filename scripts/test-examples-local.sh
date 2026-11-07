#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
ly=${LAMPYR_CLI:-"$repo_root/build/ly"}
base_port=${LAMPYR_EXAMPLES_PORT:-19080}
logs=$(mktemp -d)
dev_pid=

cleanup() {
  if [[ -n "$dev_pid" ]] && kill -0 "$dev_pid" 2>/dev/null; then
    kill "$dev_pid" 2>/dev/null || true
    wait "$dev_pid" 2>/dev/null || true
  fi
  rm -rf "$logs"
}
trap cleanup EXIT

source "$repo_root/scripts/runtime-examples.sh"
examples+=("${standalone_examples[@]}")
select_runtime_examples

project="$logs/workspace"
mkdir -p "$project/services"
printf 'apiVersion: lampyr.dev/v1alpha1\nkind: Workspace\n' > "$project/lampyr.workspace.yaml"
for example in "${examples[@]}"; do
  IFS='|' read -r service _ <<< "$example"
  mkdir "$project/services/$service"
  tar -C "$repo_root/docs/examples/$service" --exclude=.lampyr -cf - . |
    tar -C "$project/services/$service" -xf -
done
expose_workspace_services "$project"

for index in "${!examples[@]}"; do
  IFS='|' read -r service expected <<< "${examples[$index]}"
  port=$((base_port + index))
  log="$logs/$service.log"

  "$ly" dev \
    --port "$port" \
    "$project/services/$service" >"$log" 2>&1 &
  dev_pid=$!

  response=
  for _ in $(seq 1 180); do
    if ! kill -0 "$dev_pid" 2>/dev/null; then
      cat "$log" >&2
      wait "$dev_pid" || true
      echo "$service exited before serving a request" >&2
      exit 1
    fi
    if response=$(curl --fail --silent --show-error \
      -H "Host: $service.default.localhost" "http://127.0.0.1:$port/test" 2>/dev/null); then
      break
    fi
    sleep 1
  done

  if [[ "$response" != *"$expected"* ]]; then
    cat "$log" >&2
    echo "$service returned an unexpected response: $response" >&2
    exit 1
  fi

  if [[ "$service" == *-standalone ]]; then
    python3 -c 'import json,sys; body=json.loads(sys.argv[1]); assert body == {"message": sys.argv[2], "path": "/test"}, body' \
      "$response" "$expected"
  fi

  kill "$dev_pid"
  wait "$dev_pid" 2>/dev/null || true
  dev_pid=
  echo "local example passed: $service"
done

if [[ "${LAMPYR_CI_CONTRACTS:-1}" == 1 ]]; then
  python3 "$repo_root/tests/contracts/test_contract.py" --ly "$ly" --vm
fi