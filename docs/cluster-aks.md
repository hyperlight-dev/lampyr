---
title: AKS walkthrough
description: Create AKS, deploy a first Lampyr Service, observe it, and clean up.
---

# AKS walkthrough

Create an AKS cluster, install Lampyr, and deploy the default Lampyr Service.
Azure resources incur charges while they exist, even when Lampyr Services are
idle.

## Before you start

Run this walkthrough from a Lampyr repository checkout. You need:

- an Azure subscription where you can create resource groups, AKS, and related
  networking resources;
- Docker, `kubectl` on `PATH`, `just`, and the repository build prerequisites;
- local `/dev/kvm` access for the Azure provider and `ly dev`;
- an OCI registry where you can push images and the cluster can pull them; and
- a domain you control if you want normal public DNS instead of the temporary
  `curl --resolve` test below.

Lampyr publishes rolling preview infrastructure images to
`ghcr.io/hyperlight-dev/lampyr`. Resolve each `:preview` tag to its digest
before installation. AKS nodes must have pull access to GHCR, or use equivalent
digest-pinned references from a registry they can pull from.

The basic AKS installation does not create an application registry or configure
private registry credentials. This walkthrough assumes the service repository
permits anonymous pulls. Authenticate locally before pushing, then make the
published repository public.

Grant Pods pull fiberd templates without credentials too, and AKS makes no
registry they could pull them from, so the installation names an anonymously
pullable repository for them as `zygoteRegistries`; `ly deploy` pushes every
template there. A template is sealed to the cluster's fleet key, so only the
cluster can open it, but anyone who can reach the repository can download it.
The ACR that `--eventing` creates does not allow anonymous pulls, so it cannot
be that repository. `ly cluster install` refuses an AKS install without
`zygoteRegistries` before it creates anything. See
[Fiberd templates](cli/cluster.md#fiberd-templates).

Build the CLI and Azure provider, put `ly` on your `PATH`, and check the local
requirements:

```bash
repo_root="$PWD"
just build-ly azure::build
export PATH="$repo_root/build:$PATH"
ly doctor
```

The Azure provider is built separately from `ly`. The `azure::build` recipe
creates its artifact at `providers/azure/build/rootfs` in the checkout.

`ly doctor` checks the local workstation, not Azure. Local KVM is required to
run the Azure provider during installation and provider-backed cleanup, as well
as for `ly dev`. The AKS build later in this walkthrough creates its
hardware-bound snapshot on an AKS node.

Set the values used throughout the walkthrough:

```bash
export SUBSCRIPTION_ID=11111111-1111-1111-1111-111111111111
export REGISTRY=docker.io/OWNER
export DOMAIN=services.example.com
```

Replace the example subscription, registry, and domain. Log in to the registry
with its normal Docker authentication flow before continuing. This example
uses a repository named `lampyr-app` for the service image and one named
`lampyr-templates` for its fiberd templates; create both as public
repositories before pushing, or use equivalent anonymously pullable
repositories elsewhere.

## Create a Workspace

From the checkout root, put the local CLI build first on `PATH` and scaffold a
temporary Workspace. The absolute build path remains valid after changing
directories:

```bash
repo_root="$PWD"
export PATH="$repo_root/build:$PATH"
workspace=$(mktemp -d "$PWD/.lampyr-aks-walkthrough.XXXXXX")
ly scaffold workspace "$workspace"
cd "$workspace"
```

Create the Azure provider configuration:

```bash
cat > azure.yaml <<EOF
subscriptionId: $SUBSCRIPTION_ID
fiberdImage: ghcr.io/hyperlight-dev/lampyr/fiberd:preview@sha256:FIBERD_DIGEST
routerImage: ghcr.io/hyperlight-dev/lampyr/fiber-router:preview@sha256:ROUTER_DIGEST
nodeSetupImage: ghcr.io/hyperlight-dev/lampyr/azure-node-setup:preview@sha256:NODE_SETUP_DIGEST
domain: $DOMAIN
authenticationMode: browser
zygoteRegistries:
  - $REGISTRY/lampyr-templates
EOF
```

A release's platform installs fiberd-k8s-controller and the images its grant
Pods run by default. From a checkout, which has none, add the
`controllerManifest`, `grantImage` and `helperImage` that
`just preview-mirror-images destination=REGISTRY/lampyr` writes to
`build/preview-azure.yaml`; see [Preview images](contributing.md#preview-images).

The default installation uses resource group and cluster name `lampyr`, region
`westus3`, one `Standard_D4s_v5` node, Ubuntu 24.04, and Kubernetes context
`aks-lampyr`. See [`ly cluster`](cli/cluster.md) for resource group, location,
node, Kubernetes version, and domain options.

## Create the AKS cluster

Install Lampyr and wait for AKS, Knative, Kourier, KVM capacity, and the Lampyr
platform to become ready:

```bash
ly cluster install --provider azure \
  --provider-config azure.yaml \
  --approve-provider-manifests
```

If the Azure provider artifact is missing, rebuild it from any directory with
`just --justfile "$repo_root/Justfile" azure::build`, then retry installation.

The Azure provider prints a browser sign-in URL. Open the full URL in a browser
on this workstation and complete sign-in while `ly` remains running. The browser
returns to a temporary localhost callback; when using WSL or a remote shell,
the browser must be able to reach that callback on the machine running `ly`.
The provider keeps its own credential cache and does not reuse the host
`az login` cache or host Azure environment credentials. Your organization's
Conditional Access policies still apply.

Browser authentication is the default. For a remote or headless environment
where the browser cannot reach the localhost callback, set
`authenticationMode: deviceCode` in `azure.yaml` instead.

Installation creates AKS `lampyr`, or reconciles it when it already exists.
Nodes must provide containerd 2 and KVM through CDI. New installations use
Ubuntu 24.04; when reconciling an existing cluster, use Ubuntu 24.04 or Azure
Linux 3 node pools.

AKS installation requires all three image settings and does not create an
extra Azure registry automatically. The node-setup image runs as a privileged
per-node init container with the host root mounted read-only and loads the KVM
modules on a node without `/dev/kvm`, so review and trust it like other
provider manifests. It must use a SHA-256 digest so script
changes produce an explicit DaemonSet rollout. Image references are recorded
in installation metadata for subsequent reconciliation and retained-cluster
cleanup.

Grant Pods on AKS run with fiberd-k8s-controller's security context, as on
every other provider: the hyperlight runtime runs them privileged, so AppArmor
leaves them unconfined. The provider adds only the workload identity label to
them. The controller refuses a grant whose Pod carries an AppArmor
annotation, so confining them again needs fiberd-k8s-controller to admit an
AppArmor profile its GrantPolicy lists.

Check the installed platform:

```bash
ly cluster health --context aks-lampyr
```

## Inspect and run the default app

The Workspace contains a default Lampyr Service at `services/app`. Inspect its
manifest, handler, and Dockerfile:

```bash
ls -1 services/app/Dockerfile services/app/handler.js services/app/lampyr.yaml
```

If the workstation has local KVM, run the app locally:

```bash
ly dev --port 8080 services/app > dev.log 2>&1 &
dev_pid=$!
curl --fail --retry 60 --retry-connrefused --retry-delay 1 \
  -H 'Host: app.default.localhost' \
  http://127.0.0.1:8080/test
kill "$dev_pid"
wait "$dev_pid" || true
```

The response is:

```json
{"message":"hello from Hyperlight edge","path":"/test"}
```

## Build and deploy to AKS

Hyperlight snapshots are CPU-vendor-specific. Create the snapshot on the target
AKS cluster instead of assuming the workstation CPU matches. A template build
needs the helper and fiberd's template tools: a released platform carries them,
and a checkout builds them first with `just template-tools`, from the
fiberd commit `upstream/pins.env` pins:

```bash
just template-tools   # from a checkout only
ly build --context aks-lampyr services/app
```

Root filesystem and OCI image construction remain local. Lampyr copies the
minimum snapshot inputs to a temporary KVM Pod, copies the completed snapshot
back, and removes the Pod.

If the transfer fails with `websocket: close 1006` or `connection reset by peer`,
retry using kubectl's SPDY transport on clusters that support it:

```bash
KUBECTL_REMOTE_COMMAND_WEBSOCKETS=false \
  ly build --context aks-lampyr services/app
```

This changes only the streaming transport used by this command. The message
`tar: Removing leading '/' from member names` is a harmless copy warning.

Deploy the service image through the external registry:

```bash
ly deploy \
  --context aks-lampyr \
  --image "$REGISTRY/lampyr-app:v1" \
  services/app
```

`ly deploy` publishes the local image if needed, resolves it to an immutable
digest, creates the Lampyr Service resources, and prints its HTTPS URL.

## Test it on AKS

Read the public Kourier ingress address:

```bash
INGRESS_IP=$(
  kubectl --context aks-lampyr -n kourier-system get service kourier \
    --output 'jsonpath={.status.loadBalancer.ingress[0].ip}'
)
printf '%s\n' "$INGRESS_IP"
```

For normal public access, create a wildcard DNS record for
`*.default.$DOMAIN` pointing to this address. Until DNS propagates, test the
deployed route directly:

```bash
curl --fail --insecure \
  --resolve "app.default.$DOMAIN:443:$INGRESS_IP" \
  "https://app.default.$DOMAIN/test"
```

The response is:

```json
{"message":"hello from Hyperlight edge","path":"/test"}
```

The generated development certificate is self-signed, so this example uses
`--insecure`. Replace it with production certificate management before serving
real traffic.

## Observe and clean up

Inspect the deployed Service and its recent structured application log:

```bash
ly status --context aks-lampyr services/app
ly logs --context aks-lampyr --tail 10 services/app
```

Delete the Service, uninstall Lampyr, and delete AKS:

```bash
ly delete --context aks-lampyr services/app
ly cluster uninstall \
  --provider azure \
  --name lampyr \
  --context aks-lampyr \
  --approve-provider-manifests \
  --delete-cluster
```

Omit `--delete-cluster` to retain AKS. The Azure resource group and external
registry are retained in either case; inspect them for remaining billable
resources and remove published images when they are no longer needed.

Keep the Workspace after uninstall. The local provider installation records
its generated infrastructure source there and needs it for reconciliation or
later cleanup operations.

## Further reading

- [Walkthrough](walkthrough.md) for the equivalent local Kind flow.
- [Azure Event Grid](azure-event-grid-source.md) for event-driven delivery,
  managed ACR access, and Azure workload identities.
- [`ly build`](cli/build.md) for target-cluster snapshot creation.
- [`ly deploy`](cli/deploy.md) for revisions, traffic, and rollback.
