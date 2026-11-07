---
title: Contributing
description: Build, test, document, and report issues for Lampyr.
---

# Contributing

Lampyr is an open-source platform. Start by opening an issue before a
large behavioral or architectural change so the intended scope can be agreed
before implementation.

## Prerequisites

- Docker, Rust, Go, `just`, `cpio`, and `jq` for checkout builds
- Host Kind and kubectl only for the integration test harness; ordinary installation packages Kind and manages kubectl
- Python 3 with `venv` for provider unit tests
- A registry that the cluster can pull from for deployment

## Build from source

The release workflow publishes a preview after its validation
gates pass. See [`ly setup`](cli/setup.md) to install it. To change or validate
Lampyr itself, build the CLI from a repository checkout:

```bash
git clone https://github.com/hyperlight-dev/lampyr.git
cd lampyr
just build-ly
build/ly doctor
```

`just setup-hluk` downloads the pinned Hyperlight-Unikraft executable
and matching kernel into `build/`, verifying archive and file SHA-256 digests.
Runtime filesystem images are pulled from upstream GHCR by immutable digest.
The same assets are used locally and in CI; no Cargo or Unikraft kernel
compilation is needed. Fiberd is a pinned Go module, and fiberd-k8s-controller
an installation input. Knative Serving, Eventing, and Kourier use release
manifests and images.

Snapshot artifacts are plain OCI images packaged with the standard Dockerfile
frontend. Grant Pods run stock fiberd, which fiberd-k8s-controller deploys
and issues grants to, with Lampyr's helper (`build/hluk-helper`) restoring
the snapshots; nodes need KVM and the device plugin, not a separate container
runtime shim.

`build/serving-controller` reconciles Knative Revisions into CapacityGrants and
ships in the `fiberd` image. `build/fiber-router` routes application requests
and manages fiber acquisition and release through each grant's own Pod.

The GNU Linux `hluk` release requires glibc 2.39 or newer on the host. The
default Kind configuration uses the pinned Debian 13-based Kubernetes v1.35.8
image from Kind v0.33.0. The k3d provider uses K3s nodes; grant Pods supply
their own compatible GNU userspace.

Repository checkouts are discovered automatically for local builds. See
[`ly setup`](cli/setup.md) for the release-archive mechanism.

Infrastructure providers are independent guest projects under `providers/`.
`ly` accepts their opaque configuration files and invokes their declared
lifecycle operations; it contains no Kind/k3d/Azure configuration parser or
provider-specific lifecycle implementation. See [provider
authoring](provider-authoring.md) to create and package one, and [`ly
cluster`](cli/cluster.md) for the user-facing commands.

Omit `--provider` to manage an existing Kubernetes context directly. This path
does not open provider state or artifacts and needs no local hypervisor.
Kind, k3d, and Azure implementations are never linked into `ly`.

Shared platform lifecycle runs natively in `ly`. The Azure guest owns Event
Grid source configuration and emits bounded manifest/status/delete plans with
validated infrastructure bindings. `ly` applies them; guests have no Kubernetes,
host-command, platform-bootstrap, or image-build callbacks. The Kind guest
includes its own PIE Kind and Docker clients and receives only scoped Docker
daemon access through an authenticated bridge.

Build `just kind::build cli::runtime-images` before a checkout's first Kind
installation. Runtime images are built out of band, never by installation.
Azure Event Grid requires prepublished controller and adapter digests in
`eventGridControllerImage` and `eventGridAdapterImage`. The maintained provider
kernel is built separately from the ordinary downloaded HLUK assets.

Follow the [Kind cluster guide](cluster-kind.md) or
[k3d cluster guide](cluster-k3d.md), or the [AKS cluster guide](cluster-aks.md),
to install the platform, then use the
[walkthrough](walkthrough.md) to run your first service.

