#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
source "$repo_root/scripts/fiberd-controller-inputs.sh"
cluster=${LAMPYR_EXISTING_KIND_CLUSTER:-lampyr-existing-ci}
registry=${LAMPYR_EXISTING_KIND_REGISTRY:-lampyr-existing-ci-registry}
registry_port=${LAMPYR_EXISTING_KIND_REGISTRY_PORT:-}
http_port=${LAMPYR_EXISTING_KIND_HTTP_PORT:-30080}
https_port=${LAMPYR_EXISTING_KIND_HTTPS_PORT:-30443}
context="kind-$cluster"
project=$(mktemp -d /tmp/lampyr-existing-kind-ci.XXXXXX)
service="$project/services/hello-nodejs"
source "$repo_root/scripts/runtime-examples.sh"
export KUBECONFIG="$project/kubeconfig"
cluster_created=false
registry_created=false
platform_installed=false

cleanup() {
  status=$?
  if [[ $status -ne 0 && $cluster_created == true ]]; then
    kubectl --context "$context" get pods --all-namespaces -o wide 2>&1 || true
    kubectl --context "$context" get events --all-namespaces \
      --sort-by=.lastTimestamp 2>&1 | tail -n 100 || true
    kind export logs --name "$cluster" "$project/kind-logs" >/dev/null 2>&1 || true
  fi
  if [[ $platform_installed == true ]]; then
    "$repo_root/build/ly" cluster uninstall \
      --context "$context" \
      --workspace "$project" >/dev/null 2>&1 || status=1
  fi
  if [[ $cluster_created == true ]]; then
    kind delete cluster --name "$cluster" >/dev/null 2>&1 || status=1
  fi
  if [[ $registry_created == true ]]; then
    docker rm --force "$registry" >/dev/null 2>&1 || status=1
  fi
  rm -r -- "$project"
  return "$status"
}
trap cleanup EXIT

if [[ ! -r /dev/kvm || ! -w /dev/kvm ]]; then
  echo "/dev/kvm must be readable and writable for the existing Kind integration test" >&2
  exit 1
fi
if kind get clusters | grep -Fxq "$cluster"; then
  echo "Kind cluster $cluster already exists; refusing to replace it" >&2
  exit 1
fi
if docker container inspect "$registry" >/dev/null 2>&1; then
  echo "Docker container $registry already exists; refusing to replace it" >&2
  exit 1
fi
prepare_fiberd_template_tools "$repo_root"

"$repo_root/build/ly" scaffold workspace "$project" >/dev/null
rm -r -- "$project/services/app"
cp -a "$repo_root/docs/examples/hello-nodejs" "$service"
rm -rf -- "$service/.lampyr"
sed -i 's/^    cpu: 250m$/    cpu: 10m/' "$service/lampyr.yaml"
expose_workspace_services "$project"
export LAMPYR_HOME="$project/data"

cat >"$project/kind.yaml" <<EOF
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

cluster_created=true
kind create cluster \
  --name "$cluster" \
  --config "$project/kind.yaml" \
  --wait 180s

registry_created=true
registry_publish="127.0.0.1::5000"
if [[ -n $registry_port ]]; then
  registry_publish="127.0.0.1:$registry_port:5000"
fi
docker run --detach \
  --name "$registry" \
  --network kind \
  --publish "$registry_publish" \
  registry:2 >/dev/null
if [[ -z $registry_port ]]; then
  registry_port=$(
    docker container inspect \
      --format '{{(index (index .NetworkSettings.Ports "5000/tcp") 0).HostPort}}' \
      "$registry"
  )
fi

# Built grant and helper images go to the registry, which the node pulls
# from by the name the host pushes to.
for node in $(kind get nodes --name "$cluster"); do
  docker exec "$node" mkdir -p "/etc/containerd/certs.d/localhost:$registry_port"
  cat <<EOF | docker exec -i "$node" tee "/etc/containerd/certs.d/localhost:$registry_port/hosts.toml" >/dev/null
server = "http://$registry:5000"

[host."http://$registry:5000"]
  capabilities = ["pull", "resolve"]
EOF
done
cluster_input_images "localhost:$registry_port"
push_cluster_inputs "localhost:$registry_port"

fiberd_image=${LAMPYR_EXISTING_KIND_FIBERD_IMAGE:-ghcr.io/hyperlight-dev/lampyr/fiberd:preview}
router_image=${LAMPYR_EXISTING_KIND_ROUTER_IMAGE:-ghcr.io/hyperlight-dev/lampyr/fiber-router:preview}
mapfile -t images < <(jq -r '.[]' <<<"$controller_images")
kind load docker-image \
  --name "$cluster" \
  "$fiberd_image" \
  "$router_image" \
  "${images[@]}"

# No provider made this cluster's registry, so the install names it as a
# mirror, from the name the host pushes to to the one the kind network
# reaches. ly deploy pushes templates there, and zygoteRegistries
# defaults to it.
jq -n --arg fiberd "$fiberd_image" --arg router "$router_image" \
  --argjson port "$https_port" \
  --arg controllerManifest "$controller_manifest" \
  --arg grantImage "$grant_image" \
  --arg helperImage "$helper_image" \
  --arg source "localhost:$registry_port" \
  --arg target "$registry:5000" \
  '{fiberdImage:$fiberd,routerImage:$router,httpsPort:$port,
    controllerManifest:$controllerManifest,grantImage:$grantImage,
    helperImage:$helperImage,
    registryMirrors:{($source):{registry:$target,plainHttp:true}}}' \
  >"$project/platform.json"
