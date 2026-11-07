---
title: ly scale
---

# `ly scale`

Change the maximum concurrent Fibers for the active Revision.

```bash
build/ly scale --max-fibers COUNT [--context CONTEXT] [directory]
```

Example:

```bash
build/ly scale --max-fibers 8 services/hello-edge
```

`--max-fibers` must be between 1 and 4096. `--context` defaults to
kubeconfig `current-context`.

`ly scale` sets the Revision's `autoscaling.knative.dev/max-scale`, from which
Lampyr rewrites the Revision's CapacityGrant. It then waits, up to 4 minutes,
until fiberd-k8s-controller reports the grant Ready with the new limit and has
finished replacing its Pod. It fails at once, and restores the previous limit,
if the controller refuses the grant or the rollout gives up.

## How far one Revision scales

All of a Revision's Fibers run in one home Pod on one node. Up to 100 Fibers,
the home's memory ceiling is 1.5 GiB and its Pod's memory limit 2 GiB. Above
that the ceiling is 16 MiB per Fiber, at most 64 GiB, and the limit is the
ceiling plus 512 MiB. fiberd-k8s-controller makes the Pod's memory limit its
request too, so the scheduler places the home only on a node with that memory
free.

The home's Pod also has a 10 GiB ephemeral-storage limit, whatever its Fiber
limit, and the controller makes that its request too. So every home reserves
10 GiB of its node's allocatable ephemeral storage, used or not, and a node
fits at most its allocatable ephemeral storage divided by 10 GiB homes: a node
with about 100 GB allocatable fits about nine. A home that fits no node stays
Pending with `Insufficient ephemeral-storage`; give nodes larger disks, or add
nodes.

The home's CPU is only a request, never a limit, so a home is not throttled and
may use every CPU its node has free while it restores Fibers. Up to 100 Fibers
it requests 10m; above that a millicore per Fiber, at most two CPUs, so the
scheduler keeps room for it. If no node fits the resized home for 30 seconds,
`ly scale` fails with the scheduler's message and restores the previous
limit. On a cluster that autoscales, adding a node can take longer than that;
once the node is up, run `ly scale` again.

A `ly scale` that changes the Fiber limit replaces the home Pod. Requests in
flight on the old home are dropped, and requests that arrive before the new
home is ready get 503; clients should retry. A request that still finds the
old home asks for the new one and clones there instead.

Requests reach the home through Kourier's Envoy gateway and fiber-router. Envoy's
default circuit breakers allow about 1,024 concurrent requests to one Revision,
so beyond roughly 1,000 Fibers at once the extra requests fail with 503 even
when `--max-fibers` allows more. Kourier doesn't expose those limits yet.

A home restores at most two Fibers per CPU at once, at most 16, and refuses a
clone whose deadline can't cover the restores ahead of it: grant Pods run stock
fiberd, which does not bound restores, so Lampyr's helper does. Lampyr no
longer keeps home arguments such as `-max-restores`. Set the home's size
through `--max-fibers`, not by editing the CapacityGrant: Lampyr replaces the
grant's whole spec the next time it changes it.

A request waits up to 3 seconds for fiber-router to find its home and then up
to 3 more for the home's clone, so about 6 seconds in all, before fiber-router
answers 503. A grant already at `--max-fibers` is answered with 503 and
`Retry-After` at once, rather than sent to another home. A home's clone rate
limit, which allows up to 200 clones a second after a burst of 200 and covers
starts and attaches, is not answered that way: the request waits about its
`Retry-After`, jittered, and tries again within the clone's deadline, and gets
503 with `Retry-After` only if that deadline would pass first. A home whose
helper is restoring too many Fibers to start one more in time is waited out the
same way, 250ms to 1s at a time. On a small or busy node, a large burst of cold
requests can miss those deadlines, because restores compete for CPU.
fiber-router's `-acquire-timeout` flag raises both. The router Deployment
Lampyr installs has no args, so create the list:

```bash
kubectl -n lampyr-system patch deployment fiber-router --type json -p \
  '[{"op":"add","path":"/spec/template/spec/containers/0/args","value":["-acquire-timeout=30s"]}]'
```

If the router already has args, append instead, so the others are kept:

```bash
kubectl -n lampyr-system patch deployment fiber-router --type json -p \
  '[{"op":"add","path":"/spec/template/spec/containers/0/args/-","value":"-acquire-timeout=30s"}]'
```

`ly cluster install` resets the router Deployment, so patch it again after a
reinstall.