For k3d, build `just k3d::build cli::runtime-images`. Run
`just k3d::test` for the isolated provider lifecycle and
`just test-k3d-integration` for all six runtime examples on a disposable cluster.
The k3d harness uses Docker and kubectl for assertions, but does not need host k3d.

## Test suites

Run `just check-no-skipped-tests` to reject disabled tests. CI runs this check on
every pull request. Do not add skip, ignore, or expected-failure markers. If a
test needs a different environment, give it an explicit suite and run that
suite in CI. Semgrep checks Rust, Python, Go, JavaScript, and TypeScript.
It does not scan ignored build output. The check runs separately in CI, not as
part of normal local tests. To run it locally, use `just check-no-skipped-tests`;
this optional command needs Python 3 and installs pinned Semgrep in `build/lint-venv`.

`just cli::test` runs tests that need no hypervisor. KVM tests run in the
dedicated recipes below with the required provider artifacts and registry.
They fail if required inputs are missing; they do not report a skip.

| Recipe | Coverage |
|---|---|
| `just cli::test-runtime` | Guest deadlines, capabilities, and native snapshot loading; set `LAMPYR_TEST_ROOTFS` and optionally `LAMPYR_TEST_KERNEL` to override the embedded native kernel |
| `just kind::test` | Kind provider lifecycle, guest binaries, and Docker bridge |
| `just k3d::test` | k3d provider lifecycle |
| `just azure::test` | Offline Azure guest authentication, Event Grid plans, and Bicep compilation; no cloud credentials |
| `just test-kind-integration` | Full platform lifecycle, including Rust registry publish and cache reuse |

`just providers::test-snapshots` installs checksum-pinned ORAS under
`build/tools/` and always runs the offline OCI graph copy test.

Handler contracts select tests by `--runtime` and transport (`--vm` or native).
Native process shutdown and host-filesystem assertions run in the native suite,
not the VM suite. The default native suite runs every contract.
`just examples-check` runs the native contracts; `just test-examples-local` runs
the VM contracts. Both run in CI and through `just test`.

### Cluster suite inputs

The cluster suites (`just test-kind-integration` and its prebuilt variants,
`just test-k3d-integration`, `just test-existing-kind-integration`,
`just test-knative-portability` and `just test-azure-eventgrid-integration`)
install fiberd-k8s-controller, which has no
release yet. They read its installation inputs from the environment and stop
before creating anything when one is missing:

| Variable | Value |
|---|---|
| `LAMPYR_CONTROLLER_MANIFEST` | fiberd-k8s-controller's install manifest, a path or an `https://` URL |
| `LAMPYR_GRANT_IMAGE` | The fiberd image grant Pods run, pinned by `@sha256:` digest |
| `LAMPYR_HELPER_IMAGE` | The `lampyr-hluk-helper` image, pinned by `@sha256:` digest |
| `LAMPYR_FIBERD_SRC` | A fiberd checkout, from which `just fiberd-template-tools` builds `build/zygotectl` and `build/grant-issuer` for `ly build` and `ly deploy`; the recipe also builds the helper, from Lampyr's `helper/`. Optional when they are already in `build/` |

The cluster's nodes must be able to pull both images. `ly deploy` pushes
fiberd templates to the installation's local registry, which
`zygoteRegistries` defaults to, so the Kind and k3d suites list none; the
existing-cluster suite names its registry as a `registryMirrors` input
instead, and the Azure suite gives `zygoteRegistries`.

In place of all four, and never alongside them, `LAMPYR_CLUSTER_INPUTS` may
name a directory of inputs built rather than published, which every suite but
the Azure one accepts:

```bash
just upstream::cluster-inputs build/cluster-inputs
LAMPYR_CLUSTER_INPUTS="$PWD/build/cluster-inputs" just test-existing-kind-integration
```

