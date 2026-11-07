#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
source "$repo_root/scripts/fiberd-controller-inputs.sh"
cluster=${LAMPYR_CI_CLUSTER:-lampyr-ci}
registry=${LAMPYR_CI_REGISTRY:-lampyr-ci-registry}
registry_port=${LAMPYR_CI_REGISTRY_PORT:-5001}
http_port=${LAMPYR_CI_HTTP_PORT:-18080}
https_port=${LAMPYR_CI_HTTPS_PORT:-18443}
context="kind-$cluster"
node="$cluster-control-plane"
suite=${LAMPYR_CI_SUITE:-full}
case "$suite" in
  full|runtime|modes|lifecycle|eventing-lifecycle) ;;
  *)
    echo "LAMPYR_CI_SUITE must be full, runtime, modes, lifecycle, or eventing-lifecycle" >&2
    exit 1
    ;;
esac
project=$(mktemp -d /tmp/lampyr-kind-ci.XXXXXX)
"$repo_root/build/ly" scaffold workspace "$project" >/dev/null
rm -r -- "$project/services/app"
mkdir -p "$project/docker-only-bin"
ln -s "$(command -v docker)" "$project/docker-only-bin/docker"
export LAMPYR_HOME="$project/data"
kind_config="$project/kind.yaml"
tag=${GITHUB_SHA:-local}
images="localhost:$registry_port/ci"
platform_prefix="lampyr.local/ci"
phase_started=$SECONDS
current_service=
cluster_owned=false
registry_owned=false
provider_network=
source "$repo_root/scripts/runtime-examples.sh"
examples+=("${standalone_examples[@]}")
if [[ "$suite" == "full" || "$suite" == "runtime" ]]; then
  select_runtime_examples
fi
eventing=${LAMPYR_CI_EVENTING:-}
if [[ -z "$eventing" ]]; then
  if [[ "$suite" == "full" || "$suite" == "eventing-lifecycle" ]]; then
    eventing=1
  else
    eventing=0
  fi
fi
eventing_only=${LAMPYR_CI_EVENTING_ONLY:-0}
case "$eventing_only" in
  0|1) ;;
  *)
    echo "LAMPYR_CI_EVENTING_ONLY must be 0 or 1" >&2
    exit 1
    ;;
