---
title: Fiber-backed Knative Serving control plane
description: Reuse Knative service reconcilers while executing every Revision as Hyperlight Fibers.
---

# Fiber-backed Knative Serving control plane

This design evaluates
Lampyr's remaining dependency on the full Knative Serving installation. This
design preserves Knative's public service APIs and mature control-plane
semantics while replacing only Revision execution.

## Ownership

Knative maintains the reused reconciler packages. Lampyr pins those packages,
assembles the controller binary, owns deployment compatibility, and implements
Fiber Revision behavior.

| Capability | Owner |
|---|---|
| Service → Configuration and Route | upstream Knative Service reconciler |
| Configuration → immutable Revision | upstream Knative Configuration reconciler |
| routing-state labels | upstream Knative Labeler reconciler |
| traffic, tags, URLs, visibility, TLS and KIngress | upstream Knative Route reconciler |
| stale Revision removal | upstream Knative GC reconciler |
| custom domains | upstream Knative DomainMapping reconciler |
| snapshot artifact and CapacityGrant lifecycle | Lampyr Revision reconciler |
| Fiber acquisition and release | Lampyr `fiber-router` and Fiberd |
| supported Knative template validation | Lampyr |
| controller packaging, RBAC and installation | Lampyr |

Lampyr does not run the stock Knative Revision or ServerlessService
reconcilers, KPA, Activator, queue-proxy, or application Pods.

## Control-plane flow

```text
Knative Service
  |
  +--> upstream Service reconciler
         |
         +--> Knative Configuration
         |      |
         |      +--> upstream Configuration reconciler
         |             |
         |             +--> Knative Revision
         |                    |
         |                    +--> Lampyr Revision reconciler
         |                           |
         |                           +--> verify Lampyr artifact
         |                           +--> CapacityGrant and Home
         |                           +--> Revision Kubernetes Service
         |                           +--> router Revision metadata
         |                           +--> Knative Revision status
         |
         +--> Knative Route
                |
                +--> upstream Route reconciler
                       |
                       +--> traffic percentages
                       +--> tagged URLs
                       +--> domains and visibility
                       +--> KIngress
```

The upstream GC reconciler removes stale Revisions. DomainMapping reconciliation
creates custom-domain KIngress and TLS state.

## Request path

```text
Client
  |
  v
Kourier
  |  selects a Revision from Knative Route traffic
  |  adds Knative-Serving-Revision and namespace headers
  v
Revision Kubernetes Service
  |
  v
shared fiber-router
  |  validates and loads the selected Revision
  |  consumes the internal Knative selection headers
  |  acquires or reuses its Fiber
  v
Hyperlight Fiber
```

`fiber-router` no longer chooses weighted traffic targets. It preserves
fresh-per-request and reuse execution, request completion, streams,
WebSockets, cancellation, and timeouts for the Revision already selected by
Knative routing.

## Revision backend feasibility gate

The upstream Route reconciler routes each traffic target to a Kubernetes Service
named after the Revision. The Lampyr Revision reconciler must create that
network identity without creating an application Pod.

Test these strategies in order:

1. an `ExternalName` Service targeting
   `fiber-router.lampyr-system.svc.cluster.local`;
2. a normal Service plus reconciled EndpointSlices that mirror shared
   `fiber-router` endpoints.

The selected strategy must prove:

- KIngress readiness;
- weighted Revision splits;
- Revision and namespace headers;
- tagged routes;
- HTTP, streaming, and WebSockets; and
- recovery after router endpoint replacement.

If both strategies fail with the unchanged upstream Route reconciler and
Kourier, stop. Do not introduce a custom Route reconciler or Knative fork
without a new architectural decision.

### Feasibility result

The `ExternalName` strategy is proven on Kind with the stock Kourier controller
and gateway:

- KIngress became Ready;
- 50/50 backend splits reached both Revision aliases;
- Kourier appended the standard Revision and namespace headers;
- a tagged hostname selected its Revision;
- HTTP, streamed responses, and WebSockets passed through; and
- the alias recovered after the shared `fiber-router` Deployment restarted.

Kourier translates an `ExternalName` backend into an Envoy logical-DNS cluster,
so no EndpointSlice mirroring is required.

### Portable Knative Service proof

The executable portability proof builds one handler through two independent
paths: a normal runnable OCI image and a Lampyr snapshot artifact. It applies
one identical digest-pinned Knative Service to stock Knative and Lampyr, then
requires identical responses, defaulted Service specs, and upstream Service CRD
specs:

```bash
just test-knative-portability
```

