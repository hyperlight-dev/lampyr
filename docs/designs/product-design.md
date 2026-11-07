---
title: Product design
description: Product and architecture decisions for Lampyr, as recorded in September 2026.
---

# Product Design: Lightweight Lampyr Services on Hyperlight

## Status

Design record, September 2026.

This document records the product and architecture decisions made while
turning the prototype in this repository into an open-source project. The
implementation has since moved on in places: a standalone `fiber-router`
replaced the custom Knative activator, and each CapacityGrant has its own
Fiberd home Pod rather than a shared node agent. The
[Lampyr Service overview](../lampyr-service.md) describes current behavior.
`docs/designs/project-plan.md` remains the broader exploratory plan.

## Product definition

Build a focused Lampyr Services platform that gives developers a serverless-like
experience on small, disposable Kubernetes clusters.

The platform:

- Runs ordinary applications in hardware-isolated Hyperlight microVMs.
- Uses Unikraft runtimes and target-specific snapshots for fast activation.
- Uses Kubernetes to admit, place, and account for blocks of capacity.
- Creates short-lived microVM instances locally without a new Pod per request.
- Hides Kubernetes, snapshot, and runtime details behind a small CLI.
- Supports familiar programming models instead of requiring a proprietary
  function API.
- Preserves lower-level escape hatches for custom OCI artifacts and runtimes.

Hyperlight, Unikraft, Knative, fiberd, OCI images, and snapshots are
implementation components. They are not the developer-facing product model.

## Beachhead scenario

The initial scenario is **small edge HTTP services**.

Typical user:

> A team has a small API or event handler that should run close to users. They
> want low idle cost, fast activation, familiar languages, straightforward
> operations, and no dependence on a proprietary functions programming model.

The initial platform must also establish the protocol foundations needed for:

- Short HTTP request-response.
- Streaming HTTP responses.
- WebSocket connections.
- Asynchronous event handlers.

All four protocol classes are v1 requirements.

## Product principles

### Keep the common path small

The intended workflow is:

```text
initialize
develop locally
deploy
observe
scale
```

Developers should not need to understand:

- CPIO archives.
- Hyperlight snapshot layouts.
- Knative activator internals.
- containerd configuration.
- Grant placement mechanics.

### Use familiar application models

The platform supports both:

1. A normal HTTP server that listens on `PORT`.
2. Optional language-specific function-handler adapters.

The normal HTTP contract is the portability baseline. Function adapters are
convenience layers and must not be the only supported model.

### Make policy immutable before activation

Request-time data may choose an admitted Service or session, but may not change:

- Image or snapshot.
- Command or arguments.
- Runtime.
- Mounts.
- Identity.
- Privileges.
- Network policy.
- Resource shape.

Executable configuration is fixed in an immutable Revision before a Grant
becomes ready.

### Scale execution to zero

When a function is idle:

- Active Hyperlight microVMs reach zero.
- Active function processes reach zero.
- Function-specific Revision Pods are not required.

Shared infrastructure may remain:

- Regional routing.
- Knative activators.
- Grant directory.
- Node runtime agents.
- Minimum ready Grant placements.
- Cached and memory-mapped snapshots.

This is function-instance scale-to-zero, not complete infrastructure
scale-to-zero.

### Optimize the dominant layer

The measured prototype reduced the Hyperlight workload path enough that
Kubernetes Pod activation became the dominant cost. The design therefore moves
activation to an already-admitted node-local runtime instead of continuing to
optimize small restore phases while retaining per-request Pod creation.

## Developer-facing experience

## CLI

The CLI is the primary public interface.

Proposed command shape:

```text
ly scaffold service
ly dev
ly build
ly push
edge snapshot
ly deploy
ly status
ly logs
ly scale
ly delete
```

The CLI manages generated Kubernetes resources but does not require developers
to edit them.

## Project manifest

A small manifest is the source of truth.

Example:

```yaml
name: thumbnail-api
runtime: nodejs
command: node server.js

protocols:
  http: true
  streaming: true
  websocket: false
  events: false

resources:
  memory: 256Mi
  cpu: 250m
  maxConcurrency: 1

routes:
  - host: thumbnails.example.com

network:
  outbound:
    - storage.example.com:443

scaling:
  regionalWarmFloor: 1
  maxFibers: 100
```

