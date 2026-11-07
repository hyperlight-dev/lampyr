# Lampyr tour: a sandbox per request

> **Preview code.** The tour runs on Lampyr's main branch, built from source. Expect rough edges.

Some workloads need every request to run in its own sandbox: user-submitted code, functions for many tenants. This
tour runs a small snippet runner on one Kind node in two ways, side by side: as a plain Kubernetes Deployment, one
container (pod) per sandbox, and on Lampyr, one [Hyperlight](https://github.com/hyperlight-dev/hyperlight) micro-VM
per request. Then you build a two-service snippet playground with Lampyr, locally and on the cluster.

If your requests can safely share one process, one warm container serves them faster than either; this tour is about
the case where they can't. On plain Kubernetes, without a pool of warm pods, a sandbox per request means a new pod per
request. The baseline doesn't do that; it measures what each new pod costs to start and to keep.

## Words you'll see

| Word | Meaning |
|---|---|
| Fiber | A Hyperlight micro-VM that serves one request (by default; see reuse mode in [`ly deploy`](../../docs/cli/deploy.md)). It is restored from a snapshot of the booted runtime that `ly build` took; the app's own code starts after the restore. |
| home pod | The pod that runs all of one Revision's Fibers. Lampyr asks for a `fiberd.io` CapacityGrant, the Kubernetes object that reserves capacity for a Revision's Fibers (how many, and how much memory), and fiberd-k8s-controller creates the pod from it. It carries the label `lampyr.dev/home=true`; the tour's scripts find it by that label and `serving.knative.dev/service`, not by name. |
| fiberd | The daemon in the home pod that starts and stops Fibers. It restores each one through Lampyr's helper, `lampyr-hluk-helper`, which runs beside it. |
| Revision | Knative's immutable version of a Service's code and settings; each `ly deploy` of changed code makes a new one. |
| fiber-router | Lampyr's router: it takes each request from Kourier and gets it a Fiber in the Revision's home pod. |
| Kourier | The ingress Knative uses here; it routes each request to fiber-router. |

## What you need

- Linux on x86-64 (amd64) with KVM: `ly build` makes snapshots locally only on that architecture. `/dev/kvm` must
  exist and be readable and writable by your user. Nested virtualization works, for example an Azure Dv5 or Ev5 VM;
  the numbers below come from one.
- 8 vCPUs. We didn't test a smaller node. On the old home pod, which was limited to 2 CPUs, 1,000 micro-VMs took
  about 80 s to be running; home pods now have no CPU limit. A request can wait for one up to about twice
  fiber-router's 120 s `-acquire-timeout`, about 240 s: once for its home, then once for its Fiber.
- About 20 GiB of free memory: at 1,000 Fibers the runner's home pod may use up to 16 MiB per Fiber plus 512 MiB,
  about 16.1 GiB (here it reached about 10 GiB RSS, 11 GiB working set). About 20 GiB of free disk.
- Docker with Buildx, and your user in the `docker` group; `cpio`, which `just kind::build` uses. The active Buildx
  builder must export OCI layouts, as a `docker-container` one does (`docker buildx create --use`), for
  `just upstream::cluster-inputs`.
- `just` (we used 1.46), Rust via rustup (the checkout pins 1.91.0), Go 1.26 or later, `kubectl`, `helm`, `curl`,
  `jq`, `python3` and [`oha`](https://github.com/hatoo/oha). `curl` and `oha` must resolve `*.localhost` to 127.0.0.1;
  curl 8.5 and oha 1.9 do. If yours don't, add the tour's hostnames (`runner.default.localhost`,
  `playground.default.localhost`) to `/etc/hosts` for 127.0.0.1.
- Free local ports 8080 (`ly dev`), 19090 and 13000 (`PROMETHEUS_PORT` and `GRAFANA_PORT` change the last two).
- For `ly dev` (section 4): a private, non-loopback IPv4 interface on the host. Hyperlight's network allowlist
  forbids loopback, so local bindings listen on that interface.

## Before you start: the runner runs any code it is sent

The runner executes every snippet it receives. On Lampyr the micro-VM is the runner's sandbox, and a runner micro-VM
has no outbound network destination. In the container baseline the container is the sandbox: it runs as an
unprivileged user without a service account token, and a NetworkPolicy admits only the `prober` (and the kubelet's
probes) and lets the replicas send nothing. A snippet in a baseline container can still start processes
(`child_process`); Kind sets no PID limit, so only the container's 512 MiB memory limit and the node's `pid_max` bound
how many it starts. Kind publishes its ingress on 127.0.0.1 only, and `setup.sh` forwards Prometheus and Grafana to
127.0.0.1 too, but other users of a shared host can still reach them there. Run the tour on a machine and cluster you
don't share, and don't expose the runner beyond it.

## 1. Build Lampyr

```bash
git clone https://github.com/hyperlight-dev/lampyr && cd lampyr   # the lampyr checkout; run everything from here
just build-ly
just kind::build
just cli::runtime-images
just upstream::cluster-inputs build/cluster-inputs
LAMPYR_CLUSTER_INPUTS=$PWD/build/cluster-inputs just use-cluster-inputs 5002
export PATH="$PWD/build:$PATH"
ly doctor
```

`kind::build` builds the Kind provider; `cli::runtime-images` builds the serving controller and fiber-router images.
A checkout has no release's controller inputs, so `upstream::cluster-inputs` builds them from the commits
`upstream/pins.env` pins: fiberd-k8s-controller, which creates the home pods, and the stock fiberd and
`lampyr-hluk-helper` images the home pods run. `use-cluster-inputs 5002` serves those images from a registry on
`127.0.0.1:5002`, the port the Kind provider shares its local registry on, makes them this checkout's defaults for
`ly cluster install`, and puts the tools `ly build` and `ly deploy` use for fiberd templates in `build/`. See
[the Kind cluster guide](../../docs/cluster-kind.md) for more.

## 2. Install Lampyr on Kind and set up the tour

```bash
export NAME=lampyr                               # your installation's name; the tour's scripts read it
ly cluster install --provider kind --name $NAME --allow-docker
demos/lampyr-tour/bin/setup.sh
```

`ly cluster install` asks you to approve the Kind provider's Kubernetes manifests; answer `y`, or pass
`--approve-provider-manifests`. `setup.sh` took about 3 minutes here. If you run `ly cluster install` again later, run
`setup.sh` again too: reinstalling resets fiber-router's arguments.

`setup.sh`:
- builds and deploys the runner, then the playground, with `ly deploy` to the installation's local registry.
  `demos/lampyr-tour/app/lampyr.workspace.yaml` exposes both, so section 3 can call the runner directly;
- deletes Revisions that serve no traffic, since each keeps a home pod;
- raises the runner's Fiber limit (`fibers.max`, per Revision) from 100 to 1,000 (`BURST`), and sets fiber-router's
  `-acquire-timeout` to 120 s instead of its default 3 s (added to fiber-router's other arguments; it applies to every
  Service on the installation), so a request can wait up to about twice that, about 240 s, for its home and then its
  Fiber, and a burst of 1,000 new requests on 8 vCPUs can all start;
- installs Prometheus and Grafana (kube-prometheus-stack) into `monitoring`, and adds the `prometheus-community`
  chart repository to your Helm configuration;
- creates the container baseline in `baseline`: the same runner as a Deployment at 0 replicas (a re-run keeps a
  running baseline's replica count), the NetworkPolicy, and the `prober`;
- forwards Prometheus to `127.0.0.1:19090` and Grafana to `127.0.0.1:13000` (`PROMETHEUS_PORT`, `GRAFANA_PORT`),
  then prints the registry and the runner's and the playground's URLs.

Paste the export lines `setup.sh` printed, then:

```bash
export PATH=$PWD/demos/lampyr-tour/bin:$PATH     # cold-start, compare, stats, prune-revisions, wait-for
export CTX=kind-$NAME
curl -k $RUNNER/run -d 'return 6 * 7'                 # {"result":42,"vm":"..."}
```

## 3. Kubernetes containers vs Lampyr

Run the steps in order, in the same terminal.

```bash
# a) One new sandbox, with none running (Lampyr's home pod is already up)
RUNS=3 cold-start --containers 1
RUNS=3 cold-start --fibers $RUNNER/ 1

# b) Ask for 1,000 container sandboxes, then look at the node (maxPods stays at its default, 110)
kubectl --context $CTX -n baseline scale deploy runner --replicas=1000
compare containers                               # rerun until "running" stops growing (87 here)
kubectl --context $CTX -n baseline scale deploy runner --replicas=0
kubectl --context $CTX -n baseline wait --for=delete pod -l app=runner --timeout=180s || true   # none left is fine

# c) Ask for 1,000 Lampyr sandboxes, each held for two minutes
ulimit -n 4096                                   # oha opens 1,000 connections; fails if your hard limit is lower
burst=$(mktemp)
oha -n 1000 -c 1000 --insecure --no-tui "$RUNNER/work?ms=120000" > "$burst" &
stats --watch                                    # Ctrl-C once "busy Fibers" stops growing (1,000, or a few dozen short of it)
compare fibers                                   # within the two minutes
wait; grep -E 'Success rate|responses' "$burst"    # with stragglers, a little over twice the hold: about 4 minutes
```

Not every burst reaches 1,000 busy micro-VMs at once. These bursts ran on the old home pod (see "What we measured"):

- On the runner, with requests held 120 s and the ramp timed, bursts reached 1,000 after 24.8 s and 29 s, and one
  peaked at 971. Runner bursts held 90 s or less also reached 1,000, or peaked just short.
- In every burst we ran to the end, all 1,000 responses were 200. The stragglers get a micro-VM only after earlier
  requests finish, so the slowest took about 2.2 times the hold.
- On older platform images (main at `48124d6`), three bursts held 90 s against an earlier, similar Node.js service
  reached 1,000 once and peaked at 979 and 981.
- We haven't confirmed why they wait. Kourier's Envoy circuit breakers, which allow about 1,024 concurrent requests
  per Revision (see [`ly scale`](../../docs/cli/scale.md)), are one possible cause, though these bursts stalled below
  that number.
- On a busy single-node cluster, the burst can briefly cost the control plane its leader lease, so the scheduler or
  controller-manager restarts.

`stats --watch`, or the **Busy Fibers** panel of Grafana's **Lampyr** dashboard, shows the busy count rising; time it
from when you started `oha`.

### What we measured

On an Azure E8s v5 VM (8 vCPUs, 64 GiB) with nested KVM, Kind, and the default Lampyr installation plus the settings
above, measured as described in "How this was measured". Lampyr's platform images (the home pod's and fiber-router's)
were built from main at `fd97b5c` (the tour adds no platform code). Every Lampyr number here was measured on the old
home pod, which ran Lampyr's own fiberd, before home pods became fiberd-k8s-controller grant Pods running stock fiberd
and `lampyr-hluk-helper`; they may differ on the new home. The services' images were built from this tour. The VM also
ran other Kind clusters and services; both sides shared that load.

| | Kubernetes containers | Lampyr |
|---|---|---|
| Isolation | a process on the node's shared kernel, locked down (see below) | its own micro-VM per request, isolated by the hypervisor (KVM) |
| One new sandbox (Lampyr's home pod already up) | 0.92 s | 100 ms |
| 1,000 sandboxes on one node | 87 Running, 913 Pending ("Too many pods") | 1,000 running in one home pod after 25 to 29 s (one burst peaked at 971); 0 Pending |
| Memory per running sandbox (RSS) | 11.0 MiB, a lower bound | about 10 MiB (9.7 to 10.2), not counting kernel memory |
| Memory with nothing running | 0 (scaled to zero) | about 10 MiB before any burst, 71 MiB after one burst (one run) |

- **Isolation.** The containers run non-root, with seccomp `RuntimeDefault`, all capabilities dropped and a
  read-only root file system; state persists across requests until the pod is replaced. On Lampyr, each micro-VM's
  VMM process and fiberd run in the home pod on the shared kernel, and the Fibers share the home's network namespace
  and memory ceiling.
- **One new sandbox.** Lampyr's home pod is already up, and the container time includes the `kubectl scale` call that
  starts it. Containers: 0.82, 0.92 and 0.92 s, median 0.92 s; other runs' medians at these settings: 0.9 to 1.0 s.
  Lampyr: 100, 120 and 100 ms, median 100 ms (10 ms resolution); other runs' medians at these settings: 100 to 150 ms,
  and 260 ms under heavy host load.
- **1,000 sandboxes.** The containers stopped at the kubelet's default maxPods of 110, which we didn't raise, with 23
  pods already on the node (Kubernetes' and Kind's own, Lampyr's platform and home pods, the tour's monitoring stack
  and its prober). Raised, one Kind node still holds at most about 250 pods with the default /24 pod CIDR, so 1,000
  don't fit on one node. On Lampyr we raised only the runner's `fibers.max`, from 100 to 1,000; the bursts are
  described above.
- **Memory per running sandbox.** Containers: about 960 MiB for 87 containers (still rising slowly). Lampyr, at
  default settings: 9,707 MiB for the home pod with 1,000 micro-VMs in one burst (2026-11-06), and 10,246 MiB in a
  later burst (2026-11-07), read once the count had been at 1,000 for about 40 s; other runs, read from the home's
  cgroup `memory.stat`, fall between the two. On Lampyr, most of each micro-VM's memory is the snapshot's pages,
  shared by all of them and counted once in page cache; kernel memory such as KVM's page tables isn't in RSS either.
- **Memory with nothing running.** The runner's home pod read about 10 MiB before any burst (2026-11-06). After a
  1,000-request burst (held 10 s) it settled at about 71 MiB within about 5 minutes, and stayed there for the two
  minutes we watched (2026-11-06, sampled every minute).

Lampyr also has a fixed cost the baseline doesn't: its fiber-router and controllers, Knative Serving, Kourier and the
KVM device plugin used 234 MiB of RSS with nothing running (`stats`, 2026-11-06), not counting the local registry
container, which runs outside the cluster. That was measured on the old home pod, before home pods became
fiberd-k8s-controller grant Pods. The platform now also runs fiberd-k8s-controller and cert-manager, with its
csi-driver, which `stats` counts too, so the number will change.

### How this was measured

- One new sandbox: each side is timed from just before the start to the first HTTP answer, on the same clock. Kind
  shares the host kernel, so the `prober` pod and the host read the same `/proc/uptime`. The container side starts
  from just before the scale request, once the prober is connected, and the prober reaches the new replica directly by
  pod IP, inside the cluster. The Lampyr side starts from just before the request, which goes from the host through
  Kourier (HTTPS) and fiber-router to the home pod. `cold-start` runs each side once, untimed, before measuring, so
  the container image and the runner's snapshot are cached on the node; it reports the median of 3 runs. On the Lampyr
  side it retries a run that gets no answer, a 429 or a 5xx, at most 3 attempts in all, and fails on any other status;
  none of the runs reported here needed a retry. Each time is the difference of two `/proc/uptime` reads, which have
  10 ms steps, so it is good to about ±10 ms; the prober tries each new replica every 50 ms, so a container's time can
  also be up to about 50 ms late, more if a connection attempt hangs (each waits at most 1 s).
- The headline numbers come from one run each: one new sandbox (both sides) from 2026-11-07; the 24.8 s burst, the 87
  containers and their memory from 2026-11-06; the micro-VMs' memory range from one run on each of those days.
- Lampyr restores each micro-VM from a snapshot taken when the service was built, inside a home pod that is already
  running. That snapshot and that pod are Lampyr's head start. The container baseline is a plain Deployment; we
  didn't measure Knative's own scale-from-zero.
- 1,000 sandboxes: the time is from the moment `oha` starts until fiber-router reports 1,000 busy micro-VMs, from
  Prometheus or from polling fiber-router's metrics directly (the gauge is scraped every 5 s, so that is an upper
  bound). The node's 8 vCPUs were 76 to 100% busy in the two timed bursts we sampled (7.7 of 8 cores in the one we
  profiled), mostly in the micro-VM processes restoring and starting the runner.
- Memory is RSS from the kubelet's cAdvisor samples; on both sides it leaves out shared page cache, such as the
  snapshot's pages. Per container: the running replicas' RSS divided by their number. That is a lower bound, since
  it leaves out each pod's pause container and containerd shim. Per micro-VM: the whole home pod's RSS, fiberd
  included, divided by the busy micro-VMs. The containers are idle; the micro-VMs are each holding a request. Idle
  memory is the runner's home pod (all its containers) with no micro-VM running, read with `compare fibers`.
- CPU: the runner's `resources.cpu` in `lampyr.yaml` is not enforced per Fiber; all Fibers share the home pod's CPU.

`stats --watch` shows the same picture over time. Grafana's **Lampyr** dashboard has it as graphs. fiber-router
serves these metrics on port 9090:

| Metric | Meaning |
|---|---|
| `lampyr_router_fibers_busy` | Fibers serving a request right now, by Revision |
| `lampyr_router_fiber_acquire_seconds` | time to clone or attach a Fiber; `kind` is `error` or `canceled` when none was acquired |
| `lampyr_router_request_duration_seconds` | request duration by status code; 499 means the client went away |
| `lampyr_router_shed_total` | requests the home refused, with a 503 and `Retry-After`: at `fibers.max`, under memory or pids pressure, or still refusing when the acquire timeout would pass, for its clone rate limit or for restoring too many Fibers to start one in time |
| `lampyr_router_clone_retries_total` | clones fiber-router tried again within the acquire timeout, by `reason`: `rate_limit` and `restores_busy` after waiting the home out, `home_unreachable` after a Clone reached no home |

The two histograms' largest finite bucket is 30 s, so `stats` and the dashboard show anything longer, such as the
burst's two-minute requests, as 30 s.

## 4. Build an app with Lampyr

The playground takes a snippet and asks the runner to run it through a binding. The runner has no sandbox of its own:
each run is a fresh micro-VM (by default; reuse mode keeps Fibers between requests).

```text
you ──▶ playground ──(binding: runner)──▶ runner
```

`ly dev` stays in the foreground. In a second terminal, from the `lampyr` checkout:

```bash
build/ly dev demos/lampyr-tour/app --port 8080
```

Then, in the first terminal:

```bash
P=http://playground.default.localhost:8080
wait-for $P/
curl $P/run -d 'return 6 * 7'
curl $P/run -d 'globalThis.runs = (globalThis.runs || 0) + 1; fs.writeFileSync("/tmp/note", "hi"); return runs'
curl $P/run -d 'globalThis.runs = (globalThis.runs || 0) + 1; fs.writeFileSync("/tmp/note", "hi"); return runs'
curl $P/run -d 'return fs.existsSync("/tmp/note")'
```

Both runs of the second snippet answer 1, and the last one finds no file. Every run starts from the same clean
snapshot. The playground's `lampyr.yaml` declares the binding (`bindings: {runner: runner}`), and the playground reads
the URL from `LAMPYR_BINDING_RUNNER`. `wait-for` waits until a URL answers and prints how long after `ly dev` started
that was. In our runs, `ly dev` built, snapshotted and served both services 12.9 to 17.1 s after it started, with
Docker's build cache warm.

To change the code: the playground refuses snippets over 100 bytes, so this one is refused:

```bash
curl $P/run -d 'const words = ["user", "code", "runs", "in", "its", "own", "micro-VM"]; return words.map((w) => w.toUpperCase()).join(" ")'
# {"error":"snippet too long (max 100 bytes)"}; after the change: {"result":"USER CODE RUNS IN ITS OWN MICRO-VM",...}
```

Raise `MAX_SNIPPET_BYTES` in `demos/lampyr-tour/app/services/playground/server.js` to 4096:

```bash
sed -i 's/^const MAX_SNIPPET_BYTES = 100;/const MAX_SNIPPET_BYTES = 4096;/' demos/lampyr-tour/app/services/playground/server.js
```

Stop `ly dev` with Ctrl-C and start it again in the second terminal, then `wait-for $P/` and run the snippet again.
To deploy the change: if `ly dev` is still running, stop it first (`ly build` and `ly dev` both write under the
service's `.lampyr/`), then, in the first terminal:

```bash
ly build demos/lampyr-tour/app/services/playground
ly deploy --context $CTX --image $REGISTRY/playground:tour demos/lampyr-tour/app/services/playground
curl -k $PLAYGROUND/run -d 'const words = ["user", "code", "runs", "in", "its", "own", "micro-VM"]; return words.map((w) => w.toUpperCase()).join(" ")'
prune-revisions                                  # delete the old Revision and its home pod
```

A snippet that never ends, such as `for (;;) {}`, holds its micro-VM until the client gives up or the Revision's request
timeout (Knative's default, 300 s) passes. Through the playground, which gives up after 30 s, on the old home pod,
fiber-router's busy count went from 2 (the playground's micro-VM and the runner's) back to 0 within about 12 s of the
playground's 504. In the container baseline it blocks that container, and every later request to it, until something
restarts it: the readiness probe only marks the pod NotReady (behind a Service, that takes it out of the endpoints), and
there is no liveness probe. A snippet's stray asynchronous errors, such as a rejected promise nobody awaits, are logged
and the runner keeps serving.

## Limits

- **Throughput is not what this shows.** When requests can share a process, one warm container serves them with far
  less overhead per request than a micro-VM per request.
- **One home pod per Revision, on one node.** A Service's Fibers are capped by `fibers.max` and by the home's memory
  ceiling, which Lampyr sizes from `fibers.max` (see [`ly scale`](../../docs/cli/scale.md)). The scheduler sees only
  the home pod, not the Fibers' declared resources.
- **Fibers share their home.** They share its CPU, network namespace and memory ceiling, so a heavy snippet slows the
  others, and memory or pids pressure makes the home shed new requests.
- **Starting many at once.** Each home's helper restores at most two Fibers per CPU the helper may use at a time, and at
  most 16. It queues the rest and refuses a request it can't start before its deadline; fiber-router then waits a moment
  and tries again until its acquire timeout passes, so a burst of 1,000 new requests on 8 vCPUs takes tens of seconds
  before all 1,000 are running.
- **Idle.** Each Revision keeps its home pod, which holds memory even with no micro-VM running, and the platform is
  Lampyr's fixed cost.
- **Bindings are discovery, not access control.** A binding adds its hostname to the micro-VM's network allowlist.
  Other Services' hostnames don't resolve in the micro-VM, but every Knative Service's internal hostname resolves to
  the same Kourier address. On our cluster, a micro-VM with one binding reached another Service by connecting to that
  address, or to its binding's hostname, and sending the other Service's Host header; a connection to the same address
  on port 443 got past the allowlist and was refused by the address itself. The runner declares no binding, so its
  micro-VMs' allowlist holds only loopback, which Hyperlight denies.
- **Other container sandboxes.** The baseline uses runc. Kata Containers or gVisor give containers VM or
  user-space-kernel isolation, and a pool of warm pods hides pod start time; we measured neither.
- **AKS.** Lampyr also installs on AKS, where the Azure provider creates the cluster (see
  [the AKS walkthrough](../../docs/cluster-aks.md)). This tour's scripts assume Kind.

## Clean up

If `ly dev` is still running, stop it with Ctrl-C. Then, in the first terminal:

```bash
git checkout demos/lampyr-tour/app                   # if you changed MAX_SNIPPET_BYTES
rm -rf demos/lampyr-tour/app/.lampyr demos/lampyr-tour/app/services/*/.lampyr   # what ly build and ly dev wrote
rm -f "$burst"
demos/lampyr-tour/bin/setup.sh --stop                # the port-forwards
helm repo remove prometheus-community                # if you don't use it otherwise
ly cluster uninstall --provider kind --name $NAME --delete-cluster --allow-docker
docker rm -f lampyr-cluster-inputs-5002              # the controller inputs' registry
rm -f build/controller-inputs.json build/fiberd-k8s-controller.yaml   # this checkout's install defaults
```

Deleting the cluster removes the app, the baseline and the monitoring stack. Left behind: `infra/kind/<name>/` in the
`lampyr` checkout, `~/.lampyr/installations/kind/<name>/`, `~/.local/state/lampyr-tour-kind-<name>/`, the Helm
repository entry if you kept it, any `/etc/hosts` entries you added, the build outputs (`build/`, `cli/target/`,
`providers/kind/build/`), the preview runtime images `just cli::runtime-images` built, the images `ly` built, and the
base images they pulled.
