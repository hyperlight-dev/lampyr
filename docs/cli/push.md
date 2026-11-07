---
title: ly push
---

# `ly push`

Push the image created by `ly build` to an OCI registry.

```bash
build/ly push --image REGISTRY/REPOSITORY:TAG [--artifact PATH] [directory]
```

ACR example:

```bash
build/ly push \
  --image myregistry.azurecr.io/hello-edge:v1 \
  services/hello-edge
```

The command verifies that the local image matches the artifact, tags it with
the supplied name, pushes it, and prints the immutable registry digest.

| Option | Required | Purpose |
|---|---|---|
| `--verbose` | No | Stream registry push logs instead of the quiet progress display. |
| `--image` | Yes | Destination registry image with an explicit tag. |
| `--artifact` | No | Override the local artifact directory. |

Registry authentication is configured outside Lampyr.

The default output shows artifact verification, registry upload, and immutable
digest resolution. Use `ly push --verbose --image REGISTRY/REPOSITORY:TAG
[directory]` for the underlying Docker logs. Failed steps include captured
diagnostics in either mode; see [command output](README.md#command-output).
