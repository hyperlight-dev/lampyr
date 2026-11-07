#!/usr/bin/env python3

import argparse
import concurrent.futures
import http.client
import json
import math
import ssl
import statistics
import subprocess
import time
import urllib.parse

import fiberd_grants

# What the gate measures on a grant Pod running stock fiberd:
#
# - Fibers started are fiberd's create and resume audit records for the
#   grant Pod, which fiberd stamps with the Pod's UID; the spool is under
#   -state/private now.
# - Active Fibers are fiber-router's lampyr_router_fibers_busy for the
#   Revision, since the grant Pod serves its own status only over mutual
#   TLS to the router. Idle pooled reuse Fibers aren't counted.
# - The runtime leak check is gone. It counted hluk processes and per-Fiber
#   cgroup leaves, but stock fiberd's Hyperlight backend runs every Fiber of
#   a grant as a sandbox thread of one helper process in the grant's cgroup,
#   so nothing per Fiber is left to count.


def command(*args):
    return subprocess.check_output(args, text=True).strip()


def health(context, revision, home):
    records = fiberd_grants.audit_records(context, home)
    return {
        "activeFibers": fiberd_grants.router_fibers_busy(context, revision),
        "totalFibersStarted": fiberd_grants.fibers_started(records),
    }


def service_pods(context, service):
    output = command(
        "kubectl",
        "--context",
        context,
        "--namespace",
        "default",
        "get",
        "pods",
        "--selector",
        f"serving.knative.dev/service={service},!lampyr.dev/home",
        "--output",
        "name",
    )
    return len(output.splitlines()) if output else 0


def request_once(parsed, expected):
    context = ssl.create_default_context()
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE
    started = time.perf_counter()
    connection = http.client.HTTPSConnection(
        parsed.hostname,
        parsed.port or 443,
        timeout=10,
        context=context,
    )
    connection.request("GET", parsed.path or "/")
    response = connection.getresponse()
    first = response.read(1)
    ttfb_ms = (time.perf_counter() - started) * 1000
    body = first + response.read()
    clone_ms = response.getheader("X-Fiberd-Clone-Ms")
    connection.close()
    if response.status != 200:
        raise RuntimeError(f"HTTP {response.status}: {body[:200]!r}")
    text = body.decode()
    if expected and expected not in text:
        raise RuntimeError(f"response missing {expected!r}: {text[:200]!r}")
    if clone_ms is None:
        raise RuntimeError("response missing X-Fiberd-Clone-Ms")
    return ttfb_ms, int(clone_ms)


def percentile(values, percent):
    ordered = sorted(values)
    index = max(0, math.ceil(len(ordered) * percent) - 1)
    return ordered[index]


def wait_idle(context, service, revision, home, timeout=60):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if (
            fiberd_grants.router_fibers_busy(context, revision) == 0
            and service_pods(context, service) == 0
            and fiberd_grants.grant_settled(context, revision)
        ):
            # The spool grows with every Fiber, so it is read once idle.
            return health(context, revision, home)
        time.sleep(0.5)
    raise RuntimeError("platform did not return to zero-Fiber idle state")


def active_revision(service):
    revisions = {
        target["revisionName"]
        for target in service.get("status", {}).get("traffic", [])
        if target.get("percent", 0) > 0
    }
    if len(revisions) != 1:
        raise RuntimeError(
            f"service must have exactly one active traffic Revision, got {len(revisions)}"
        )
    return next(iter(revisions))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--url", required=True)
    parser.add_argument("--service", required=True)
    parser.add_argument("--context", default="kind-lampyr")
    parser.add_argument("--requests", type=int, default=10000)
    parser.add_argument("--concurrency", type=int, default=16)
    parser.add_argument("--p95-ms", type=float, default=250)
    parser.add_argument("--expected", default="")
    parser.add_argument("--output", default="")
    args = parser.parse_args()

    parsed = urllib.parse.urlparse(args.url)
    service = json.loads(command(
        "kubectl",
        "--context",
        args.context,
        "--namespace",
        "default",
        "get",
        "service.serving.knative.dev",
        args.service,
        "--output",
        "json",
    ))
    revision = active_revision(service)
    grant = fiberd_grants.grant(args.context, revision)
    home = fiberd_grants.home_pod(args.context, grant)
    before = wait_idle(args.context, args.service, revision, home, 120)

    ttfb = []
    clone = []
    failures = []
    started = time.time()
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrency) as pool:
        futures = [
            pool.submit(request_once, parsed, args.expected)
            for _ in range(args.requests)
        ]
        for future in concurrent.futures.as_completed(futures):
            try:
                request_ms, clone_ms = future.result()
                ttfb.append(request_ms)
                clone.append(clone_ms)
            except Exception as error:
                failures.append(str(error))

    after = wait_idle(args.context, args.service, revision, home, 120)
    result = {
        "service": args.service,
        "revision": revision,
        "requests": args.requests,
        "concurrency": args.concurrency,
        "failures": len(failures),
        "durationSeconds": round(time.time() - started, 3),
        "ttfbMs": {
            "p50": round(statistics.median(ttfb), 3) if ttfb else None,
            "p95": round(percentile(ttfb, 0.95), 3) if ttfb else None,
            "max": round(max(ttfb), 3) if ttfb else None,
        },
        "cloneMs": {
            "p50": round(statistics.median(clone), 3) if clone else None,
            "p95": round(percentile(clone, 0.95), 3) if clone else None,
            "max": max(clone) if clone else None,
        },
        "fibersStarted": after["totalFibersStarted"] - before["totalFibersStarted"],
        "activeFibersAfter": after["activeFibers"],
        "servicePodsAfter": service_pods(args.context, args.service),
        "sampleFailures": failures[:10],
    }
    result["activationRetries"] = result["fibersStarted"] - args.requests
    rendered = json.dumps(result, indent=2, sort_keys=True)
    print(rendered)
    if args.output:
        with open(args.output, "w", encoding="utf-8") as output:
            output.write(rendered + "\n")

    if failures:
        raise SystemExit("release gate failed: request loss")
    if args.p95_ms > 0 and result["ttfbMs"]["p95"] >= args.p95_ms:
        raise SystemExit(
            f"release gate failed: p95 {result['ttfbMs']['p95']}ms >= {args.p95_ms}ms"
        )
    if result["fibersStarted"] < args.requests:
        raise SystemExit("release gate failed: activation count is below request count")
    if result["activeFibersAfter"] != 0 or result["servicePodsAfter"] != 0:
        raise SystemExit("release gate failed: workload did not return to zero")


if __name__ == "__main__":
    main()
