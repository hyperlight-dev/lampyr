#!/usr/bin/env bash

# Shared by the cluster integration suites. fiberd-k8s-controller has no
# release yet, so its manifest, the stock fiberd image grant Pods run and
# Lampyr's helper image are installation inputs, as for ly cluster install:
#
#   LAMPYR_CONTROLLER_MANIFEST  the controller's install manifest, a path or
#                               an https:// URL
#   LAMPYR_GRANT_IMAGE          the fiberd image grant Pods run, pinned by
#                               @sha256: digest
#   LAMPYR_HELPER_IMAGE         the lampyr-hluk-helper image, pinned by
#                               @sha256: digest
#   LAMPYR_FIBERD_SRC           a fiberd checkout that builds zygotectl and
#                               grant-issuer for ly build and ly deploy;
#                               unset, they must already be in build/
#
# Or, in place of all four, and refused alongside any of them, inputs
# built rather than published:
#
#   LAMPYR_CLUSTER_INPUTS       the directory just upstream::cluster-inputs
#                               wrote, as CI does
#
# Sourcing checks them, so a suite fails before it creates anything.
# Built images are pulled from the suite's own registry, so a suite calls
# cluster_input_images with that registry before it installs, adds
# controller_images to the images its provider loads, and calls
# push_cluster_inputs once the registry serves and before it deploys.
# zygoteRegistries is not an input: ly cluster install defaults it to the
# Kind or k3d local registry, where ly deploy pushes templates, and the
# Azure suite gives its own.

missing_input() {
  printf 'set %s to %s, or LAMPYR_CLUSTER_INPUTS to built inputs\n' "$1" "$2" >&2
  exit 1
}

cluster_inputs=
# shellcheck disable=SC2034 # read by the suites that source this
controller_images='[]'
if [[ -n ${LAMPYR_CLUSTER_INPUTS:-} ]]; then
  # The built inputs replace all four, so one also set would be ignored.
  for published in LAMPYR_CONTROLLER_MANIFEST LAMPYR_GRANT_IMAGE LAMPYR_HELPER_IMAGE \
    LAMPYR_FIBERD_SRC; do
    [[ -z ${!published:-} ]] || {
      printf 'set LAMPYR_CLUSTER_INPUTS or %s, not both\n' "$published" >&2
      exit 1
    }
  done
  cluster_inputs=$(realpath -- "$LAMPYR_CLUSTER_INPUTS")
  for input in inputs.json fiberd-k8s-controller.yaml images/fiberd-k8s-controller.tar \
    images/upstream-fiberd/index.json images/hluk-helper/index.json \
    bin/hluk-helper bin/zygotectl bin/grant-issuer; do
    [[ -f $cluster_inputs/$input ]] || {
      printf 'LAMPYR_CLUSTER_INPUTS has no %s: %s\n' "$input" "$cluster_inputs" >&2
      exit 1
    }
  done
  controller_manifest=$cluster_inputs/fiberd-k8s-controller.yaml
  # Set by cluster_input_images, from the registry the suite pushes to.
  grant_image=
  helper_image=
