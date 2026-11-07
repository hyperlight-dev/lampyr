---
title: ly event
---

# `ly event`

Publish a CloudEvent to a service:

```bash
build/ly event publish --id order-123 --data '{"order":123}' services/hello-edge
```

| Option | Default | Purpose |
|---|---|---|
| `--id` | generated | Event ID, reused across retries. |
| `--data` | `{}` | JSON event data. |
| `--path` | `/events` | Request path. |
| `--url` | deployed service URL | Override the destination. |
| `--context` | current-context | Kubeconfig context for URL discovery. |
| `--insecure` | `false` | Disable TLS certificate verification. |

With `events:` in `lampyr.yaml`, publishing uses its retry and local
dead-letter settings. Without it, Handler Applications in every runtime
receive one direct attempt with no retry or dead-letter record. Standalone
Applications require `events:` to publish.

TLS verification is enabled unless `--insecure` is set or an automatically
discovered destination uses `localhost` or `*.localhost`.

Without `--url`, publishing uses the deployed Knative Service URL. A
cluster-local service is not assumed reachable from your machine: pass
`--url` with a reachable endpoint, such as a port-forward URL. Explicit URLs
retain the same verification and retry behavior.

## Provider event sources

Provider sources require deployed `events:` policy. For Azure, install with
`--eventing` and define a source in the provider workspace:

```yaml
eventSources:
  blob-created:
    storageAccount: lyevents123
    container: events
```

Initialization stores these definitions in `event-sources.json`.

```bash
build/ly event source create --provider azure --name blob-created services/hello-edge
build/ly event source status --provider azure --name blob-created services/hello-edge
build/ly event source delete --provider azure --name blob-created services/hello-edge
```

| Option | Default | Purpose |
|---|---|---|
| `--name` | required | Source definition and Trigger name. |
| `--provider` | required | Provider that owns the source. |
| `--context` | current-context | Kubeconfig context. |
| `--namespace` | `default` | Service and source namespace. |

Azure accepts `storageAccount`, `resourceGroup`, `container`, `eventTypes`, or
an absolute `sourceResourceId` as source settings, not CLI options. The default
event type is `Microsoft.Storage.BlobCreated`.

Sources deliver to `/events/knative`; the sample returns the required HTTP 204.
Deleting a source removes its Source and Trigger, not the shared Broker.