The manifest defines product intent. The CLI translates it into:

- Service.
- Revision.
- Artifact build.
- Policy.
- Regional Grant.
- Route.
- Observability configuration.

## Escape hatches

The platform provides three levels:

```text
Supported template
  -> custom build and handler hooks
  -> custom OCI rootfs, snapshot, and entrypoint
```

Users may drop down without abandoning the platform's admission, routing,
identity, and capacity model.

## Runtime and language support

All runtimes supported by Hyperlight-Unikraft may ship in v1. Current template
support tiers and protocol coverage are defined in the
[runtime compatibility matrix](../runtime-compatibility.md).

Each runtime template publishes:

- Support tier.
- Supported operating systems for local development.
- Production worker requirements.
- Conformance status.
- Protocol support.
- Recommended memory.
- Known limitations.
- Upgrade compatibility.

Support tier does not imply support for every protocol.

Initial runtime catalog includes:

- JavaScript and Node.js.
- Python.
- Go.
- Rust.
- Other Hyperlight-Unikraft runtimes as their templates become available.

## Protocol contract

## HTTP

- One active HTTP request per fiber.
- Default timeout: Knative's default of 300 seconds (`timeoutSeconds`).
- Operators may lower the timeout.
- A platform hard maximum prevents unbounded fibers.
- Synchronous failures return to the caller; the platform does not silently
  replay a request after response bytes may have been sent.

## Streaming

- One streaming response per fiber.
- The fiber remains active until the stream closes, the client disconnects, or
  the duration cap expires.
- Default maximum duration: one hour.
- Backpressure must propagate from the client through the proxy to the guest.

## WebSockets

- One WebSocket connection per fiber.
- The fiber remains active for the connection lifetime.
- Default maximum duration: one hour.
- Ping, close, cancellation, and abnormal disconnect semantics must be tested.

## Events

- At-least-once delivery.
- The source is acknowledged only after handler success.
- Retry behavior is configurable per Service.
- Retry policy includes bounded exponential backoff and a terminal dead-letter
  destination.
- Exactly-once execution is not a v1 goal.

## Cancellation

On client disconnect or cancellation:

1. Propagate cancellation to the fiber.
2. Allow a short bounded drain period.
3. Terminate the fiber if it has not exited.
4. Release its port, cgroup, identity, and ledger entry.

## Overload

The platform fails fast when no capacity is available.

- Do not create an unbounded activation queue.
- Return a clear overload response.
- Include retry guidance when useful.
- Do not automatically fall back to normal per-instance Pod activation in v1.

## Domain model

## Project

A developer-owned source project and manifest.

Key fields:

- Name.
- Runtime template.
- Build configuration.
- Local development configuration.
- Target environments.

## Service

A stable logical application endpoint.

Key fields:

- Service identity.
- Routes.
- Current traffic policy.
- Protocol contract.
- Scaling policy.
- Tenant.

## Revision

An immutable executable version of a Service.

Key fields:

- OCI rootfs digest.
- Snapshot digests by target compatibility class.
- Command and environment schema.
- Runtime version.
- Resource shape.
- Network and filesystem policy.
- Identity policy.
- Protocol settings.

Runtime and snapshot upgrades always create a new Revision. Revisions can be
canaried and rolled back.

## Policy

The admitted security and resource boundary for a Revision.

Key fields:

- Tenant.
- Service identity policy.
- CPU, memory, PID, and fiber ceilings.
- Network allowlists.
- Filesystem capabilities.
- Mounts.
- Duration limits.
- Audit class.

## Grant

A Grant is a pre-approved regional block of capacity for one immutable
Revision and one tenant.

It contains:

- Revision reference.
- Policy reference.
- Regional capacity ceiling.
- Per-placement capacity.
- Maximum fibers.
- Minimum ready placements.
- Expiry.
- Lease policy.
- Artifact references.

A request may consume capacity from a Grant but cannot reshape it.

## Grant placement

A regional Grant has one or more node placements.

Each placement:

- Runs on one selected node.
- Has a node-local runtime agent.
- Holds a verified snapshot.
- Advertises current capacity and health.
- Enforces a local fiber ceiling.

The regional Grant spans placements, while each placement remains a concrete
failure and resource boundary.

