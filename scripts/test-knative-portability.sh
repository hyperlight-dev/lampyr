#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
source "$repo_root/scripts/fiberd-controller-inputs.sh"
fixture="$repo_root/scripts/fixtures/knative-portability"
suffix=${LAMPYR_PORTABILITY_SUFFIX:-$$}
vanilla_cluster="knative-vanilla-$suffix"
lampyr_cluster="knative-lampyr-$suffix"
registry="lampyr-portability-registry-$suffix"
work=$(mktemp -d /tmp/lampyr-knative-portability.XXXXXX)
workspace="$work/workspace"
service_dir="$work/service"
service_manifest="$work/service.yaml"
h2c_manifest="$work/h2c-service.yaml"
lampyr_network=
port_forward_pid=
registry_owned=false
vanilla_cluster_owned=false
lampyr_cluster_owned=false

read -r dynamic_registry_port dynamic_platform_port dynamic_http_port \
    dynamic_https_port dynamic_forward_port < <(
    python3 - <<'PY'
import socket

sockets = []
ports = []
for _ in range(5):
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    sockets.append(sock)
    ports.append(sock.getsockname()[1])
print(*ports)
PY
)
registry_port=${LAMPYR_PORTABILITY_REGISTRY_PORT:-$dynamic_registry_port}
platform_port=${LAMPYR_PORTABILITY_PLATFORM_PORT:-$dynamic_platform_port}
http_port=${LAMPYR_PORTABILITY_HTTP_PORT:-$dynamic_http_port}
https_port=${LAMPYR_PORTABILITY_HTTPS_PORT:-$dynamic_https_port}
forward_port=${LAMPYR_PORTABILITY_FORWARD_PORT:-$dynamic_forward_port}
normal_image="localhost:$registry_port/knative-portability-normal:v1"
snapshot_image="localhost:$registry_port/knative-portability-snapshot:v1"
fiberd_image="lampyr.local/knative-portability-fiberd:$suffix"
router_image="lampyr.local/knative-portability-router:$suffix"
builder=${BUILDX_BUILDER:-$(docker context inspect --format '{{.Name}}')}