esac
if [[ "$eventing_only" == 1 ]]; then
  IFS='|' read -r eventing_service _ <<<"${examples[0]:-}"
  if [[ "$eventing" != 1 || ${#examples[@]} -ne 1 || "$eventing_service" != "hello-nodejs" ]]; then
    echo "LAMPYR_CI_EVENTING_ONLY requires only hello-nodejs with Eventing enabled" >&2
    exit 1
  fi
fi
runtime_extras=${LAMPYR_CI_RUNTIME_EXTRAS:-}
if [[ -z "$runtime_extras" ]]; then
  if [[ "$suite" == "full" || "$suite" == "modes" ]]; then
    runtime_extras=1
  else
    runtime_extras=0
  fi
fi
runtime_mode_checks=${LAMPYR_CI_MODE_CHECKS:-all}
case "$runtime_mode_checks" in
  all|fresh|reuse) ;;
  *)
    echo "LAMPYR_CI_MODE_CHECKS must be all, fresh, or reuse" >&2
    exit 1
    ;;
esac

phase_done() {
  local name=$1
  local elapsed=$((SECONDS - phase_started))
  echo "kind phase: $name (${elapsed}s)"
  phase_started=$SECONDS
}

if [[ ! -r /dev/kvm || ! -w /dev/kvm ]]; then
  echo "/dev/kvm must be readable and writable for the Kind integration test" >&2
  exit 1
fi
if [[ "$suite" == "full" || "$suite" == "runtime" || "$runtime_extras" == 1 ]]; then
  prepare_fiberd_template_tools "$repo_root"
fi

cleanup() {
  status=$?
  provider_cleanup_failed=false
  if [[ $status -ne 0 && $cluster_owned == true ]] && kind get clusters | grep -Fxq "$cluster"; then
    kubectl --context "$context" -n default get \
      service.serving.knative.dev,revision.serving.knative.dev,route.serving.knative.dev,deployment,pod -o wide 2>&1 || true
    kubectl --context "$context" describe nodes 2>&1 || true
    if [[ -n "$current_service" ]]; then
      kubectl --context "$context" -n default describe \
        service.serving.knative.dev/"$current_service" 2>&1 || true
      kubectl --context "$context" -n default describe pods \
        --selector "serving.knative.dev/service=$current_service" 2>&1 || true
    fi
    kubectl --context "$context" -n default get events \
      --sort-by=.lastTimestamp 2>&1 || true
    kubectl --context "$context" -n knative-serving get \
      deployment/lampyr-serving-controller -o wide 2>&1 || true
    kubectl --context "$context" -n knative-serving get pods \
      -l app=lampyr-serving-controller -o wide 2>&1 || true
    kubectl --context "$context" -n knative-serving describe \
      deployment/lampyr-serving-controller 2>&1 || true
    kubectl --context "$context" -n knative-serving get events \
      --sort-by=.lastTimestamp 2>&1 || true
    kubectl --context "$context" -n knative-eventing get \
      deployment,pod -o wide 2>&1 || true
    kubectl --context "$context" get \
      broker,trigger,inmemorychannel --all-namespaces -o wide 2>&1 || true
    kubectl --context "$context" -n knative-eventing get events \
      --sort-by=.lastTimestamp 2>&1 || true
    kubectl --context "$context" -n lampyr-system get \
      deployment,pod -o wide 2>&1 || true
    kubectl --context "$context" -n lampyr-system get events \
      --sort-by=.lastTimestamp 2>&1 || true
    for deployment in mt-broker-ingress mt-broker-filter imc-dispatcher eventing-controller; do
      kubectl --context "$context" -n knative-eventing logs \
        "deployment/$deployment" --tail=100 2>&1 || true
    done
    kubectl --context "$context" -n fiberd-system logs \
      deployment/fiberd-k8s-controller --tail=100 2>&1 || true
    kubectl --context "$context" -n default get capacitygrants -o yaml 2>&1 || true
    kubectl --context "$context" -n default logs -l lampyr.dev/home=true \
      --all-containers=true --tail=100 --prefix=true 2>&1 || true
    kubectl --context "$context" -n knative-serving logs deployment/lampyr-serving-controller \
      --tail=100 2>&1 || true
    kubectl --context "$context" -n knative-serving logs deployment/net-kourier-controller \
      --tail=100 2>&1 || true
    kubectl --context "$context" -n lampyr-system logs deployment/fiber-router \
      --tail=100 2>&1 || true
    docker exec "$node" journalctl -u containerd \
      --no-pager -n 100 2>&1 || true
  fi
  if [[ $status -ne 0 && ${LAMPYR_CI_KEEP_FAILED_RESOURCES:-0} == 1 ]]; then
    echo "Keeping failed integration resources: cluster=$cluster registry=$registry project=$project" >&2
    return "$status"
  fi
  if [[ $status -ne 0 && $registry_owned == true && -n "$provider_network" ]] &&
    docker inspect "$registry" | jq -e \
      --arg network "$provider_network" '.[0].NetworkSettings.Networks | has($network)' >/dev/null; then
    if ! docker network disconnect "$provider_network" "$registry"; then
      provider_cleanup_failed=true
      echo "Failed to disconnect registry $registry from network $provider_network" >&2
    fi
  fi
  installation="$LAMPYR_HOME/installations/kind/$cluster/host/installation.json"
  if [[ ($cluster_owned == true || $registry_owned == true) && -f "$installation" ]]; then
    if "$repo_root/build/ly" cluster uninstall \
      --provider kind --name "$cluster" --context "$context" \
      --delete-cluster; then
      cluster_owned=false
    else
      provider_cleanup_failed=true
      status=1
      echo "Provider cleanup failed; preserving state at $project" >&2
    fi
  fi
  if [[ $cluster_owned == true ]]; then
    kind delete cluster --name "$cluster" >/dev/null 2>&1 || true
  fi
  if [[ $registry_owned == true ]]; then
    docker rm -f "$registry" >/dev/null 2>&1 || true
  fi
  if [[ $provider_cleanup_failed == true ]]; then
    return "$status"
  fi
  rm -rf "$project"
  return "$status"
}
trap cleanup EXIT

if kind get clusters | grep -Fxq "$cluster"; then
  echo "Kind cluster $cluster already exists; refusing to replace it" >&2
  exit 1
fi
if docker container inspect "$registry" >/dev/null 2>&1; then
  echo "Docker container $registry already exists; refusing to replace it" >&2
  exit 1
fi

if [[ "$suite" == "full" || "$suite" == "runtime" ]]; then
  for example in "${examples[@]}"; do
    IFS='|' read -r service _ <<< "$example"
    cp -a "$repo_root/docs/examples/$service" "$project/services/$service"
    rm -rf "$project/services/$service/.lampyr"
    sed -i 's/^    cpu: 250m$/    cpu: 10m/' "$project/services/$service/lampyr.yaml"
  done
  expose_workspace_services "$project"
  "$repo_root/build/ly" build "$project"
  phase_done "build runtime examples"
fi

# Two workers leave room for Serving and Eventing on two-core hosts.
cat >"$kind_config" <<EOF
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
containerdConfigPatches:
  - |
    [plugins.'io.containerd.cri.v1.images'.registry]
      config_path = '/etc/containerd/certs.d'
nodes:
  - role: control-plane
    image: kindest/node:v1.35.8@sha256:07b2536e30b803ed61d1677a79df6115f798ce64c80f9e22f6ed45afd09323c0
    extraMounts:
      - hostPath: /dev/kvm
        containerPath: /dev/kvm
        readOnly: true
    extraPortMappings:
      - containerPort: 30080
        hostPort: $http_port
        listenAddress: 127.0.0.1
        protocol: TCP
      - containerPort: 30443
        hostPort: $https_port
        listenAddress: 127.0.0.1
        protocol: TCP
  - role: worker
    image: kindest/node:v1.35.8@sha256:07b2536e30b803ed61d1677a79df6115f798ce64c80f9e22f6ed45afd09323c0
    extraMounts:
      - hostPath: /dev/kvm
        containerPath: /dev/kvm
        readOnly: true
    extraPortMappings:
      - containerPort: 30443
        hostPort: $((https_port + 1))
        listenAddress: 127.0.0.1
        protocol: TCP
  - role: worker
    image: kindest/node:v1.35.8@sha256:07b2536e30b803ed61d1677a79df6115f798ce64c80f9e22f6ed45afd09323c0
    extraMounts:
      - hostPath: /dev/kvm
        containerPath: /dev/kvm
        readOnly: true
    extraPortMappings:
      - containerPort: 30443
        hostPort: $((https_port + 2))
        listenAddress: 127.0.0.1
        protocol: TCP
EOF

docker run -d \
  --name "$registry" \
  --restart=always \
  -p "127.0.0.1:$registry_port:5000" \
  registry:2 >/dev/null
registry_owned=true
# Built grant and helper images go to this registry, which the nodes pull
# from once it is configured on them below.
cluster_input_images "localhost:$registry_port"
push_cluster_inputs "localhost:$registry_port"
phase_done "start local registry"

fiberd_image="$platform_prefix-fiberd:$tag"
router_image="$platform_prefix-router:$tag"
builder=${BUILDX_BUILDER:-$(docker context inspect --format '{{.Name}}')}
docker buildx build --builder "$builder" --platform linux/amd64 --load \
  --tag "$fiberd_image" \
  --file "$repo_root/fiberd/ci.Dockerfile" "$repo_root" &
fiberd_build_pid=$!
docker buildx build --builder "$builder" --platform linux/amd64 --load \
  --tag "$router_image" \
  --file "$repo_root/activator/ci.Dockerfile" "$repo_root" &
router_build_pid=$!
image_build_status=0
for pid in "$fiberd_build_pid" "$router_build_pid"; do
  if ! wait "$pid"; then
    image_build_status=1
  fi
done
if ((image_build_status != 0)); then
  echo "One or more platform image builds failed" >&2
  exit 1
fi
phase_done "assemble platform images"

cluster_owned=true
# ly deploy pushes templates to the provider's registry, which
# zygoteRegistries defaults to.
jq -n --rawfile source "$kind_config" \
  --arg fiberd "$fiberd_image" \
  --arg router "$router_image" \
  --argjson registryPort "$((registry_port + 1))" \
  --arg controllerManifest "$controller_manifest" \
  --arg grantImage "$grant_image" \
  --arg helperImage "$helper_image" \
  --argjson images "$controller_images" \
  '{source:$source,fiberdImage:$fiberd,routerImage:$router,
    registryPort:$registryPort,controllerManifest:$controllerManifest,
    grantImage:$grantImage,helperImage:$helperImage,
    images:$images}' > "$project/provider.json"