For active Services, the platform keeps at least one ready placement per
failure zone.

## Fiber

A Fiber is one Hyperlight microVM incarnation created from a Revision snapshot.

Key fields:

- Fiber ID.
- Grant ID.
- Placement ID.
- Revision ID.
- Fence.
- Endpoint.
- Lease expiry.
- Cgroup.
- Identity.
- Start time.
- Protocol class.

A Fiber is not a Kubernetes Pod.

## Fence

A fence uniquely identifies an incarnation:

```text
grant / agent epoch / sequence
```

Credentials, endpoints, leases, and audit records are scoped to the exact
fence. An agent epoch change invalidates earlier incarnations.

## Route

A platform-managed DNS and TLS endpoint mapped to a Service.

The developer sees a platform domain. Knative, Kourier, Gateway API, or another
implementation may provide the underlying routing.

## Artifact

An immutable, content-addressed object:

- Rootfs OCI image.
- Hyperlight snapshot.
- Runtime metadata.
- Compatibility metadata.
- Builder provenance metadata.

## Control-plane architecture

The control plane is hybrid.

## Cluster-local state

Kubernetes CRDs and controllers represent:

- Service bindings.
- Revision deployment state.
- Grant placements.
- Node runtime capabilities.
- Local readiness.
- Cluster routes.

The CLI hides these resources.

## External multi-region state

An external control plane manages:

- Tenant and project identity.
- Global Service and Revision records.
- Regional Grant intent.
- Artifact catalog.
- Multi-region routing policy.
- Billing and usage aggregation.
- Cross-region health.

## Grant directory

The activator uses a directory of ready Grant placements.

Each entry contains:

- Region and zone.
- Node and endpoint.
- Revision.
- Available fiber capacity.
- Observed clone latency.
- Health.
- Pressure.
- Lease expiry.

Selection considers:

- Revision match.
- Tenant and policy match.
- Region and data policy.
- Health.
- Free capacity.
- Activation latency.
- Node pressure.

## Multi-region routing

The platform-managed domain chooses among regions that:

- Satisfy residency and policy requirements.
- Have ready Grant capacity.
- Are healthy.

Selection then optimizes for latency. If a selected regional placement fails,
the activator retries once on another ready placement, then fails fast.

## Data-plane architecture

## Request path

```mermaid
accTitle: Lampyr data-plane request path
accDescr: A client request follows the platform route to the shared activator, Grant directory, node runtime agent, verified snapshot, and Hyperlight microVM before returning to the client.
flowchart LR
    Client --> Edge[Platform route]
    Edge --> Activator[Shared activator]
    Activator --> Directory[Grant directory]
    Directory --> Agent[Node runtime agent]
    Agent --> Snapshot[Verified snapshot]
    Snapshot --> Fiber[Hyperlight microVM]
    Activator --> Fiber
    Fiber --> Activator
    Activator --> Client
```

## Activation sequence

1. Route resolves the Service and immutable Revision.
2. Activator queries its cached Grant directory.
3. Activator selects a healthy placement with capacity.
4. Node agent verifies the Grant, deadline, and capacity.
5. Node agent reserves a fiber slot.
6. Node agent creates a child cgroup.
7. Node agent creates a Hyperlight microVM from the loaded snapshot.
8. Node agent applies environment, identity, network, and filesystem policy.
9. Node agent binds a local endpoint.
10. Node agent returns the Fiber ID, fence, endpoint, and lease.
11. Activator proxies the request or connection.
12. On completion, the agent releases the fiber and all associated resources.

## Snapshot model

Snapshots are target-specific.

A compatibility class includes:

- Host architecture.
- Hypervisor backend.
- Required CPU features.
- Hyperlight host ABI.
- Unikraft kernel ABI.
- Runtime and driver versions.
- Memory layout.

The same source, rootfs, command, and manifest are used everywhere, but CI
produces a content-addressed snapshot for each supported target class.

Local development:

- Uses native Hyperlight on Linux, macOS, or Windows.
- Uses the same project manifest and OCI contract.
- Uses the snapshot built for that local target class.

Production:

- Requires workers compatible with the configured runtime backend.
- Uses a snapshot built for the production target's compatibility class.

Bit-identical artifacts are required within a compatibility class, not across
incompatible hypervisor backends.