cleanup() {
    status=$?
    trap - EXIT INT TERM
    set +e
    if [[ -n "$port_forward_pid" ]]; then
        kill "$port_forward_pid" 2>/dev/null
        wait "$port_forward_pid" 2>/dev/null
    fi
    if [[ "${LAMPYR_PORTABILITY_KEEP:-0}" == 1 ]]; then
        printf 'Kept workspace: %s\n' "$work"
        printf 'Vanilla context: kind-%s\n' "$vanilla_cluster"
        printf 'Lampyr context: kind-%s\n' "$lampyr_cluster"
        printf 'Registry: localhost:%s\n' "$registry_port"
        exit "$status"
    fi
    if [[ "$registry_owned" == true && -n "$lampyr_network" ]]; then
        docker network disconnect "$lampyr_network" "$registry" >/dev/null 2>&1
    fi
    if [[ "$lampyr_cluster_owned" == true ]] &&
        kind get clusters | grep -Fxq "$lampyr_cluster"; then
        (
            cd "$repo_root"
            build/ly cluster uninstall \
                --provider kind \
                --name "$lampyr_cluster" \
                --workspace "$workspace" \
                --context "kind-$lampyr_cluster" \
                --delete-cluster \
                --allow-docker \
                --non-interactive
        ) >/dev/null 2>&1 || kind delete cluster --name "$lampyr_cluster"
    fi
    if [[ "$vanilla_cluster_owned" == true ]] &&
        kind get clusters | grep -Fxq "$vanilla_cluster"; then
        kind delete cluster --name "$vanilla_cluster"
    fi
    if [[ "$registry_owned" == true ]]; then
        docker rm --force "$registry" >/dev/null 2>&1
    fi
    rm -rf -- "$work"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

for tool in curl docker jq kind kubectl python3; do
    command -v "$tool" >/dev/null || {
        printf 'missing required tool: %s\n' "$tool" >&2
        exit 1
    }
done
[[ -r /dev/kvm && -w /dev/kvm ]] || {
    echo "/dev/kvm must be readable and writable" >&2
    exit 1
}
if docker container inspect "$registry" >/dev/null 2>&1; then
    echo "container name already exists: $registry" >&2
    exit 1
fi
for cluster in "$vanilla_cluster" "$lampyr_cluster"; do
    if kind get clusters | grep -Fxq "$cluster"; then
        echo "Kind cluster name already exists: $cluster" >&2
        exit 1
    fi
done

if [[ "${LAMPYR_PORTABILITY_PREBUILT:-0}" != 1 ]]; then
    (
        cd "$repo_root"
        just setup-hluk build-fiberd build-control-plane build-ly kind::build
    )
fi
prepare_fiberd_template_tools "$repo_root"

registry_owned=true
docker run --detach \
    --restart=always \
    --publish "127.0.0.1:$registry_port:5000" \
    --name "$registry" \
    registry:2 >/dev/null
# Built grant and helper images go to this registry, which the Lampyr
# cluster's node pulls from once configure_registry has run on it.
cluster_input_images "localhost:$registry_port"
push_cluster_inputs "localhost:$registry_port"

cp -a "$repo_root/docs/examples/hello-nodejs" "$service_dir"
rm -rf -- "$service_dir/.lampyr"
cp "$fixture/handler.js" "$service_dir/handler.js"
sed -i 's/^name: hello-nodejs$/name: portability-proof/' \
    "$service_dir/lampyr.yaml"

docker buildx build \
    --builder "$builder" \
    --load \
    --platform linux/amd64 \
    --file "$fixture/Dockerfile.normal" \
    --tag "$normal_image" \
    "$service_dir"
normal_push=$(docker push "$normal_image")
printf '%s\n' "$normal_push"
normal_digest=$(
    sed -n 's/^v1: digest: \(sha256:[0-9a-f]\{64\}\).*$/\1/p' \
        <<<"$normal_push"
)
[[ -n "$normal_digest" ]]
normal_ref="localhost:$registry_port/knative-portability-normal@$normal_digest"

"$repo_root/build/ly" build "$service_dir"
snapshot_push=$(
    "$repo_root/build/ly" push \
        --image "$snapshot_image" \
        "$service_dir"
)
printf '%s\n' "$snapshot_push"
snapshot_ref=$(sed -n 's/^pushed image //p' <<<"$snapshot_push")
[[ "$snapshot_ref" == localhost:"$registry_port"/knative-portability-snapshot@sha256:* ]]


docker buildx build \
    --builder "$builder" \
    --load \
    --platform linux/amd64 \
    --tag "$fiberd_image" \
    --file "$repo_root/fiberd/ci.Dockerfile" \
    "$repo_root"
docker buildx build \
    --builder "$builder" \
    --load \
    --platform linux/amd64 \
    --tag "$router_image" \
    --file "$repo_root/activator/ci.Dockerfile" \
    "$repo_root"

cat >"$work/vanilla-kind.yaml" <<EOF
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
containerdConfigPatches:
  - |
    [plugins.'io.containerd.cri.v1.images'.registry]
      config_path = '/etc/containerd/certs.d'
nodes:
  - role: control-plane
    image: kindest/node:v1.35.8@sha256:07b2536e30b803ed61d1677a79df6115f798ce64c80f9e22f6ed45afd09323c0
EOF

cat >"$work/lampyr-kind.yaml" <<EOF
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
EOF

jq -n \
    --rawfile source "$work/lampyr-kind.yaml" \
    --arg fiberd "$fiberd_image" \
    --arg router "$router_image" \
    --argjson registryPort "$platform_port" \
    --arg controllerManifest "$controller_manifest" \
    --arg grantImage "$grant_image" \
    --arg helperImage "$helper_image" \
    --argjson images "$controller_images" \
    '{source:$source,fiberdImage:$fiberd,routerImage:$router,
      registryPort:$registryPort,controllerManifest:$controllerManifest,
      grantImage:$grantImage,helperImage:$helperImage,
      images:$images}' >"$work/provider.json"

