#!/usr/bin/env python3

import argparse
import contextlib
import datetime
import json
import math
import os
from pathlib import Path
import re
import socket
import subprocess
import sys
import tempfile
import time
import urllib.parse


BASELINE = "lampyr-scale-baseline"
PROMETHEUS_PROXY = (
    "/api/v1/namespaces/lampyr-observability/"
    "services/http:prometheus:9090/proxy/api/v1/query"
)


def command(*args):
    return subprocess.check_output(args, text=True).strip()


def duration_seconds(value):
    match = re.fullmatch(r"([1-9][0-9]*)(s|m)", value)
    if not match:
        raise ValueError("duration must be a positive number of seconds or minutes")
    amount = int(match.group(1))
    return amount * (60 if match.group(2) == "m" else 1)


def active_revision(context, service, namespace):
    value = json.loads(
        command(
            "kubectl",
            "--context",
            context,
            "--namespace",
            namespace,
            "get",
            "service.serving.knative.dev",
            service,
            "--output",
            "json",
        )
    )
    revisions = {
        target["revisionName"]
        for target in value.get("status", {}).get("traffic", [])
        if target.get("percent", 0) > 0
    }
    if len(revisions) != 1:
        raise RuntimeError(
            f"service must have exactly one active Revision, got {len(revisions)}"
        )
    return next(iter(revisions))


def prometheus_query(context, query, evaluation_time=None):
    parameters = {"query": query}
    if evaluation_time is not None:
        parameters["time"] = f"{evaluation_time:.3f}"
    path = PROMETHEUS_PROXY + "?" + urllib.parse.urlencode(parameters)
    result = json.loads(command("kubectl", "--context", context, "get", "--raw", path))
    if result.get("status") != "success":
        raise RuntimeError(f"Prometheus query failed: {result}")
    values = result["data"]["result"]
    if not values:
        return None
    if len(values) != 1:
        raise RuntimeError(f"Prometheus query returned {len(values)} series: {query}")
    value = float(values[0]["value"][1])
    return value if math.isfinite(value) else None


def metric_queries(namespace, revision, window, baseline, window_seconds=30):
    revision_match = f'namespace="{namespace}",revision="{revision}"'
    running_home = (
        '* on(namespace,pod) group_left() '
        f'(kube_pod_status_phase{{namespace="{namespace}",phase="Running"}} == 1)'
    )
    running_router = (
        '* on(namespace,pod) group_left() '
        '(kube_pod_status_phase{namespace="lampyr-system",phase="Running"} == 1)'
    )
    home = (
        '* on(namespace,pod) group_left() '
        f'(kube_pod_labels{{namespace="{namespace}",label_lampyr_dev_home="true",'
        f'label_serving_knative_dev_revision="{revision}"}} {running_home})'
    )
    baseline_pods = f'namespace="{namespace}",pod=~"{baseline}-.*"'
    return {
        "lampyrRequests": (
            "sum(lampyr_router_request_duration_seconds_count"
            f"{{{revision_match}}})"
        ),
        "lampyrErrors": (
            "sum(lampyr_router_request_duration_seconds_count"
            f'{{{revision_match},code!~"2..|3.."}})'
        ),
        "lampyrRequestP95Seconds": (
            "histogram_quantile(0.95,sum by(le)(increase("
            f"lampyr_router_request_duration_seconds_bucket{{{revision_match}}}"
            f"[{window}])))"
        ),
        "lampyrAcquireP95Seconds": (
            "histogram_quantile(0.95,sum by(le)(increase("
            f"lampyr_router_fiber_acquire_seconds_bucket{{{revision_match}}}"
            f"[{window}])))"
        ),
        "lampyrShed": (
            "sum(lampyr_router_shed_total" f"{{{revision_match}}})"
        ),
        "lampyrRetries": (
            "sum(lampyr_router_clone_retries_total" f"{{{revision_match}}})"
        ),
        "lampyrPeakBusyFibers": (
            "max_over_time((sum(lampyr_router_fibers_busy"
            f"{{{revision_match}}}))[{window}:5s])"
        ),
        "lampyrPeakInFlight": (
            "max_over_time((sum(lampyr_router_requests_in_flight"
            f"{{{revision_match}}}))[{window}:5s])"
        ),
        "lampyrRouterCPU": (
            "sum(increase(container_cpu_usage_seconds_total"
            f'{{namespace="lampyr-system",container="router"}}[{window}])'
            f" {running_router}"
            ")"
            f" / {window_seconds:.3f}"
        ),
        "lampyrRouterMemoryBytes": (
            "max_over_time((sum(container_memory_working_set_bytes"
            '{namespace="lampyr-system",container="router"} '
            f"{running_router}))"
            f"[{window}:5s])"
        ),
        "lampyrHomeCPU": (
            "(sum(increase(container_cpu_usage_seconds_total"
            f'{{namespace="{namespace}",container="fiberd"}}[{window}]) '
            f"{home})) / {window_seconds:.3f}"
        ),
        "lampyrHomeMemoryBytes": (
            "max_over_time((sum(container_memory_working_set_bytes"
            f'{{namespace="{namespace}",container="fiberd"}} {home}))[{window}:5s])'
        ),
        "baselineCPU": (
            "sum(increase(container_cpu_usage_seconds_total"
            f"{{{baseline_pods},container=\"app\"}}[{window}])"
            f" {running_home}"
            ")"
            f" / {window_seconds:.3f}"
        ),
        "baselineMemoryBytes": (
            "max_over_time((sum(container_memory_working_set_bytes"
            f"{{{baseline_pods},container=\"app\"}} {running_home}))"
            f"[{window}:5s])"
        ),
    }


