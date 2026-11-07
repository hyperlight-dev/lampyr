#!/usr/bin/env bash

# Shared by local and cluster integration suites.
examples=(
  "hello-nodejs|hello from Hyperlight edge"
  "hello-python|hello from Python on Hyperlight edge"
  "hello-go|hello from Go on Hyperlight edge"
  "hello-rust|hello from Rust on Hyperlight edge"
  "hello-dotnet|hello from .NET on Hyperlight edge"
)
# Local and Kind suites append these; k3d stays at five Handler examples (node disk).
standalone_examples=(
  "hello-nodejs-standalone|hello from Node.js"
  "hello-python-standalone|hello from Python"
  "hello-go-standalone|hello from Go"
  "hello-rust-standalone|hello from Rust"
  "hello-dotnet-standalone|hello from .NET"
)

# Declare the fixture services exposed through the normal workspace contract.
expose_workspace_services() {
  python3 - "$1" <<'PY'
from pathlib import Path
import re
import sys

root = Path(sys.argv[1])
manifest = root / "lampyr.workspace.yaml"
services = []
for path in sorted((root / "services").glob("*/lampyr.yaml")):
    matches = [
        match.group(1)
        for line in path.read_text().splitlines()
        if (match := re.fullmatch(r"name:\s*([a-z0-9](?:[a-z0-9-]*[a-z0-9])?)\s*", line))
    ]
    if len(matches) != 1:
        raise SystemExit(f"{path} has no single unquoted top-level service name")
    services.extend(matches)
workspace = "apiVersion: lampyr.dev/v1alpha1\nkind: Workspace\ningress:\n"
workspace += (
    "  services:\n" + "".join(f"    - {service}\n" for service in services)
    if services
    else "  services: []\n"
)
manifest.write_text(workspace)
PY
}

select_runtime_examples() {
  if [[ -z "${LAMPYR_CI_EXAMPLES:-}" ]]; then
    return
  fi
  local requested=",${LAMPYR_CI_EXAMPLES// /,},"
  local selected=()
  local example service
  for example in "${examples[@]}"; do
    IFS='|' read -r service _ <<< "$example"
    if [[ "$requested" == *",$service,"* ]]; then
      selected+=("$example")
      requested=${requested/,$service,/,}
    fi
  done
  if [[ "$requested" != "," ]]; then
    local unknown=${requested#,}
    echo "Unknown LAMPYR_CI_EXAMPLES value(s): ${unknown%,}" >&2
    exit 1
  fi
  if ((${#selected[@]} == 0)); then
    echo "LAMPYR_CI_EXAMPLES must select at least one runtime example" >&2
    exit 1
  fi
  examples=("${selected[@]}")
}

nonempty_line_count() {
  awk 'NF { count++ } END { print count + 0 }' <<<"$1"
}
