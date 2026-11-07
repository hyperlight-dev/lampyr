# lampyr-hluk-helper

fiberd runs a Hyperlight backend through a helper process that speaks
[fiberd's helper protocol](https://github.com/Azure/fiberd/blob/main/hack/hyperlight/PROTOCOL.md)
on fd 3. This helper runs Lampyr snapshots: every fiber is a
hyperlight-unikraft micro-VM restored from the snapshot `ly build` made.

```text
lampyr-hluk-helper --template-dir <dir>   # fd 3 is fiberd's control socket
                   [--max-restores N]     # fibers restoring at once
lampyr-hluk-helper --version              # the four facts snapshots depend on,
                                          # then its template-dir capability
```

`--version`, as the only argument, prints two lines (beside other
arguments it fails, so a template argument cannot end a warm cleanly
without `READY`):

```text
lampyr-hluk-helper/0.3.0+hluk-<commit> hyperlight_host/<version> kvm GenuineIntel
template-dir
```

The first is the four facts (the helper with its hyperlight-unikraft commit,
the hyperlight_host crate, the hypervisor and the CPU vendor), which fiberd
reads alone and `READY` repeats. The second says the helper restores the
template fiberd unpacks: fiberd passes `--template-dir` only to a helper that
prints it, and `ly build` refuses a helper that does not.

Flags take one or two dashes, and a value either after `=` or as the next
argument, so `-template-dir <dir>` and `--template-dir=<dir>` are the same.
`--port-range LOW-HIGH` sets the guests' ports (see [Ports](#ports)),
`--max-restores N` how many fibers restore at once (see
[Restores](#restores)); fiberd's `--guest` is ignored and its `--engine`
refused (see [Limits](#limits)). fiberd passes a template command's
arguments after its own, so a template could set `--port-range` and
`--max-restores`; Lampyr's templates set neither, and take the defaults.

`<dir>` is a Lampyr artifact directory, holding `metadata.json` and
`snapshot/`. fiberd, since commit
[549f616](https://github.com/Azure/fiberd/commit/549f616) (see its
[PROTOCOL.md](https://github.com/Azure/fiberd/blob/549f616/hack/hyperlight/PROTOCOL.md)),
pulls the grant's template, unseals it, unpacks it read-only and passes that
directory as `--template-dir` to a helper that advertises `template-dir`; the
helper never writes into it. Only the tests run the helper by hand against a
template directory. See the
[design](../docs/designs/hyperlight-helper.md).

## What each command does

| fiberd says | the helper |
|---|---|
| (start) | loads the snapshot once and answers `READY` with its facts |
| `CLONE` | restores a sandbox, writes the fiber's incarnation, starts the app with `PORT`, `LAMPYR_FIBER_ID` (the fence it is cloned under) and its [bindings](#bindings) set, and answers `CLONED` and its first `W` once the app listens and the endpoint is bound |
| `PARK` | stops serving, writes the sandbox, its port and its `W` into the directory, and answers `PARKED` |
| `RESUME` | restores a sandbox from a park under a new fence and answers `CLONED` and its first `W` |
| `KILL` | ends the sandbox and answers `EXITED` |

The app serves on a port the helper picks. The endpoint fiberd
names is a unix socket, which the helper bridges to that port.

## Bindings

A template's project may name other Lampyr Services as `bindings`. Each
fiber gets `LAMPYR_BINDING_NAME`, the Service's URL in cluster DNS
(`http://SERVICE.default.svc.cluster.local`), and may connect to the
addresses those names resolve to, on any port, resolving them through the
grant Pod's nameservers with `ndots:1`. It may also send DNS questions for
those names, and no others, over UDP to the Pod's nameservers and the
well-known public resolvers (8.8.8.8, 8.8.4.4, 1.1.1.1 and 1.0.0.1), and
reach nothing else. A Lampyr Service's
name resolves to the cluster's shared internal ingress, so a fiber with any
binding can reach every Knative Service through it, not only the ones it
binds. A fiber with no bindings has no egress at all. A resumed park keeps
the variables it was cloned with.

hyperlight-unikraft drops guest traffic to link-local addresses
(169.254.0.0/16 and fe80::/10), nameservers included, so the guest
is given only the Pod's other nameservers. A template with bindings
fails its warm on a Pod whose nameservers are all link-local, as with
AKS LocalDNS or NodeLocal DNSCache.

## Incarnation

Before its endpoint exists, a fiber's fence and generation are in
`/run/fiberd/incarnation`, a read-only JSON file:

```json
{"fence":"grant/1/7","generation":1}
```

A resumed park keeps the variables it was cloned with, so its
`LAMPYR_FIBER_ID` names its first incarnation's fence, not the one `RESUME`
gave it. An app that needs its current fence reads this file.

hyperlight-unikraft reseeds the guest's random generators from host entropy
on every restore, so each clone draws different bytes. fiberd's seed is
checked, then dropped. It never reaches the guest, the log, or a park.

## Restores

A restore is CPU-bound until its app listens, and fiberd's hyperlight
backend does not cap how many it asks for at once, so the helper does: at
most `--max-restores` fibers (by default two per CPU the helper may use, at
most 16: a grant Pod has no CPU limit, so two per node CPU) restore at once,
and a `CLONE` or `RESUME` waits for a slot in arrival order. Without the
bound, a burst of cold requests (1000 at once) starves the grant Pod: every
restore slows, all of them miss their deadlines, and the fibers already
serving get no CPU either.

The deadline a `CLONE` or `RESUME` names counts from its arrival, the wait
for a slot included. Once a restore has finished, the helper estimates how
long one takes (a running average, from the slot to the app listening),
and refuses with an `ERROR` at once a command whose deadline cannot cover
the restores ahead of it and its own, or that is left too little time
after its wait, rather than queue it only to miss its deadline. A helper
with nothing restoring or waiting never refuses, so a first restore, or one
after the estimate grew past every deadline, refreshes the estimate.
fiberd's first `CLONE` after `READY`, the probe it times to size its
default deadlines, never waits and is never refused. A `KILL` of a fiber
still waiting for its slot takes it out of the queue, and it answers
`EXITED <fence> exit:137` without restoring.

## Ports

A guest's listening socket is a host socket, bound where the app asks. Its
port comes from `--port-range` (default `10000-19999`), which must lie
outside the kernel's ephemeral ports, so no guest's bind to port 0 and no
outgoing connection can take it. A port belongs to one fiber at a time.
Before a fiber is reported serving, every listener on its port must be
this helper's own, and one of them must take connections to 127.0.0.1,
where the endpoint connects (on 127.0.0.1 or 0.0.0.0, either v4-mapped
too, such as `::ffff:0.0.0.0`, or on `::` without `IPV6_V6ONLY`, which
the kernel reports with the listener or, from the /proc tables, the
socket is asked, and bound to no device or to `lo`),
or the fiber fails to start: a port left free on 127.0.0.1 is any
process's to bind, even beside a listener bound to another device, by a
socket bound to `lo`. Every connection its endpoint
takes then checks, before it reaches the port and after, that those are
still the same sockets and still listening, and that the ones taking its
connections are still bound to no device or to `lo` (Linux lets a
listening socket move to another device, though the guest cannot ask it
to); while they are, Linux lets no other socket bind the port where the
endpoint's connections would reach it.
Another address on the port, such as 127.0.0.2 or `::1`, can still be
bound, but takes none of them. The exception is `SO_REUSEPORT`: if the
guest sets it, a process running as the helper's own user that sets it too
can bind the port and take every connection, which such a process could do
to the helper directly anyway. If the guest replaces its listener, a
connection made across the change is dropped, since it may have reached
another process's listener in between, and the first check is made again
for the next ones. A park resumed while an earlier resume of it or its
parked original lives, or onto a port something else took or outside the
helper's range, is an `ERROR`. Whether a park may be resumed again once
those have ended is fiberd's call, since fiberd owns parks.

The helper asks the kernel for a port's listeners over sock_diag, a
netlink socket that needs no capability. Where it cannot (a seccomp
profile that refuses netlink sockets, or a kernel without `tcp_diag`), it
says so once on stderr and reads `/proc/net/tcp` and `tcp6` instead, up
to their last listener, which is several times slower under load. The
kernel answers in datagrams of about 260 listeners, and takes up a dump
split across them by counting, so a socket that comes or goes in between
can hide another. So on a port whose listeners do not fit in one
datagram the search fails, and with it the birth or the connection it
was for, rather than fall back to `/proc`, which is read in pieces the
same way.

Guests get no network egress: their endpoint is reached from the host, and
without egress no fiber can reach another's port.

## Limits

- Apps that bind `0.0.0.0` are reachable on the host's other interfaces
  too, not only through their endpoint, as with a guest run by `hluk` on its
  own. Keep the host's network policy closed to the port range until
  hyperlight-unikraft can bind guest listeners to loopback.

- `W`, what a fiber dirtied since the warm snapshot, is
  hyperlight-unikraft's count (`AppSandbox::dirty_bytes`) since its
  sandbox was last restored or snapshotted, plus what the park the fiber
  was resumed from recorded, and what it counted before a sync park that
  failed after its snapshot was taken. It is reported with `CLONED`, then
  between guest steps, when it has changed, at most every 100ms, so a
  guest still inside one step is held only by the grant's memory ceiling.
  A count that cannot be read means the sandbox is broken: the `CLONE` or
  `RESUME` is an `ERROR` if it has not answered yet, a serving fiber ends
  with `EXITED <fence> exit:1`, and a `PARK` that cannot read it before
  its snapshot is an `ERROR`, as any failed park is. The count is how far
  the guest's page allocator has moved, not what the guest wrote. `W`
  after a clone includes everything the app wrote as it started. What the
  kernel writes on a restore to resume the guest is not counted, but `W`
  still grows as a resumed app runs again; after a sync park that failed
  after its snapshot was taken, what the kernel writes to pick up again is
  counted. The growth after a resume and after a failed sync park comes to
  a few MiB each on the helper's Python test fixture. The count runs ahead
  of what the guest wrote by up to about 1 MiB, and by more around a large
  allocation, whose chunk is several times its size. It also adds up
  across parks and resumes: what a resumed guest writes counts again,
  pages it had written before included. Lampyr sets `wBudget` to the
  service's memory, so a fiber parked and resumed many times can reach it
  without using that much: give the service more memory than it uses. The
  guest controls the count: a guest that reports less than it uses is held
  only by the grant's memory ceiling.
- Fibers are threads of one helper process, so they share the grant's
  memory accounting, and a helper crash ends every fiber of the grant.
- A fiber takes `PARK` and `KILL` between guest steps. An idle guest's
  step waits up to 250ms for its timers and sockets, so an idle fiber
  wakes only a few times a second. A `PARK` waits for the rest of that
  step. A `KILL` wakes its thread at once with a signal (`SIGRTMAX`,
  whose handler the helper installs and does nothing), unless the guest
  is not parked on a socket or has no network, when the step is a sleep
  the signal does not end. A `PARK` is not signalled, as the signal can
  hand the guest an `EINTR` it does not expect, which a park would keep.
  If the helper cannot handle `SIGRTMAX`, it says so once on stderr, and
  a `KILL` waits out the step too. A guest that keeps running (a busy
  loop) cannot be parked: the `PARK` is an `ERROR` after 5s and the fiber
  keeps serving. `KILL` interrupts such a guest, and one that spins as its
  `CLONE` or `RESUME` restores it, before it serves.
- A `PARK` that has not written its park within 14s of arriving, just
  inside fiberd's 15s wait for the answer, is an `ERROR`, and never
  `PARKED` after. When the write returns, a sync park's fiber serves again
  and an async park's ends with `EXITED`. A write that never returns keeps
  the fiber waiting with it.
- Each park adds a memory layer for the pages no earlier layer holds
  (those the fiber wrote since its last snapshot), and an earlier layer
  stays while it still backs a mapped page; hyperlight 0.19 caps a
  snapshot at 30 data layers. A fiber whose every park-and-resume cycle
  keeps pages no later cycle rewrites can no longer be parked after about
  28 cycles; one that keeps nothing levels off well below the cap. A
  clone, which starts from the warm snapshot, is unaffected.
- KVM only, because hyperlight-unikraft is built without MSHV.
- A fiber splices at most 64 connections at once and closes any more as
  they arrive. A spliced connection has no idle timeout, so streams and
  WebSockets stay open for as long as both sides keep them.
- No device engines: fibers have no relay to one, so a grant that names
  one (`--engine`) is an `ERROR` at warm.

## Image

`ghcr.io/hyperlight-dev/lampyr/hluk-helper` holds `/fiberd-helper/helper`, as
fiberd-k8s-controller's `helperImage` expects. Its base is fiberd's
proc base.

```text
just helper::test                             # lint and the tests that need no hypervisor
                                              # (LAMPYR_HELPER_NO_MPTCP=1 on a host without MPTCP)
just helper::incarnation-template <ly> <out>  # build tests/fixtures/incarnation's template
just helper::test-kvm <template> <out>        # drive it over the protocol, on KVM
                                              # (<out> or LAMPYR_HELPER_INCARNATION_TEMPLATE)
just helper::image                            # build the image locally
```
