---
title: Scale testing
description: Compare Lampyr with the same application in a Kubernetes Deployment.
---

# Scale testing

The opt-in scale framework compares a Lampyr Service with the same Go
application in a normal Kubernetes Deployment. It uses oha for client latency
and throughput, and queries the optional observability add-on for platform
metrics. It does not run in CI.

Install the add-on and deploy the committed Go standalone example through
Lampyr. Build and publish the normal Kubernetes image from the same source:

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

The default sweep runs each target for 30 seconds at concurrency 1, 4, 16,
and 64. The framework:

- creates a temporary one-replica Kubernetes Deployment and Service;
- warms both targets;
- runs the same client against Lampyr and Kubernetes;
- queries Prometheus for request, Fiber, CPU, and memory measurements;
- writes `build/scale-results.json` after every scenario; and
- writes a concise Markdown comparison beside it.

The Kubernetes target uses a local Service port-forward, while the Lampyr URL
uses its configured ingress. Record that path difference when interpreting
results. The result metadata records both paths. The framework monitors the
port-forward and stops if it exits, but the tunnel can still limit high
concurrency results. This framework is for repeatable comparisons, not a claim
that the two network paths have identical overhead.

The report calculates HTTP success from response status codes and transport
errors. Its latency columns include only successful HTTP responses. Platform
error and shed counts come from Lampyr metrics. The runner waits for a final
Prometheus scrape after each load interval. If needed, it waits for up to five
more scrape intervals so short smoke tests still have enough cAdvisor samples.
CPU measurements include that collection interval.

Use the Lampyr Platform Grafana dashboard while a run is active. The JSON
result remains the source for comparisons across commits or machines. It
records the image digest, namespace, replica count, oha version, Git state, and
a limited node summary without node addresses or machine identifiers.

Keep the node type, cluster configuration, workload image, Lampyr settings,
duration, and concurrency sweep unchanged when comparing revisions.