configure_registry() {
    local node=$1
    docker exec "$node" \
        mkdir -p "/etc/containerd/certs.d/localhost:$registry_port"
    cat <<EOF | docker exec -i "$node" \
        tee "/etc/containerd/certs.d/localhost:$registry_port/hosts.toml" \
        >/dev/null
server = "http://$registry:5000"

[host."http://$registry:5000"]
  capabilities = ["pull", "resolve", "push"]
EOF
}

"$repo_root/build/ly" scaffold workspace "$workspace" >/dev/null
install_lampyr() {
    "$repo_root/build/ly" cluster install \
        --provider kind \
        --name "$lampyr_cluster" \
        --workspace "$workspace" \
        --provider-config "$work/provider.json" \
        --allow-docker \
        --non-interactive
}
lampyr_cluster_owned=true
install_lampyr
lampyr_context="kind-$lampyr_cluster"
lampyr_node="$lampyr_cluster-control-plane"
for deployment in controller activator autoscaler; do
    if kubectl --context "$lampyr_context" -n knative-serving get \
        deployment "$deployment" >/dev/null 2>&1; then
        echo "stock Knative deployment/$deployment is installed" >&2
        exit 1
    fi
done
if kubectl --context "$lampyr_context" -n knative-serving get \
    horizontalpodautoscaler activator >/dev/null 2>&1; then
    echo "stock Knative Activator autoscaling is still installed" >&2
    exit 1
fi
lampyr_network=$(
    docker inspect "$lampyr_node" |
        jq -er '.[0].NetworkSettings.Networks | keys[] | select(startswith("lampyr-"))'
)
docker network connect "$lampyr_network" "$registry"
configure_registry "$lampyr_node"
home_metadata=$(
    jq -nc '{
      labels:{"portability.lampyr.dev/proof":"true"},
      annotations:{"portability.lampyr.dev/profile":"same-service"}
    }'
)
kubectl --context "$lampyr_context" -n kube-system patch \
    configmap lampyr-install \
    --type merge \
    --patch "$(jq -nc --arg metadata "$home_metadata" \
        '{data:{homePodMetadata:$metadata}}')"
# A Revision runs on Lampyr only with a fiberd template sealed to the
# cluster's fleet key, which ly deploy builds, seals, pushes and records.
# Deploy the service once to get it, then delete it, so the shared KService
# below creates portability-proof-00001 afresh and carries the same template
# reference on both clusters; vanilla Knative ignores the annotation.
template_revision=$(
    "$repo_root/build/ly" deploy \
        --context "$lampyr_context" \
        --image "$snapshot_image" \
        "$service_dir" |
        awk '$1 == "revision" {print $2}'
)
[[ -n "$template_revision" ]]
template_ref=$(
    kubectl --context "$lampyr_context" -n default get \
        "revision/$template_revision" \
        -o jsonpath='{.metadata.annotations.lampyr\.dev/fiberd-template}'
)
# ly deploy pushes templates to the installation's registry, which the
# provider runs on platform_port.
[[ "$template_ref" == localhost:"$platform_port"/lampyr/fiberd-templates@sha256:* ]]
"$repo_root/build/ly" delete --context "$lampyr_context" "$service_dir"

sed \
    -e "s|__NORMAL_IMAGE__|$normal_ref|" \
    -e "s|__SNAPSHOT_IMAGE__|$snapshot_ref|" \
    -e "s|__FIBERD_TEMPLATE__|$template_ref|" \
    "$fixture/service.yaml.tmpl" >"$service_manifest"
cat >"$h2c_manifest" <<EOF
apiVersion: serving.knative.dev/v1
kind: Service
metadata:
  name: portability-proof-h2c
  namespace: default
spec:
  template:
    metadata:
      annotations:
        autoscaling.knative.dev/max-scale: "2"
        lampyr.dev/snapshot-artifact: $snapshot_ref
        lampyr.dev/fiberd-template: $template_ref
    spec:
      containerConcurrency: 1
      containers:
        - image: $normal_ref
          ports:
            - name: h2c
              containerPort: 8080
EOF
printf 'Shared KService: %s\n' "$service_manifest"
sha256sum "$service_manifest"

