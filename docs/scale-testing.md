---
title: Scale testing
description: Compare the same Node.js application on Lampyr, Kubernetes, and Knative.
---

# Scale testing

Compare the same Node.js app on Lampyr, a Kubernetes Deployment, and optionally
stock Knative.

## Before you start

**The runner does not create clusters or install Lampyr or Knative.** You need:

- A running [Lampyr cluster](cluster-kind.md) with the
  [observability add-on](observability.md#install-the-optional-add-on).
- `docs/examples/hello-nodejs-standalone` [deployed through Lampyr](cli/deploy.md),
  with a URL you can reach. Use this app without other traffic during the test.
- A Linux x86-64 machine with `just`, Docker, Python 3, `kubectl`, and `jq`.

Your Kubernetes access must allow test workload creation and deletion, Pod
reads, `exec` in the fiberd container, and port-forwarding.

Run the commands below from the repository root. Replace the uppercase values.

## Run Lampyr and Kubernetes

The Deployment runs in your Lampyr cluster. Build and push the container image
from the same Node.js source to a registry that the cluster can reach:

```bash
just scale-baseline-image REGISTRY/IMAGE:TAG
docker push REGISTRY/IMAGE:TAG
docker buildx imagetools inspect REGISTRY/IMAGE:TAG
```

Copy the digest from the inspect output. Use it with your Lampyr context,
Service name, and app URL:

```bash
just scale-test \
  LAMPYR_CONTEXT \
  SERVICE \
  "https://LAMPYR_URL" \
  REGISTRY/IMAGE@sha256:DIGEST
```

Use the HTTP or HTTPS URL printed by `ly deploy`. Its path and query are sent
to every target. The runner installs its pinned load generator, oha.

## Add stock Knative

Use a **separate, existing cluster** with Knative Serving and working ingress.
The `default` namespace must exist in both clusters, and both must be able to
pull the container image. Install its metrics collectors:

```bash
bash scripts/install-scale-metrics.sh KNATIVE_CONTEXT
```

Then run this command instead of `just scale-test`:

```bash
just install-oha
python3 scripts/scale-test.py \
  --context LAMPYR_CONTEXT \
  --service SERVICE \
  --lampyr-url "https://LAMPYR_URL" \
  --baseline-image REGISTRY/IMAGE@sha256:DIGEST \
  --knative-context KNATIVE_CONTEXT
```

For local Knative ingress, add `--knative-ingress-url http://127.0.0.1:PORT`
if the Service URL is not reachable. Do not include a path or query.

## Read the results

The runner measures startup three times per target:

| Target | Startup measurement |
|---|---|
| Lampyr | Zero Fibers to first response, with the same home running |
| Deployment | Zero Pods to one Ready Pod; not request-triggered |
| Knative | Zero replicas to first response |

Images stay cached. The runner then tests warm traffic for 30 seconds at each
concurrency: 1, 4, 16, and 64. Lampyr's default fresh mode still creates a Fiber
per request.

Read `build/scale-results.md` for the comparison and `build/scale-results.json`
for the full data. Client results come from oha; CPU and memory come from each
cluster's Prometheus. Knative includes its queue-proxy. Shared ingress and
control-plane costs are excluded.

Network paths differ: Deployment uses a port-forward; Lampyr and Knative use
ingress. Keep cluster sizes and test settings the same when comparing runs.
The runner removes its temporary workloads, not your Lampyr app or clusters.
