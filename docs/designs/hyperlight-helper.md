---
title: Hyperlight helper for Fiberd
description: Run Lampyr snapshots under Fiberd's stock hyperlight backend through a Lampyr-owned helper.
---

# Hyperlight helper for Fiberd

A Lampyr home is a grant Pod of fiberd-k8s-controller running Fiberd's release
image (`grantImage` in `lampyr-install`). Fiberd's stock hyperlight backend
drives a helper process over a line protocol
([PROTOCOL.md](https://github.com/Azure/fiberd/blob/main/hack/hyperlight/PROTOCOL.md))
on fd 3. Lampyr ships that helper, `lampyr-hluk-helper`, as the image
`ghcr.io/hyperlight-dev/lampyr/hluk-helper` (`helperImage`), and `ly deploy`
seals each Revision's snapshot into a Fiberd template the helper restores
([ADR 0006](../adr/0006-fiberd-hyperlight-helper.md)).

Grant Pods no longer run Lampyr's own Fiberd build (`cmd/fiberd-home`) or its
Go backend (`internal/fiberdintegration/hluk`), which pulled the snapshot
artifact itself and started one `hluk snapshot run` process per Fiber; both
have been removed, along with the grant issuer that signed their grants.

## Inside a home Pod

One grant has one home Pod. Lampyr's serving controller names the helper
image in the grant's `helperImage`, and fiberd-k8s-controller puts the helper
there: an init container copies `/fiberd-helper/helper` from the image into a
shared emptyDir. Fiberd, since commit
[549f616](https://github.com/Azure/fiberd/commit/549f616) (see its
[PROTOCOL.md](https://github.com/Azure/fiberd/blob/549f616/hack/hyperlight/PROTOCOL.md)),
pulls the grant's template, unseals it, unpacks the hluk snapshot read-only,
and starts the helper with `--template-dir <dir>` once the helper advertises
the `template-dir` capability (below). Only the helper's tests run it by hand
against a template directory.

The helper loads the snapshot once and answers `READY` with the four facts its
snapshots depend on (the helper with its hyperlight-unikraft commit, the
hyperlight_host crate, the hypervisor and the CPU vendor). Fiberd records them
as the platform of every park, so a park only resumes under a matching helper.
A template built by another hyperlight-unikraft commit or on another CPU vendor
is refused at warm.

`--version` prints the same four facts on its first line and `template-dir` on
a second. That line is how Fiberd tells a helper that restores the template it
unpacks: it passes `--template-dir` only to a helper that prints it, and reads
the facts from the first line alone. `ly build` runs the
helper's `--version` where the snapshot is made, refuses a helper without the
line, and records the first line in the artifact's metadata as
`runtime.fiberdHelper`, which zygotectl seals with. Being metadata, the facts
are part of the Revision ID, so a build for another helper is another Revision. The helper takes its flags as Fiberd spells them: one or
two dashes, and a value after `=` or as the next argument.

Each `CLONE` restores a sandbox from the shared snapshot on a thread of its
own, starts the app with `PORT` set, and answers `CLONED` once the app listens
and the Fiber's endpoint is bound. `PARK` writes the sandbox to the directory
Fiberd names, `RESUME` restores one from a park, and `KILL` ends it. Every end
sends `EXITED`, after the sandbox's memory is freed. `KILL` interrupts the
guest until the Fiber has ended, so it also ends a guest that spins as it
serves, or as a `CLONE` or `RESUME` restores it, before it answers.

## Reaching a Fiber

Fiberd names a unix socket as each Fiber's endpoint, and a hyperlight-unikraft
guest serves on a host TCP socket, so the helper bridges the socket to the
app's port. Fiberd's host relays a TCP port to each unix endpoint, because the
hyperlight backend serves unix only, so fiber-router keeps dialing Fibers over
TCP. Grant Pods run Fiberd's image, which relays on 30000-32767, and the
grant Pods' NetworkPolicy (`lampyr-grant-homes`) opens only that range and
Fiberd's API to fiber-router.

The app's own port comes from the helper's range, 10000-19999 by default,
outside the kernel's ephemeral ports and Fiberd's endpoint range. A port
belongs to one Fiber at a time, and a Fiber serves only once every listener on
its port is the helper's own; each connection checks those listeners are still
the guest's. Guests get no network egress, so a Fiber cannot reach another's
port. Apps that bind `0.0.0.0` are still reachable on the Pod's address until
hyperlight-unikraft can bind guest listeners to loopback
([hyperlight-unikraft#255](https://github.com/hyperlight-dev/hyperlight-unikraft/issues/255));
the Pod's NetworkPolicy keeps that range closed meanwhile.

## Who a Fiber is

Before its endpoint exists, a Fiber's fence and generation are in
`/run/fiberd/incarnation` and a `CLONE` payload in `/run/fiberd/payload`, a
read-only mount. hyperlight-unikraft reseeds the guest's random generators from
host entropy on every restore, so every clone draws different bytes. Fiberd's
seed is checked and dropped; it never reaches the guest, the log or a park.

## Latency

In fresh mode, Lampyr's default, every request clones a Fiber, so clone to
serving is what a request waits for. With the release build that time is about
40-60 ms for Rust, 120-190 ms for Node.js and 330-440 ms for Python on a busy
host, no slower than the old backend's `hluk snapshot run`. Python's is its
app starting after the restore; a snapshot warmup that does more of that work
shortens it.

A clone without a deadline of its own gets Fiberd's fixed 500 ms, which Python
clones miss once several start at once, and the clone then fails rather than
waits. [Fiberd#97](https://github.com/Azure/fiberd/pull/97) sizes the default
from a probe clone instead.

## Bounded restores

Fiberd's hyperlight backend does not cap concurrent restores (only its gVisor
backend has `MaxRestores`), and a restore is CPU-bound until its app listens.
An unbounded burst of cold requests starves the grant Pod: every restore
slows, all of them miss their deadlines, and the Fibers already serving get
no CPU. Lampyr's own Fiberd home bounded restores for this; grant Pods
run stock Fiberd, so the helper bounds them instead, where it can tell a
restore from the rest of a Fiber's life.

At most `--max-restores` Fibers restore at once, by default two per CPU the
helper may use, at most 16. A grant Pod requests CPU without a limit, so that
is two per node CPU; a CPU quota on its cgroup would lower it. A `CLONE` or
`RESUME` waits for a slot in arrival order, and its deadline counts from its
arrival, the wait included. From the restores that finished, the helper
keeps a running average of how long one takes, from its slot to its app
listening; a restore that timed out counts as a lower bound, and one that
failed or was killed not at all. A command whose deadline cannot
cover the restores ahead of it and its own is refused at once with an
`ERROR`, which Fiberd takes as a failed birth, as is one left too little time
after its wait. A helper with nothing restoring or waiting never refuses, so
a stale estimate cannot shut it out. Fiberd's probe, the first `CLONE` after
`READY`, which it times to size its default deadlines (four times the probe,
floored at 500 ms for a create and 2 s for a resume, capped at 5 s), never
waits and is never refused, so the deadlines it sizes are a restore's alone.
A `KILL` can overtake a birth, a queued one too: the Fiber leaves the queue
and answers `EXITED` without restoring.

## Memory

`W`, what a Fiber dirtied since the warm snapshot, is hyperlight-unikraft's
count (`AppSandbox::dirty_bytes`, from hyperlight's scratch allocator) since
its sandbox was last restored or snapshotted, plus, for a resumed Fiber, what
its park recorded, and, when a sync park failed after its snapshot was taken
and the Fiber serves again, what was counted before that snapshot. The helper
reports it with `CLONED`, then between guest steps, when it has changed, at
most every 100 ms, so a guest still inside one step is held only by the grant's
memory ceiling. A count that cannot be read means the sandbox is broken: the
`CLONE` or `RESUME` is an `ERROR` if it has not answered yet, a serving Fiber
ends with `EXITED <fence> exit:1`, and a `PARK` that cannot read it before its
snapshot is an `ERROR`, as any failed park is.

The count is how far the guest's page allocator has moved, not what the guest
wrote. `W` after a clone includes everything the app wrote as it started (about
23 MiB with the helper's Python test fixture). What the kernel writes on a
restore to resume the guest is not counted, but `W` still grows as a resumed
app runs again (with the Python fixture, about 1.4 MiB idle and 1.7 MiB more on
its first request). After a sync park that failed after its snapshot was taken,
what the kernel writes to pick up again on the next entry is counted (with the
Python fixture, `W` grew about 4.4 MiB by the end of the next request).
hyperlight-unikraft's own figures for a small guest function call are about
0.6 MiB after a restore and 1.3 MiB after a snapshot. The count runs ahead of
what the guest wrote by up to about 1 MiB, as the kernel's frame allocator
takes its memory a chunk at a time, and by more around a large allocation,
whose chunk is several times its size. And it adds up across a Fiber's parks
and resumes: what a resumed guest writes counts again, pages it had written
before included. Lampyr's serving controller and `ly` set a grant's `wBudget`
to the service's memory. Since `W` can count more than the Fiber uses, a Fiber
can reach it without using that much, above all one parked and resumed many
times, so the service's memory needs headroom.

The guest controls the count: a grant's `wBudget` stops a Fiber that reports
too much, but one that reports too little is held only by the grant's memory
ceiling. That ceiling applies to the grant's cgroup, which holds the whole
helper: pressure there can make Fiberd refuse new clones, and going past it
ends the helper, and with it every Fiber of the grant, rather than refusing
one clone.

## Orphaned Fibers

The helper keeps a Fiber until Fiberd ends it with `KILL`; it never times one
out itself. Fiberd does not reclaim a single unreleased Fiber either: its lease
covers the whole grant. So a Fiber that fiber-router loses track of, after a
router restart, keeps its slot under `fibers.max` and its memory until the
grant is yielded or revoked. Reclaiming it needs fiber-router to release what
it held, or Fiberd to bound how long a Fiber may go unreleased.

## Limits

- Each park adds a memory layer for the pages no earlier layer holds (those the
  Fiber wrote since its last snapshot), and an earlier layer stays while it
  still backs a mapped page; hyperlight 0.19 caps a snapshot at 30 data layers.
  A Fiber whose every park-and-resume cycle keeps pages no later cycle rewrites
  can no longer be parked after about 28 cycles; one that keeps nothing levels
  off well below the cap. A clone, which starts from the warm snapshot, is
  unaffected.
- Fibers are threads of one helper process, so they share the grant's cgroup,
  and a helper crash ends every Fiber of the grant. Moving to a process per
  Fiber, each in its own cgroup leaf, waits on how Fiberd hands a helper
  per-Fiber cgroups.
- KVM only: hyperlight-unikraft is built without MSHV.