PATH="$project/docker-only-bin" "$repo_root/build/ly" cluster install \
  --provider kind \
  --name "$cluster" \
  --provider-config "$project/provider.json" \
  --workspace "$project" \
  --allow-docker

phase_done "create owned cluster and install Serving"

# Reclaim build storage before the Eventing reinstall.
# Keep platform images available for the provider's second apply.
if [[ "${LAMPYR_CI_RECLAIM_STORAGE:-1}" == 1 ]]; then
  docker buildx prune --builder "$builder" --all --force >/dev/null
  context_builder=$(docker context inspect --format '{{.Name}}')
  if [[ "$context_builder" != "$builder" ]]; then
    docker buildx prune --builder "$context_builder" --all --force >/dev/null
  fi
  docker image rm \
    hluk-node-rootfs:latest \
    hluk-python-rootfs:latest \
    hluk-go-rootfs:latest \
    hluk-rust-rootfs:latest \
    hluk-dotnet-aot-rootfs:latest >/dev/null 2>&1 || true
  docker image prune --force >/dev/null
fi
phase_done "reclaim image storage"

if [[ "$eventing" == 1 ]]; then
  jq 'del(.source)' "$project/provider.json" > "$project/eventing.json"
  PATH="$project/docker-only-bin" "$repo_root/build/ly" cluster install \
    --provider kind \
    --eventing \
    --name "$cluster" \
    --provider-config "$project/eventing.json"
