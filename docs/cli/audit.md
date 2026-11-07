---
title: ly audit
---

# `ly audit`

Read recent `fiberd` lifecycle audit records for a Lampyr Service.

```bash
build/ly audit [--context CONTEXT] [--tail COUNT] [directory]
```

Example:

```bash
build/ly audit --context aks-lampyr --tail 20 blob-handler
```

The output includes Grant admission, Fiber creation, release, fencing, and
other runtime lifecycle events for Grants owned by the selected Lampyr
Service, and the gap and checkpoint records of each grant Pod's spool. `ly
audit` reads only running grant Pods, the last 5000 lines, at most 7 MiB, of
each one's spool, so a Pod that has stopped or been replaced, and its records,
are not shown.

| Option | Default | Purpose |
|---|---:|---|
| `--context` | current-context | Exact kubeconfig context. |
| `--tail` | `20` | Maximum records to print from each grant Pod. |