vanilla_cluster_owned=true
kind create cluster \
    --name "$vanilla_cluster" \
    --config "$work/vanilla-kind.yaml" \
    --wait 120s
if ! docker inspect "$registry" |
    jq -e '.[0].NetworkSettings.Networks | has("kind")' >/dev/null; then
    docker network connect kind "$registry"
fi
configure_registry "$vanilla_cluster-control-plane"
vanilla_context="kind-$vanilla_cluster"
kubectl --context "$vanilla_context" apply -f \
    https://github.com/knative/serving/releases/download/knative-v1.23.0/serving-crds.yaml
kubectl --context "$vanilla_context" wait \
    --for=condition=Established \
    crd/services.serving.knative.dev \
    --timeout=180s
kubectl --context "$vanilla_context" apply -f \
    https://github.com/knative/serving/releases/download/knative-v1.23.0/serving-core.yaml
if ! kubectl --context "$vanilla_context" -n knative-serving wait \
    --for=condition=Available deployment --all --timeout=300s; then
    # Say why, since the cluster is deleted on the way out.
    kubectl --context "$vanilla_context" get pods -A -o wide >&2 || true
    kubectl --context "$vanilla_context" -n knative-serving describe pods >&2 || true
    kubectl --context "$vanilla_context" get events -A --sort-by=.lastTimestamp >&2 || true
    exit 1
fi
kubectl --context "$vanilla_context" apply -f \
    https://github.com/knative/net-kourier/releases/download/knative-v1.23.0/kourier.yaml
kubectl --context "$vanilla_context" -n knative-serving patch \
    configmap config-network \
    --type merge \
    -p '{"data":{"ingress-class":"kourier.ingress.networking.knative.dev"}}'
kubectl --context "$vanilla_context" -n knative-serving rollout status \
    deployment/net-kourier-controller --timeout=180s
kubectl --context "$vanilla_context" -n kourier-system rollout status \
    deployment/3scale-kourier-gateway --timeout=180s
kubectl --context "$vanilla_context" apply -f "$service_manifest"
kubectl --context "$vanilla_context" -n default wait \
    --for=condition=Ready kservice/portability-proof --timeout=300s

kubectl --context "$vanilla_context" -n kourier-system port-forward \
    service/kourier-internal "$forward_port:80" \
    >"$work/port-forward.log" 2>&1 &
port_forward_pid=$!
for _ in $(seq 1 30); do
    if curl --silent --output /dev/null \
        "http://127.0.0.1:$forward_port/"; then
        break
    fi
    sleep 1
done
vanilla_response=$(
    curl --fail --silent --show-error --max-time 60 \
        -H 'Host: portability-proof.default.svc.cluster.local' \
        "http://127.0.0.1:$forward_port/portable"
)
kubectl --context "$vanilla_context" -n default get pods \
    -l serving.knative.dev/service=portability-proof \
    -o json >"$work/vanilla-pods.json"
kill "$port_forward_pid"
wait "$port_forward_pid" 2>/dev/null || true
port_forward_pid=

kubectl --context "$lampyr_context" apply -f "$service_manifest"
kubectl --context "$lampyr_context" -n default wait \
    --for=condition=Ready kservice/portability-proof --timeout=300s
kubectl --context "$lampyr_context" apply -f "$h2c_manifest"
kubectl --context "$lampyr_context" -n default wait \
    --for=condition=Ready kservice/portability-proof-h2c --timeout=300s
kubectl --context "$lampyr_context" -n default get \
    service/portability-proof-h2c-00001 -o json |
    jq -e '
      .spec.type == "ExternalName" and
      .spec.externalName == "fiber-router-h2c.lampyr-system.svc.cluster.local" and
      .spec.ports == [{
        "appProtocol":"kubernetes.io/h2c",
        "name":"http2",
        "port":81,
        "protocol":"TCP",
        "targetPort":81
      }]
    ' >/dev/null
kubectl --context "$lampyr_context" -n default get \
    ingresses.networking.internal.knative.dev -o json |
    jq -e '
      any(
        .items[].spec.rules[].http.paths[].splits[];
        .serviceName == "portability-proof-h2c-00001" and
        .servicePort == 81
      )
    ' >/dev/null