The recipe builds the stock fiberd image and fiberd-k8s-controller from the
commits `upstream/pins.env` pins, and the helper, its image, `zygotectl` and
`grant-issuer` from this checkout and that fiberd. Its buildx builder must
export OCI layouts, as a `docker-container` one does. A suite pushes the
fiberd and helper images to its own registry, keeping their digests, and has
its provider load the controller image, which the manifest names by a
`lampyr.local/` tag. CI builds the directory once, in the `cluster-inputs`
job, and every cluster job installs it. The walkthrough and new-developer
jobs, which follow the docs rather than a suite, make it the checkout's
default controller inputs instead, as a release's platform package carries
them: `just use-cluster-inputs PORT` serves its images from a registry on
`127.0.0.1:PORT`, which the Kind provider shares when installed with that
`registryPort`, and writes `build/controller-inputs.json` naming them.
No other machine can pull those images, so `just cli::package` refuses a
`build/controller-inputs.json` that names an image on a local registry.

## Service handler and runtime adapter changes

Generated Handler Applications keep the handler, manifest, and final image
`Dockerfile` user-owned. Their tracked `.lampyr-adapter/` contains editable
protocol and rootfs build support plus a pristine, version-controlled baseline
for explicit upgrades; the baseline is merge ancestry, not a signature, so
restore trusted metadata and baseline files from version control rather than
recomputing hashes. Do not have builds or deploys rewrite adapter source.
Standalone Applications have no adapter and are entirely customer-owned.

Hook names and native types for each runtime are listed in the
[handler contract](designs/handler-contract.md). Use
`ly scaffold service NAME --upgrade` for an explicit three-way adapter upgrade;
it refuses Standalone Applications. See
[`ly scaffold`](cli/scaffold.md) for ownership, limits, and recovery details.

## Preview artifacts

Every successful `main` release run replaces the GitHub prerelease and
tag named `preview-release`. The previous tag, release, and assets are deleted;
the rolling channel does not retain a rollback release.

## Versioned releases

Pushing a `vX.Y.Z` tag runs the same release workflow, packaging, gates, and
attestations as the preview. Before anything builds, the workflow requires the
tagged commit to be on `main`, no existing `vX.Y.Z` release, and every
committed version reference (`VERSION`, `cli/Cargo.toml`, `cli/Cargo.lock`,
the walkthrough doctor output, and a `CHANGELOG.md` section) to equal `X.Y.Z`.
Prepare that commit with the `release-version` skill or directly:

```bash
just release-version-set X.Y.Z
just release-version-check tag=vX.Y.Z
```

The release publishes `ghcr.io/hyperlight-dev/lampyr/<image>:X.Y.Z` images
and an immutable `vX.Y.Z` GitHub release with `lampyr-*-X.Y.Z.tar.gz` assets,
all attested against `refs/tags/vX.Y.Z`. Source keeps `:preview` image tags;
the release build stamps `X.Y.Z` into the CLI's default images and the packaged
platform manifests. Install it with `scripts/install-ly.sh vX.Y.Z`.

fiberd and fiberd-k8s-controller publish no images or install manifest yet,
so every preview and versioned release builds them from the commits
`upstream/pins.env` pins, as `upstream-fiberd` and `fiberd-k8s-controller`.
Packaging then writes `build/fiberd-k8s-controller.yaml`, the controller's
`config/default` running its published image by digest, and
`build/controller-inputs.json`, which names that manifest and the published
`upstream-fiberd` and `hluk-helper` digests, into both platform archives
(`just upstream::controller-inputs`). `ly cluster install` takes
`controllerManifest`, `grantImage` and `helperImage` from them when an install
is not given them, and `zygoteRegistries` defaults to the Kind or k3d
installation's own template repository. Both platform archives also carry
the fiberd template tools that `ly build` and `ly deploy`
run (`just platform-template-tools`): `build/hluk-helper`, built from this
checkout for Linux/amd64 like `build/hluk`, since it runs where snapshots
are made, and
`build/zygotectl` and `build/grant-issuer`, which run on the user's machine,
built from the pinned fiberd for each archive's own target into
`build/template-tools/TARGET` (`just upstream::template-tools`). To move
to new upstream commits, change `upstream/pins.env`, and `go.mod`'s
`github.com/Azure/fiberd` with `FIBERD_COMMIT`.

