---
title: Use Fiberd's hyperlight backend with a Lampyr helper
description: Replace Lampyr's own Fiberd build and hluk backend with Fiberd's stock backend driving a Lampyr helper.
---

# Use Fiberd's hyperlight backend with a Lampyr helper

**Status:** Accepted

Lampyr will run Fibers through Fiberd's stock hyperlight backend, which drives
a helper process over Fiberd's helper protocol. Lampyr owns and ships that
helper, `lampyr-hluk-helper`, as the image Fiberd's grant Pods will take
through fiberd-k8s-controller's planned `helperImage`. Fiberd will pull
Lampyr's snapshot as the payload of its own artifact and hand the helper the
unpacked directory.

Lampyr will retire its own Fiberd build (`cmd/fiberd-home`) and its Go hluk
backend (`internal/fiberdintegration/hluk`) once grant Pods run Fiberd's image
with the helper. The helper lives in this repository because it reads Lampyr's
artifact layout and must restore the snapshots `ly build` makes with the same
hyperlight-unikraft commit.

See the [design](../designs/hyperlight-helper.md).

## Consequences

- A grant Pod runs Fiberd's release image, so Lampyr no longer builds or
  patches Fiberd. Fiberd relays endpoints on its own port range, so the
  serving controller's grant Pod spec and the grant Pods' NetworkPolicy move
  with it.
- The helper's version facts are part of every park's platform: a helper
  release that changes its park layout or its hyperlight-unikraft commit
  invalidates older parks.
- Lampyr's snapshot must reach Fiberd in Fiberd's artifact format, which
  changes how `ly push` publishes it.
- Until hyperlight-unikraft binds guest listeners to loopback, a guest that
  binds `0.0.0.0` is reachable on the Pod's address.
- Fiberd's memory accounting sees `W`, what a Fiber dirtied since the warm
  snapshot, from hyperlight-unikraft's count, which the guest controls, so
  the grant's memory ceiling, not `wBudget`, is what holds a guest that
  reports less than it uses.
