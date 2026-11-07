---
title: ly doctor
---

# `ly doctor`

Check that this host can build and run Lampyr Services.

```bash
build/ly doctor
```

It reports the Lampyr runtime payload it would use and each prerequisite:

- **ly**: the running CLI version.
- **build**: the source commit embedded at build time and whether that build
  used a clean or dirty tracked worktree. Builds made outside the Lampyr Git
  checkout report the commit and state as `unknown`.
- **application**: when run in a service directory, validates the manifest and
  reports its application model and effective command. A standalone server is
  not started or hook-checked.
- **platform**: a repository checkout containing the current directory, or a
  payload installed under `LAMPYR_HOME` by [`ly setup`](setup.md).
- **docker** and **kubectl** on `PATH`.
- On Linux, read/write access to **`/dev/kvm`**. If the device exists but is unusable,
  join the group that owns it, or restore its group with
  `sudo chgrp kvm /dev/kvm` if a Kind cluster changed it.

`ly doctor` exits non-zero when a prerequisite is missing or a local service
manifest is invalid. It checks only this
host; see [`ly cluster health`](cluster.md) for a cluster.