lampyr_response=$(
    curl --fail --silent --show-error --max-time 60 \
        -H 'Host: portability-proof.default.localhost' \
        "http://127.0.0.1:$http_port/portable"
)

normalized_vanilla=$(jq --compact-output --sort-keys . <<<"$vanilla_response")
normalized_lampyr=$(jq --compact-output --sort-keys . <<<"$lampyr_response")
[[ "$normalized_vanilla" == "$normalized_lampyr" ]] || {
    printf 'vanilla response: %s\n' "$normalized_vanilla" >&2
    printf 'Lampyr response: %s\n' "$normalized_lampyr" >&2
    exit 1
}
[[ "$(jq -r .namespace <<<"$vanilla_response")" == "" ]]
[[ "$(jq -r .revision <<<"$vanilla_response")" == "" ]]

vanilla_spec=$(
    kubectl --context "$vanilla_context" -n default get \
        kservice portability-proof -o json |
        jq -S '.spec' |
        sha256sum |
        cut -d' ' -f1
)
lampyr_spec=$(
    kubectl --context "$lampyr_context" -n default get \
        kservice portability-proof -o json |
        jq -S '.spec' |
        sha256sum |
        cut -d' ' -f1
)
[[ "$vanilla_spec" == "$lampyr_spec" ]]

vanilla_crd=$(
    kubectl --context "$vanilla_context" get \
        crd/services.serving.knative.dev -o json |
        jq -S '.spec' |
        sha256sum |
        cut -d' ' -f1
)
lampyr_crd=$(
    kubectl --context "$lampyr_context" get \
        crd/services.serving.knative.dev -o json |
        jq -S '.spec' |
        sha256sum |
        cut -d' ' -f1
)
[[ "$vanilla_crd" == "$lampyr_crd" ]]

jq -e --arg image "$normal_ref" '
  .items | length > 0 and
  any(.[]; any(.spec.containers[]; .name == "user-container" and .image == $image))
' "$work/vanilla-pods.json" >/dev/null
kubectl --context "$lampyr_context" -n default get \
    service/portability-proof-00001 -o json |
    jq -e '
      .spec.type == "ExternalName" and
      .spec.externalName == "fiber-router.lampyr-system.svc.cluster.local"
    ' >/dev/null
# The serving controller writes fiberd-k8s-controller's CapacityGrant: the
# zygote is the Revision's fiberd template, provider home metadata lands on
# the grant's Pod template, and the controller reports Ready in a condition.
template_digest=$(
    kubectl --context "$lampyr_context" -n default get \
        revision/portability-proof-00001 \
        -o jsonpath='{.metadata.annotations.lampyr\.dev/fiberd-template}' |
        sed -n 's/^.*@\(sha256:[0-9a-f]\{64\}\)$/\1/p'
)
[[ -n "$template_digest" ]]
grant_uid=$(
    kubectl --context "$lampyr_context" -n default get \
        capacitygrants.fiberd.io/portability-proof-00001 \
        -o jsonpath='{.metadata.uid}'
)
kubectl --context "$lampyr_context" -n default get \
    capacitygrants.fiberd.io/portability-proof-00001 -o json |
    jq -e --arg digest "$template_digest" '
      .spec.zygote.digest == $digest and
      .spec.podTemplate.metadata.labels["portability.lampyr.dev/proof"] == "true" and
      .spec.podTemplate.metadata.annotations["portability.lampyr.dev/profile"] == "same-service" and
      .status.observedGeneration == .metadata.generation and
      any(.status.conditions[]?; .type == "Ready" and .status == "True")
    ' >/dev/null
# The controller labels the grant's Pods with its name and UID.
kubectl --context "$lampyr_context" -n default get pods \
    --selector "fiberd.io/grant=portability-proof-00001,fiberd.io/grant-uid=$grant_uid" \
    -o json |
    jq -e '
      .items | length > 0 and all(.[];
        .metadata.labels["portability.lampyr.dev/proof"] == "true" and
        .metadata.annotations["portability.lampyr.dev/profile"] == "same-service")
    ' >/dev/null

