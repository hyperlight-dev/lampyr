---
title: Create a provider
description: Build, configure, package, and use an infrastructure provider.
---

# Create a provider

A Cluster Provider creates infrastructure and returns a standard Kubernetes
kubeconfig. The provider runs in an isolated Hyperlight guest.
`ly` controls Kubernetes platform installation, health checks, and service
operations.

Use [`providers/kind/`](https://github.com/hyperlight-dev/lampyr/tree/main/providers/kind)
as a small, maintained example.

## Use registry snapshots

Maintained release provider manifests include their snapshot repository and
digest. `ly` automatically fetches only the compatible snapshot when the local
index is missing.

Release platform and provider archives carry no local snapshot layout, so the
first start of a release provider fetches its snapshot into
`LAMPYR_HOME/cache/provider-snapshots`. If the registry is unreachable or
refuses the pull, the provider cold-boots.

Set `LAMPYR_SNAPSHOT_REGISTRY` only to override the manifest repository, for
example while testing a custom publication:

```bash
export LAMPYR_SNAPSHOT_REGISTRY=ghcr.io/example/provider-snapshots
```

To download snapshots before starting a provider:

```bash
ly provider pull-snapshots \
  --registry ghcr.io/example/provider-snapshots \
  --index sha256:<snapshotIndex.sha256>
```

This command downloads snapshots, not a complete provider package.
The provider manifest pins the index digest. The rootfs and provider program
must still be available locally. See [registry details](#pull-snapshots-from-a-registry)
for authentication, caching, and errors.

## Select a provider

Set `--provider` to the `id` in the artifact manifest.

```bash
ly cluster install \
  --provider example \
  --name development \
  --provider-config example.yaml \
  --provider-artifact ./build/rootfs
```

Use `--provider-artifact` to specify the artifact path.
`ly` saves this path for subsequent operations on the installation.
If you do not specify a path, `ly` searches these directories in the active
Lampyr runtime:

```text
providers/PROVIDER/artifact/
providers/PROVIDER/build/rootfs/
```

In a source checkout, use this command to build the artifact:

```bash
just --justfile providers/PROVIDER/Justfile build
```

`ly` does not build provider artifacts.
Release platform archives contain maintained artifacts at
`LAMPYR_HOME/platforms/VERSION/providers/PROVIDER/artifact`.
Linux releases install Linux/x86_64 cold artifacts. The
`lampyr-platform-darwin-arm64` bundle installed by `ly setup` contains the
macOS/aarch64 artifacts; their Linux ARM64 guest rootfs remains at the same
path. On macOS, for example:

```text
~/.lampyr/platforms/VERSION/providers/PROVIDER/artifact/rootfs.cpio
```

`ly provider pull-snapshots` fetches only snapshots; it does not install providers.

## Configuration and customer source

Use JSON or YAML with `--provider-config`.
Lampyr reads the configuration as an object.
The selected provider defines and checks the configuration fields.

During the first installation, the provider creates the initial customer source
files at this path:

```text
WORKSPACE/infra/PROVIDER/CLUSTER_NAME/
```

`CLUSTER_NAME` is the value of `ly cluster --name`.
Subsequent operations use these source files.
The provider must not replace customer changes without notification.
Lampyr stores private provider state and authentication data under `LAMPYR_HOME`.

## Artifact contents

A typical provider project contains these files and directories:

```text
providers/example/
├── artifact.json
├── provider.py
├── Dockerfile
├── Justfile
├── templates/
└── tests/
```

The build must produce these files:

```text
build/rootfs/
├── manifest.json
├── provider.py
├── rootfs.cpio
├── snapshot/          # optional native OCI layout before assembly
└── kernel             # optional custom kernel
```

A provider can include a Hyperlight OCI snapshot of its root filesystem.
Use `providers/package.py` to create the snapshot and package the artifact.
This script packages the snapshot, program, and optional kernel.
It records their SHA-256 digests in `manifest.json`.
Maintained packages also record `coldCompatibility` (`linux`/`x86_64` or
`macos`/`aarch64`) and omit `kernel`, causing cold boot to use the native
kernel embedded in `ly`.
By default, the script captures a native OCI layout when the host has a
supported hypervisor. Explicit `variant` mode requires capture; `none` produces
a cold-only artifact. The build modes are described below.

The script records the native host architecture, hypervisor, CPU vendor, and
snapshot key.
It also records the kernel and host-contract `SNAPSHOT_KEY`.
If KVM is not accessible, the script creates an artifact without a snapshot.

The CLI restores a supplied single snapshot only if its compatibility metadata
matches the host.
The snapshot key must match the current kernel and host contract.
It checks single snapshots without matching metadata but does not restore them.
If the artifact has no snapshot, the CLI starts the guest from the root
filesystem. This procedure is called a cold boot.

`ly` uses installed cold-boot assets in place; it does not copy the rootfs or a
custom kernel into a temporary directory. The artifact directory and its files
must be owned by the current user, must not be writable by other users, and
must not be symlinks. Group write access is accepted only from the user's own
private group (the default under umask 002 on Ubuntu and Debian), and only on
entries without a POSIX ACL on a local filesystem, and only when nsswitch.conf reads
accounts and groups from local sources (`files`, `systemd`); with LDAP, sssd, NIS or
winbind, group write access is refused. Keep the directory immutable while an artifact, snapshot,
or provider is in use. Provider packaging secures its output and build
directories. Platform setup creates private install directories and normalizes
extracted files. Neither changes unrelated ancestors. Snapshot startup does not
read the rootfs or kernel. Cold startup verifies each cold asset's recorded
SHA-256 digest once immediately before boot, then gives Hyperlight that same
installed path.

### Local OCI snapshot variants

Use Hyperlight's native OCI layout from `snapshot_to()`.
Add its index path and SHA-256 digest to the provider manifest:

```json
{
  "snapshotIndex": {
    "path": "snapshot/index.json",
    "sha256": "<64 lowercase hex characters>"
  },
  "snapshotRepository": "ghcr.io/example/provider-snapshots"
}
```

`snapshotRepository` is used when the digest-pinned index is not included
locally. The path is relative to the provider artifact. Blobs and `oci-layout`
are relative to the directory that contains `index.json`. Use the native config
v5, memory v2 (one or more layers), and transport v1 blobs without a wrapper.

The CLI selects a manifest by these native descriptor annotations:

| Annotation | Required match |
|---|---|
| `dev.hyperlight.snapshot.arch` | Host architecture, such as `x86_64`. |
| `dev.hyperlight.snapshot.hypervisor` | Host backend, such as `kvm` or `hvf`. |
| `dev.hyperlight.snapshot.cpu.vendor` | Host CPU vendor, such as `GenuineIntel` or Apple implementer `0x61`. |
| `org.opencontainers.image.ref.name` | A tag ending in `-SNAPSHOT_KEY` for the current kernel and host contract. |

Host detection supports Linux/KVM on x86_64 and macOS/HVF on Apple Silicon.
For multiple matches, the smallest digest wins; size breaks equal-digest ties.
No match causes a cold boot. `snapshotIndex` takes precedence over `snapshot`.
An invalid index or corrupt selected snapshot causes an error, not a cold boot.

These compatibility labels are the minimum selection keys, not a substitute
for Hyperlight's restore-time checks of snapshot internals and CPU features.
The provider's protocol/runtime version, program, memory, and cold-boot assets
remain governed by the enclosing provider manifest. Variant assembly and
publication are separate build operations; Lampyr never builds or publishes a
snapshot during provider startup.

### Build and publish snapshot indexes

Maintained preview and versioned releases build and publish these indexes
automatically. For a custom provider, first build each platform's cold artifact:

```bash
just kind::build none
just azure::build rootfs none
just kind::build none linux/arm64 macos aarch64
just azure::build rootfs none linux/arm64 macos aarch64
```

Distribute the resulting `providers/PROVIDER/build/rootfs/` directory to the
same path in each native builder's checkout of the same source revision, then
capture without rebuilding the rootfs. Do not independently re-export a rootfs
for each vendor: filesystem/CPIO metadata can differ even at the same revision.

```bash
just kind::snapshot
just azure::snapshot
just k3d::snapshot
```

These are equivalent to passing `--snapshot-mode variant` to
`providers/package.py`. Hyperlight writes its native OCI layout under
`build/rootfs/snapshot/`, including host architecture, hypervisor, CPU vendor
and kernel/host snapshot key annotations. The provider manifest pins
`snapshot/index.json`. There is no user-supplied target platform override.
Copy the complete packaged directory from each clean builder
to a staging area, for example `build/variants/kind-intel/` and
`build/variants/kind-amd/`. Build hosts must be trusted, have KVM access, use the
same pinned runtime, and capture before any customer credentials or state are
supplied. Maintained artifacts omit an external kernel so cold boot uses the
native kernel embedded in `ly`.

For a single native build, `just kind::build variant` or
`just azure::build rootfs variant` combines rootfs building and capture.

To omit native capture entirely, use `just kind::build none` or
`just azure::build rootfs none`. This needs no KVM for packaging and produces
only the existing rootfs, program and optional kernel references.

Assemble the available variants offline:

```bash
just providers::snapshot-assemble \
  providers/kind/build/rootfs build/snapshot-layouts/kind \
  build/variants/kind-intel build/variants/kind-amd
```

The first directory supplies the base provider manifest and cold-boot assets;
remaining directories supply native layouts and their compatibility metadata.
Assembly creates missing output parents privately, rejects unsafe existing
ancestors, and removes group/other write permissions from the emitted artifact.
Logical provider fields must match exactly, including program, memory, identity,
version, runtime and capabilities. `rootfs`, `kernel` and
`coldCompatibility` may differ between declared platform builds. Referenced
cold assets and all native blobs are checksum-verified.
Cold assets must meet the loader's limits: 2 GiB rootfs, 64 MiB kernel and
64 KiB UTF-8 program. Indexes are parsed from the exact checksum-verified bytes.
Provider paths must be relative, at most 1024 UTF-8 bytes, and contain no
empty, dot, traversal or `.git`/`.hg`/`.svn` components. All descriptor
annotation values must be strings. Aggregate variant counts are checked before
copying each input layout's blobs.
Provider file descriptors contain exactly `path` and `sha256`. Shared native blobs are verified once per
unchanged input file and copied once per output digest; every descriptor and
each distinct input file remain validated.
JSON inputs must be UTF-8 with unique object keys, Unicode-scalar strings
(no unpaired surrogates), and no non-JSON constants;
index and manifest schema versions must be the integer `2`, not `2.0`.
Publication validates the source graph once and revalidates its staged copy.
Enclosing provider metadata is checked against the loader's required fields,
types, identity, runtime, memory, capability and environment/source constraints.
Prepared release artifacts must also use runtime version `0.21.0`, as required
by the current CLI.
Different runtimes, provider versions, programs or logical provider metadata
cannot be combined. Repeated compatibility tuples, unsafe paths/symlinks, oversized inputs
and more than 128 variants fail explicitly. Missing listed variants are errors;
omit variants that were not built. Supplying no variants deliberately produces
an empty snapshot index with cold-boot assets.

The output is both a standalone provider artifact and an OCI image layout:

```text
manifest.json             # pins index.json by SHA-256
provider.py
rootfs.cpio
kernel                    # only when a custom artifact supplies one
oci-layout
index.json                # native multi-platform/multi-vendor OCI index
blobs/sha256/HEX           # pinned index, native manifests, config, memory, transport
```

Index serialization and variant order are canonical. Native manifest, config,
memory and transport bytes are preserved without adding a snapshot wrapper.
Descriptors retain native compatibility and snapshot-key annotations; standard
OCI platforms (`linux/amd64`, `darwin/arm64`) are added for registry tooling.
Publication uses a temporary ORAS catalog referencing the pinned native index;
the catalog does not change the published index digest or consumer contract.
Identical captured bytes and provider inputs produce identical index and blob
digests regardless of input directory names or variant argument order. This is
not a promise that independent VM boots yield identical memory snapshots; their
captured bytes can differ. Keep the captured inputs and use a pinned Python/Rust
toolchain for reproducible release assembly. No build time, local path, tag, registry address or authentication material is
added to the OCI graph. The provider manifest records its repository
separately. Native
reference-name annotations retain Hyperlight's runtime version and snapshot key.

Publication requires an explicit command, repository and tag, plus **ORAS 1.2+**
on `PATH`. ORAS uses standard Docker credential configuration, including
`DOCKER_CONFIG`, Docker credential helpers and `docker login HOST` (or
`oras login HOST`). No new secrets or credential flags are needed. Do not put
credentials in repository arguments, provider manifests or build configuration.
Only `HOST/REPOSITORY` is accepted: no URL schemes, userinfo, tags or digests.
TLS verification stays enabled; the tooling provides no insecure/plain-HTTP
publication switch.

```bash
# This is the only step that writes to a registry. Run it explicitly when authorized.
just providers::snapshot-publish \
  build/snapshot-layouts/kind ghcr.io/example/kind-snapshots \
  provider-0.3.0 build/published-providers/kind
```

The command validates and freezes the local graph, copies the full multi-platform
index and all referenced variants without platform filtering, then fetches the
published index **by digest** and verifies its exact bytes. The success output is
`HOST/REPOSITORY@sha256:HEX`; tags are discovery aliases, never runtime inputs.
Only after success does it create the output provider directory. That directory
retains the complete verified OCI layout, the pinned `snapshotIndex`, all
cold-boot assets, and `snapshotRepository`. Provider startup selects the local
variant without a registry request; the repository remains available for cache
repair or artifacts whose local layout was intentionally removed:

```bash
ly provider pull-snapshots --registry ghcr.io/example/kind-snapshots \
  --index sha256:<snapshotIndex.sha256>
```

After preparing `kind/`, `k3d/` and `azure/` under one directory, existing release
packaging consumes the local OCI layouts without rebuilding or overwriting them:

```bash
# Assemble and publish each maintained provider before release packaging.
cp -a build/published-providers/k3d build/release-providers/k3d
just package-preview "$PWD/build/release-providers"
# Or, after normal version preparation:
just package-release vX.Y.Z "$PWD/build/release-providers"
```

The optional `provider-artifacts` argument is also supported by `cli::package`,
`cli::package-preview`, `cli::package-release` and
`scripts/package-platform.py --provider-artifacts DIRECTORY`. Local indexes and
every referenced variant blob are verified and retained in platform/provider
archives.
Packaging freezes provider files into private staging, revalidates their
manifest and pinned graph, then uses those same files for both archives.
Installer file-size, total-size, entry-count and path checks run before copying
provider inputs into staging, and run again when writing the archives.
The combined platform input list, including all providers and platform base
files, is also checked before either provider is copied.
Staging retains the original preflight manifest as its comparison baseline;
later source reads cannot replace it. Lookup errors such as a non-directory
intermediate component remain errors.
After all archives succeed, packaging atomically records the platform version
and the frozen provider versions in `.provider-versions.json` in the output
directory. Alias creation uses this metadata rather than reopening mutable
source graphs, and refuses it when the checkout's version differs; keep it with
the versioned archives when creating aliases in a separate invocation.
Packaging removes the previous marker before replacing the first archive.
A failed rebuild therefore cannot create aliases from a mixed archive set;
rerun packaging successfully before creating aliases.
Provider assets cannot use the reserved release path `LICENSE` or its descendants.
Source mutations during staging fail explicitly; later source changes cannot
alter the archived provider bytes.
Manifest comparisons use canonical JSON, so numeric type changes during staging
are rejected rather than treated as equal values.
Prepared artifacts must match the checkout's declared provider identity and
program. These consistency checks are not build-input provenance: build the
canonical cold assets from the intended release templates, rootfs recipe and
pinned kernel before capturing variants.
Assembled layouts retain their index blob in the release archive, so an
extracted local artifact can also be published. Native input layouts need not
contain that extra content-addressed copy.
Release archives must fit the existing installers: each entry and the
compressed archive at most 1 GiB, total extracted file bytes at most 4 GiB,
and at most 100,000 entries. Packaging rejects larger layouts instead of emitting
uninstallable archives. Native snapshot assembly/publication still supports 4 GiB memory blobs, but
release packaging rejects layouts that exceed installer limits. Hard-linked
inputs are stored as separate regular archive files.
Archive names must also meet the loader path rules above and use only ASCII
letters, digits, `.`, `_`, `/`, `@`, `+`, `=`, or `-`, as required by the shell
installer.
Release packaging never invokes publication implicitly. No workflow changes or
GitHub Actions are required to use these recipes.

Output directories must be new. Assembly and publication stage locally, leave
inputs unchanged, clean temporary files on failure and rename the completed
output only at success. Invalid inputs fail before network publication.
Publication checks that the frozen provider manifest still equals the
validated input and constructs the pinned index blob from its frozen bytes.
Authentication/permission errors, helper failures, timeouts, copy failures,
unsupported registry media types and post-publication digest mismatches fail
the build; they do not emit a success-shaped provider artifact. After the copy,
publication also confirms that every child manifest, config and layer of the
graph is in the registry with the expected digest and size, without
downloading them. ORAS output is suppressed because registry/helper diagnostics
can contain credentials; set `LAMPYR_ORAS_VERBOSE=1` to show it while
troubleshooting. Each ORAS command has a ten-minute deadline; the copy allows
longer for large graphs, assuming at least 1 MiB/s. A failed/interrupted
publication can leave remote blobs or an updated tag: remote writes cannot be
rolled back atomically. Fix the cause and retry from the unchanged local layout
with a new local output directory; runtime consumers remain digest-pinned.

At startup, an absent compatible variant (including an empty index) cold-boots
from the packaged assets; a missing/corrupt selected blob or restore error does
not. Packaged release providers select their local OCI variant before
consulting the registry/cache path described below.

`just providers::test-snapshots` exercises deterministic archives and indexes,
input rejection, explicit publication/auth invocation, redacted failures and
release preservation without contacting any registry or requiring KVM/secrets.
The recipe installs checksum-pinned ORAS and copies the actual multi-platform graph between
two local layouts. These checks are included in `cli::test-packaging`; Rust
artifact tests exercise the generated output through the real loader.

### Native snapshot descriptor contents

| Descriptor | Media type | Content |
|---|---|---|
| `config` | `application/vnd.hyperlight.snapshot.config.v5+json` | Hyperlight configuration, including architecture, hypervisor, CPU vendor, ABI version, and the memory layers' layout. |
| Every layer but the last (1 to 31) | `application/vnd.hyperlight.snapshot.memory.v2` | Raw snapshot memory and page tables: up to 30 data layers, and one more layer when the page tables are in none of them. |
| Last layer | `application/vnd.hyperlight.snapshot.transport.v1` | Hyperlight transport state. |

The CLI verifies digests and sizes. Hyperlight loads the selected manifest
directly by digest and checks the same memory file handle that it maps.
The CLI does not copy the snapshot or rewrite its index.
**Keep the layout immutable while the loaded artifact or provider is in use.**
Do not overwrite or truncate its blobs. Hyperlight keeps memory mapped;
later file changes can corrupt the guest. Publish updates in a new directory.

Limits: 128 variants; 64 KiB each for index, marker, manifest, and config;
4 GiB memory across its layers; 2 MiB transport. Legacy snapshot archive limits
are unchanged.
Assembly preserves the native blobs. `providers/package.py` writes the
single-snapshot format by default or retains the native layout in variant mode.

### Pull snapshots from a registry

Public registries need no credentials: run the commands above without
`docker login`.

For private registries, configure a Docker credential helper in
`$DOCKER_CONFIG/config.json` (default: `~/.docker/config.json`):

```json
{
  "credHelpers": {
    "ghcr.io": "pass"
  }
}
```

Install and initialize the selected helper (`docker-credential-pass` in this
example), then run `docker login ghcr.io` with your username and access token.
Use `"credsStore": "desktop"` instead if Docker Desktop manages your credentials.
Lampyr reads credentials only from helpers, not inline `auths` or identity
tokens. Existing complete caches need neither credentials nor network access.

Pulls use `rust-oci-client` with verified TLS. HTTPS registries require HTTPS
for token endpoints and redirects too. HTTP is allowed only on literal
loopback addresses for local testing.

Snapshots are cached under `LAMPYR_HOME/cache/provider-snapshots/INDEX_SHA256/`.
A complete cache works offline. The provider manifest's repository is used by
default; `LAMPYR_SNAPSHOT_REGISTRY` overrides it.
Existing local indexes take precedence. Keep layouts immutable while providers
are running; remove corrupt cache entries only after stopping those providers.

An absent or unavailable default registry index, no compatible variant, or a
host without a detected hypervisor or CPU vendor permits cold boot from the
platform-specific local rootfs. Loading a provider on a host
without a detected hypervisor or CPU vendor does not contact the registry.
Unavailable default repositories emit a warning. Explicit registry overrides,
integrity failures, and restore failures are errors.
Pulls verify digests and sizes and support cancellation. Each request times out
after 120 seconds; blob downloads time out after 120 seconds without data.
Metadata, token and error responses are cut off at 64 KiB while they are read;
memory and transport downloads are bounded to 4 GiB and 2 MiB.

## Artifact manifest

Use `artifact.json` to specify the provider identity, runtime compatibility, and
operations.
Specify the minimum capabilities that each operation requires.

```json
{
  "protocolVersion": 1,
  "id": "example",
  "version": "0.1.0",
  "runtimeVersion": "0.21.0",
  "memoryMiB": 512,
  "environment": {
    "PATH": "/usr/local/bin:/usr/bin:/bin"
  },
  "networkHosts": ["api.example.com"],
  "operations": {
    "describe": [],
    "init": ["workspace-read", "workspace-scaffold", "state"],
    "apply": ["workspace-read", "state", "network"],
    "inspect": ["state", "network"],
    "connect": ["state", "network"],
    "delete": ["state", "network"]
  }
}
```

Set `runtimeVersion` to the Hyperlight-Unikraft guest runtime version that the
provider requires.
This version must match the version that `ly` supports.

Use these capabilities:

| Capability | Access |
|---|---|
| `workspace-read` | Read the infrastructure source for this installation at `/workspace`. |
| `workspace-scaffold` | Create the initial customer files through `workspace.scaffold`. |
| `state` | Read and write private provider JSON state within the size limits. |
| `network` | Connect only to hosts in `networkHosts`. |
| `auth-cache` | Read and write files in the private provider directory `/auth`. |
| `docker` | Use the Docker bridge with user permission. |
| `provider-manifests` | Return Kubernetes manifests that Lampyr applies with cluster credentials. |

Lampyr grants capabilities for each operation.
Providers do not receive Kubernetes credentials or direct access to `kubectl`.

Docker access requires both conditions:

1. The operation specifies the `docker` capability in `artifact.json`.
2. The user grants that installation access with `--allow-docker`.

One condition alone does not permit Docker access.
When both conditions apply, Lampyr gives the provider access to its authenticated
Docker bridge.
Lampyr does not give the provider a Docker executable.
It does not mount the host socket in the provider guest.

Bridge connection errors include `peer=<address:port>` and a `phase`:
`first-byte`, `tls-handshake`, `daemon-connect`, or `authenticated-forwarding`.
TCP EOF before authentication is reported as EOF, not a handshake timeout.
Handshake timeouts include TLS byte counts, elapsed and last-I/O times, the
negotiated protocol, the client certificate count, and pending read/write state.
The error context identifies the failed read, write, or connection operation.
Use these fields to locate the failed connection and phase, not to infer the
root cause of a provider upload failure. Peer addresses identify sockets, not
guest processes. The bridge does not log request data or credentials.

Use `provider-manifests` only for `apply`.
A provider with this capability has access equivalent to a Kubernetes cluster
operator.
Lampyr writes the `before` and `after` manifests to private local YAML files.
It shows their resource identities and security-sensitive types.
It then asks for approval.

These files use standard Kubernetes YAML.
You can examine the files or apply them manually in phase order.
For automated operations, specify `--approve-provider-manifests`.

## Provider protocol

Lampyr calls `invoke` in `provider.py` with this request:

```json
{
  "protocolVersion": 1,
  "operation": "apply",
  "installationId": "32-character-installation-id",
  "config": {}
}
```

For success, return:

```json
{"protocolVersion": 1, "status": "ok", "result": {}}
```

For an error, return:

```json
{
  "protocolVersion": 1,
  "status": "error",
  "code": "invalid-config",
  "message": "location is required"
}
```

Implement the `describe` operation.
It must return an exact copy of the verified artifact manifest.

Implement these Cluster Provider operations:

| Operation | Responsibility |
|---|---|
| `init` | Check the configuration and create the initial infrastructure source files. |
| `apply` | Update the infrastructure to the required state. Write the shared runtime manifest plan to provider state key `runtime`. |
| `inspect` | Report if the infrastructure exists, belongs to this installation, and is ready. |
| `connect` | Return a standard kubeconfig that contains the CA, client certificate, and client key data. |
| `delete` | Delete only infrastructure that belongs to this installation. |

During `apply`, the provider writes a runtime plan to provider state:

- `before`: Manifests that Lampyr applies before it installs the shared platform.
- `after`: Manifests that Lampyr applies after it installs the shared platform and Eventing.
- `cleanup`: Manifests that Lampyr applies during removal from a cluster that the user keeps.
- `options`: Checked settings for the shared Lampyr platform.

Lampyr does all Kubernetes operations.
The provider does not receive Kubernetes credentials.

The installation interface shows **BEFORE LAMPYR PLATFORM** and
**AFTER LAMPYR PLATFORM** for the first two phases.
During installation, Lampyr does not show or write the cleanup manifests.
If the user keeps the cluster during removal, Lampyr shows the cleanup manifests
before it applies them.

## Event Source Providers

The same plugin can implement `source-create`, `source-status`, and
`source-delete`.
The source provider must be the same as the cluster provider.
The cluster installation must include `--eventing`.

Specify every kind that a source plan can return in the artifact manifest:

```json
"sourceKinds": [
  {"apiVersion": "eventing.knative.dev/v1", "kind": "Broker"},
  {"apiVersion": "eventing.knative.dev/v1", "kind": "Trigger"},
  {"apiVersion": "sources.example.dev/v1", "kind": "ExampleSource"}
]
```

Lampyr permits Broker, Trigger, and custom API groups that belong to the provider.
For a custom API group, the approved provider plan must install a Namespaced CRD.
Lampyr does not permit core Kubernetes resources, workloads, Secrets, RBAC, Jobs,
or built-in Knative source types.
Broker and Trigger must use the default Broker.
They must use the `/events/knative` path for the selected Lampyr Service.

Every source operation receives this request:

```json
{
  "name": "blob-created",
  "namespace": "default",
  "service": "hello-edge",
  "observations": []
}
```

The `source-create` operation returns:

```json
{
  "sharedManifests": [],
  "manifests": [],
  "observations": [],
  "timeoutSeconds": 300
}
```

- `sharedManifests`: Provider resources that multiple sources use.
- `manifests`: Resources that belong to the named source and service.
- `observations`: Returned manifests whose Kubernetes state is necessary for readiness checks.
- `timeoutSeconds`: Time limit, in seconds, for creation and readiness checks.

Lampyr checks the plan limits and adds ownership labels.
It permits objects only in the requested namespace.
It applies the manifests and saves the resource inventory.

The `source-status` operation receives the requested observations and returns:

```json
{"ready": true, "reason": "Ready", "message": "Source is ready.", "terminal": false}
```

The `source-delete` operation returns only a `manifests` array.
Each item must identify a resource in the saved inventory for this source.
The provider cannot delete other objects.
Lampyr does all Kubernetes read, apply, and delete operations.
The provider does not receive Kubernetes credentials.

## Build and test

Add these recipes:

```just
test-unit:
    python3 -m unittest discover --start-directory tests --pattern 'test_*.py'

build:
    # Create build/rootfs/rootfs.cpio before you run this script.
    python3 ../package.py . rootfs --kernel ../runtime/build/elfloader_hyperlight-x86_64
```

Use `providers/package.py` to assemble the final artifact, copy files, and
calculate digests.
Do not assemble the final artifact or write `manifest.json` with another procedure.

As a minimum, test these conditions:

- The provider checks the configuration before cloud or Docker access.
- The provider checks installation ownership and handles repeated attempts.
- Provider state and output stay within their limits.
- Infrastructure updates safely use customer changes to source files.
- Connection output and kubeconfig credentials are correct.
- The provider refuses to delete infrastructure that belongs to another installation.
- Event Source operations check source ownership, plan limits, and readiness, and permit repeated delete attempts.

Then test the packaged artifact:

```bash
ly cluster install \
  --provider example \
  --provider-artifact providers/example/build/rootfs \
  --provider-config example.yaml
```

Do not put credentials in customer source, generated runtime plans, or error
messages.
