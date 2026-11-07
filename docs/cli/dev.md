---
title: ly dev
---

# `ly dev`

Run a Lampyr Service locally with native Hyperlight.

```bash
build/ly dev [--port PORT] [directory]
```

Example:

```bash
build/ly dev --port 8080 services/hello-edge
```

The command builds the rootfs using the service root as its build context,
creates a native Hyperlight snapshot, and starts the lifecycle-aware router at
`http://SERVICE.default.localhost:8080`. Each request restores a fresh Fiber by default;
`execution.mode: reuse` opts into live-Fiber reuse. Handler Applications use
`.lampyr-adapter/Dockerfile.rootfs`; Standalone Applications use
root-level `Dockerfile.rootfs`.

Standalone Applications run their customer-owned `application.command` without
adapter or hook checks. Servers must honor `LAMPYR_LISTEN_HOST` and `PORT`.
`--port` sets the router's public port; each Fiber gets its own internal port.
The supplied Handler and Standalone templates support this configuration.

For Handler Applications, the handler, `lampyr.yaml`, and final `Dockerfile` are user-owned; tracked
`.lampyr-adapter/` contains the editable protocol server and build support.
Before starting the VM, `ly dev` checks required and declared hooks inside the
built image. Modified or outdated adapters remain runnable with a
best-effort warning; `ly dev` never upgrades source automatically. See
[`ly scaffold`](scaffold.md) for the hook names, ownership rules, and explicit
upgrade process.

| Option | Default | Purpose |
|---|---:|---|
| `--port` | `8080` | Local HTTP port. |
| `--metrics` | `false` | Print one line per completed request and an aggregate summary when dev exits. |

Metrics include service, method, path, status, total duration, Fiber clone
duration, Fiber ID, and whether the Fiber was created or attached for reuse:

```text
metric service=worker-api method=GET path="/invoke" status=200 duration_ms=128 clone_ms=117 fiber_id="worker-api-2" fiber=CREATE
metrics summary requests=3 errors=0 create=2 attach=1 avg_duration_ms=54 avg_clone_ms=39
```

## Workspace development

