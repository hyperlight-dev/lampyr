---
title: Lampyr field guide
description: Practical documentation for building and operating Lampyr Services.
---

# Lampyr field guide

Lampyr is a platform for running familiar applications in
hardware-isolated Hyperlight microVMs with a serverless-like workflow on
Kubernetes.

Lampyr Service instances can reach zero when idle. Shared routing, activation,
placement, and node runtime infrastructure remains running so the next request
can restore a prepared microVM.

## From setup to your first service

1. **Install Lampyr:** check the [local requirements](requirements.md), then
   install or [build the CLI from source](contributing.md#build-from-source).
2. **Create a cluster:** use [Kind](cluster-kind.md) or [k3d](cluster-k3d.md) locally, or choose
   [AKS on Azure](cluster-aks.md).
3. **Create and deploy a service:** follow the [walkthrough](walkthrough.md)
   to scaffold an app, run it locally, build it, deploy it, and send a request.

Once your service is running, use [status](cli/status.md), [logs](cli/logs.md),
[audit](cli/audit.md), and [scale](cli/scale.md) to operate it.

## Advanced integrations

Add event-driven delivery after the basic service workflow is working:
the [Azure Event Grid guide](azure-event-grid-source.md) covers provider setup,
Blob events, and local Kind testing. Use the [events and sources
reference](cli/event.md) for the commands.

## Current limitations

- Kind is the recommended first cluster. AKS requires nested virtualization
  and creates billable Azure resources.
- Runtime templates have different protocol coverage. Check the
  [compatibility matrix](runtime-compatibility.md) before choosing a runtime.
- Published release downloads are not established by the current release
  workflow, so source build is the dependable onboarding path today.
- Performance and density claims need a deeper, current measurement before
  they can be used for planning.

## Documentation map

| Goal | Start here |
|---|---|
| Check host requirements | [Local requirements](requirements.md) |
| Build the CLI | [Contributing](contributing.md#build-from-source) |
| Create a cluster | [Kind](cluster-kind.md), [k3d](cluster-k3d.md), [AKS](cluster-aks.md) |
| Deploy your first service | [Walkthrough](walkthrough.md) |
| Compare supported templates | [Runtime examples](examples.md) |
| Develop locally | [`ly scaffold`](cli/scaffold.md), [`ly dev`](cli/dev.md) |
| Develop multi-service workers | [Local worker usage](cli/dev.md#workspace-development), [source-checkout workflow](cli/dev.md#contributor-workflow) |
| Build and deploy | [`ly build`](cli/build.md), [`ly deploy`](cli/deploy.md); optionally [`ly push`](cli/push.md) |
| Operate a service | [`ly status`](cli/status.md), [`ly logs`](cli/logs.md), [`ly scale`](cli/scale.md), [metrics](observability.md) |
| Develop an event source | [Azure Event Grid source](azure-event-grid-source.md) |
| Create an infrastructure provider | [Provider authoring](provider-authoring.md) |
| Understand the architecture | [Lampyr Service](lampyr-service.md), [product design](designs/product-design.md) |
| Explore future direction | [Project plan](designs/project-plan.md) |