"$repo_root/build/ly" cluster install \
  --context "$context" \
  --workspace "$project" \
  --provider-config "$project/platform.json"
platform_installed=true

if [[ -e "$LAMPYR_HOME/installations" || -e "$project/providers" ]]; then
  echo "Existing-cluster installation unexpectedly initialized an infrastructure provider" >&2
  exit 1
fi

"$repo_root/build/ly" build "$service"
image="localhost:$registry_port/ci/hello-nodejs:${GITHUB_SHA:-local}"
"$repo_root/build/ly" deploy \
  --context "$context" \
  --image "$image" \
  "$service"

revision_uid=$(
  kubectl --context "$context" get revisions.serving.knative.dev \
    --selector serving.knative.dev/service=hello-nodejs -o json |
    jq -er '.items | select(length == 1) | .[0].metadata.uid'
)
"$repo_root/build/ly" deploy \
  --context "$context" \
  --image "$image" \
  "$service"
if ! kubectl --context "$context" get revisions.serving.knative.dev \
  --selector serving.knative.dev/service=hello-nodejs -o json |
  jq -e --arg uid "$revision_uid" \
    '.items | length == 1 and .[0].metadata.uid == $uid' >/dev/null; then
  echo "Repeated deployment did not preserve the original Knative Revision" >&2
  exit 1
fi

response=$(curl --fail --silent --show-error --insecure \
  --retry 10 \
  --retry-all-errors \
  --retry-delay 2 \
  "https://hello-nodejs.default.localhost:$https_port/test")
grep -Fq "hello from Hyperlight edge" <<<"$response"

# Exposure is mutable Service policy, not immutable Revision identity.
internal="$project/services/hello-internal"
cp -a "$service" "$internal"
rm -r -- "$internal/.lampyr"
sed -i 's/^name: hello-nodejs$/name: hello-internal/' "$internal/lampyr.yaml"
"$repo_root/build/ly" build "$internal"
"$repo_root/build/ly" deploy --context "$context" \
  --image "localhost:$registry_port/ci/hello-internal:${GITHUB_SHA:-local}" "$internal"
internal_status=$("$repo_root/build/ly" status --context "$context" "$internal")
grep -Fq 'http://hello-internal.default.svc.cluster.local' <<<"$internal_status"
code=$(curl --silent --show-error --insecure --output "$project/internal-response" \
  --write-out '%{http_code}' \
  "https://hello-internal.default.localhost:$https_port/test")
[[ "$code" == 404 ]]
kubectl --context "$context" run hostname-exposure-probe --restart=Never \
  --image=busybox:1.37.0 --command -- sh -ec \
  'wget -qO- http://hello-nodejs.default.svc.cluster.local/test; wget -qO- http://hello-internal.default.svc.cluster.local/test'
kubectl --context "$context" wait --for=jsonpath='{.status.phase}'=Succeeded \
  pod/hostname-exposure-probe --timeout=90s
kubectl --context "$context" logs hostname-exposure-probe | grep -Fq 'hello from Hyperlight edge'
kubectl --context "$context" delete pod hostname-exposure-probe --wait=true

printf 'apiVersion: lampyr.dev/v1alpha1\nkind: Workspace\ningress:\n  services: []\n' \
  > "$project/lampyr.workspace.yaml"
"$repo_root/build/ly" deploy --context "$context" --image "$image" "$service"
[[ $(kubectl --context "$context" get ksvc hello-nodejs \
  -o jsonpath='{.status.url}') == http://hello-nodejs.default.svc.cluster.local ]]
code=$(curl --silent --show-error --insecure --output /dev/null \
  --write-out '%{http_code}' "https://hello-nodejs.default.localhost:$https_port/test")
[[ "$code" == 404 ]]

# A label outside the last-applied configuration must also be removed.
kubectl --context "$context" annotate ksvc hello-nodejs kubectl.kubernetes.io/last-applied-configuration-
printf 'apiVersion: lampyr.dev/v1alpha1\nkind: Workspace\ningress:\n  services: [hello-nodejs]\n' \
  > "$project/lampyr.workspace.yaml"
"$repo_root/build/ly" deploy --context "$context" --image "$image" "$service"
curl --fail --silent --show-error --insecure \
  "https://hello-nodejs.default.localhost:$https_port/test" | grep -Fq 'hello from Hyperlight edge'
[[ $(kubectl --context "$context" get revisions.serving.knative.dev \
  --selector serving.knative.dev/service=hello-nodejs -o json |
  jq -er '.items | select(length == 1) | .[0].metadata.uid') == "$revision_uid" ]]
"$repo_root/build/ly" delete --context "$context" "$internal"

"$repo_root/build/ly" status --context "$context" "$service"
"$repo_root/build/ly" delete --context "$context" "$service"
"$repo_root/build/ly" cluster health --context "$context" --workspace "$project"
"$repo_root/build/ly" cluster uninstall --context "$context" --workspace "$project"
platform_installed=false

kind get clusters | grep -Fxq "$cluster"
kubectl --context "$context" get namespace kube-system >/dev/null
if kubectl --context "$context" -n kube-system get configmap lampyr-install >/dev/null 2>&1; then
  echo "Lampyr installation metadata remains after uninstall" >&2
  exit 1
fi

echo "Existing Kind cluster installation and service lifecycle passed"