Service and workspace development both use native snapshots and the
**same lifecycle-aware router as Kubernetes**. There is no cold-run dev mode
or experimental flag.
`ly` starts and owns the router; it is not another tool to install or operate.
No Kubernetes cluster, registry, host Node.js, Rust or Go toolchain is required
by the packaged runtime. Docker/Buildx still builds the Linux rootfs. Hardware
virtualization, a matching signed Hyperlight binary, and a private non-loopback
IPv4 interface are required. The supervisor is included in the platform payload.
Platform packages include the native supervisor for Linux/amd64 or macOS/ARM64.
For a source checkout, use the
[contributor workflow](#contributor-workflow).

```bash
ly dev path/to/service
ly dev path/to/workspace --port 8080
```

List the exposed services in the workspace's
`lampyr.workspace.yaml`:

```yaml
apiVersion: lampyr.dev/v1alpha1
kind: Workspace
ingress:
  services:
    - worker-api
```

Each entry refers to `name` in the service's `lampyr.yaml`, not its folder.
The existing router generates `<service>.default.localhost` hostnames. Unknown
names, duplicate names and the replaced `ingress.service` field are errors before
builds start. No extra ingress application or proxy process is needed.

Missing or empty `ingress.services` exposes nothing, even for one service.
Dev prints a warning and keeps the session internal-only. Direct service dev
outside a workspace exposes the requested service; inside a workspace it follows
the workspace's exposure list. New workspaces list `app` by default.

Use the printed hostname URLs. If your client does not resolve `.localhost`,
connect to loopback with the matching Host header:

```bash
curl -H 'Host: worker-api.default.localhost' http://127.0.0.1:8080/invoke
```

Open `http://localhost:8080/` or `http://127.0.0.1:8080/` for an index linking
only to exposed services. Other undeclared hosts return a helpful 404 page
with those same links; they never select an internal service.

All directories under `services/` containing `lampyr.yaml` are built before
ingress starts. Service names must be unique and every binding must resolve in
this session. Missing local services are errors, never implicit calls to a
deployed service.

Handler and Standalone applications use the same snapshot lifecycle.
Their server must honor
`LAMPYR_LISTEN_HOST` in addition to `PORT`. Use `0.0.0.0` as its default host
on deployment; native dev sets `127.0.0.1`. A server that also accepts traffic
on the selected private host interface is refused, rather than exposing a
direct path around lifecycle routing. For an older Handler adapter, run
`ly scaffold service ./path/to/service --upgrade` to get the bind-host support.
Lampyr does not rewrite existing application source automatically.

Declare logical service bindings in the **service's own** `lampyr.yaml`:

```yaml
bindings:
  processor: worker-processor
```

Binding names are lowercase identifiers. Lampyr projects them as uppercase
environment-variable suffixes, so `processor` becomes
`LAMPYR_BINDING_PROCESSOR`. The application appends its HTTP path to that URL.
This is a URL, not a hostname: preserve its entire base path. Native dev uses
capability-protected endpoints on a private host interface; the deployed
prototype supplies `http://worker-processor.default.svc.cluster.local`
through the grant Pods' helper image (`helperImage`), which links
Hyperlight-Unikraft v0.21.0 (commit 34c0d64) and restores only snapshots
that commit wrote. Its fibers may connect only to the addresses their
bindings' names resolve to, on any port, and send DNS questions for those
names over UDP to the Pod's nameservers and the well-known public resolvers
(8.8.8.8, 8.8.4.4, 1.1.1.1 and 1.0.0.1). Hyperlight-Unikraft drops guest
traffic to link-local nameservers (169.254.0.0/16 and fe80::/10), so a service
with bindings does not warm on grant Pods whose nameservers are all
link-local, as with AKS LocalDNS or NodeLocal DNSCache. Lampyr Services share
the cluster's internal ingress, so any binding reaches every Knative Service
through it.
Binding names are lowercase identifiers starting with `a-z`; only the
projected environment variable suffix is uppercase. Targets are service names
in the same deployment namespace (currently `default`).

Hyperlight's restrictive network policy intentionally denies loopback even
when allowlisted. Native dev therefore uses private-interface binding
listeners with unguessable URL capabilities, not `--net` or a policy bypass.
The normal ingress is loopback-only; internal listeners reject requests
without their capability, and the ingress Host header can select only an
explicitly exposed service. This is development access control, not a production
tenant isolation or network-policy equivalence claim. Host-IP allowlisting
is coarser than per-port isolation.

Fresh-per-request is the default for HTTP and direct CloudEvents. Explicit
`execution.mode: reuse` retains revision-scoped live state until
`execution.idleTimeout`. Streams keep their Fiber until completion or client
cancellation; proxy errors release the Fiber. Native homes cap concurrency
at eight Fibers per service and return 503 when full. They do not emulate
signed Grants, lease renewal, CPU quotas, scheduling or Kubernetes resource
accounting. Memory is the configured snapshot memory, not a cgroup budget.

Snapshots are immutable for the life of a dev invocation. Restart `ly dev`
after source/configuration edits; file watching and cross-command discovery
are not implemented. Snapshots are rebuilt for the native target and must
never be copied to a different architecture or hypervisor for deployment.

### Executable multi-service example

`samples/local-worker-workspace/` contains dependency-free Node.js
Standalone Applications. Its API has several
HTTP handlers, calls a fresh processor, emits a CloudEvent to that processor,
and calls a service explicitly configured for reuse:

```bash
ly dev samples/local-worker-workspace
curl http://worker-api.default.localhost:8080/invoke
curl http://worker-api.default.localhost:8080/emit
curl http://worker-api.default.localhost:8080/reuse
```

Only `worker-api` is exposed in this example. Processor and reused remain
internal and receive calls through bindings. You can expose any workspace
service by adding its name to `ingress.services`. For example:

```yaml
ingress:
  services: [worker-api, worker-processor, worker-reused]
```

After restarting `ly dev`, you can call the added services directly:

```bash
curl http://worker-processor.default.localhost:8080/count
curl http://worker-reused.default.localhost:8080/count
```

The API is optional if you do not need its `/invoke` and `/emit` application
logic; you can expose processor and reused without it.

The example responses include `fiberId`. Fresh requests receive different IDs.
The reused service keeps one ID while its Fiber is live and receives a new ID
after the idle timeout releases it.

`/emit` converts HTTP ingress into an outgoing binary CloudEvent and waits
for its acknowledgement. This is **synchronous direct event delivery**, not
a queued, durable, retrying Event Source. Fresh state is not durable state;
use an external state store for durable processing. This fixture intentionally
avoids npm downloads; it does not demonstrate that the default Handler
scaffold's dependency installation works on a restricted network.

The same source and `bindings` configuration are retained in build artifacts.
The pinned Hyperlight-Unikraft v0.21.0 release includes the restricted-DNS fix
from [hyperlight-dev/hyperlight-unikraft#248](https://github.com/hyperlight-dev/hyperlight-unikraft/pull/248).
Node's connection-style lookup (`AI_ADDRCONFIG`) works without a custom
runtime build. UDP connection setup does not grant permission to send:
each payload still passes the destination policy check. TCP restrictions
remain unchanged.

Existing clusters keep their installed helper image (`helperImage`). Before
deployment, reinstall with one that links v0.21.0 and rebuild provider and
service snapshots. v0.21.0 cannot restore snapshots from v0.20.1 or earlier.
See [runtime upgrades](deploy.md#runtime-upgrades).
Do not use unrestricted networking to work around an old runtime.

The shared behavioral runner accepts either native HTTP or deployed HTTPS:

```bash
python3 tests/contracts/test_worker_workspace.py --url http://127.0.0.1:8080
python3 tests/contracts/test_worker_workspace.py \
  --url https://YOUR_CLUSTER_INGRESS --host worker-api.default.YOUR_DOMAIN
```

Only for a test cluster with a self-signed certificate, add `--insecure`.
The runner checks fresh state, bindings, event content, explicit reuse and
idle reset, errors, streaming, and client cancellation. The unchanged runner
passed the complete graph on both targets with the corrected development
runtime and fresh target-specific snapshots, including the deployed bound
calls and emitted CloudEvent. The deployed run passed without request retries.
Early requests during concurrent cluster builds previously encountered the
existing three-second clone deadline; resource/performance equivalence is
not established by the successful application contract.

The focused regression separates ordinary DNS from connection-style lookup
and keeps an undeclared name as a negative control:

```bash
python3 tests/contracts/test_restricted_dns.py \
  --hluk PATH_TO_VERIFIED_HLUK --rootfs PATH_TO_ROOTFS \
  --allowed github.com --tcp-port 443 --loopback-controls
```

Use `--snapshot PATH` instead of `--rootfs` to check a restored runtime, and
`--resolv-conf PATH` for a cluster-local nameserver configuration.
`--loopback-controls` creates real host TCP/UDP loopback listeners and checks
that guest payloads cannot reach them. These receiver-backed controls passed
on native cold/restored guests; Linux host socket regressions also passed.
KVM cold/restored checks covered declared public, A-record and ExternalName
resolution plus denial of an undeclared sibling sharing the same ingress IP.

The test checks connection-style DNS and denied destinations with the released
runtime. Create fresh snapshots when the kernel/host-contract key changes.
This is not a full resource, security or durable-delivery conformance suite.

## Contributor workflow

From this Lampyr checkout, build the CLI and its companion, then start the
workspace. This needs Rust, Go, Just, Docker/Buildx and native virtualization.
No Hyperlight source checkout, Cargo overrides or runtime descriptor is needed:

```bash
just build-ly
build/ly dev samples/local-worker-workspace --port 8080
```

Lampyr downloads and verifies the released v0.21.0 executable and matching
kernel automatically. The CLI uses the released v0.21.0 Cargo library.
`ly dev` creates fresh native snapshots on each run. Old custom-source
deployment artifacts must be rebuilt with `ly build`.

In another terminal, run the requests or the behavioral runner above.