fi

provider_network=$(docker inspect "$node" | jq -er '.[0].NetworkSettings.Networks | keys[] | select(startswith("lampyr-"))')
docker network connect "$provider_network" "$registry"
kind_nodes=$(kind get nodes --name "$cluster")
for kind_node in $kind_nodes; do
  docker exec "$kind_node" mkdir -p "/etc/containerd/certs.d/localhost:$registry_port"
  cat <<EOF | docker exec -i "$kind_node" tee "/etc/containerd/certs.d/localhost:$registry_port/hosts.toml" >/dev/null
server = "http://$registry:5000"

[host."http://$registry:5000"]
  capabilities = ["pull", "resolve", "push"]
EOF
done
phase_done "install platform"

verify_broker_event() {
  local service=$1
  cat <<EOF | kubectl --context "$context" apply -f -
apiVersion: eventing.knative.dev/v1
kind: Broker
metadata:
  name: default
  namespace: default
---
apiVersion: eventing.knative.dev/v1
kind: Trigger
metadata:
  name: lampyr-eventing-smoke
  namespace: default
spec:
  broker: default
  filter:
    attributes:
      type: dev.lampyr.smoke
  subscriber:
    ref:
      apiVersion: serving.knative.dev/v1
      kind: Service
      name: $service
    uri: /events/knative
EOF
  kubectl --context "$context" -n default wait \
    --for=condition=Ready \
    broker/default \
    trigger/lampyr-eventing-smoke \
    --timeout=180s
  broker_url=$(kubectl --context "$context" -n default get broker default \
    -o jsonpath='{.status.address.url}')
  audit=$("$repo_root/build/ly" logs \
    --context "$context" \
    --tail 0 \
    "$project/services/$service")
  before_event_records=$(nonempty_line_count "$audit")
  kubectl --context "$context" -n default run lampyr-eventing-publisher \
    --image=curlimages/curl:8.12.1 \
    --restart=Never \
    --rm \
    --attach \
    --command -- \
    curl --fail-with-body --silent --show-error \
      --retry 10 \
      --retry-all-errors \
      --retry-delay 2 \
      --retry-max-time 300 \
      --request POST \
      --header 'Ce-Specversion: 1.0' \
      --header 'Ce-Type: dev.lampyr.smoke' \
      --header 'Ce-Source: urn:lampyr:kind-integration' \
      --header 'Ce-Id: lampyr-eventing-smoke' \
      --header 'Content-Type: application/json' \
      --data '{"message":"eventing broker delivery"}' \
      "$broker_url"
  event_delivered=false
  for _ in $(seq 1 60); do
    audit=$("$repo_root/build/ly" logs \
      --context "$context" \
      --tail 0 \
      "$project/services/$service" || true)
    after_event_records=$(nonempty_line_count "$audit")
    if ((after_event_records > before_event_records)); then
      event_delivered=true
      break
    fi
    sleep 1
  done
  if [[ "$event_delivered" != "true" ]]; then
    echo "Broker event did not reach $service" >&2
    exit 1
  fi
  kubectl --context "$context" -n default delete \
    trigger/lampyr-eventing-smoke broker/default \
    --wait=true --timeout=120s
  phase_done "deliver Broker event to $service"
}

