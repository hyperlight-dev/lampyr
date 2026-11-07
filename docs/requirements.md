---
title: Local requirements
description: Host requirements for running Lampyr and creating local clusters.
---

# Local requirements

Run `ly doctor` to check this machine:

```bash
ly doctor
```

## Required locally

- **Docker** builds service filesystems and OCI images. The Kind and k3d providers
  also use the Docker daemon through Lampyr's authenticated bridge.
- **KVM** must be available read/write at `/dev/kvm` for `ly dev`, local
  snapshots, and local Kind/k3d workloads.
- **kubectl** must be available for Kubernetes lifecycle and service
  operations.

Host Kind and k3d are not required. Each provider packages its own cluster and
Docker clients.

## Kubernetes

You need either:

- Docker and KVM for a local [Kind](cluster-kind.md) or [k3d](cluster-k3d.md) cluster,
- a Kubernetes context to an existing cluster.

Nodes must support nested virtualization. See the [AKS
guide](cluster-aks.md) before creating billable resources.

## Snapshot compatibility

Hyperlight snapshots are CPU-vendor-specific. Build locally when the target
cluster uses the same CPU vendor, or use `ly build --context CONTEXT` to create
the snapshot on the destination cluster.

## Building Lampyr itself

See [Contributing](contributing.md#prerequisites).
