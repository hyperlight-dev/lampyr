---
title: Scale testing
description: Compare the same Node.js application on Lampyr, Kubernetes, and Knative.
---

# Scale testing

The opt-in scale framework compares a Lampyr Service with the same Node.js
application in a Kubernetes Deployment and, optionally, stock Knative. It uses oha for client latency
and throughput, and queries the optional observability add-on for platform
metrics. The live comparison does not run in CI.

Install the add-on and deploy `docs/examples/hello-nodejs-standalone` through
Lampyr. This Express application returns JSON with `message` and `path` fields.
Build and publish the container image from that same source:

```bash
just scale-baseline-image REGISTRY/IMAGE:TAG
docker push REGISTRY/IMAGE:TAG
docker buildx imagetools inspect REGISTRY/IMAGE:TAG
```

Pass the digest-pinned image and the Lampyr URL:

```bash
just scale-test \
  CONTEXT \
  SERVICE \
  https://LAMPYR_URL \
  REGISTRY/IMAGE@sha256:DIGEST
```

The runner uses the Lampyr URL's path and query for both targets, during warmup
and load. For example, `"https://LAMPYR_URL/compute?iterations=1000"` also sends
`/compute?iterations=1000` to Kubernetes. Quote URLs that contain query parameters.
Use an absolute HTTP or HTTPS URL without credentials or a fragment.

## Stock Knative target

Use a separate cluster with stock Knative Serving and working ingress. Context
aliases for the Lampyr cluster are rejected. The selected namespace must exist
in both clusters. Install the metrics collectors in the stock cluster:

```bash
bash scripts/install-scale-metrics.sh KNATIVE_CONTEXT
```

This script requires `kubectl` and `jq`. It selects the metrics collectors from
the shared manifest. It does not install Lampyr, UI services, tracing services,
or Serving controllers.
Prometheus remains cluster-internal. The script omits the UI-only Prometheus
NetworkPolicy so the runner can query through the Kubernetes API proxy.

Add the Knative context to the runner:

```bash
python3 scripts/scale-test.py \
  --context LAMPYR_CONTEXT \
  --service SERVICE \
  --lampyr-url https://LAMPYR_URL \
  --baseline-image REGISTRY/IMAGE@sha256:DIGEST \
  --knative-context KNATIVE_CONTEXT
```

The runner creates a temporary Knative Service with the same image digest as
the Deployment. Both use a 768 MiB application memory limit. Knative uses
`min-scale=1` and `max-scale=1`; the client controls request concurrency.
The runner removes only the Service it created. This is a warm, fixed-capacity
comparison, not a cold-start or autoscaling test.

The Knative Service URL must be reachable from the load generator. For local
ingress exposed on a different address or port, add
`--knative-ingress-url http://127.0.0.1:PORT`. Supply only the origin, without a
path or query. The runner sends the Service's routing Host header and copies
the Lampyr request path and query for both warmup and load.

## Measurements

The default sweep runs each target for 30 seconds at concurrency 1, 4, 16,
and 64. The framework:

- creates a temporary one-replica Deployment and Service with unique names;
- creates a stock Knative Service when a Knative context is supplied;
- warms both targets;
- runs the same client against Lampyr and Kubernetes;
- queries Prometheus for request, Fiber, CPU, and memory measurements;
- writes `build/scale-results.json` after every scenario; and
- writes a concise Markdown comparison beside it.

The Kubernetes target uses a local Service port-forward, while the Lampyr URL
uses its configured ingress. Record that network difference when interpreting
results. The result metadata records both network paths. The framework monitors the
port-forward and stops if it exits, but the tunnel can still limit high
concurrency results. This framework is for repeatable comparisons, not a claim
that the two network paths have identical overhead.

The runner requires one Deployment replica and a duration of at least 30 seconds.
Both targets use a 10-second request timeout, including warmup. Cleanup removes
only resources this run created.
Custom JSON output names must not use the `.md` extension.
The context must allow read access to the measured Pods.

Client latency, throughput, and HTTP success come from oha for all targets.
CPU and memory come from each cluster's Prometheus, which scrapes kubelet
cAdvisor. kube-state-metrics supplies Pod membership. The Knative JSON records
the application and queue-proxy separately; the report includes both. Combined
peak memory is measured at one time, not the sum of separate peaks. Shared
ingress and control-plane costs are not included. Node details are recorded
for both clusters; use matching node sizes and configurations.

The report calculates HTTP success from response status codes and transport
errors. Its latency columns include only successful HTTP responses. Platform
error and shed counts come from Lampyr metrics. The runner waits for a final
healthy Prometheus scrape from every router after each load interval. It also
requires fresh cAdvisor samples from the measured containers and fresh
kube-state-metrics inputs. Missing or changed input series reject the scenario.
Coverage must match the running Pods from the Kubernetes API.
It can wait for up to five more scrape intervals. Counter resets reject the
scenario. The measurement window includes the pre-load router and resource
samples after warmup, and the final collection interval. Peak memory is the
maximum combined usage at one time.

Use the Lampyr Platform Perses dashboard while a run is active. The JSON
result remains the source for comparisons across commits or machines. It
records the image digest, namespace, replica count, oha version, Git state, and
a limited node summary without node addresses or machine identifiers.

Keep the node type, cluster configuration, workload image, Lampyr settings,
duration, and concurrency sweep unchanged when comparing revisions.
