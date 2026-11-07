---
title: ly verify
---

# `ly verify`

Verify a local artifact's metadata and content digests.

```bash
build/ly verify [artifact-directory]
```

Example:

```bash
build/ly verify \
  services/hello-edge/.lampyr/artifact/linux-amd64-kvm
```

The command checks the artifact schema, snapshot digest, kernel digest, and
content-derived Revision ID. Image signing and registry policy are out of band.
