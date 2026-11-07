---
title: Lampyr CLI
description: Command reference for developing, deploying, and operating Lampyr Services.
---

# Lampyr CLI

The `ly` CLI creates, builds, publishes, deploys, and operates Lampyr Services.

| Command | Purpose |
|---|---|
| [`ly scaffold`](scaffold.md) | Create a Workspace or Lampyr Service. |
| [`ly setup`](setup.md) | Install the platform payload for this host. |
| [`ly doctor`](doctor.md) | Check the platform and host prerequisites. |
| [`ly dev`](dev.md) | Run a service or multi-service workspace locally with Hyperlight snapshots. |
| [`ly build`](build.md) | Build the snapshot and local OCI image. |
| [`ly push`](push.md) | Push the local image to a registry. |
| [`ly verify`](verify.md) | Check local artifact metadata and digests. |
| [`ly deploy`](deploy.md) | Deploy an immutable image Revision. |
| [`ly status`](status.md) | Show service and Fiber status. |
| [`ly logs`](logs.md) | Read application-handler logs. |
| [`ly audit`](audit.md) | Read Fiber lifecycle audit records. |
| [`ly scale`](scale.md) | Change the active Revision's Fiber limit. |
| [`ly delete`](delete.md) | Remove a deployed Lampyr Service. |
| [`ly event`](event.md) | Publish an event with retries. |
| [`ly cluster`](cluster.md) | Install and manage the platform. |
| [`ly version`](version.md) | Print the CLI version. |

For the local worker spike, start with the
[multi-service workspace usage guide](dev.md#workspace-development).
It covers service bindings, HTTP and synchronous events, execution policy,
and the [source-checkout workflow](dev.md#contributor-workflow).

Commands that operate on a project accept an optional project directory. They
use the current directory when it is omitted.

Kubernetes operations use kubeconfig `current-context` unless `--context NAME`
selects an exact context. Merged `KUBECONFIG` files are supported; context names
are not expanded with `kind-` or `aks-` prefixes. Local commands do not require a
context. Build remains local unless `--context` selects a remote snapshot
builder. See [`ly cluster`](cluster.md) for cluster creation and reconciliation.

## Command output

Installs, builds, pushes, and deployments show concise progress instead of raw
Docker, Kind, or kubectl logs. In an interactive terminal, a small fixed-position
spinner and elapsed timer indicate that each step is running. Completed steps
display `ok`. Redirected progress and CI use plain start/completion lines without
animation or ANSI codes.
Progress goes to stderr; command results stay on stdout.

Add `--verbose` before the project directory to stream underlying tool logs:

```bash
ly cluster install --verbose --provider kind --allow-docker
ly build --verbose services/demo-app
ly push --verbose --image REGISTRY/demo-app:v1 services/demo-app
```

Failed steps always report the underlying error and the last 64 KiB of captured
tool output, even without `--verbose`. Interactive application output from
`ly dev` and records from `ly logs` and `ly audit` are never hidden.
Set `NO_COLOR` to disable color, or `TERM=dumb` to disable animation as well.
