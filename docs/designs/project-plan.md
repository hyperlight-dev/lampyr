---
title: Exploratory project plan
description: Broader project direction and aspirations that are not all implemented.
---

# Project Plan: Lightweight Lampyr Services on Hyperlight

## Status

Draft project direction based on the working Hyperlight and Unikraft,
Knative, snapshot, Fiberd home, and standalone protocol-consumer prototypes.

> This document is exploratory. It includes target-state ideas and should not
> be read as a list of currently implemented product behavior. See
> [Product design](product-design.md) for the confirmed baseline and the
> [walkthrough](../walkthrough.md) for the implemented workflow.

## Executive summary

Build a small, focused execution platform for lightweight services that gives
developers a serverless-like experience on disposable, single-purpose
Kubernetes clusters without forcing a proprietary programming model.

The platform should make common workloads easy to deploy while retaining
escape hatches for teams that need direct control. Hyperlight microVMs and
Unikraft provide fast, hardware-isolated execution. Kubernetes provides the
admitted capacity and operational boundary. A node-local activation layer
avoids creating a new Pod for every short-lived instance.

The intended product is not "Hyperlight as a Kubernetes runtime" exposed
directly to users. Hyperlight, Unikraft, OCI artifacts, snapshots, and
Knative are implementation building blocks behind a simpler product
experience.

## Vision

Make small Lampyr Services feel nearly serverless to operate:

- Start with a minimal, disposable Kubernetes cluster.
- Run many small services efficiently on a small number of VMs.
- Autoscale without requiring developers to understand most Kubernetes
  machinery.
- Support familiar languages such as Go, Rust, Python, JavaScript, and .NET.
- Avoid a required service-specific programming model.
- Provide opinionated defaults with clear escape hatches.
- Make multi-region deployment straightforward.
- Minimize persistent node state by streaming and reaping OCI artifacts.
- Reduce platform and programming-model lock-in.

## Target users

Teams that:

- Build small HTTP, event-driven, agent, or Lampyr Services.
- Care about efficient execution and predictable infrastructure cost.
- Do not want to pay a large per-request platform premium.
- Already know standard languages and container-oriented workflows.
- Want less operational overhead than general-purpose Kubernetes.
- Need stronger isolation than running arbitrary code as a normal process.
- Want portability across local, edge, and cloud environments.

## Initial scenarios

### Edge HTTP services

Deploy small APIs close to users without maintaining a large regional
platform footprint.

### Secure agent execution

Run agent-generated or untrusted code inside a hardware-isolated,
default-deny Hyperlight microVM.

### Event handlers

Start isolated handlers quickly for queues, webhooks, scheduled work, and
automation.

### Multi-tenant code execution

Provide short-lived sandboxes for CI tasks, developer tools, notebooks, and
code interpreters.

## Product principles

### Developer experience first

The common path should look like:

```text
initialize project
choose runtime
deploy
observe
scale
```

Developers should not need to understand snapshot layouts, queue-proxy behavior,
or containerd configuration.

### Do not require a new programming model, but provide one

Provide supported paths for common languages, but preserve lower-level
integration options:

```text
Easy path: supported runtime template (not implemented yet)
Provide: ability to use common runtimes as is
Escape hatch: custom OCI rootfs and entrypoint
Lower level: direct Hyperlight/Unikraft integration
```

### Disposable and immutable infrastructure

Clusters and worker nodes should be replaceable. Persistent state should live
outside the execution cluster.