COUNTER_METRICS = {
    "lampyrRequests",
    "lampyrErrors",
    "lampyrShed",
    "lampyrRetries",
}


def counter_snapshot(context, queries):
    return {
        name: prometheus_query(context, query)
        for name, query in queries.items()
        if name in COUNTER_METRICS
    }


def collect_metrics(context, queries, before, evaluation_time):
    collected = {}
    for name, query in queries.items():
        value = prometheus_query(context, query, evaluation_time)
        if name not in COUNTER_METRICS:
            collected[name] = value
            continue
        if name == "lampyrRequests" and (before[name] is None or value is None):
            raise RuntimeError(
                "Lampyr request metrics are missing; check the observability add-on "
                "and requested namespace"
            )
        previous = before[name] or 0
        current = value or 0
        if current < previous:
            raise RuntimeError(f"Prometheus counter reset during scenario: {name}")
        collected[name] = current - previous
    return collected


def baseline_manifest(image, replicas, namespace):
    labels = {"app": BASELINE}
    return {
        "apiVersion": "v1",
        "kind": "List",
        "items": [
            {
                "apiVersion": "apps/v1",
                "kind": "Deployment",
                "metadata": {"name": BASELINE, "namespace": namespace},
                "spec": {
                    "replicas": replicas,
                    "selector": {"matchLabels": labels},
                    "template": {
                        "metadata": {"labels": labels},
                        "spec": {
                            "automountServiceAccountToken": False,
                            "securityContext": {
                                "runAsNonRoot": True,
                                "runAsUser": 65532,
                                "runAsGroup": 65532,
                                "seccompProfile": {"type": "RuntimeDefault"},
                            },
                            "containers": [
                                {
                                    "name": "app",
                                    "image": image,
                                    "ports": [{"name": "http", "containerPort": 8080}],
                                    "readinessProbe": {
                                        "httpGet": {"path": "/", "port": "http"}
                                    },
                                    "resources": {
                                        "requests": {
                                            "cpu": "250m",
                                            "memory": "256Mi",
                                        },
                                        "limits": {"memory": "256Mi"},
                                    },
                                    "securityContext": {
                                        "allowPrivilegeEscalation": False,
                                        "capabilities": {"drop": ["ALL"]},
                                        "readOnlyRootFilesystem": True,
                                        "runAsNonRoot": True,
                                    },
                                }
                            ]
                        },
                    },
                },
            },
            {
                "apiVersion": "v1",
                "kind": "Service",
                "metadata": {"name": BASELINE, "namespace": namespace},
                "spec": {
                    "selector": labels,
                    "ports": [{"name": "http", "port": 80, "targetPort": "http"}],
                },
            },
        ],
    }


def kubectl_manifest(context, action, value, *extra):
    return subprocess.run(
        ["kubectl", "--context", context, action, "--filename", "-", *extra],
        input=json.dumps(value),
        text=True,
        check=True,
        stdout=subprocess.DEVNULL,
    )


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


