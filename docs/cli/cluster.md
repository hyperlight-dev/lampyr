---
title: ly cluster
---

# `ly cluster`

Install, check, or remove Lampyr on any existing Kubernetes context. A provider
is optional: its guest only provisions infrastructure and returns a standard kubeconfig.
`ly` owns Kubernetes, platform installation, Eventing, source lifecycle, and
health; it contains no Kind, k3d, or Azure provisioning implementation.

From a checkout, create a customer Workspace before installing. The examples
below use `build/workspace`; use an existing Workspace instead if you have one:

```bash
build/ly scaffold workspace build/workspace
```

```bash
build/ly cluster install --provider kind --name lampyr \
  --allow-docker --workspace build/workspace
build/ly cluster health --workspace build/workspace
build/ly cluster uninstall --delete-cluster --workspace build/workspace
```

Without `--provider`, commands use the existing kubeconfig/context directly:
no provider artifact, private provider state, Docker, or local hypervisor is
needed. A fresh installation defaults to the standard preloaded platform image
tags:

```yaml
# platform.yaml
fiberdImage: ghcr.io/hyperlight-dev/lampyr/fiberd:preview
routerImage: ghcr.io/hyperlight-dev/lampyr/fiber-router:preview
domain: services.example.com
```

```bash
build/ly cluster install --context customer --workspace build/workspace
build/ly cluster health --context customer --workspace build/workspace
build/ly cluster uninstall --context customer --workspace build/workspace
```

The defaults are `ghcr.io/hyperlight-dev/lampyr/fiberd:preview` and
`ghcr.io/hyperlight-dev/lampyr/fiber-router:preview`. The images are public on
GHCR; an offline cluster can preload them before installation.
The local ingress defaults use NodePorts `30080` and `30443`; use
`--provider-config platform.yaml` to override the images, ingress, domain, or
HTTPS port for a managed cluster.

Omit `--context` to use the current kubeconfig context. Existing Kubernetes
authentication is used unchanged; `ly` does not replace its credentials.
Provider-created contexts also contain validated embedded credentials and need
no Lampyr credential plugin after installation.
Uninstall removes Lampyr, not the existing cluster. Context names are opaque:
`ly` never infers a provider from a `kind-`, `k3d-`, or `aks-` prefix.

Only infrastructure provisioning/deletion uses a provider. With
`--delete-cluster`, an omitted provider is looked up from the installation's
ConfigMap; clusters without a recorded provider cannot be deleted this way.

Run cluster commands from inside a Workspace, or use `--workspace` to point to
a different Workspace root. With a provider, Lampyr infers the infrastructure
source as `infra/PROVIDER/NAME`; `--workspace` never points to that nested
source directory.
All cluster lifecycle commands read bootstrap manifests from the Workspace's
`k8s/` directory. These files are copied during `ly scaffold workspace` and are
customer-editable; Lampyr Home retains the immutable runtime and provider
artifacts.

The checkout's `just cluster-install` recipes create `build/workspace` if it
does not exist. `just cluster-health` and `just cluster-uninstall` select the
same Workspace. Set `LAMPYR_WORKSPACE` to use another Workspace for these
recipes. The checkout root does not need a `lampyr.workspace.yaml` file.

Existing installations remain bound to their original Workspace and are not
moved automatically. If an installation used the checkout root, manage it
from the previous checkout revision that still has its manifest. Use a new
installation name for a separate Workspace; `--workspace` does not reassign
an existing installation. Keep the old installation state until its resources
have been accounted for and cleaned up.

| Option | Purpose |
|---|---|
| `--provider ID` | External provider identity, not a built-in enumeration. |
| `--name NAME` | Installation name; defaults to `lampyr` with an explicit provider. |
| `--provider-config FILE` | JSON/YAML platform settings, plus opaque infrastructure settings when using a provider. |
| `--provider-artifact DIR` | Verified provider artifact override, saved for subsequent operations. |
| `--workspace DIR` | Workspace root; infrastructure is inferred as `infra/PROVIDER/NAME`. |
| `--context NAME` | Exact Kubernetes context. |
| `--allow-docker` | Grant the provider access to the Docker daemon. |
| `--approve-provider-manifests` | Apply provider-supplied Kubernetes manifests without prompting. |
| `--eventing` | Install the shared Knative Eventing substrate. |
| `--delete-cluster` | Ask the provider to delete infrastructure during uninstall. |
| `--non-interactive` | Do not permit interactive authentication. |
| `--verbose` | Include captured provider diagnostics. |

## Provider-owned configuration

See [provider authoring](../provider-authoring.md) for the artifact, protocol,
capability, packaging, and installation contract.

Kind owns ingress ports, optional initial YAML `source`, prebuilt image loading,
and the local registry. For example:

```yaml
# kind.yaml
httpPort: 18080
httpsPort: 18443
registryPort: 5002
```

```bash
build/ly cluster install --provider kind --provider-config kind.yaml \
  --allow-docker --workspace build/workspace
```

The provider scaffolds complete infrastructure into `infra/<provider>/<name>`
by default. After initialization, edit that customer-owned source directly;
the provider will not silently rewrite it from new defaults.

k3d accepts the same port and prebuilt-image settings, with a k3d v1alpha5
`source` rather than Kind YAML:

```bash
build/ly cluster install --provider k3d --allow-docker --workspace build/workspace
```

It owns a dedicated registry rather than reusing a shared one. Select an
unused `registryPort` when running multiple local installations. See the
[k3d guide](../cluster-k3d.md) for KVM, node configuration, ownership, and examples.

Azure owns subscriptions, authentication, Bicep, AKS, workload identities, and
Event Grid infrastructure. It emits node preparation and integration manifests
for `ly` to apply:

```yaml
# azure.yaml
subscriptionId: 11111111-1111-1111-1111-111111111111
resourceGroupName: lampyr
location: westus3
nodeVmSize: Standard_D4s_v5
nodeCount: 1
authenticationMode: browser
fiberdImage: ghcr.io/hyperlight-dev/lampyr/fiberd:preview@sha256:FIBERD_DIGEST
routerImage: ghcr.io/hyperlight-dev/lampyr/fiber-router:preview@sha256:ROUTER_DIGEST
nodeSetupImage: ghcr.io/hyperlight-dev/lampyr/azure-node-setup:preview@sha256:NODE_SETUP_DIGEST
domain: services.example.com
eventGridControllerImage: ghcr.io/hyperlight-dev/lampyr/eventgrid-controller:preview@sha256:CONTROLLER_DIGEST
eventGridAdapterImage: ghcr.io/hyperlight-dev/lampyr/eventgrid-adapter:preview@sha256:ADAPTER_DIGEST
eventSources:
  blob-created:
    storageAccount: images
    container: uploads
# Anonymously pullable; see Fiberd templates below.
zygoteRegistries:
  - lampyrtemplates.azurecr.io/lampyr/fiberd-templates
```

```bash
build/ly cluster install --provider azure --eventing \
  --provider-config azure.yaml \
  --approve-provider-manifests --workspace build/workspace
```

The Azure guest signs in using its dedicated per-installation cache. Host
`az login` and host Azure environment credentials are not reused. The current
`authenticationMode` values are `browser` (the default) and `deviceCode` for
remote or headless environments where the browser cannot reach the callback.
The current
non-AAD AKS configuration returns embedded client-certificate credentials,
which `ly` validates and merges directly into the standard kubeconfig. On
Unix hosts, Lampyr protects both the installation directory and its Azure CLI
cache with owner-only (`0700`) permissions before every use.

## fiberd-k8s-controller

`ly cluster install` installs cert-manager and its csi-driver unless the
cluster has them, then fiberd-k8s-controller from `controllerManifest`. That
is a local file, resolved against the `--provider-config` file that names it,
or an `https://` URL, which the install downloads and applies as the URL serves
it then: no digest pins it, so name a file to install a fixed manifest.
`grantImage` and `helperImage` must be pinned by `sha256` digest in a fully
qualified repository, such as `ghcr.io/team/fiberd@sha256:...`, since the
GrantPolicy lists their repositories. `ly cluster install` checks all three
before the provider creates anything. A reinstall applies cert-manager only to
a cluster without it, so Lampyr never upgrades a cert-manager it installed.

## Fiberd templates

`ly deploy` pushes every Revision's fiberd template to one repository per
installation. Grant Pods pull templates only from the repositories the
installation lists as `zygoteRegistries`, and `ly cluster install` picks the
repository this way:

- If `zygoteRegistries` is given and admits a local registry mirror's
  in-cluster target, the repository is `lampyr/fiberd-templates` in that
  mirror: the host pushes through the mirror's key, and grant Pods pull from
  its target.
  This is the normal case on Kind and k3d, whose local registry the provider
  names as a mirror.
- Otherwise, if `zygoteRegistries` is given, the repository is its first
  entry. If that entry's registry is a local mirror's in-cluster target, at a
  path the first rule didn't pick, the host pushes through that mirror's key
  at the given path, so it can reach the registry. Otherwise it pushes to the
  entry directly.
- If `zygoteRegistries` is not given, a registry mirror can be the template
  registry only if it is a local registry: its key is `localhost`,
  `127.0.0.1` or `::1`, or it is reached over plain HTTP and its key is no
  public registry such as `docker.io`. The repository is then
  `lampyr/fiberd-templates` in it, and `zygoteRegistries` defaults to that
  repository, spelled as the cluster reaches it. Without exactly one
  such mirror, as on AKS or an existing cluster without one, the install is
  refused until it gives `zygoteRegistries`.