run_runtime_checks() {
for example in "${examples[@]}"; do
  IFS='|' read -r service expected <<< "$example"
  current_service=$service
  service_image="$images/$service:$tag"

  if [[ "$service" == "hello-nodejs" && "$eventing_only" != 1 ]]; then
    just --justfile "$repo_root/Justfile" cli::test-registry \
      "$project/services/$service" "localhost:$registry_port/registry-contract:$tag"
  fi

  "$repo_root/build/ly" push \
    --image "$service_image" \
    "$project/services/$service"
  "$repo_root/build/ly" deploy \
    --context "$context" \
    --image "$service_image" \
    "$project/services/$service"
  phase_done "push and deploy $service"

  pods=unknown
  for _ in $(seq 1 180); do
    pods=$(kubectl --context "$context" -n default get pods \
      --selector "serving.knative.dev/service=$service,lampyr.dev/home=true" \
      --no-headers 2>/dev/null | wc -l)
    if [[ "$pods" == "1" ]]; then
      break
    fi
    sleep 1
  done
  if [[ "$pods" != "1" ]]; then
    echo "Service $service must have exactly one CapacityGrant home Pod" >&2
    exit 1
  fi

  if [[ "$eventing_only" == 1 ]]; then
    verify_broker_event "$service"
    "$repo_root/build/ly" delete --context "$context" "$project/services/$service"
    phase_done "verify Eventing lifecycle for $service"
    continue
  fi

  url="https://$service.default.localhost:$https_port/test"
  headers="$project/services/$service.headers"
  response="$project/services/$service.response"
  curl --fail --silent --show-error --insecure \
    --retry 10 --retry-all-errors --retry-delay 1 \
    --dump-header "$headers" \
    --output "$response" \
    "$url"
  if ! grep -Fq "$expected" "$response"; then
    echo "$service returned an unexpected response:" >&2
    cat "$response" >&2
    exit 1
  fi
  if [[ "$service" == *-standalone ]]; then
    jq -e --arg message "$expected" '. == {message: $message, path: "/test"}' "$response" >/dev/null
  fi
  grep -qi '^X-Fiberd-Clone-Ms:' "$headers"
  phase_done "activate $service"

  if [[ "$service" == "hello-nodejs" ]]; then
    for request in $(seq 1 100); do
      response=$(curl --fail --silent --show-error --insecure --max-time 15 \
        "https://$service.default.localhost:$https_port/request-$request")
      if [[ "$response" != *"$expected"* ]]; then
        echo "$service returned an unexpected response for request $request:" >&2
        echo "$response" >&2
        exit 1
      fi
    done
    phase_done "verify 100 sequential requests for $service"
  fi

  case "$service" in
    hello-nodejs|hello-go|hello-rust|hello-dotnet)
      stream_response="$project/services/$service.stream"
      curl --fail --silent --show-error --insecure \
        --retry 10 --retry-all-errors --retry-delay 1 \
        --output "$stream_response" \
        "https://$service.default.localhost:$https_port/stream"
      for chunk in 1 2 3; do
        if ! grep -Fq "\"chunk\":$chunk" "$stream_response"; then
          echo "$service streaming response is missing chunk $chunk:" >&2
          cat "$stream_response" >&2
          exit 1
        fi
      done
      phase_done "verify streaming for $service"
      ;;
  esac

  case "$service" in
    hello-nodejs|hello-python|hello-go|hello-rust|hello-dotnet)
      python3 "$repo_root/scripts/test-example-websocket.py" "$https_port" "$service"
      phase_done "verify WebSocket for $service"
      ;;
  esac

  case "$service" in
    hello-nodejs|hello-python|hello-go|hello-rust|hello-dotnet)
      event_id="kind-e2e-$service"
      event_response=$(curl --fail --silent --show-error --insecure \
        --retry 10 --retry-all-errors --retry-delay 1 \
        --request POST \
        --header 'Content-Type: application/json' \
        --header 'Ce-Specversion: 1.0' \
        --header "Ce-Id: $event_id" \
        --header 'Ce-Type: dev.lampyr.kind-e2e' \
        --header 'Ce-Source: urn:lampyr:kind-e2e' \
        --header 'X-Lampyr-Delivery-Attempt: 1' \
        --data '{"message":"protocol smoke test"}' \
        "https://$service.default.localhost:$https_port/events")
      if ! jq -e --arg id "$event_id" \
        '.event.id == $id and .event.attempt == 1' \
        <<<"$event_response" >/dev/null; then
        echo "$service event response did not echo CloudEvent metadata:" >&2
        echo "$event_response" >&2
        exit 1
      fi
      phase_done "verify event metadata for $service"
      ;;
  esac

  service_status=$("$repo_root/build/ly" status --context "$context" "$project/services/$service")
  printf '%s\n' "$service_status"
  if ! grep -Eq '^capacity [^ ]+ ready, max [0-9]+ fibers$' <<<"$service_status"; then
    echo "the CapacityGrant of $service is not ready" >&2
    exit 1
  fi
  # A grant Pod serves only mutual TLS to fiber-router, so busy Fibers are
  # the router's to count.
  revision=$(kubectl --context "$context" -n default get \
    "service.serving.knative.dev/$service" -o jsonpath='{.status.latestReadyRevisionName}')
  busy=unknown
  for _ in $(seq 1 10); do
    busy=$(python3 -c '
import sys
sys.path.insert(0, sys.argv[1])
import fiberd_grants
print(fiberd_grants.router_fibers_busy(sys.argv[2], sys.argv[3]))
' "$repo_root/scripts" "$context" "$revision")
    if [[ "$busy" == 0 ]]; then
      break
    fi
    sleep 0.5
  done
  if [[ "$busy" != 0 ]]; then
    echo "fresh-per-request fibers did not return to zero for $service: $busy busy" >&2
    exit 1
  fi
  "$repo_root/build/ly" scale --context "$context" --max-fibers 4 "$project/services/$service"
  "$repo_root/build/ly" logs --context "$context" --tail 10 "$project/services/$service"
  audit=
  for _ in $(seq 1 30); do
    audit=$("$repo_root/build/ly" audit \
      --context "$context" \
      --tail 10 \
      "$project/services/$service" || true)
    if grep -q '"event"' <<<"$audit"; then
      break
    fi
    sleep 1
  done
  if ! grep -q '"event"' <<<"$audit"; then
    echo "fiberd audit record was not available for $service" >&2
    exit 1
  fi
  if [[ "$service" == "hello-nodejs" && "$eventing" == 1 ]]; then
    verify_broker_event "$service"
  fi
  if [[ "$service" == "hello-nodejs" && "${LAMPYR_CI_RELEASE_GATES:-0}" == 1 ]]; then
    release_results="$repo_root/build/release-results"
    rm -rf -- "$release_results"
    mkdir -p "$release_results"
    "$repo_root/build/ly" scale --context "$context" --max-fibers 100 "$project/services/$service"
    python3 "$repo_root/scripts/release-gate.py" \
      --context "$context" \
      --service "$service" \
      --url "$url" \
      --requests 100 \
      --concurrency 1 \
      --output "$release_results/latency.json"
    python3 "$repo_root/scripts/release-gate.py" \
      --context "$context" \
      --service "$service" \
      --url "$url" \
      --requests 300 \
      --concurrency 4 \
      --p95-ms 0 \
      --output "$release_results/reliability.json"
    "$repo_root/build/ly" scale --context "$context" --max-fibers 4 "$project/services/$service"
    python3 "$repo_root/scripts/disruption-gate.py" \
      --context "$context" \
      --service "$service" \
      --url "$url" \
      --placement-timeout-seconds 15 \
      --output "$release_results/disruptions.json"
    phase_done "run release gates for $service"
  fi
  "$repo_root/build/ly" delete --context "$context" "$project/services/$service"
  phase_done "verify lifecycle for $service"
done
}