"$repo_root/build/ly" scale \
    --context "$lampyr_context" \
    --max-fibers 3 \
    "$service_dir"
[[ "$(
    kubectl --context "$lampyr_context" -n default get \
        revision/portability-proof-00001 \
        -o jsonpath='{.metadata.annotations.autoscaling\.knative\.dev/max-scale}'
)" == 3 ]]
[[ "$(
    kubectl --context "$lampyr_context" -n default get \
        capacitygrants.fiberd.io/portability-proof-00001 \
        -o jsonpath='{.spec.fibers.max}'
)" == 3 ]]
kubectl --context "$lampyr_context" -n default delete \
    capacitygrants.fiberd.io/portability-proof-00001 --wait=true
grant_repaired=false
for _ in $(seq 1 180); do
    grant_state=$(
        kubectl --context "$lampyr_context" -n default get \
            capacitygrants.fiberd.io/portability-proof-00001 \
            -o jsonpath='{.spec.fibers.max}:{.status.conditions[?(@.type=="Ready")].status}' \
            2>/dev/null || true
    )
    if [[ "$grant_state" == "3:True" ]]; then
        grant_repaired=true
        break
    fi
    sleep 1
done
[[ "$grant_repaired" == true ]]
kubectl --context "$lampyr_context" -n default wait \
    --for=condition=Ready revision/portability-proof-00001 --timeout=180s

install_lampyr
for deployment in controller activator autoscaler; do
    if kubectl --context "$lampyr_context" -n knative-serving get \
        deployment "$deployment" >/dev/null 2>&1; then
        echo "reinstall created stock Knative deployment/$deployment" >&2
        exit 1
    fi
done
if [[ -n "$(
    kubectl --context "$lampyr_context" -n default get pods \
        --selector 'serving.knative.dev/service=portability-proof,!lampyr.dev/home' \
        --output name
)" ]]; then
    echo "stock Revision reconciliation created application Pods during reinstall" >&2
    exit 1
fi

kubectl --context "$lampyr_context" -n default delete \
    service/portability-proof-00001 --wait=true
backend_repaired=false
for _ in $(seq 1 50); do
    if [[ "$(
        kubectl --context "$lampyr_context" -n default get \
            service/portability-proof-00001 \
            -o jsonpath='{.spec.externalName}' 2>/dev/null || true
    )" == "fiber-router.lampyr-system.svc.cluster.local" ]]; then
        backend_repaired=true
        break
    fi
    sleep 1
done
[[ "$backend_repaired" == true ]]
repaired_response=$(
    curl --fail --silent --show-error --max-time 60 \
        -H 'Host: portability-proof.default.localhost' \
        "http://127.0.0.1:$http_port/portable"
)
[[ "$(jq --compact-output --sort-keys . <<<"$repaired_response")" == "$normalized_lampyr" ]]

printf '\nIdentical response: %s\n' "$normalized_vanilla"
printf 'Defaulted KService spec: sha256:%s\n' "$vanilla_spec"
printf 'Knative Service CRD spec: sha256:%s\n' "$vanilla_crd"
printf '\nVanilla workloads:\n'
jq -r '
  .items[] |
  "\(.metadata.name): " +
  ([.spec.containers[] | "\(.name)=\(.image)"] | join(", "))
' "$work/vanilla-pods.json"
printf '\nLampyr backend:\n'
kubectl --context "$lampyr_context" -n default get \
    service/portability-proof-00001 \
    capacitygrants.fiberd.io/portability-proof-00001

if [[ "${LAMPYR_PORTABILITY_KEEP:-0}" == 1 ]]; then
    printf '\nVanilla request:\n'
    printf 'kubectl --context %q -n kourier-system port-forward service/kourier-internal %s:80\n' \
        "$vanilla_context" "$forward_port"
    printf "curl -H 'Host: portability-proof.default.svc.cluster.local' http://127.0.0.1:%s/portable\n" \
        "$forward_port"
    printf '\nLampyr request:\n'
    printf "curl -H 'Host: portability-proof.default.localhost' http://127.0.0.1:%s/portable\n" \
        "$http_port"
fi
