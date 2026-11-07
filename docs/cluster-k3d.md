---
title: k3d cluster guide
description: Install Lampyr on k3d and deploy every supported runtime example.
---

# k3d cluster guide

The k3d provider creates a local K3s cluster using Docker. Like Kind, it runs
inside a Hyperlight guest with explicit Docker authorization; host k3d and Go
are not needed for installation. The provider packages k3d **v5.9.0** and a
Docker client, and pins the K3s **v1.35.5-k3s1** node image by digest.

## Requirements

Use Linux/amd64 with Docker, writable `/dev/kvm`, and the
[local requirements](requirements.md). The provider mounts KVM into its nodes.
Lampyr's Fiberd image supplies the GNU userspace needed by HLUK; the K3s node
image does not need a separate Hyperlight runtime or glibc installation.

Traefik and ServiceLB are disabled so Lampyr's Kourier ingress owns routing.
The Kubernetes API and HTTP/HTTPS ingress bind to dynamically allocated
loopback ports. The URL printed by `ly deploy` is local to this machine.

## Install from a checkout

A release's platform installs fiberd-k8s-controller and the images its grant
Pods run by default, but a checkout has none to install, so prepare them
first. Mirror the preview's images to a registry the k3d nodes can pull
from (see [Preview images](contributing.md#preview-images)):
`just preview-mirror-images` writes their `controllerManifest`, `grantImage`
and `helperImage` to `build/preview-kind.yaml`, whose keys the k3d provider
takes too. `just template-tools` builds the fiberd template tools that
`ly build` and `ly deploy` run. Then, from the repository Workspace:

```bash
just preview-mirror-images destination=REGISTRY/lampyr
just build-ly k3d::build cli::runtime-images template-tools
build/ly cluster install --provider k3d --name lampyr-k3d \
  --provider-config build/preview-kind.yaml --allow-docker
build/ly cluster health --context k3d-lampyr-k3d
```

`just use-cluster-inputs` does not work for k3d: it serves built images from
a registry that only the Kind provider shares, so k3d nodes cannot pull them.

The provider imports prebuilt runtime images into K3s; installation never
builds or publishes images. `build/preview-kind.yaml` also names the
preview's `fiberdImage` and `routerImage`, which the provider runs in place
of the ones `just cli::runtime-images` built; delete those two lines to run
the built ones, or change them to select other prepared images.

The provider creates an installation-owned `registry:2` container on
`127.0.0.1:5002`, configures K3s/containerd to pull through it, and records
it as the registry mirror through which the serving controller points grant
Pods at fiberd templates. Unlike Kind, this provider does not adopt a shared
registry. If that port is already occupied, choose a free port. An install
takes one `--provider-config` file, so start it from a copy of
`build/preview-kind.yaml` to keep the controller inputs:

```bash
cp build/preview-kind.yaml k3d.yaml
```

```yaml
# k3d.yaml, below the copied lines
registryPort: 5502
httpPort: 18080
httpsPort: 18443
```

```bash
build/ly cluster install --provider k3d --name lampyr-k3d \
  --provider-config k3d.yaml --allow-docker
```

Omit `httpPort` and `httpsPort` (or use zero) for dynamically assigned ports.
Cluster names must be DNS labels of at most 32 characters.

Add `--eventing` to install the shared Knative Eventing substrate. Azure Event
Grid infrastructure still requires the Azure provider.

## Deploy the examples

All six [runtime examples](examples.md) are provider-neutral: Node.js, Python,
Go, Rust, and .NET use the same source and manifests on Kind and k3d.
There are no k3d-specific forks to keep synchronized.

Create a workspace and copy the example into its services directory:

```bash
build/ly scaffold workspace k3d-demo
cp -R docs/examples/hello-nodejs k3d-demo/services/hello-nodejs
```

In `k3d-demo/lampyr.workspace.yaml`, explicitly expose the example:

```yaml
apiVersion: lampyr.dev/v1alpha1
kind: Workspace
ingress:
  services: [hello-nodejs]
```

Build and deploy using the default registry port:

```bash
build/ly build k3d-demo/services/hello-nodejs
build/ly deploy --context k3d-lampyr-k3d \
  --image localhost:5002/hello-nodejs:v1 k3d-demo/services/hello-nodejs
```

Substitute any example directory and image name from the runtime table. If
you changed `registryPort`, use that port in the image reference. `ly deploy`
pushes the built artifact automatically.

Use the printed URL and accept the local self-signed certificate:

```bash
curl -k https://hello-nodejs.default.localhost:PRINTED_PORT/test
```

## Customer-owned infrastructure

The provider scaffolds `infra/k3d/NAME/k3d-config.yaml`, a k3d v1alpha5
configuration. An initial `source` setting can supply custom YAML. After
initialization, edit the scaffolded file directly; reinstalls preserve it.
Keep the installation network, ownership labels, loopback API, the loopback
mapping of port 30443, disabled load balancer, and enabled image volume. k3d
requires its tools container and image volume during creation; the provider
checks these resources as well as the Kubernetes node containers.

Reinstalling reconciles Lampyr but does not silently replace a cluster or
apply topology/image/port changes. Explicitly delete and recreate the
installation to apply changed infrastructure. The exception is a cluster whose
creation failed: the next install replaces it, using the edited source.
Registry configuration is generated separately and never overwrites the
customer source.

## Remove Lampyr

Remove only the platform, retaining K3s and the local registry:

```bash
build/ly cluster uninstall --context k3d-lampyr-k3d
```

Remove the owned cluster, tools container, image volume, registry, and network:

```bash
build/ly cluster uninstall --provider k3d --name lampyr-k3d --delete-cluster
```

The provider refuses to adopt unrelated clusters or delete replaced
containers/volumes. These operations never delete other installations.

## Test k3d support

```bash
just k3d::test-unit
just k3d::test
just test-k3d-integration
```

The guest lifecycle test checks creation, connection, ownership, source
preservation, reinstall, and deletion without invoking host k3d. The
integration suite creates a disposable installation and runs all six committed
examples through build, push, deployment, HTTP activation, idle with no active
fibers, reactivation, and deletion. It also tests streaming, WebSockets, and event metadata where
supported, then verifies retained and full cluster uninstall.

For an independently created k3d cluster, the
[existing-context workflow](cli/cluster.md) also works. You must configure
KVM, registry reachability, and ingress on that cluster yourself.
