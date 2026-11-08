---
title: Kind cluster guide
description: Install, verify, and remove Lampyr on Kind.
---

# Kind cluster guide

Use Kind for the first Lampyr installation. It provides a local Kubernetes
environment for developing and testing Lampyr Services.

The provider includes pinned Kind **v0.33.0** and its Docker client; neither
host Kind nor Go is needed for installation. The default configuration pins the
Kubernetes **v1.35.8** node image built for that release, based on Debian 13
with glibc 2.41. Custom node images must provide glibc 2.39 or newer to run
the `hluk` executable.

Updating the provider does not upgrade existing clusters. Recreate an older
cluster when you are ready to replace its node image; installation does not
silently delete or replace it.

## Install

With `ly` installed from a release (see the
[README](https://github.com/hyperlight-dev/lampyr#install-the-cli)), the platform already carries the
provider, runtime images and fiberd-k8s-controller inputs:

From a repository checkout, prepare the CLI and runtime inputs instead:

```bash
repo_root="$PWD"
just build-ly
just kind::build
just cli::runtime-images
just upstream::cluster-inputs build/cluster-inputs
LAMPYR_CLUSTER_INPUTS=$PWD/build/cluster-inputs just use-cluster-inputs 5002
export PATH="$repo_root/build:$PATH"
```

With either installation, create a Workspace and run the remaining commands
from inside it:

```bash
ly scaffold workspace kind-demo
cd kind-demo
ly cluster install --provider kind --allow-docker
```

A release's platform installs fiberd-k8s-controller and the images its grant
Pods run by default, but a checkout has none to install.
`just upstream::cluster-inputs` builds them from the commits
`upstream/pins.env` pins, with a Buildx builder that exports OCI layouts, such
as a `docker-container` one. `just use-cluster-inputs 5002` serves the images
from a registry on `127.0.0.1:5002`, the port the provider shares its local
registry on by default, makes them this checkout's defaults, and puts the
fiberd template tools in `build/` for `ly build` and `ly deploy`. Use the
`registryPort` an install gives in place of `5002`. This works for Kind only,
whose provider shares that registry, and the defaults stay: every later
install from this checkout that gives no controller inputs takes them, k3d's
or another port's too. To undo it, delete `build/controller-inputs.json` and
`build/fiberd-k8s-controller.yaml`, and remove the registry with
`docker rm -f lampyr-cluster-inputs-5002`. Alternatively, give the
published `controllerManifest`, `grantImage` and `helperImage` that
`just preview-mirror-images` writes to `build/preview-kind.yaml` in a
`--provider-config` file (see [Preview images](contributing.md#preview-images)),
and build the fiberd template tools with `just template-tools`.

The external Kind provider creates or reconciles `lampyr`. Use `--name` to
choose another installation. `ly` installs shared Knative and Lampyr
infrastructure, waits for KVM capacity, and verifies the platform rollout.

`just cli::runtime-images` builds the serving controller and router images before
installation. The guest loads those prebuilt images into Kind; installation
never builds or publishes runtime images. Supply
`fiberdImage: IMAGE` in a `--provider-config` file to use a prepared image available to
the Kind nodes instead.
Use `routerImage: IMAGE` to supply
that image explicitly.

The provider reuses a running `registry:2` or `registry:3` container bound to
`127.0.0.1:5002` when one exists without another installation's ownership label.
Otherwise it creates an installation-owned registry. The `registryPort`
configuration changes the host port. Full cluster deletion removes owned
registries, but only disconnects shared registries from the installation's network;
their containers and images are preserved.
The installer configures containerd to translate `localhost:5002` to the
registry container when Kind's nodes pull images, and records the registry as
a mirror, through which the serving controller points grant Pods at fiberd
templates pushed to `localhost:5002`.
Deployment creates one grant Pod per CapacityGrant, running stock fiberd with
Lampyr's helper. fiberd-k8s-controller issues each Pod's grant and renews it
automatically.

Build and deploy the default app in the Workspace created above to the local
registry. The scaffold exposes `app` through `ingress.services`:

```bash
ly build services/app
ly deploy --image localhost:5002/app:v1 services/app
```

`ly deploy` tags and pushes the locally built image automatically. Do not pass
the `lampyr.local/...` cache tag printed by `ly build` as the destination.

Kind publishes ingress on dynamically assigned `127.0.0.1` ports by default,
so it does not collide with existing local clusters or web servers. Use the URL
printed by `ly deploy`; it works from this machine and not from the network.
Its certificate is self-signed:

```bash
curl -k https://app.default.localhost:PRINTED_PORT/test
```

To request fixed ports, set `httpPort` and `httpsPort` in provider
configuration; see [`ly cluster`](cli/cluster.md).

To add Knative Eventing and an InMemoryChannel-backed
Broker, install explicitly with:

```bash
ly cluster install --provider kind --eventing --allow-docker
```

This installs the pinned Eventing 1.23.0 manifests. The InMemoryChannel-backed
Broker is non-durable development infrastructure. Azure Event Grid sources are
available only on Azure-provider installations.

## Check health

```bash
ly cluster health
```

## Remove Lampyr

```bash
ly cluster uninstall
```

Add `--delete-cluster` to remove the Kind cluster as well. See
[`ly cluster`](cli/cluster.md) for all options.

## Next step

Follow the [walkthrough](walkthrough.md) to scaffold and deploy your first
Lampyr Service.
