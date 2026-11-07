---
title: ly status
---

# `ly status`

Show the deployed service, current Revision, URL, traffic, and Fiber capacity.

```bash
build/ly status [--context CONTEXT] [directory]
```

Example:

```bash
build/ly status services/hello-edge
```

`--context` defaults to kubeconfig current-context.

The output also identifies the application model configured in the local
manifest: `handler` or `standalone`.

For each Revision that takes traffic, a `capacity` line reports its
CapacityGrant as fiberd-k8s-controller does, with the grant's Fiber limit:

```text
capacity hello-edge-00001 ready, max 100 fibers
```

The grant is `pending` while the controller rolls out its Pod, with the
controller's reason, and `failed` when the controller refuses the grant or the
rollout gave up. The grant Pod serves only mutual TLS to fiber-router, so
`ly status` no longer reports the home's running Fibers or epoch.
