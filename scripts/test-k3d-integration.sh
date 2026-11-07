#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
source "$repo_root/scripts/fiberd-controller-inputs.sh"
ly="$repo_root/build/ly"
cluster="lampyr-k3d-$$"
context="k3d-$cluster"
project=$(mktemp -d /tmp/lampyr-k3d-ci.XXXXXX)
export KUBECONFIG="$project/kubeconfig"
export LAMPYR_HOME="$project/data"
owned=false
source "$repo_root/scripts/runtime-examples.sh"
lifecycle_only=${LAMPYR_CI_LIFECYCLE_ONLY:-0}
if [[ "$lifecycle_only" != 1 ]]; then
  select_runtime_examples
  prepare_fiberd_template_tools "$repo_root"
fi

cleanup() {
  status=$?
  if [[ $status -ne 0 && $owned == true ]]; then
    kubectl --context "$context" get pods --all-namespaces -o wide >&2 || true
    kubectl --context "$context" get events --all-namespaces \
      --sort-by=.lastTimestamp >&2 || true
    kubectl --context "$context" -n fiberd-system logs deployment/fiberd-k8s-controller --tail=100 >&2 || true
    kubectl --context "$context" -n default logs -l lampyr.dev/home=true \
      --all-containers=true --tail=100 --prefix=true >&2 || true
  fi
  if [[ $owned == true ]]; then
    if ! "$ly" cluster uninstall --provider k3d --name "$cluster" \
      --context "$context" --workspace "$project" --delete-cluster; then
      echo "k3d cleanup failed; preserving installation state at $project" >&2
      return 1
    fi
  fi
  rm -r -- "$project"
  return "$status"
}
trap cleanup EXIT

[[ -r /dev/kvm && -w /dev/kvm ]] || {
  echo "/dev/kvm must be readable and writable for the k3d integration test" >&2
  exit 1
}
if [[ -n $(docker ps --all --filter "label=k3d.cluster=$cluster" --format '{{.Names}}') ]]; then
  echo "Cluster $cluster already exists; refusing to replace it" >&2
  exit 1
fi
registry_port=$(python3 - <<'PY'
import socket
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
PY
)
"$ly" scaffold workspace "$project" >/dev/null
rm -r -- "$project/services/app"
if [[ "$lifecycle_only" != 1 ]]; then
for example in "${examples[@]}"; do
  IFS='|' read -r service _ <<< "$example"
  cp -a "$repo_root/docs/examples/$service" "$project/services/$service"
  if [[ -d "$project/services/$service/.lampyr" ]]; then
    rm -r -- "$project/services/$service/.lampyr"
  fi
  sed -i 's/^    cpu: 250m$/    cpu: 10m/' "$project/services/$service/lampyr.yaml"
done
fi
expose_workspace_services "$project"
# ly deploy pushes templates to the provider's registry, which
# zygoteRegistries defaults to. Built grant and helper images go there
# too, once install has created it.
cluster_input_images "localhost:$registry_port"
jq -n \
  --argjson registryPort "$registry_port" \
  --arg controllerManifest "$controller_manifest" \
  --arg grantImage "$grant_image" \
  --arg helperImage "$helper_image" \
  --argjson images "$controller_images" \
  '{registryPort:$registryPort,controllerManifest:$controllerManifest,
    grantImage:$grantImage,helperImage:$helperImage,
    images:$images}' > "$project/provider.json"
owned=true
"$ly" cluster install --provider k3d --name "$cluster" \
  --workspace "$project" --provider-config "$project/provider.json" --allow-docker
push_cluster_inputs "localhost:$registry_port"
https_port=$(kubectl --context "$context" -n kube-system get configmap lampyr-install \
  -o jsonpath='{.data.httpsPort}')
installation_id=$(kubectl --context "$context" -n kube-system get configmap lampyr-install \
  -o jsonpath='{.data.installationId}')
[[ $installation_id =~ ^[0-9a-f]{32}$ ]]
# Grant Pods reach the templates by the name k3d gives the registry.
policy=$(kubectl --context "$context" get grantpolicies.fiberd.io default \
  -o jsonpath='{.spec.registries}')