## Preview images

The workflow publishes these canonical GHCR image tags, where
`<full-release-commit>` is the complete commit backing `preview-release`:

- `ghcr.io/hyperlight-dev/lampyr/fiberd:preview-<full-release-commit>`
- `ghcr.io/hyperlight-dev/lampyr/fiber-router:preview-<full-release-commit>`
- `ghcr.io/hyperlight-dev/lampyr/hluk-helper:preview-<full-release-commit>`
- `ghcr.io/hyperlight-dev/lampyr/azure-node-setup:preview-<full-release-commit>`
- `ghcr.io/hyperlight-dev/lampyr/eventgrid-controller:preview-<full-release-commit>`
- `ghcr.io/hyperlight-dev/lampyr/eventgrid-adapter:preview-<full-release-commit>`
- `ghcr.io/hyperlight-dev/lampyr/upstream-fiberd:preview-<full-release-commit>`
- `ghcr.io/hyperlight-dev/lampyr/fiberd-k8s-controller:preview-<full-release-commit>`

The workflow also updates `:preview` convenience aliases. Those aliases move
and are not the consistency boundary for a release, so do not use them for
consistency-sensitive mirroring.

Each Linux/amd64 image is built by BuildKit with an SBOM and maximum-mode
provenance. The workflow adds GitHub build-provenance attestations and
immediately verifies each digest against the Lampyr release workflow, the
`main` source ref, and the exact publishing commit. The installer is a
checksummed and attested release asset; it similarly checks strict SHA-256 files
and exact-commit GitHub attestations for the CLI and platform archives before
installation.

To consume the images from another registry, authenticate `gh` for
attestations, authenticate Docker to the destination registry, and use Docker
Buildx:

```bash
gh auth status --hostname github.com
gh auth token | docker login ghcr.io \
  --username "$(gh api user --jq .login)" \
  --password-stdin
docker login REGISTRY

just preview-mirror-images destination=REGISTRY/lampyr
```

The mirror resolves the commit behind `preview-release`, selects all eight
canonical `preview-<full-release-commit>` tags automatically, and verifies
every source digest's GitHub attestation before copying it. Those include the
platform's default controller inputs, `upstream-fiberd`, `fiberd-k8s-controller`
and `hluk-helper`. It also downloads the preview's Linux platform archive,
verifies its attestation, and checks that every image its fiberd-k8s-controller
manifest pulls is one of the verified images. It then writes destination digest
references to the Kind provider config at `build/preview-kind.yaml` and the
Azure image-settings fragment at `build/preview-azure.yaml`, and the manifest,
pulling the mirrored controller image, to
`build/preview-fiberd-k8s-controller.yaml`, which both name as
`controllerManifest` beside `grantImage` and `helperImage`. The Azure fragment
includes `nodeSetupImage` and named Event Grid controller and adapter image
settings, and must be merged into a customer's complete Azure provider config
before installation.

Docker credentials are used only by the mirror operation. Lampyr never creates
registry credentials or Kubernetes image pull secrets: pre-pull private images
for Kind, or separately grant AKS/custom-registry pull access as described in
the cluster guides.

## Build and test

Build the platform and run the repository validation:

```bash
just build
just test
```

For a smaller CLI loop:

```bash
just build-ly
just test-unit
just kind::test-unit azure::test-unit
```

Provider unit recipes use isolated virtual environments with hash-locked test
dependencies; Docker, KVM, and cloud credentials are not required.