Lampyr runs a Revision only from a fiberd template sealed to the cluster's
fleet key, so the proof first deploys the service with `ly deploy` to build,
seal and push one, deletes it, and gives the shared Knative Service that
template's `lampyr.dev/fiberd-template`, which stock Knative ignores.
fiberd-k8s-controller has no release yet, so the proof, like every cluster
suite, needs its installation inputs: `LAMPYR_CONTROLLER_MANIFEST`,
`LAMPYR_GRANT_IMAGE` and `LAMPYR_HELPER_IMAGE`, and, unless `build/zygotectl`
and `build/grant-issuer` exist, `LAMPYR_FIBERD_SRC` naming the fiberd checkout
that builds them. In place of all four, `LAMPYR_CLUSTER_INPUTS` may name the
directory `just upstream::cluster-inputs` builds them into, as CI does (see
[Cluster suite inputs](../contributing.md#cluster-suite-inputs)).

For a demo that leaves both Kind clusters running and prints their curl
commands:

```bash
LAMPYR_PORTABILITY_KEEP=1 just test-knative-portability
```

## Fiber Revision reconciliation

The custom Go reconciler:

1. accepts the documented single-container Knative template subset;
2. requires a digest-pinned runnable container image in the standard image
   field;
3. requires a digest-pinned Lampyr snapshot OCI artifact in
   `lampyr.dev/snapshot-artifact`;
4. verifies the snapshot artifact and records the independently supplied
   container image;
5. translates resources and execution settings into CapacityGrant policy;
6. creates one CapacityGrant and Home for the Revision;
7. creates the Revision backend Service;
8. publishes Revision execution metadata to `fiber-router`;
9. reports standard Knative conditions;
10. finalizes all owned resources; and
11. recovers idempotently after restarts.

Revision readiness requires a verified artifact, admitted CapacityGrant, ready
Home, ready backend Service, and published router metadata.

## Scaling translation

Lampyr does not install KPA.

| Knative setting | Lampyr meaning |
|---|---|
| `containerConcurrency` | must be `1`; one active request is admitted per Fiber |
| `maxScale` | maximum concurrent Fibers for the Revision |
| `minScale` | must be `0` until reserved Fiber capacity is implemented |
| `timeoutSeconds` | maximum request, stream, or WebSocket lifetime |

Both execution modes require `containerConcurrency=1` and `minScale=0`.
Unsupported autoscaling classes or metrics make the Revision Ready condition
false.

## Supported Knative template subset

Initially support:

- one digest-pinned runnable container image;
- one digest-pinned `lampyr.dev/snapshot-artifact` annotation;
- a memory resource request;
- `timeoutSeconds`;
- `containerConcurrency`;
- `maxScale`;
- traffic percentages and tags;
- cluster-local visibility;
- DomainMapping and custom domains; and
- Revision labels and annotations.

Stock Knative ignores the Lampyr annotation and runs the normal image. Lampyr
uses the same YAML but restores the snapshot artifact. `ly build` and `ly push`
continue to produce only the Lampyr snapshot artifact. A normal runnable image
is produced independently when stock-Knative portability is being tested.

Reject command, argument, environment, and working-directory overrides,
sidecars, arbitrary volumes, unsupported probes and timeout variants,
unsupported runtime classes, security/service-account settings, CPU requests,
resource limits, positive `minScale`, concurrency other than one, custom
autoscaler classes, and mutable or non-Lampyr images.
gRPC templates declare a named `h2c` container port so stock Knative and Lampyr
select the same backend protocol.

## CLI and resource migration

`ly deploy` builds and publishes the Lampyr artifact, creates or updates a
Knative Service, and waits for standard status conditions. Direct
`kubectl apply` is supported for an already-published Lampyr artifact.
The CLI intentionally uses the snapshot digest in both image fields and is not
a stock-Knative image builder. The portable form demonstrated by
`test-knative-portability` uses external build tooling for the runnable image
and applies the resulting Service YAML directly.

Other commands use Knative Service, Route, Revision, and CapacityGrant state:

- `ly status`
- `ly scale`
- `ly logs`
- `ly audit`
- `ly event`
- `ly delete`

Remove `LampyrService`, `EdgeRevision`, `EdgeRoute`, and `EdgePolicy`. Retain
AzureEventGridSource and CapacityGrant. Knative Eventing subscribers reference
the Knative Service directly.

## Controller distribution

Knative's stock controller binary registers every Serving reconciler and cannot
disable only Pod-based Revision and ServerlessService reconciliation.

Lampyr therefore packages a Go controller entrypoint composed from selected
upstream constructors and the Lampyr Revision reconciler. The binary is added to
the existing platform build and Fiberd image and is deployed independently.

Installation applies upstream Serving CRDs plus the unchanged webhook,
configuration, certificate, and RBAC resources required by the public APIs. It
filters the stock controller, Activator, autoscaler, and their workload support
resources before apply; those workloads are never installed and then disabled.

Do not copy or modify upstream reconciler source.

## Networking evolution

The initial implementation uses upstream KIngress and Kourier. Preserve the
Knative networking abstraction so a provider can later return manifests for:

- `net-gateway-api`;
- a compatible GatewayClass and Gateway;
- Azure Application Gateway for Containers or an AKS-managed gateway; and
- cloud load-balancer, identity, firewall, TLS, and DNS bindings.

`ly` applies and manages provider-returned Kubernetes manifests. Changing the
gateway must not change service, revision, routing, CapacityGrant, or Fiber
semantics.

## Eventing

Knative Eventing remains optional and unchanged. Preserve exact Broker and
Trigger APIs, provider-backed Event Sources, CloudEvents HTTP delivery,
retries, dead-letter behavior, and Addressable Knative Service sinks.

InMemoryChannel remains a development backend. Durable production Broker
selection is separate work.
