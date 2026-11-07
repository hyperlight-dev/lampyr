---
title: ly build
---

# `ly build`

Use `ly build` to create a Hyperlight snapshot and load a local OCI image.

```bash
build/ly build [--output PATH] [--target TARGET] [--context CONTEXT]
  [directory]
```

Build one service for `ly deploy`:

```bash
build/ly build services/hello-edge
```

When the platform has its helper, `build/hluk-helper`, `ly build` runs the
helper's `--version` where the snapshot is made and records its facts with the
artifact; a helper that fails or lacks the `template-dir` capability is an
error. `ly deploy` creates a Revision only from an artifact with those facts,
since grant Pods run only fiberd templates. A release platform carries the
helper; in a checkout, build it with `just template-tools`. Without
it, `ly build` notes that the artifact is for `ly dev`, `ly push` and
`ly verify` alone.

Run the command from a Workspace root to build every service in `services/`.
Lampyr builds the services in name order:

```bash
build/ly build
```

Use `--output` only when you build one service. Each service needs its own
artifact directory.

## Build steps

Lampyr performs these steps:

1. Lampyr validates the application model. For a Handler Application, it
   builds the root file system with `.lampyr-adapter/Dockerfile.rootfs` and
   checks the handler hooks inside the built image. For a Standalone
   Application, it builds the customer-owned root-level `Dockerfile.rootfs`.
2. Lampyr creates the Hyperlight snapshot.
3. Lampyr writes the kernel, snapshot, and metadata under `.lampyr/artifact/`.
4. Lampyr builds the local OCI image with `Dockerfile`.

Lampyr builds against the active Docker context so it can use local runtime
images. Custom Buildx builder selection is not supported.

Runtime base images are pinned by digest. Lampyr maps the Dockerfile's
`hluk-<driver>-rootfs:latest` reference to that digest with a Buildx named
context; it does not create or replace a shared local tag. Concurrent builds
can therefore use different runtime versions without changing each other's base.

For manual Docker builds, `just --quiet setup-rootfs nodejs` prepares the base
and prints its context assignment. Pass that output as one `--build-context`
argument to `docker buildx build`. The `runtime-assets --rootfs` helper prints
the same assignment. `ly build` and `ly dev` pass it automatically.

The root file system build uses the service root as its context. Adapter
recipes can copy files from `.lampyr-adapter/` and user application files.
Standalone Applications use their customer-owned root-level `Dockerfile.rootfs`
and `application.command`. Lampyr does not check hooks or read adapter metadata
for them; their Dockerfile compiles the server and installs its dependencies.
The server must listen on `0.0.0.0:$PORT`. An optional `events:` block trusts
the server's event endpoint. Build output and artifact metadata identify the
application model, and Lampyr never rewrites standalone source or recipes.

If `.lampyr-adapter/` exists but does not contain its root file system recipe,
the build fails. Lampyr does not use a legacy recipe as a fallback.

Builds do not change adapter source, metadata, or baseline files. Tracked
adapter files contribute to the source digest and revision identity. Files
under `.lampyr/` do not.

## Adapter status

Build, dev, and deploy report modified and outdated adapters. They continue
with a warning. Lampyr does not upgrade adapter files. A difference in editable
source hashes does not indicate invalid metadata.

Corrupt metadata, a changed pristine baseline, or an unfinished upgrade is an
error. Lampyr does not fall back to another build recipe. Use
[`ly scaffold service NAME --upgrade`](scaffold.md#upgrade-an-adapter-explicitly)
to update an adapter. Resolve any conflicts before you build.

## Snapshot compatibility

Snapshot creation does not run application preload code. The service entrypoint
starts after the snapshot is restored. Optional `snapshotWarmup` metadata
supplies runtime code to `hluk --warm-exec` for local and cluster snapshots.
The Node.js template initializes Fastify routes without loading the handler.
Lampyr uses Hyperlight-Unikraft release
assets. If you change the pinned kernel or host contract, update the runtime
assets. Then rebuild existing service snapshots before you deploy them.

| Option | Default | Description |
|---|---|---|
| `--verbose` | `false` | Show build tool output instead of the quiet progress display. |
| `--output` | `.lampyr/artifact/linux-amd64-kvm` | Set the artifact directory. |
| `--target` | `linux-amd64-kvm` | Set the snapshot compatibility target. |
| `--context` | Local host | Create the hardware-bound snapshot in a temporary KVM Pod on this Kubernetes context. `--snapshot-context` is an alias. |

Hyperlight snapshots depend on the CPU vendor. Create the snapshot on the
target cluster if it uses a different CPU vendor than your workstation:

```bash
build/ly build --context aks-lampyr services/hello-edge
```

Lampyr builds the root file system and OCI image on this machine. For a cluster
snapshot, Lampyr copies the root file system and `hluk` binary to a temporary
privileged builder Pod. The Pod creates the snapshot. Lampyr copies the
snapshot back and deletes the Pod.

The CLI library, runtime executable and kernel use Hyperlight-Unikraft v0.21.0
release pins. Rebuild snapshots from earlier releases or custom source builds;
v0.21.0 cannot restore snapshots from v0.20.1 or earlier. Deployment snapshots
use `linux-amd64-kvm`; native dev snapshots are separate and must not be copied
between architectures or hypervisors.

By default, Lampyr shows progress for root file system creation, snapshot
creation, metadata, and OCI image packaging. It does not show Docker or `buildx`
logs. If a step fails, Lampyr shows the captured error details. Use
`ly build --verbose [directory]` to show tool output. See
[command output](README.md#command-output).