@contextlib.contextmanager
def baseline_server(context, image, replicas, namespace):
    if "@sha256:" not in image:
        raise ValueError("baseline image must be digest-pinned")
    manifest = baseline_manifest(image, replicas, namespace)
    forward = None
    try:
        kubectl_manifest(context, "apply", manifest)
        subprocess.run(
            [
                "kubectl",
                "--context",
                context,
                "--namespace",
                namespace,
                "rollout",
                "status",
                f"deployment/{BASELINE}",
                "--timeout=300s",
            ],
            check=True,
        )
        port = free_port()
        forward = subprocess.Popen(
            [
                "kubectl",
                "--context",
                context,
                "--namespace",
                namespace,
                "port-forward",
                f"service/{BASELINE}",
                f"{port}:80",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        deadline = time.time() + 30
        while time.time() < deadline:
            if forward.poll() is not None:
                raise RuntimeError("baseline port-forward exited before becoming ready")
            with socket.socket() as connection:
                if connection.connect_ex(("127.0.0.1", port)) == 0:
                    break
            time.sleep(0.1)
        else:
            raise RuntimeError("baseline port-forward did not become ready")
        yield {"url": f"http://127.0.0.1:{port}/", "process": forward}
    finally:
        original_error = sys.exc_info()[0] is not None
        if forward is not None:
            forward.terminate()
            try:
                forward.wait(timeout=5)
            except subprocess.TimeoutExpired:
                forward.kill()
                forward.wait()
        try:
            kubectl_manifest(
                context, "delete", manifest, "--ignore-not-found=true", "--wait=true"
            )
        except subprocess.CalledProcessError as error:
            if not original_error:
                raise
            print(f"warning: baseline cleanup failed: {error}", file=sys.stderr)


def require_running(process, name):
    return_code = process.poll()
    if return_code is not None:
        raise RuntimeError(f"{name} exited with status {return_code}")


def run_oha(oha, url, duration, concurrency):
    output = subprocess.check_output(
        [
            str(oha),
            "-z",
            duration,
            "-c",
            str(concurrency),
            "--wait-ongoing-requests-after-deadline",
            "--stats-success-breakdown",
            "--no-tui",
            "--output-format",
            "json",
            "--insecure",
            url,
        ],
        text=True,
    )
    result = json.loads(output)
    client = {
        "summary": result["summary"],
        "metrics": result.get("metrics", {}),
        "statusCodeDistribution": result["statusCodeDistribution"],
        "errorDistribution": result["errorDistribution"],
        "successfulLatencyMs": {
            name: value * 1000 if value is not None else None
            for name, value in result["latencyPercentilesSuccessful"].items()
        },
    }
    client["httpSuccessRate"] = http_success_rate(client)
    return client


def http_success_rate(client):
    statuses = client.get("statusCodeDistribution", {})
    errors = client.get("errorDistribution", {})
    total = sum(statuses.values()) + sum(errors.values())
    if total == 0:
        return 0
    successful = sum(
        count for code, count in statuses.items() if 200 <= int(code) < 400
    )
    return successful / total


def warm_target(oha, url):
    output = subprocess.check_output(
        [
            str(oha),
            "-n",
            "20",
            "-c",
            "1",
            "--stats-success-breakdown",
            "--no-tui",
            "--output-format",
            "json",
            "--insecure",
            url,
        ],
        text=True,
    )
    client = json.loads(output)
    if http_success_rate(client) != 1:
        raise RuntimeError(
            "target warmup did not return only successful HTTP responses: "
            f"statuses={client.get('statusCodeDistribution', {})} "
            f"errors={client.get('errorDistribution', {})}"
        )


def display_number(value, precision=2):
    if value is None:
        return "n/a"
    return f"{value:.{precision}f}"


def add_metrics(*values):
    if any(value is None for value in values):
        return None
    return sum(values)


def target_resource_names(target):
    if target == "lampyr":
        return (
            "lampyrRouterCPU",
            "lampyrHomeCPU",
            "lampyrRouterMemoryBytes",
            "lampyrHomeMemoryBytes",
        )
    return ("baselineCPU", "baselineMemoryBytes")


def target_resources_available(target, metrics):
    return all(metrics[name] is not None for name in target_resource_names(target))


def atomic_write(path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w", dir=path.parent, prefix=f".{path.name}.", delete=False
        ) as handle:
            temporary = Path(handle.name)
            handle.write(content)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def write_json(result, path):
    atomic_write(path, json.dumps(result, indent=2, sort_keys=True) + "\n")


def write_report(result, path):
    lines = [
        "# Lampyr scale comparison",
        "",
        f"- Commit: `{result['metadata']['commit']}`",
        f"- Context: `{result['metadata']['context']}`",
        f"- Revision: `{result['metadata']['revision']}`",
        f"- Duration per run: `{result['metadata']['duration']}`",
        f"- Baseline image: `{result['metadata']['baselineImage']}`",
        f"- Lampyr path: {result['metadata']['networkPaths']['lampyr']}",
        f"- Kubernetes path: {result['metadata']['networkPaths']['kubernetes']}",
        "",
        "| Target | Concurrency | Requests/s | HTTP success | p50 ms | p95 ms | p99 ms | Platform errors | Shed | CPU cores | Peak memory MiB |",
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for scenario in result["scenarios"]:
        client = scenario["client"]
        metrics = scenario["platform"]
        if scenario["target"] == "lampyr":
            cpu = add_metrics(
                metrics["lampyrRouterCPU"], metrics["lampyrHomeCPU"]
            )
            memory = add_metrics(
                metrics["lampyrRouterMemoryBytes"],
                metrics["lampyrHomeMemoryBytes"],
            )
            errors = metrics.get("lampyrErrors")
            shed = metrics.get("lampyrShed")
        else:
            cpu = metrics["baselineCPU"]
            memory = metrics["baselineMemoryBytes"]
            errors = None
            shed = None
        latency = client.get("successfulLatencyMs", {})
        lines.append(
            "| {target} | {concurrency} | {rps:.2f} | {success:.4f} | "
            "{p50} | {p95} | {p99} | {errors} | {shed} | {cpu} | {memory} |".format(
                target=scenario["target"],
                concurrency=scenario["concurrency"],
                rps=client["summary"]["requestsPerSec"],
                success=client["httpSuccessRate"],
                p50=display_number(latency.get("p50")),
                p95=display_number(latency.get("p95")),
                p99=display_number(latency.get("p99")),
                errors=display_number(errors, 0),
                shed=display_number(shed, 0),
                cpu=display_number(cpu, 3),
                memory=display_number(
                    memory / (1 << 20) if memory is not None else None
                ),
            )
        )
    atomic_write(path, "\n".join(lines) + "\n")


def node_summary(value):
    nodes = []
    for item in value.get("items", []):
        metadata = item.get("metadata", {})
        status = item.get("status", {})
        info = status.get("nodeInfo", {})
        labels = metadata.get("labels", {})
        nodes.append(
            {
                "name": metadata.get("name"),
                "instanceType": labels.get("node.kubernetes.io/instance-type"),
                "allocatable": status.get("allocatable", {}),
                "kubeletVersion": info.get("kubeletVersion"),
                "operatingSystem": info.get("operatingSystem"),
                "osImage": info.get("osImage"),
                "kernelVersion": info.get("kernelVersion"),
                "containerRuntimeVersion": info.get("containerRuntimeVersion"),
            }
        )
    return nodes


def safe_url(value):
    parsed = urllib.parse.urlsplit(value)
    host = parsed.hostname or ""
    if ":" in host:
        host = f"[{host}]"
    netloc = host
    if parsed.port is not None:
        netloc += f":{parsed.port}"
    return urllib.parse.urlunsplit((parsed.scheme, netloc, parsed.path, "", ""))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--context", required=True)
    parser.add_argument("--service", required=True)
    parser.add_argument("--lampyr-url", required=True)
    parser.add_argument("--baseline-image", required=True)
    parser.add_argument("--namespace", default="default")
    parser.add_argument("--baseline-replicas", type=int, default=1)
    parser.add_argument("--duration", default="30s")
    parser.add_argument("--concurrency", default="1,4,16,64")
    parser.add_argument("--oha", default="build/tools/oha")
    parser.add_argument("--output", default="build/scale-results.json")
    args = parser.parse_args()

    seconds = duration_seconds(args.duration)
    concurrencies = [int(value) for value in args.concurrency.split(",")]
    if not concurrencies or any(value < 1 for value in concurrencies):
        raise ValueError("concurrency values must be positive integers")
    if args.baseline_replicas < 1:
        raise ValueError("baseline replicas must be a positive integer")
    oha = Path(args.oha)
    if not oha.is_file():
        raise FileNotFoundError(f"load generator not found: {oha}")
    revision = active_revision(args.context, args.service, args.namespace)
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    metadata = {
        "context": args.context,
        "service": args.service,
        "namespace": args.namespace,
        "revision": revision,
        "lampyrUrl": safe_url(args.lampyr_url),
        "baselineImage": args.baseline_image,
        "baselineReplicas": args.baseline_replicas,
        "duration": args.duration,
        "concurrency": concurrencies,
        "commit": command("git", "rev-parse", "HEAD"),
        "dirty": bool(command("git", "status", "--porcelain")),
        "ohaVersion": command(str(oha), "--version"),
        "networkPaths": {
            "lampyr": "configured ingress URL",
            "kubernetes": "kubectl Service port-forward",
        },
        "startedAt": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "nodes": node_summary(
            json.loads(
                command(
                    "kubectl",
                    "--context",
                    args.context,
                    "get",
                    "nodes",
                    "--output",
                    "json",
                )
            )
        ),
    }
    result = {"schema": 1, "metadata": metadata, "scenarios": []}
    with baseline_server(
        args.context, args.baseline_image, args.baseline_replicas, args.namespace
    ) as baseline:
        for target, url in [
            ("lampyr", args.lampyr_url),
            ("kubernetes", baseline["url"]),
        ]:
            if target == "kubernetes":
                require_running(baseline["process"], "baseline port-forward")
            warm_target(oha, url)
        time.sleep(6)
        for concurrency in concurrencies:
            for target, url in [
                ("lampyr", args.lampyr_url),
                ("kubernetes", baseline["url"]),
            ]:
                if target == "kubernetes":
                    require_running(baseline["process"], "baseline port-forward")
                snapshot_queries = metric_queries(
                    args.namespace, revision, "1s", BASELINE, 1
                )
                before = counter_snapshot(args.context, snapshot_queries)
                if before["lampyrRequests"] is None:
                    raise RuntimeError(
                        "Lampyr request metrics are missing after warmup; check the "
                        "observability add-on and requested namespace"
                    )
                started = datetime.datetime.now(datetime.timezone.utc)
                client = run_oha(oha, url, args.duration, concurrency)
                load_ended = datetime.datetime.now(datetime.timezone.utc)
                if target == "kubernetes":
                    require_running(baseline["process"], "baseline port-forward")
                platform = None
                for wait_seconds in (6, 5, 5, 5, 5, 5):
                    time.sleep(wait_seconds)
                    evaluated = time.time()
                    measured_seconds = evaluated - started.timestamp()
                    window = f"{math.ceil(measured_seconds * 1000)}ms"
                    queries = metric_queries(
                        args.namespace,
                        revision,
                        window,
                        BASELINE,
                        measured_seconds,
                    )
                    resources = {
                        name: prometheus_query(
                            args.context, queries[name], evaluated
                        )
                        for name in target_resource_names(target)
                    }
                    if target_resources_available(target, resources):
                        platform = collect_metrics(
                            args.context, queries, before, evaluated
                        )
                        break
                else:
                    raise RuntimeError(
                        f"Prometheus did not collect CPU and memory metrics for {target}"
                    )
                scenario = {
                    "target": target,
                    "concurrency": concurrency,
                    "startedAt": started.isoformat(),
                    "loadEndedAt": load_ended.isoformat(),
                    "metricsEvaluatedAt": datetime.datetime.fromtimestamp(
                        evaluated, datetime.timezone.utc
                    ).isoformat(),
                    "metricsWindowSeconds": measured_seconds,
                    "client": client,
                    "platform": platform,
                }
                result["scenarios"].append(scenario)
                write_json(result, output)
                write_report(result, output.with_suffix(".md"))
                p95 = display_number(client["successfulLatencyMs"].get("p95"))
                print(
                    f"{target} concurrency={concurrency} "
                    f"rps={client['summary']['requestsPerSec']:.2f} "
                    f"http_success={client['httpSuccessRate']:.4f} "
                    f"p95={p95}ms"
                )
    result["metadata"]["completedAt"] = datetime.datetime.now(
        datetime.timezone.utc
    ).isoformat()
    write_json(result, output)
    write_report(result, output.with_suffix(".md"))


if __name__ == "__main__":
    main()
