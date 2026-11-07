---
title: ly deploy
---

# `ly deploy`

Publish the built OCI image and deploy it as an immutable Lampyr Service
Revision.

```bash
build/ly deploy --image REGISTRY/REPOSITORY:TAG [options] [directory]
```

Example:

```bash
build/ly deploy \
  --image myregistry.azurecr.io/hello-edge:v1 \
  services/hello-edge
```

The command checks whether the registry already has the image built from the
local artifact, and pushes it only if needed. It resolves the image to a
digest, checks it against the local artifact, applies the Knative Service,
waits for its Revision and Route, then prints the HTTPS URL. Use `ly push`
to publish the image separately without deploying it.

For the default Kind installation, use its local registry:

```bash
build/ly deploy --image localhost:5002/demo-app:v1 services/demo-app
```

A new Revision also needs a fiberd template, from the helper facts
`ly build` records when the platform has its helper. Deploy seals it to the cluster and
pushes it to the installation's template repository, tagged with the
Revision's name, as [`ly cluster`](cluster.md#fiberd-templates) describes.

The progress display separates artifact verification, template sealing, and
the Service's configuration, which includes waiting for its Revision, Route
and visibility to become ready. Lampyr's serving controller gives each
Revision a `fiberd.io/v1alpha1 CapacityGrant`, for which fiberd-k8s-controller
runs a grant Pod with stock fiberd and Lampyr's helper. The controller issues
and renews the Pod's grant and reports its readiness in the CapacityGrant's
conditions. The CLI waits at most 4 minutes for the Revision, its grant among
it, to become ready. It does not stop early when the controller refuses a
grant; the deploy times out, and the Revision's `ResourcesAvailable` condition
gives the reason.

Service handlers, `lampyr.yaml`, and the final `Dockerfile` remain user-owned;
the tracked `.lampyr-adapter/` directory supplies editable runtime and build
support. Deploy checks adapter metadata and reports modified or outdated
adapters with a best-effort warning without rewriting source. Upgrade adapters
only with [`ly scaffold service NAME --upgrade`](scaffold.md#upgrade-an-adapter-explicitly).
Standalone Applications have no adapter; every file in them is user-owned.

## Runtime upgrades

Hyperlight-Unikraft v0.21.0 includes the restricted-DNS fix needed by service
bindings. Custom runtime descriptors and per-service home overrides are no
longer used.

A cluster from before this release ran snapshots in its `fiberdImage` home.
The reinstall this release requires replaces that home with grant Pods, which
restore snapshots with the `lampyr-hluk-helper` an installation names as
`helperImage`; it links Hyperlight-Unikraft v0.21.0, and a release's platform
installs its own when an install is not given one. Snapshots from v0.20.1 or
earlier are incompatible with v0.21.0. Rebuild provider artifacts first,
since `ly cluster install` refuses ones built for an older runtime, and use
`--provider-artifact` when the installation uses one. Then reinstall, and
rebuild and redeploy each service.

## External exposure

Deployment reads `ingress.services` from the enclosing `lampyr.workspace.yaml`.
Listed services use Knative's generated external hostname and the installed
cluster domain. Other services get Knative's `cluster-local` visibility and an
internal address such as `http://worker-processor.default.svc.cluster.local`.
Deployment outside a workspace also defaults to cluster-local.

Bindings can call both public and internal services through cluster DNS.
Exposure does not change binding URLs or snapshot identity. After editing the
exposure list, redeploy each affected service; a config edit alone does not
change resources already running in Kubernetes. Deploy reports visibility and
waits for the Service and Route URLs to reflect it. Custom DNS and trusted
certificates remain separate cluster configuration.

## Execution policy

The default is a fresh fiber for each request, released when the response,
stream, or WebSocket ends. Opt into live-fiber reuse in `lampyr.yaml`:

```yaml
execution:
  mode: reuse
  idleTimeout: 30s
```

Reuse is scoped to an immutable service revision, not to an individual user.
Idle fibers are released; the next request starts from the original snapshot.
Application memory is neither durable nor guaranteed to follow a user between
fibers. `idleAction: release` is accepted explicitly; park/resume is not supported
and is rejected during manifest validation. Build and redeploy after changing
the policy so it becomes part of the immutable Revision.

| Option | Default | Purpose |
|---|---|---|
| `--image` | required | Registry image destination with an explicit tag. |
| `--artifact` | project artifact | Override the artifact directory. |
| `--context` | current-context | Exact kubeconfig context. |
| `--namespace` | `default` | Deployment namespace. |
| `--traffic` | current Revision `100%` | Comma-separated `revision=percent` targets. |