## Artifact pipeline

Production snapshots are built in dedicated CI builders.

```mermaid
accTitle: Lampyr artifact pipeline
accDescr: Source is assembled into a root filesystem, warmed, captured as a snapshot, tested, recorded with provenance metadata, and published as OCI artifacts.
flowchart LR
    Source --> Rootfs[Build rootfs]
    Rootfs --> Warm[Boot and warm runtime]
    Warm --> Snapshot[Capture snapshot]
    Snapshot --> Test[Conformance and stress tests]
    Test --> Metadata[Record digests and provenance]
    Metadata --> Registry[Publish OCI artifacts]
```

Required metadata:

- Source commit.
- Builder identity.
- Rootfs digest.
- Snapshot digest.
- Runtime versions.
- Compatibility class.
- Test results.
- Builder provenance metadata.

Nodes verify metadata and artifact digests before loading an artifact.

In the current implementation, each `fiberd` placement pulls immutable OCI
artifacts by digest into a pod-scoped `emptyDir`. The cache survives container
restarts within that Pod, but Pod replacement intentionally causes a verified
repull from the registry. This keeps correctness independent of host paths,
staging Pods, persistent volumes, and a second cache lifecycle.

A future optimization may let `fiberd` consume containerd's existing content
store and snapshot lifecycle. That would reuse already-downloaded OCI content
and containerd's garbage collection rather than introducing a separate durable
artifact cache owned by Lampyr.

## Node runtime

The target runtime is a resident process, not a new `hluk` CLI process per
request.

Fiber v2 now provides:

- The `CapacityGrant` protocol and typed miss responses.
- Grant admission, leases, fences, ledger persistence, and audit spooling.
- Host endpoint allocation and child cgroups.
- Pressure handling, revocation, and restart reconciliation.
- A backend interface for multiple execution mechanisms.

Lampyr's focused `hluk` backend implements that interface for prepared
Hyperlight-Unikraft snapshots. It starts one `hluk snapshot run` process per
Fiber; moving to an in-process reusable Hyperlight sandbox remains future work.
Readiness requires a TCP handshake and a listening socket owned by that guest
process before the clone deadline. A canceled clone kills and reaps its process
rather than publishing a ready Fiber.

## Capacity and autoscaling

## Warm floor

For an active Service, maintain at least one ready Grant placement in each
selected failure zone.

A ready placement has:

- Runtime agent available.
- Snapshot verified and loaded.
- Capacity reserved.
- Routing registration active.

No function Fiber must remain active.

## Scale-out signals

Scale Grant placements using:

- Request rate.
- Activation latency.
- Active fibers.
- Available fiber slots.
- CPU pressure.
- Memory pressure.
- Snapshot working-set pressure.
- Failure and rejection rate.

## Scale-in

Scale in only when:

- The placement has no active fibers.
- Minimum zonal capacity remains.
- Artifacts can be reloaded within the service objective.
- Leases and routing entries are drained.

## Accounting

Capacity is reserved and charged at the Grant level.

Per-fiber usage is metered for:

- Tenant allocation.
- Cost reporting.
- Pressure decisions.
- Capacity planning.
- Abuse detection.

Tracked integrals include:

- CPU time.
- Memory-time.
- Fiber duration.
- Requests and bytes.
- Snapshot page faults.
- Activation count.

## Security model

## Tenant boundary

A Grant belongs to exactly one tenant and one immutable Revision.

Cross-tenant shared Grants are not allowed in v1.

## KVM access

Production uses Kubernetes DRA or a device plugin to provide KVM access to the
node runtime without a broadly privileged Pod.

## Resource isolation

Each Fiber receives a child cgroup containing:

- `memory.max`.
- CPU controls.
- `pids.max`.
- `memory.oom.group`.

The Grant cgroup remains the aggregate hard ceiling.

## Network isolation

Network policy is layered:

1. Kubernetes policy restricts the Grant placement.
2. Hyperlight per-fiber allowlists restrict outbound destinations.
3. The node runtime allocates only approved listen ports.

## Filesystem isolation

- Snapshots contain immutable guest data only.
- Host mounts are pre-admitted in the Revision policy.
- Hyperlight filesystem capabilities are explicitly allowlisted.
- No Fiber may select an arbitrary host path at activation time.

