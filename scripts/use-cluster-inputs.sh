#!/usr/bin/env bash
set -euo pipefail

# Makes the controller inputs in LAMPYR_CLUSTER_INPUTS the platform
# defaults of this checkout, as a release's build/controller-inputs.json
# is, so a documented ly cluster install --provider kind that gives none
# of them installs the built ones. CI's walkthrough and new-developer
# jobs run it, which follow the docs rather than a suite.
#
# The images go to a registry on 127.0.0.1:PORT, which the Kind provider
# shares when an install asks for that registryPort, so the nodes pull
# them from where the host pushed them. The controller image goes there
# too, since nothing loads it into the cluster for a documented install,
# and the manifest is rewritten to pull it by digest. The fiberd template
# tools go to build/ for ly build and ly deploy.
#
# Only the Kind provider shares that registry. The defaults stay until
# build/controller-inputs.json and build/fiberd-k8s-controller.yaml are
# deleted, and the registry until docker rm -f lampyr-cluster-inputs-PORT,
# so every later install from this checkout that gives no controller
# inputs, of any provider or registryPort, takes them.

# A port with a leading zero would name another container than the one an
# install's registryPort finds.
if (($# != 1)) || [[ ! $1 =~ ^[1-9][0-9]{0,4}$ ]] || (($1 > 65535)); then
  echo "usage: use-cluster-inputs.sh REGISTRY_PORT, a port from 1 to 65535" >&2
  exit 2
fi
port=$1
[[ -n ${LAMPYR_CLUSTER_INPUTS:-} ]] || {
  echo "set LAMPYR_CLUSTER_INPUTS to the directory just upstream::cluster-inputs wrote" >&2
  exit 1
}
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
# shellcheck source=scripts/fiberd-controller-inputs.sh
source "$repo_root/scripts/fiberd-controller-inputs.sh"

registry=localhost:$port
name=lampyr-cluster-inputs-$port
# registry:2.8.3. The Kind provider shares it by its tag, ignoring the digest.
registry_image=registry:2@sha256:a3d8aaa63ed8681a604f1dea0aa03f100d5895b6a58ace528858a7b332415373
# docker run would fail with only "port is already allocated" when another
# container, such as a Kind installation's own registry, holds the port.
others=$(docker ps --filter "publish=$port" --format '{{.Names}}' | grep -vxF -- "$name" || true)
if [[ -n $others ]]; then
  printf '127.0.0.1:%s is already published by %s; remove it or use another port\n' \
    "$port" "${others//$'\n'/, }" >&2
  exit 1
fi
if ! docker container inspect "$name" >/dev/null 2>&1; then
  docker run --detach --name "$name" --restart=always \
    --publish "127.0.0.1:$port:5000" "$registry_image" >/dev/null
elif [[ $(docker container inspect --format '{{.State.Running}}' "$name") != true ]]; then
  # The Kind provider shares only a running registry.
  docker start "$name" >/dev/null
fi
cluster_input_images "$registry"
push_cluster_inputs "$registry"

controller=$(jq -er .controllerImage "$cluster_inputs/inputs.json")
docker tag "$controller" "$registry/fiberd-k8s-controller:ci"
docker push --quiet "$registry/fiberd-k8s-controller:ci" >/dev/null
digest=$("$repo_root/build/tools/oras" resolve --plain-http \
  "$registry/fiberd-k8s-controller:ci")
[[ $digest =~ ^sha256:[0-9a-f]{64}$ ]] || {
  printf 'cannot resolve %s/fiberd-k8s-controller:ci\n' "$registry" >&2
  exit 1
}
docker image rm "$registry/fiberd-k8s-controller:ci" >/dev/null

mkdir -p "$repo_root/build"
manifest=$repo_root/build/fiberd-k8s-controller.yaml
pulled="$registry/fiberd-k8s-controller@$digest"
# The manifest runs the controller image under its lampyr.local tag, on
# image: lines of its own.
grep -Eq "^ +(- )?image: ${controller//./\\.}$" "$cluster_inputs/fiberd-k8s-controller.yaml" || {
  printf 'the built manifest does not run %s\n' "$controller" >&2
  exit 1
}
sed -E "s#^( +(- )?image: )${controller//./\\.}\$#\\1$pulled#" \
  "$cluster_inputs/fiberd-k8s-controller.yaml" >"$manifest.tmp"
grep -Eq "^ +(- )?image: $pulled$" "$manifest.tmp"
mv -- "$manifest.tmp" "$manifest"
# shellcheck disable=SC2154 # set by cluster_input_images
jq -n --arg grantImage "$grant_image" --arg helperImage "$helper_image" \
  '{controllerManifest:"build/fiberd-k8s-controller.yaml",
    grantImage:$grantImage,helperImage:$helperImage}' \
  >"$repo_root/build/controller-inputs.json"
prepare_fiberd_template_tools "$repo_root"
printf 'platform defaults: %s, %s, %s\n' "$pulled" "$grant_image" "$helper_image"