else
  [[ -n ${LAMPYR_CONTROLLER_MANIFEST:-} ]] ||
    missing_input LAMPYR_CONTROLLER_MANIFEST "the fiberd-k8s-controller install manifest"
  [[ -n ${LAMPYR_GRANT_IMAGE:-} ]] ||
    missing_input LAMPYR_GRANT_IMAGE "the digest-pinned fiberd image grant Pods run"
  [[ -n ${LAMPYR_HELPER_IMAGE:-} ]] ||
    missing_input LAMPYR_HELPER_IMAGE "the digest-pinned lampyr-hluk-helper image"
  controller_manifest=$LAMPYR_CONTROLLER_MANIFEST
  # ly resolves a relative controllerManifest against the --provider-config
  # file that names it, which a suite writes in a directory of its own.
  if [[ $controller_manifest != https://* ]]; then
    [[ -f $controller_manifest && -r $controller_manifest ]] || {
      printf 'LAMPYR_CONTROLLER_MANIFEST is not a readable file: %s\n' "$controller_manifest" >&2
      exit 1
    }
    controller_manifest=$(realpath -- "$controller_manifest")
  fi
  # shellcheck disable=SC2034 # read by the suites that source this
  grant_image=$LAMPYR_GRANT_IMAGE
  # shellcheck disable=SC2034
  helper_image=$LAMPYR_HELPER_IMAGE
  for pinned in LAMPYR_GRANT_IMAGE LAMPYR_HELPER_IMAGE; do
    [[ ${!pinned} == *@sha256:* ]] || {
      printf '%s must be pinned by a sha256 digest: %s\n' "$pinned" "${!pinned}" >&2
      exit 1
    }
  done
fi

# Names the built grant and helper images in `registry`, the host:port the
# cluster's nodes pull the suite's images from, and loads the controller
# image into Docker for the provider to load into the cluster, since the
# controller starts before a provider's registry exists. Published inputs
# need neither.
# shellcheck disable=SC2034 # read by the suites that source this
cluster_input_images() {
  local registry=$1
  [[ -n $cluster_inputs ]] || return 0
  grant_image=$registry/upstream-fiberd@$(jq -er .grantDigest "$cluster_inputs/inputs.json")
  helper_image=$registry/hluk-helper@$(jq -er .helperDigest "$cluster_inputs/inputs.json")
  docker load --quiet --input "$cluster_inputs/images/fiberd-k8s-controller.tar" >/dev/null
  controller_images=$(jq -c '[.controllerImage]' "$cluster_inputs/inputs.json")
}

# Pushes the built grant and helper images to `registry`, as named by
# cluster_input_images, keeping their digests.
push_cluster_inputs() {
  local registry=$1 image digest oras attempt
  [[ -n $cluster_inputs ]] || return 0
  just --justfile "${repo_root:?}/Justfile" providers::install-oras >/dev/null
  oras=$repo_root/build/tools/oras
  # The suite has only just started the registry.
  for attempt in $(seq 30); do
    curl --fail --silent --output /dev/null "http://$registry/v2/" && break
    ((attempt < 30)) || {
      printf 'registry %s does not answer\n' "$registry" >&2
      exit 1
    }
    sleep 1
  done
  for image in upstream-fiberd hluk-helper; do
    digest=$(jq -er '.manifests | select(length == 1) | .[0].digest' \
      "$cluster_inputs/images/$image/index.json")
    "$oras" cp --from-oci-layout --to-plain-http \
      "$cluster_inputs/images/$image@$digest" "$registry/$image:ci" >/dev/null
    [[ $("$oras" resolve --plain-http "$registry/$image:ci") == "$digest" ]] || {
      printf '%s/%s:ci does not resolve to %s\n' "$registry" "$image" "$digest" >&2
      exit 1
    }
  done
}

# ly build records the helper's facts only when build/hluk-helper exists,
# and ly deploy needs them, zygotectl and grant-issuer.
prepare_fiberd_template_tools() {
  local root=$1 tool
  if [[ -n $cluster_inputs ]]; then
    mkdir -p "$root/build"
    for tool in hluk-helper zygotectl grant-issuer; do
      install -m 0755 "$cluster_inputs/bin/$tool" "$root/build/$tool"
    done
  elif [[ -n ${LAMPYR_FIBERD_SRC:-} ]]; then
    [[ -d $LAMPYR_FIBERD_SRC ]] || {
      printf 'LAMPYR_FIBERD_SRC is not a directory: %s\n' "$LAMPYR_FIBERD_SRC" >&2
      exit 1
    }
    just --justfile "$root/Justfile" fiberd-template-tools \
      "$(realpath -- "$LAMPYR_FIBERD_SRC")"
  fi
  for tool in hluk-helper zygotectl grant-issuer; do
    [[ -x "$root/build/$tool" ]] || {
      printf 'build/%s is missing; set LAMPYR_FIBERD_SRC to a fiberd checkout, %s\n' \
        "$tool" "and just fiberd-template-tools builds it" >&2
      exit 1
    }
  done
}