[[ $policy == "[\"http://lampyr-platform-$installation_id:5000/lampyr/fiberd-templates\"]" ]] || {
  printf 'GrantPolicy registries are %s\n' "$policy" >&2
  exit 1
}
"$ly" cluster health --context "$context" --workspace "$project"
source_file="$project/infra/k3d/$cluster/k3d-config.yaml"
printf '\n# Customer-owned source must survive reinstall.\n' >> "$source_file"
source_digest=$(sha256sum "$source_file")
"$ly" cluster install --provider k3d --name "$cluster" --workspace "$project"
[[ $(sha256sum "$source_file") == "$source_digest" ]]

if [[ "$lifecycle_only" != 1 ]]; then
for example in "${examples[@]}"; do
  IFS='|' read -r service expected <<< "$example"
  directory="$project/services/$service"
  "$ly" build "$directory"
  "$ly" deploy --context "$context" --image "localhost:$registry_port/k3d/$service:k3d-test" "$directory"
  url="https://$service.default.localhost:$https_port"
  for activation in 1 2; do
    curl --fail --silent --show-error --insecure --max-time 60 \
      --retry 10 --retry-all-errors --retry-delay 1 \
      --dump-header "$project/headers" --output "$project/response" "$url/test"
    if ! grep -Fq "$expected" "$project/response"; then
      cat "$project/response" >&2
      echo "Unexpected k3d response for $service" >&2
      exit 1
    fi
    grep -qi '^X-Fiberd-Clone-Ms:' "$project/headers"
    "$ly" status --context "$context" "$directory" |
      grep -Eq '^capacity [^ ]+ ready, max [0-9]+ fibers$'
  done
  case "$service" in
    hello-nodejs|hello-go|hello-rust|hello-dotnet)
      curl --fail --silent --show-error --insecure --max-time 60 \
        --output "$project/stream" "$url/stream"
      for chunk in 1 2 3; do
        grep -Fq "\"chunk\":$chunk" "$project/stream"
      done
      ;;
  esac
  case "$service" in
    hello-nodejs|hello-python|hello-go|hello-rust|hello-dotnet)
      python3 "$repo_root/scripts/test-example-websocket.py" "$https_port" "$service"
      ;;
  esac
  case "$service" in
    hello-nodejs|hello-python|hello-go|hello-rust|hello-dotnet)
      curl --fail --silent --show-error --insecure --max-time 60 \
        --header 'Content-Type: application/json' \
        --header 'Ce-Specversion: 1.0' \
        --header 'Ce-Id: k3d-example' --header 'Ce-Type: dev.lampyr.k3d-test' \
        --header 'Ce-Source: urn:lampyr:k3d-test' \
        --header 'X-Lampyr-Delivery-Attempt: 1' --data '{"message":"k3d"}' \
        "$url/events" | jq -e '.event.id == "k3d-example" and .event.attempt == 1' >/dev/null
      ;;
  esac
  "$ly" delete --context "$context" "$directory"
  echo "k3d example passed: $service (activation, ready capacity, reactivation)"
  if [[ ${LAMPYR_CI_PRUNE_IMAGES:-} == 1 ]]; then
    # CI only: hosted runners run out of disk across all runtime examples.
    # ly builds with the Docker context's own builder, which holds each
    # example's base images and layers; BUILDX_BUILDER names another one.
    # Running k3d and registry containers keep their images, and platform
    # images were imported into the node at install.
    context_builder=$(docker context inspect --format '{{.Name}}')
    docker buildx prune --builder "$context_builder" --all --force | tail -n 1
    docker buildx prune --all --force | tail -n 1
    docker image prune --all --force | tail -n 1
    df -h /
  fi
done
fi

if [[ "${LAMPYR_CI_RUNTIME_ONLY:-0}" == 1 ]]; then
  echo "k3d selected runtime examples passed"
  exit 0
fi

"$ly" cluster uninstall --context "$context" --workspace "$project"
kubectl --context "$context" get namespace kube-system >/dev/null
if kubectl --context "$context" -n kube-system get configmap lampyr-install >/dev/null 2>&1; then
  echo "Lampyr installation metadata remains after uninstall" >&2
  exit 1
fi
"$ly" cluster uninstall --provider k3d --name "$cluster" --context "$context" \
  --workspace "$project" --delete-cluster
owned=false
[[ -z $(docker ps --all --filter "label=k3d.cluster=$cluster" --format '{{.Names}}') ]]
echo "k3d provider, selected examples, and retained/full uninstall passed"