run_runtime_mode_checks() {
reuse_dir="$project/services/reuse-mode"
mkdir -p "$reuse_dir"
for file in lampyr.yaml Dockerfile handler.js; do
  cp "$repo_root/docs/examples/hello-nodejs/$file" "$reuse_dir/$file"
done
cp -R "$repo_root/docs/examples/hello-nodejs/.lampyr-adapter" "$reuse_dir/.lampyr-adapter"
sed -i 's/^name: hello-nodejs$/name: fresh-mode/' "$reuse_dir/lampyr.yaml"
cat >"$reuse_dir/handler.js" <<'EOF'
const crypto = require('crypto');
const {Response} = require('./.lampyr-adapter/types');
let count = 0;
module.exports = {
  handle: (request, context) => Response.json({
    count: ++count,
    random: {
      crypto: crypto.randomBytes(16).toString('hex'),
      uuid: crypto.randomUUID(),
      math: Math.random(),
    },
  }),
  handleEvent: (event, context) => new Response(undefined, {status: 204}),
};
EOF

# Fibers restored from one snapshot must not share userspace random state.
verify_fresh_fibers() {
  local service=$1
  local responses=()
  for _ in 1 2 3; do
    responses+=("$(curl --fail --silent --show-error --insecure \
      "https://$service.default.localhost:$https_port/test")")
  done
  if ! printf '%s\n' "${responses[@]}" | jq -s -e '
    . as $all
    | all($all[]; .count == 1)
      and all($all[0].random | keys[]; . as $field | ([$all[].random[$field]] | unique | length) == 3)
  ' >/dev/null; then
    echo "$service fibers shared state or random values:" >&2
    printf '%s\n' "${responses[@]}" >&2
    exit 1
  fi
}

if [[ "$runtime_mode_checks" != "reuse" ]]; then
  current_service=fresh-mode
  "$repo_root/build/ly" build "$reuse_dir"
  expose_workspace_services "$project"
  "$repo_root/build/ly" deploy --context "$context" \
    --image "$images/fresh-mode:$tag" "$reuse_dir"
  verify_fresh_fibers fresh-mode
  "$repo_root/build/ly" delete --context "$context" "$reuse_dir"

  python_dir="$project/services/fresh-python"
  mkdir -p "$python_dir"
  for file in lampyr.yaml Dockerfile; do
    cp "$repo_root/docs/examples/hello-python/$file" "$python_dir/$file"
  done
  cp -R "$repo_root/docs/examples/hello-python/.lampyr-adapter" "$python_dir/.lampyr-adapter"
  sed -i 's/^name: hello-python$/name: fresh-python/' "$python_dir/lampyr.yaml"
  cat >"$python_dir/handler.py" <<'EOF'
import os
import random
import secrets
import uuid

from lampyr_types import Context, Event, ProtocolContext, Request, Response

count = 0


def handle(request: Request, context: Context) -> Response:
    global count
    count += 1
    return Response.json({
        "count": count,
        "random": {
            "secrets": secrets.token_hex(16),
            "uuid": str(uuid.uuid4()),
            "random": random.random(),
            "urandom": os.urandom(16).hex(),
        },
    })


def handle_event(event: Event, context: ProtocolContext) -> Response:
    return Response(status=204)
EOF
  current_service=fresh-python
  "$repo_root/build/ly" build "$python_dir"
  expose_workspace_services "$project"
  "$repo_root/build/ly" deploy --context "$context" \
    --image "$images/fresh-python:$tag" "$python_dir"
  verify_fresh_fibers fresh-python
  "$repo_root/build/ly" delete --context "$context" "$python_dir"
  phase_done "verify fresh-per-request state and random isolation"
fi

if [[ "$runtime_mode_checks" != "fresh" ]]; then
  sed -i 's/^name: fresh-mode$/name: reuse-mode/' "$reuse_dir/lampyr.yaml"
  cat >>"$reuse_dir/lampyr.yaml" <<'EOF'
execution:
  mode: reuse
  idleTimeout: 2s
EOF
  current_service=reuse-mode
  "$repo_root/build/ly" build "$reuse_dir"
  expose_workspace_services "$project"
  "$repo_root/build/ly" deploy --context "$context" \
    --image "$images/reuse-mode:$tag" "$reuse_dir"
  peer_dir="$project/services/peer-mode"
  mkdir -p "$peer_dir"
  for file in lampyr.yaml Dockerfile handler.js; do
    cp "$reuse_dir/$file" "$peer_dir/$file"
  done
  cp -R "$reuse_dir/.lampyr-adapter" "$peer_dir/.lampyr-adapter"
  sed -i 's/^name: reuse-mode$/name: peer-mode/' "$peer_dir/lampyr.yaml"
  "$repo_root/build/ly" build "$peer_dir"
  expose_workspace_services "$project"
  "$repo_root/build/ly" deploy --context "$context" \
    --image "$images/peer-mode:$tag" "$peer_dir"
  grants=$(kubectl --context "$context" -n default get capacitygrants.fiberd.io --output json)
  # fiberd-k8s-controller reports each grant Ready in a condition and labels
  # its Pods with the grant's UID.
  homes=$(kubectl --context "$context" -n default get pods \
    --selector 'lampyr.dev/home=true,serving.knative.dev/service in (reuse-mode,peer-mode)' \
    --output json)
  if ! jq -e --argjson homes "$homes" '
  [.items[] | select(.metadata.labels["serving.knative.dev/service"] == "reuse-mode" or .metadata.labels["serving.knative.dev/service"] == "peer-mode")] as $grants
  | [$homes.items[] | select(.status.phase == "Running" and (.metadata.deletionTimestamp | not))] as $pods
  | ($grants | length) == 2
    and ($grants | map(.metadata.uid) | unique | length) == 2
    and all($grants[]; .status.observedGeneration == .metadata.generation
      and any(.status.conditions[]?; .type == "Ready" and .status == "True"))
    and ($pods | length) == 2
    and ($pods | map(.metadata.labels["fiberd.io/grant-uid"]) | sort) == ($grants | map(.metadata.uid) | sort)
    and ($pods | map(.status.podIP) | unique | length) == 2
' <<<"$grants" >/dev/null; then
    echo "two CapacityGrants did not receive distinct admitted home Pods" >&2
    exit 1
  fi
  for expected_count in 1 2; do
    response=$(curl --fail --silent --show-error --insecure \
      "https://peer-mode.default.localhost:$https_port/test")
    jq -e --argjson expected "$expected_count" '.count == $expected' <<<"$response" >/dev/null
  done
  for expected_count in 1 2; do
    response=$(curl --fail --silent --show-error --insecure \
      "https://reuse-mode.default.localhost:$https_port/test")
    if ! jq -e --argjson expected "$expected_count" '.count == $expected' <<<"$response" >/dev/null; then
      echo "reuse did not retain live application state: $response" >&2
      exit 1
    fi
  done
  sleep 3
  response=$(curl --fail --silent --show-error --insecure \
    "https://reuse-mode.default.localhost:$https_port/test")
  if ! jq -e '.count == 1' <<<"$response" >/dev/null; then
    echo "idle release did not restore the original starting state: $response" >&2
    exit 1
  fi
  "$repo_root/build/ly" delete --context "$context" "$reuse_dir"
  response=$(curl --fail --silent --show-error --insecure \
    "https://peer-mode.default.localhost:$https_port/test")
  jq -e '.count == 1' <<<"$response" >/dev/null
  "$repo_root/build/ly" delete --context "$context" "$peer_dir"
  phase_done "verify reusable fibers and idle release"
fi
}

if [[ "$suite" == "full" || "$suite" == "runtime" ]]; then
  run_runtime_checks
fi

if [[ "$runtime_extras" == 1 ]]; then
  run_runtime_mode_checks
fi

if [[ "$suite" == "runtime" || "$suite" == "modes" ]]; then
  docker network disconnect "$provider_network" "$registry"
  docker rm -f "$registry" >/dev/null
  registry_owned=false
  "$repo_root/build/ly" cluster uninstall --provider kind --name "$cluster" \
    --context "$context" --delete-cluster
  cluster_owned=false
  phase_done "delete runtime test infrastructure"
  exit 0
fi

current_service=
"$repo_root/build/ly" cluster health --context "$context"
if [[ "$suite" == "eventing-lifecycle" ]]; then
  "$repo_root/build/ly" cluster uninstall --context "$context"
  kind get clusters | grep -Fxq "$cluster"
  if kubectl --context "$context" -n kube-system get configmap lampyr-install >/dev/null 2>&1; then
    echo "Lampyr installation metadata remains after Eventing uninstall" >&2
    exit 1
  fi
  docker network disconnect "$provider_network" "$registry"
  "$repo_root/build/ly" cluster uninstall --provider kind --name "$cluster" \
    --context "$context" --delete-cluster
  cluster_owned=false
  docker rm -f "$registry" >/dev/null
  registry_owned=false
  phase_done "verify Eventing reinstall and retained/full uninstall"
  exit 0
fi

kubectl config view --raw --minify --context "$context" > "$project/existing-kubeconfig.yaml"
chmod 0600 "$project/existing-kubeconfig.yaml"
kubectl --context "$context" -n kube-system get secret/lampyr-runtime -o json \
  | jq -r .data.runtime | base64 -d | jq .settings > "$project/platform.json"
"$repo_root/build/ly" cluster uninstall --context "$context"
kind get clusters | grep -Fxq "$cluster"
mkdir -p "$project/platform"
cp -a "$repo_root/k8s" "$project/platform/k8s"
printf 'test\n' > "$project/platform/.lampyr-platform"
printf 'apiVersion: lampyr.dev/v1alpha1\nkind: Workspace\n' > "$project/platform/lampyr.workspace.yaml"
for operation in install health uninstall; do
  (
    cd "$project/platform"
    PATH="$project/docker-only-bin" KUBECONFIG="$project/existing-kubeconfig.yaml" \
      LAMPYR_HOME="$project/existing-data" \
      "$repo_root/build/ly" cluster "$operation" --context "$context" \
        --provider-config "$project/platform.json"
  )
done
if [[ -e "$project/existing-data/installations" || -e "$project/platform/providers" ]]; then
  echo "Existing-cluster lifecycle unexpectedly initialized a provider" >&2
  exit 1
fi
phase_done "verify existing-cluster lifecycle without provider artifacts or host Kind"
docker network disconnect "$provider_network" "$registry"
"$repo_root/build/ly" cluster uninstall --provider kind --name "$cluster" \
  --context "$context" --delete-cluster
cluster_owned=false
docker rm -f "$registry" >/dev/null
registry_owned=false
phase_done "verify lifecycle and uninstall"
