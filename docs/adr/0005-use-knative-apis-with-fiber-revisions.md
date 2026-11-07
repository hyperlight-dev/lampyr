---
title: Use Knative APIs with Fiber-backed Revision reconciliation
description: Reuse Knative service semantics while replacing only Pod-based Revision execution.
---

# Use Knative APIs with Fiber-backed Revision reconciliation

**Status:** Accepted

Lampyr will use upstream Knative Service, Configuration, Revision, Route, and
DomainMapping resources as its public service control plane. Lampyr will compose
the upstream Service, Configuration, Labeler, Route, garbage-collection,
namespace-certificate, and DomainMapping reconcilers without modifying their
source, while replacing the Pod-based Revision reconciler with a Lampyr-owned
Fiber Revision reconciler.

The Fiber Revision reconciler verifies Lampyr snapshot artifacts, creates and
manages CapacityGrants and Homes, translates the supported Knative scaling
fields into Fiber policy, creates the per-Revision Kubernetes network backend
expected by Knative Route reconciliation, publishes Revision metadata to
`fiber-router`, and reports standard Knative Revision conditions.

The standard Knative container image can remain a runnable application image,
but Lampyr neither builds nor executes it. Lampyr reads the Hyperlight snapshot
artifact from the digest-pinned `lampyr.dev/snapshot-artifact` Revision
annotation. By default, `ly deploy` sets the required container-image reference
to the same snapshot artifact used by the annotation. External build tooling
can instead supply a runnable image for portable or dual-target Service YAML;
stock Knative ignores the Lampyr annotation, while Lampyr ignores the runnable
image for execution.

Lampyr will remove its public `LampyrService`, `EdgeRevision`, `EdgeRoute`, and
`EdgePolicy` APIs. It will retain upstream CapacityGrant, AzureEventGridSource,
Knative Eventing, KIngress, and Kourier.

This design deliberately does not run Knative's stock Revision reconciler,
ServerlessService reconciler, Pod Autoscaler, Activator, queue-proxy, or
application Pods. Unchanged Knative Route and Kourier reconciliation has been
proven to route each per-Revision `ExternalName` Service to the shared
`fiber-router`.

Lampyr installs the upstream Serving CRDs and the unchanged webhook,
configuration, certificate, and RBAC support required by those APIs. The stock
controller, Activator, autoscaler, and their workload support resources are
filtered out before installation rather than installed and scaled down.

## Upstream direction

Lampyr will initially own and operate the Fiber Revision reconciler while it
proves runtime stability, Knative API and status compatibility, upgrade
reliability, networking portability, and the remaining production lifecycle.
After those concerns are resolved with evidence, Lampyr intends to approach the
Knative community to explore whether a Fiber/Fiberd-backed Revision reconciler
would be useful as an upstream extension or integration.

Upstream acceptance is not a prerequisite for this decision. Lampyr must keep
the reconciler maintainable and production-ready independently, and should
bring a proven implementation and concrete compatibility results rather than
asking Knative to adopt an experimental execution path.

## Consequences

- Lampyr inherits Knative Revision naming, immutability, traffic splitting,
  tagged routes, status propagation, domains, TLS, and garbage collection.
- Lampyr owns a custom controller distribution and must test compatibility with
  every pinned Knative upgrade.
- The supported Knative Revision PodSpec is intentionally a Fiber-compatible
  subset.
- Kourier remains the first networking implementation, but the Knative
  networking abstraction permits a future Gateway API or Azure Application
  Gateway implementation.
