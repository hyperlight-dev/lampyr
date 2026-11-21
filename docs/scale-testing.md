---
title: Scale testing
description: Compare Lampyr with the same application in a Kubernetes Deployment.
---

# Scale testing

The opt-in scale framework compares a Lampyr Service with the same Go
application in a normal Kubernetes Deployment. It uses oha for client latency
and throughput, and queries the optional observability add-on for platform
metrics. The live comparison does not run in CI.

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

- creates a temporary one-replica Deployment and Service with unique names;
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

The runner requires one baseline replica and a duration of at least 30 seconds.
Both targets use a 10-second request timeout, including warmup. Cleanup removes
only resources this run created.
Custom JSON output names must not use the `.md` extension.
The context must allow read access to the measured Pods.

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