Each `zygoteRegistries` entry is a repository, a registry and a path such as
`myacr.azurecr.io/templates`, as the GrantPolicy takes it: the registry is
`localhost` or has a dot or a port, the path is lowercase, and there is no
tag or digest. `ly cluster install` refuses an entry that is not before the
provider creates anything. A given list must admit the
repository spelled as grant Pods reach it, and `ly cluster install` refuses
several admitted mirrors, since it cannot pick one. Grant Pods pull templates
without credentials, so the repository, including one pushed to directly,
must allow anonymous pulls. A template is sealed to the cluster's
fleet key, whose private half is the `default/lampyr-fiberd-seal-key`
Secret, so anyone who can get Secrets in `default` can open it, and anyone
who can reach the repository can list and download it. `ly cluster
uninstall` deletes the fleet key with the services, so no template pushed
before it opens again, and the first deploy after the next install makes a
new key. The Azure provider's Eventing ACR does not allow anonymous pulls,
so AKS needs another repository, such as a Standard ACR with anonymous pull
enabled; `ly cluster install` refuses an AKS install without
`zygoteRegistries` before it creates anything. Spell a plain HTTP repository
with `http://`. An entry may also spell `https://`, the default, which `ly
cluster install` strips from every entry, since the GrantPolicy spells TLS
registries without a scheme. An install on an existing cluster may give
its local registry, which the host and the cluster reach by different names,
as its one local registry mirror instead:

```yaml
# platform.yaml
registryMirrors:
  localhost:5002:
    registry: kind-registry:5000
    plainHttp: true
```

## Host capabilities

Artifacts declare only scoped files/state, cloud networking, authentication
cache, and Docker capabilities. There is no host command or platform callback.
Kind/k3d and their Docker clients are packaged as PIE executables in custom guest
images; no host Kind, k3d, or Go is required. On Linux/amd64, `ly` downloads and verifies
its pinned kubectl if it is absent from PATH.

A provider operation must declare the `docker` capability, and the user must
grant the installation access with `--allow-docker`. Neither is sufficient by
itself.

Providers that declare `provider-manifests` are trusted like Kubernetes cluster
operators: `ly` applies their manifests using the selected context's
credentials. Lampyr writes `before.yaml` and `after.yaml` locally, prints a
colored resource summary, and asks before applying them. During retained-cluster
uninstall, it writes and prompts separately for `cleanup.yaml`. Use
`--approve-provider-manifests` to bypass either prompt in automation. The files
are standard Kubernetes YAML and may be inspected or applied manually while
preserving their documented lifecycle phase.

Running without `--provider` does not implicitly trust a plan stored in the
cluster. Reinstall and retained-cluster uninstall show the stored resources and
require the same prompt or `--approve-provider-manifests` flag before applying
or deleting provider-managed objects.

The runtime cannot mount a host Unix socket as a transport. Docker access uses
a short-lived mutually authenticated TLS bridge to the selected Unix socket,
with an exact TCP destination policy and per-operation credentials. Set
`DOCKER_HOST=unix:///path/to/docker.sock` for a non-default socket. Docker access
is effectively host-root access, not a restricted Docker API sandbox; trust
providers before granting it. Azure requests no Docker access.

All runtime images must be built and published out of band. From a checkout,
`just cli::runtime-images` prepares the default local images before Kind or k3d
installation. Azure requires a published node preparation image digest in
`nodeSetupImage`; Event Grid additionally requires published image digests in
`eventGridControllerImage` and `eventGridAdapterImage`. Installation never
builds or pushes images.

Providers may emit `before`, `after`, and retained-uninstall `cleanup` manifests
in their private runtime plan. `ly` applies them around shared platform
bootstrap. The Azure guest owns Event Grid manifests and source create/status/delete
semantics; `ly` validates and executes its generic Kubernetes plans. Kubeconfig
validation, ownership-safe profile merging, cluster UID checks, cancellation, and
deadlines remain host-enforced.

The applied runtime plan and platform settings are retained in an
installation-owned Kubernetes Secret. Native health, reinstall, and retained
uninstall remain retryable; Event Grid source operations reopen the owning
Azure provider installation and its customer workspace.

Uninstall retains infrastructure unless `--delete-cluster` is supplied.
Providers verify ownership before destructive operations. Azure retains its
customer resource group even when deleting AKS; check for remaining billable
resources afterward.

Explicit full deletion still removes owned infrastructure if Kubernetes
cleanup fails. That cleanup failure is reported as an error, including the
possibility of external source resources needing manual cleanup.