`just test-kind-integration` uses a disposable Kind control plane and two
KVM-capable workers so the normal Serving and Eventing resource requests fit
even when each node reports two CPUs. Its default ingress ports are `18080`
for HTTP and `18443` through `18445` for HTTPS; the test removes its cluster
and registry afterward.
Lifecycle assertions use `ly audit` for every runtime; application-handler
logs are checked separately when proving event delivery.
The integration also verifies reused application state and idle release.
Its release smoke gates sample 100 sequential requests for latency and 300
requests at concurrency 4 for reliability, requiring no request failures and
no Fiber still busy afterwards. Disruption checks cover agent restart,
placement failure and recovery, the cluster refusing a CapacityGrant that
breaks GrantPolicy, and serving during a Knative control-plane outage. CI
requires placement to return HTTP 503 within 15 seconds. Busy Fibers come from
fiber-router's `lampyr_router_fibers_busy`, and Fiber starts and fiberd's epoch
from fiberd's audit spool, since a grant Pod serves its own status only to
fiber-router.

For dedicated stress runs against an installed service, use
`just release-reliability-gate SERVICE URL` (10,000 requests at concurrency 16)
and `just release-disruption-gate SERVICE URL` (the same disruption scenarios
with a stricter 5-second placement deadline). The CI smoke gates do not replace
those stress checks.
The disruption gate requires a local Docker-backed Kind cluster: it pauses the
fiberd container through the node's containerd runtime and resumes it even if
the placement probe fails. Sending SIGSTOP to PID 1 from inside the container
does not stop fiberd.

Set `LAMPYR_CI_RECLAIM_STORAGE=0` when testing against a shared Docker daemon to
leave unrelated build caches and rootfs image aliases untouched.
For failure investigation, `LAMPYR_CI_KEEP_FAILED_RESOURCES=1` keeps the disposable
cluster and registry instead of deleting them; remove those named resources
when finished. Normal and CI runs still clean up automatically.

Website-only changes can use:

```bash
just site-check
```

Preview publication and installer checks can use:

```bash
just test-preview-release
```

For the paid Azure Event Grid end-to-end test:

```bash
AZURE_SUBSCRIPTION_ID=<subscription-id> \
  LAMPYR_E2E_FIBERD_IMAGE=ghcr.io/hyperlight-dev/lampyr/fiberd:preview@sha256:DIGEST \
  LAMPYR_E2E_ROUTER_IMAGE=ghcr.io/hyperlight-dev/lampyr/fiber-router:preview@sha256:DIGEST \
  LAMPYR_E2E_NODE_SETUP_IMAGE=ghcr.io/hyperlight-dev/lampyr/azure-node-setup:preview@sha256:DIGEST \
  LAMPYR_E2E_EVENTGRID_CONTROLLER_IMAGE=ghcr.io/hyperlight-dev/lampyr/eventgrid-controller:preview@sha256:DIGEST \
  LAMPYR_E2E_EVENTGRID_ADAPTER_IMAGE=ghcr.io/hyperlight-dev/lampyr/eventgrid-adapter:preview@sha256:DIGEST \
  just test-azure-eventgrid-integration
```

It also takes the [cluster suite inputs](#cluster-suite-inputs). Grant Pods
pull fiberd templates without credentials, which the Eventing ACR does not
allow, so the suite creates a Standard ACR with anonymous pull enabled and
gives its repository as `zygoteRegistries`. Set
`LAMPYR_E2E_TEMPLATE_REPOSITORY` to an anonymously pullable repository the
host can push to, to use that instead.

This creates AKS and supporting Azure resources, verifies Blob event delivery,
then requests teardown. Its default AKS compatibility class is Intel; use a
matching builder or supply `LAMPYR_E2E_ARTIFACT`, built with the
helper, and `LAMPYR_E2E_SOURCE_IMAGE`. Verify cleanup to avoid
ongoing Azure charges.

Report bugs and propose changes through [GitHub issues](https://github.com/hyperlight-dev/lampyr/issues).
