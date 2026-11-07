---
title: Observability
description: Scrape fiber-router's Prometheus metrics.
---

# Observability

fiber-router serves Prometheus metrics on port 9090 at `/metrics`. Every
series is labelled with the Revision's `namespace` and `revision`. A Revision's
series are removed when its route goes away.

| Metric | Type | Meaning |
|---|---|---|
| `lampyr_router_fibers_busy` | gauge | Fibers serving a request right now. Idle reuse Fibers kept in a pool are not counted. |
| `lampyr_router_fiber_acquire_seconds` | histogram | Time to clone or attach a Fiber before the request is proxied. `kind` is `CREATE` (a new Fiber), `ATTACH` (a running reuse Fiber), `RESUME` (a parked Fiber), `error`, or `canceled` when the client went away first. |
| `lampyr_router_request_duration_seconds` | histogram | Request duration through the router, Fiber acquisition included, by `code`. `499` means the client went away; codes HTTP doesn't define are reported as `other`. WebSocket sessions are not observed. |
| `lampyr_router_shed_total` | counter | Requests the Revision's home refused, with a 503 and `Retry-After`: it was already running `fibers.max` Fibers, or it was under memory or task-limit (pids) pressure, or it still refused the request for now when its acquire deadline would pass: its clone rate limit, or its helper restoring so many Fibers that a new one couldn't start within the request's deadline. A request whose client went away first is counted as `canceled` in `lampyr_router_fiber_acquire_seconds` instead. fiberd-k8s-controller refusing for now to name the Revision's home also gets a 503 with `Retry-After`, but is not counted. |
| `lampyr_router_clone_retries_total` | counter | Clones the router tried again within the request's acquire deadline, by `reason`: `rate_limit` after waiting out the home's clone rate limit, `restores_busy` after waiting 250ms to 1s when the home's helper was restoring too many Fibers to start one more in time, or `home_unreachable` after asking for the grant's homes again when the Clone reached no home, as when a rollout replaced the grant's Pod. A request that still gets no Fiber is counted where it fails, as a shed or an error. |

## Scrape the router

The platform NetworkPolicy only admits ingress traffic to fiber-router, so a
scraper needs a policy of its own. With the Prometheus Operator running in a
`monitoring` namespace:

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: fiber-router-metrics
  namespace: lampyr-system
spec:
  podSelector:
    matchLabels:
      app: fiber-router
  policyTypes:
    - Ingress
  ingress:
    - from:
        - namespaceSelector:
            matchLabels:
              kubernetes.io/metadata.name: monitoring
      ports:
        - protocol: TCP
          port: 9090
---
apiVersion: monitoring.coreos.com/v1
kind: PodMonitor
metadata:
  name: fiber-router
  namespace: monitoring
spec:
  namespaceSelector:
    matchNames:
      - lampyr-system
  selector:
    matchLabels:
      app: fiber-router
  podMetricsEndpoints:
    - port: metrics
```

To look without Prometheus:

```bash
kubectl -n lampyr-system port-forward deploy/fiber-router 9090:9090 &
curl -s localhost:9090/metrics | grep '^lampyr_router_'
```

On a CNI that doesn't enforce NetworkPolicy, any Pod in the cluster can read
these metrics, and with them the namespace and Revision names.
