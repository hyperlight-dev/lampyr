---
title: Runtime examples
description: Generated Lampyr Service examples for every supported runtime.
---

# Runtime examples

Lampyr keeps a generated example for every supported runtime under
`docs/examples/`. These projects are canonical snapshots of `ly scaffold service`, so a
reviewer can see exactly what each template produces.

| Runtime | Handler project | Standalone project |
|---|---|---|
| Node.js | `hello-nodejs` | `hello-nodejs-standalone` |
| Python | `hello-python` | `hello-python-standalone` |
| Go | `hello-go` | `hello-go-standalone` |
| Rust | `hello-rust` | `hello-rust-standalone` |
| .NET | `hello-dotnet` | `hello-dotnet-standalone` |

All paths are relative to `docs/examples/`. Handler projects track the Runtime
Adapter; standalone projects own their entire server and rootfs build. Use
`ly scaffold service NAME --runtime RUNTIME --standalone` to generate the latter.

See [Runtime compatibility](runtime-compatibility.md) for support tiers and
protocol coverage.

## Run an example locally

Build the CLI, then copy an example outside the repository workspace:

```bash
just build-ly
demo=$(mktemp -d)
cp -R docs/examples/hello-dotnet "$demo/hello-dotnet"

build/ly dev \
  --port 8080 \
  "$demo/hello-dotnet"

curl http://hello-dotnet.default.localhost:8080/test
```

Use a different directory from the table to run another runtime.

## Deploy on k3d

Follow the [k3d cluster guide](cluster-k3d.md) to create the cluster. Copy the
chosen example into that workspace's `services/` directory, retaining its
service name.

Add the service to your workspace's `ingress.services` list before deployment
if it should be reachable through the cluster ingress. Unlisted services and
services deployed outside a workspace are cluster-local.

```bash
build/ly build services/hello-python
build/ly deploy --context k3d-lampyr-k3d \
  --image localhost:5002/hello-python:v1 services/hello-python
```

Use the registry port configured for your installation and the HTTPS URL
printed by `ly deploy`. Run `just test-k3d-integration` to test the five Handler examples.

## Regenerate and verify

The committed projects are generated output. After changing a runtime template,
regenerate its matching directory with `ly scaffold service`, then run:

```bash
just examples-check
```

The check regenerates every supported runtime in a temporary directory and
prints a unified diff when a committed example is stale. CI also runs every
example through local Hyperlight execution and the Kind deployment lifecycle;
the k3d lifecycle covers the five Handler examples.

For an editable quickstart project, create `services/demo-app/` using the
[walkthrough](walkthrough.md). This keeps tutorial
experiments separate from the canonical generated examples.