## Identity

Snapshots contain no tokens, secrets, certificates, or inherited credentials.

After clone:

1. Agent mints a short-lived per-fiber identity derived from the Grant.
2. Identity is bound to the exact fence and lease.
3. Credential lifetime cannot exceed the Fiber lease.
4. Restart epoch invalidates earlier credentials.

## Audit

Two audit classes:

- Best effort: append locally, acknowledge activation, ship asynchronously.
- Compliance: persist remotely before acknowledging activation.

Each record includes:

- Tenant.
- Service and Revision.
- Grant and placement.
- Fiber and fence.
- Operation.
- Result.
- Resource usage.
- Timestamps.

## Failure model

## Node or placement failure

The activator retries once against another ready placement. If no capacity is
available, the request fails fast.

There is no automatic normal-Pod fallback in v1.

## Node-agent restart

Startup sequence:

1. Increment the agent epoch.
2. Load the last local ledger snapshot.
3. List runtime reality.
4. Match live fibers to valid Grants and leases.
5. Adopt valid fibers.
6. Reap orphans and stale identities.
7. Publish readiness.

Runtime reality wins over stale ledger data.

## External control-plane outage

Existing authenticated Grants continue operating:

- No new regional capacity may be supplied.
- Existing placements may create fibers within current ceilings.
- Leases and expiries remain enforced.
- Audit is spooled locally within its configured durability class.
- When capacity is exhausted, requests fail fast.

## Observability

## Required metrics

Activation:

- Request to Fiber endpoint.
- Snapshot load.
- VM restore.
- Application readiness.
- Directory lookup.
- Agent admission.

Capacity:

- Ready placements.
- Available fiber slots.
- Active fibers.
- Rejections.
- Pressure.

Reliability:

- Activation failures.
- Leaked fibers.
- Orphan cleanup.
- Agent restart recovery.
- Route and directory staleness.

Economics:

- Idle memory per Grant placement.
- Incremental memory per Fiber.
- CPU per activation.
- Cost per million requests.
- Snapshot storage and transfer.

## Tracing

A trace spans:

```text
edge route
  -> activator
  -> Grant selection
  -> node admission
  -> snapshot clone
  -> application
  -> release
```

Trace context must be injected after clone, not baked into snapshots.

## Reliability and performance gates

## Performance

For cached production-compatible artifacts:

- p95 request-to-first-byte activation below 250ms.
- Publish p50, p90, p95, and p99.
- Measure under single request and activation storms.
- Separate control-plane, agent, restore, and application phases.

## Reliability

v1 must pass:

- 10,000-request stress tests with no lost requests.
- Zero leaked Fibers after completion.
- Agent restart reconciliation.
- Node failure and alternate-placement retry.
- Network and filesystem policy tests.
- Streaming and WebSocket disconnect tests.
- Event retry and dead-letter tests.

Routine CI and publication use smoke gates rather than the full stress target:
100 sequential requests for cached latency and 300 requests at concurrency 4
for reliability, with no lost requests. Disruption checks cover
agent restart, placement failure and recovery, invalid-grant rejection, and
serving during a Knative control-plane outage. CI allows 15 seconds for placement
to return HTTP 503. Dedicated `just release-reliability-gate` and
`just release-disruption-gate` runs retain the 10,000-request stress workload and
5-second placement deadline, respectively; publication does not require these
dedicated runs.

## Economics

Compare against:

1. Normal Knative scale-to-zero.
2. A representative managed-functions baseline.

Required evidence:

- Idle cost.
- Activation CPU.
- Memory density.
- Cost per million requests.
- Artifact storage and transfer.
- Crossover point between ready Grant capacity and full scale-to-zero.

## Delivery plan

## Milestone 0: prototype

Completed or demonstrated:

- Hyperlight-Unikraft Node.js service.
- Knative scale-to-zero.
- Snapshot creation and restore.
- hostsock reliability fix.
- fiberd gateway.
- fiberd-aware custom Knative activator.
- Measured direct and node-local activation.

## Milestone 1: initial release

Deliver:

- CLI.
- Project manifest.
- Native local development.
- One supported Kubernetes production environment.
- Platform-managed domain and TLS.
- Regional Grant and placement CRDs.
- Node runtime agent.
- All available Hyperlight-Unikraft runtime templates with published tiers.
- HTTP, streaming, WebSocket, and event contracts.
- Content-addressed target-specific snapshots.

