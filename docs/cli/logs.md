---
title: ly logs
---

# `ly logs`

Read recent structured application-handler logs for a Lampyr Service.

```bash
build/ly logs [--context CONTEXT] [--tail COUNT] [directory]
```

Example:

```bash
build/ly logs --tail 20 services/hello-edge
```

| Option | Default | Purpose |
|---|---:|---|
| `--context` | current-context | Exact kubeconfig context. |
| `--tail` | `20` | Maximum records to print from each grant Pod. |

Generated event-capable handlers emit one JSON record per event. The record
includes the CloudEvent identity and parsed event data, such as Blob URL,
content type, content length, and Blob type for Azure Blob-created events.
`ly logs` reads the last 5000 lines, at most 7 MiB, of the helper log in each
running grant Pod of the Service, `/var/lib/fiberd/run/<Pod UID>/zygote.log`,
where fiberd sends the helper's stdout and stderr, guests' output included,
and keeps the records the Service's handler wrote.