Exit criteria:

- Clean deployment from an empty supported environment.
- No manual node patching.
- Immutable and reproducible artifacts.
- p95 activation below 250ms.
- CI reliability smoke gate: 300 requests at concurrency 4 without loss.
- Dedicated reliability stress target: 10,000 requests without loss.

## Milestone 2: alpha

Deliver:

- Resident in-process Hyperlight runtime.
- Snapshot loaded once per placement.
- Child cgroups.
- Per-fiber identity and fencing.
- Policy enforcement.
- Artifact cache and reaper.
- Zonal Grant placement and demand scaling.
- Failure recovery.
- Cost and density dashboard.

## Milestone 3: multi-region preview

Deliver:

- External multi-region control plane.
- Regional Grant directory.
- Latency and health routing.
- Residency-aware placement.
- Regional artifact distribution.
- Immutable cluster replacement.
- Cross-region failure testing.

## Source and dependency strategy

The project is open source:

- Runtime.
- Control plane.
- CLI.
- Templates.
- Conformance and stress tests.
- Reference deployment.

Dependencies are pinned Go/npm modules and checksum-verified runtime release
assets.

Dependency maintenance requirements:

- Runtime releases declare their source revision and checksums.
- Dependency updates run conformance and performance gates.
- Upstream-equivalent fixes are preferred when available.
- Long-lived divergence is tracked explicitly.

## v1 non-goals

- Stateful databases.
- General-purpose Kubernetes replacement.
- Arbitrary privileged workloads.
- GPU workloads.
- Cross-tenant shared Grants.
- Unlimited long-running services.
- Exactly-once event processing.
- Windows production workers.

## Confirmed decisions

| Decision | Resolution |
|---|---|
| Product identity | Focused Lampyr Services platform |
| Beachhead scenario | Small edge HTTP services |
| Kubernetes boundary | Pre-admitted Grant capacity |
| Scale to zero | Function Fibers reach zero; shared infrastructure remains |
| Public API | CLI and small project manifest |
| Grant sharing | One tenant and immutable app Revision |
| State contract | Stateless services |
| Protocol scope | HTTP, streaming, WebSockets, and events in v1 |
| Fiber lifetime | One request or connection |
| Fiber concurrency | One active request or connection |
| Product core | Independent core; Knative is the first adapter |
| Routing | Platform-managed DNS and TLS |
| Identity | Short-lived fenced identity minted after clone |
| Local/production | Native local Hyperlight; target-compatible production backend |
| Snapshot portability | Target-specific compatibility classes |
| Runtime catalog | All available runtimes with tiered support |
| Application model | Normal HTTP servers plus optional adapters |
| Snapshot builds | Dedicated CI builders |
| Upgrade model | Immutable Revision and canary |
| Grant scope | Regional Grant spanning node placements |
| Warm floor | At least one ready placement per zone |
| Autoscaling | Demand, latency, active fibers, and pressure |
| Routing directory | Ready Grant directory |
| Failure | Retry another ready placement once, then fail fast |
| Control plane | Cluster CRDs plus external multi-region service |
| Accounting | Reserve Grant capacity and meter Fiber usage |
| Multi-region | Latency, health, capacity, and policy |
| Tenant isolation | Single tenant per Grant |
| KVM | DRA or device plugin |
| Resources | Child cgroup per Fiber |
| Network | Grant policy plus per-fiber allowlists |
| Secrets | No secrets in snapshots; identity after clone |
| Artifacts | Content-addressed, provenance-recorded, and compatibility-checked |
| Audit | Async default and synchronous compliance tier |
| Restart | Epoch, reconcile, adopt valid, reap orphaned |
| Outage | Existing Grants continue within limits |
| First deliverable | Initial CLI release |
| Performance gate | Cached p95 below 250ms |
| Reliability gate | CI: 300 requests at concurrency 4 without loss; dedicated stress target: 10,000 requests |
| Cost evidence | Full infrastructure and managed-service comparison |
| Dependency strategy | Pinned modules and verified runtime release assets |
| Open source | Runtime, control plane, CLI, templates, and tests |
