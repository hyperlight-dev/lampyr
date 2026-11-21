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
import uuid


BASELINE = "lampyr-scale-baseline"
REQUEST_TIMEOUT = "10s"
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


def prometheus_results(context, query, evaluation_time=None):
    parameters = {"query": query}
    if evaluation_time is not None:
        parameters["time"] = f"{evaluation_time:.3f}"
    path = PROMETHEUS_PROXY + "?" + urllib.parse.urlencode(parameters)
    result = json.loads(command("kubectl", "--context", context, "get", "--raw", path))
    if result.get("status") != "success":
        raise RuntimeError(f"Prometheus query failed: {result}")
    return result["data"]["result"]


def prometheus_query(context, query, evaluation_time=None):
    values = prometheus_results(context, query, evaluation_time)
    if not values:
        return None
    if len(values) != 1:
        raise RuntimeError(f"Prometheus query returned {len(values)} series: {query}")
    value = float(values[0]["value"][1])
    return value if math.isfinite(value) else None


def router_scrapes(context, evaluation_time):
    values = prometheus_results(
        context,
        'timestamp(up{job="fiber-router"}) * (up{job="fiber-router"} == bool 1)',
        evaluation_time,
    )
    if not values:
        raise RuntimeError("Prometheus router targets are missing")
    scrapes = {}
    for item in values:
        timestamp = float(item["value"][1])
        if not math.isfinite(timestamp):
            raise RuntimeError("Prometheus router scrape timestamp is not finite")
        identity = tuple(sorted(item["metric"].items()))
        scrapes[identity] = timestamp
    return scrapes


def resource_inputs(namespace, revision, baseline):
    baseline_pods = f'namespace="{namespace}",pod=~"{baseline}-.*"'
    phase = f'kube_pod_status_phase{{namespace="{namespace}",phase="Running"}}'
    return {
        "lampyrRouter": {
            "namespace": "lampyr-system",
            "selector": "app=fiber-router",
            "cpu": 'container_cpu_usage_seconds_total{namespace="lampyr-system",container="router"}',
            "memory": 'container_memory_working_set_bytes{namespace="lampyr-system",container="router"}',
            "phase": 'kube_pod_status_phase{namespace="lampyr-system",phase="Running"}',
        },
        "lampyrHome": {
            "namespace": namespace,
            "selector": f"lampyr.dev/home=true,serving.knative.dev/revision={revision}",
            "cpu": f'container_cpu_usage_seconds_total{{namespace="{namespace}",container="fiberd"}}',
            "memory": f'container_memory_working_set_bytes{{namespace="{namespace}",container="fiberd"}}',
            "phase": phase,
            "labels": (
                f'kube_pod_labels{{namespace="{namespace}",label_lampyr_dev_home="true",'
                f'label_serving_knative_dev_revision="{revision}"}}'
            ),
        },
        "baseline": {
            "namespace": namespace,
            "selector": f"app={baseline}",
            "cpu": f'container_cpu_usage_seconds_total{{{baseline_pods},container="app"}}',
            "memory": f'container_memory_working_set_bytes{{{baseline_pods},container="app"}}',
            "phase": phase,
        },
    }


def selected_resource(selector, inputs):
    membership = f'({inputs["phase"]} == 1)'
    if "labels" in inputs:
        membership = (
            f'({inputs["labels"]} * on(namespace,pod) group_left() {membership})'
        )
    return f"{selector} * on(namespace,pod) group_left() {membership}"


def resource_sample_queries(namespace, revision, baseline, target):
    inputs = resource_inputs(namespace, revision, baseline)
    prefixes = {
        "lampyr": ("lampyrRouter", "lampyrHome"),
        "kubernetes": ("baseline",),
    }[target]
    queries = {
        "kubeState": (
            'timestamp(up{job="kube-state-metrics"}) '
            '* (up{job="kube-state-metrics"} == bool 1)'
        ),
    }
    for prefix in prefixes:
        resource = inputs[prefix]
        selected_cpu = selected_resource(resource["cpu"], resource)
        selected_memory = selected_resource(resource["memory"], resource)
        selected = f"({selected_cpu}) or ({selected_memory})"
        pods = f"group by(namespace,pod) ({selected})"
        nodes = f"group by(job,instance,node) ({selected})"
        for kind in ("cpu", "memory"):
            queries[f"{prefix}{kind.title()}"] = selected_resource(
                f'timestamp({resource[kind]})', resource
            )
        for kind in ("phase", "labels"):
            if kind in resource:
                queries[f"{prefix}{kind.title()}"] = (
                    f'timestamp({resource[kind]}) '
                    f"* on(namespace,pod) group_left() ({pods})"
                )
        queries[f"{prefix}Cadvisor"] = (
            'timestamp(up{job="kubernetes-cadvisor"}) '
            '* (up{job="kubernetes-cadvisor"} == bool 1) '
            f"and on(job,instance,node) ({nodes})"
        )
    return queries


def resource_samples(context, queries, evaluation_time):
    query = " or ".join(
        f'label_replace(({expression}), "scale_input", "{name}", "", "")'
        for name, expression in queries.items()
    )
    values = prometheus_results(context, query, evaluation_time)
    samples = {}
    sources = set()
    containers = {}
    for item in values:
        source = item["metric"].get("scale_input")
        if source not in queries:
            raise RuntimeError(f"Prometheus returned an unknown resource input: {source}")
        timestamp = float(item["value"][1])
        if not math.isfinite(timestamp):
            raise RuntimeError("Prometheus resource sample timestamp is not finite")
        sources.add(source)
        samples[tuple(sorted(item["metric"].items()))] = timestamp
        identity = tuple(sorted(
            (label, value) for label, value in item["metric"].items()
            if label not in {"scale_input", "__name__", "cpu"}
        ))
        containers.setdefault(source, set()).add(identity)
    if sources != queries.keys():
        return None
    for source in queries:
        if source.endswith("Cpu"):
            memory = source.removesuffix("Cpu") + "Memory"
            if containers[source] != containers[memory]:
                return None
    return samples


def resource_inventory(context, namespace, revision, baseline, target):
    inputs = resource_inputs(namespace, revision, baseline)
    prefixes = {
        "lampyr": ("lampyrRouter", "lampyrHome"),
        "kubernetes": ("lampyrRouter", "baseline"),
    }[target]
    inventory = {}
    for prefix in prefixes:
        resource = inputs[prefix]
        pods = json.loads(command(
            "kubectl", "--context", context, "--namespace", resource["namespace"],
            "get", "pods", "--selector", resource["selector"],
            "--field-selector", "status.phase=Running", "--output", "json",
        ))
        inventory[prefix] = {
            (pod["metadata"]["namespace"], pod["metadata"]["name"])
            for pod in pods["items"]
        }
    return inventory


def resource_coverage(samples, queries, inventory, scrapes):
    if not samples:
        return False
    observed = {}
    for identity in samples:
        labels = dict(identity)
        if "namespace" in labels and "pod" in labels:
            observed.setdefault(labels["scale_input"], set()).add(
                (labels["namespace"], labels["pod"])
            )
    for prefix, pods in inventory.items():
        if not pods:
            return False
        for kind in ("Cpu", "Memory", "Phase", "Labels"):
            source = prefix + kind
            if source in queries and observed.get(source) != pods:
                return False
    routers = {
        ("lampyr-system", dict(identity).get("pod"))
        for identity in scrapes
    }
    return routers == inventory["lampyrRouter"]


def metric_queries(namespace, revision, window, baseline, window_seconds=30):
    revision_match = f'namespace="{namespace}",revision="{revision}"'
    counter_series = {
        "lampyrRequests": (
            "lampyr_router_request_duration_seconds_count"
            f"{{{revision_match}}}"
        ),
        "lampyrErrors": (
            "lampyr_router_request_duration_seconds_count"
            f'{{{revision_match},code!~"2..|3.."}}'
        ),
        "lampyrShed": f"lampyr_router_shed_total{{{revision_match}}}",
        "lampyrRetries": f"lampyr_router_clone_retries_total{{{revision_match}}}",
    }
    queries = {
        **{name: f"sum({series})" for name, series in counter_series.items()},
        **{
            f"{name}Resets": f"sum(resets({series}[{window}]))"
            for name, series in counter_series.items()
        },
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
        "lampyrPeakBusyFibers": (
            "max_over_time((sum(lampyr_router_fibers_busy"
            f"{{{revision_match}}}))[{window}:5s])"
        ),
        "lampyrPeakInFlight": (
            "max_over_time((sum(lampyr_router_requests_in_flight"
            f"{{{revision_match}}}))[{window}:5s])"
        ),
    }
    memory = {}
    for prefix, resource in resource_inputs(namespace, revision, baseline).items():
        cpu = selected_resource(f'increase({resource["cpu"]}[{window}])', resource)
        memory[prefix] = f'sum({selected_resource(resource["memory"], resource)})'
        queries[f"{prefix}CPU"] = f"sum({cpu}) / {window_seconds:.3f}"
        queries[f"{prefix}MemoryBytes"] = (
            f"max_over_time(({memory[prefix]})[{window}:5s])"
        )
    queries["lampyrMemoryBytes"] = (
        f'max_over_time((({memory["lampyrRouter"]}) + '
        f'({memory["lampyrHome"]}))[{window}:5s])'
    )
    return queries


COUNTER_METRICS = {
    "lampyrRequests",
    "lampyrErrors",
    "lampyrShed",
    "lampyrRetries",
}


def counter_snapshot(context, queries, evaluation_time=None):
    return {
        name: prometheus_query(context, query, evaluation_time)
        for name, query in queries.items()
        if name in COUNTER_METRICS
    }


def collect_metrics(context, queries, before, evaluation_time):
    collected = {}
    reset_metrics = {f"{name}Resets": name for name in COUNTER_METRICS}
    for name, query in queries.items():
        value = prometheus_query(context, query, evaluation_time)
        if name in reset_metrics:
            counter = reset_metrics[name]
            if value is None and before[counter] is not None:
                raise RuntimeError(f"Prometheus reset metrics are missing: {counter}")
            if value is not None and value > 0:
                raise RuntimeError(f"Prometheus counter reset during scenario: {counter}")
            collected[name] = value
            continue
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


def baseline_manifest(image, replicas, namespace, name):
    labels = {"app": name}
    return {
        "apiVersion": "v1",
        "kind": "List",
        "items": [
            {
                "apiVersion": "apps/v1",
                "kind": "Deployment",
                "metadata": {"name": name, "namespace": namespace},
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
                "metadata": {"name": name, "namespace": namespace},
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
    name = f"{BASELINE}-{uuid.uuid4().hex}"
    manifest = baseline_manifest(image, replicas, namespace, name)
    created = []
    forward = None
    try:
        for resource in manifest["items"]:
            kubectl_manifest(context, "create", resource)
            created.append(resource)
        subprocess.run(
            [
                "kubectl",
                "--context",
                context,
                "--namespace",
                namespace,
                "rollout",
                "status",
                f"deployment/{name}",
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
                f"service/{name}",
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
        yield {"name": name, "url": f"http://127.0.0.1:{port}/", "process": forward}
    finally:
        original_error = sys.exc_info()[0] is not None
        if forward is not None:
            forward.terminate()
            try:
                forward.wait(timeout=5)
            except subprocess.TimeoutExpired:
                forward.kill()
                forward.wait()
        if created:
            try:
                kubectl_manifest(
                    context,
                    "delete",
                    {"apiVersion": "v1", "kind": "List", "items": created},
                    "--ignore-not-found=true",
                    "--wait=true",
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
            "-t",
            REQUEST_TIMEOUT,
            "--wait-ongoing-requests-after-deadline",
            "--stats-success-breakdown",
            "--no-tui",
            "--output-format",
            "json",
            "--insecure",
            url,
        ],
        text=True,
        timeout=duration_seconds(duration) + duration_seconds(REQUEST_TIMEOUT) + 5,
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
            "-t",
            REQUEST_TIMEOUT,
            "--stats-success-breakdown",
            "--no-tui",
            "--output-format",
            "json",
            "--insecure",
            url,
        ],
        text=True,
        timeout=20 * duration_seconds(REQUEST_TIMEOUT) + 5,
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
            "lampyrMemoryBytes",
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
            memory = metrics.get("lampyrMemoryBytes")
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
    if seconds < 30:
        raise ValueError("duration must be at least 30s to measure CPU across scrapes")
    concurrencies = [int(value) for value in args.concurrency.split(",")]
    if not concurrencies or any(value < 1 for value in concurrencies):
        raise ValueError("concurrency values must be positive integers")
    if args.baseline_replicas != 1:
        raise ValueError("baseline must use one replica with Service port-forward")
    output = Path(args.output)
    if output.suffix.lower() == ".md":
        raise ValueError("JSON and Markdown output files must be distinct; do not use .md")
    oha = Path(args.oha).resolve()
    if not oha.is_file():
        raise FileNotFoundError(f"load generator not found: {oha}")
    revision = active_revision(args.context, args.service, args.namespace)
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
        "requestTimeout": REQUEST_TIMEOUT,
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
        metadata["baselineName"] = baseline["name"]
        for target, url in [
            ("lampyr", args.lampyr_url),
            ("kubernetes", baseline["url"]),
        ]:
            if target == "kubernetes":
                require_running(baseline["process"], "baseline port-forward")
            warm_target(oha, url)
        warmup_ended = datetime.datetime.now(datetime.timezone.utc).timestamp()
        time.sleep(6)
        for concurrency in concurrencies:
            for target, url in [
                ("lampyr", args.lampyr_url),
                ("kubernetes", baseline["url"]),
            ]:
                if target == "kubernetes":
                    require_running(baseline["process"], "baseline port-forward")
                snapshot_queries = metric_queries(
                    args.namespace, revision, "1s", baseline["name"], 1
                )
                sample_queries = resource_sample_queries(
                    args.namespace, revision, baseline["name"], target
                )
                for wait_seconds in (0, 5, 5, 5, 5, 5, 5):
                    if wait_seconds:
                        time.sleep(wait_seconds)
                    snapshot_time = time.time()
                    before_inventory = resource_inventory(
                        args.context, args.namespace, revision, baseline["name"], target
                    )
                    before_scrapes = router_scrapes(args.context, snapshot_time)
                    before_resources = resource_samples(
                        args.context, sample_queries, snapshot_time
                    )
                    if resource_coverage(
                        before_resources, sample_queries, before_inventory, before_scrapes
                    ) and all(
                        timestamp >= warmup_ended
                        for timestamp in (
                            *before_scrapes.values(), *before_resources.values()
                        )
                    ):
                        break
                else:
                    raise RuntimeError(
                        f"Prometheus did not collect fresh pre-load router and "
                        f"resource samples for {target}"
                    )
                measurement_started = min(
                    *before_scrapes.values(), *before_resources.values()
                ) - 0.001
                before = counter_snapshot(args.context, snapshot_queries, snapshot_time)
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
                    scrapes = router_scrapes(args.context, evaluated)
                    if scrapes.keys() != before_scrapes.keys() or any(
                        timestamp < load_ended.timestamp()
                        for timestamp in scrapes.values()
                    ):
                        continue
                    samples = resource_samples(args.context, sample_queries, evaluated)
                    inventory = resource_inventory(
                        args.context, args.namespace, revision, baseline["name"], target
                    )
                    if (
                        not resource_coverage(samples, sample_queries, inventory, scrapes)
                        or inventory != before_inventory
                        or samples.keys() != before_resources.keys()
                        or any(
                            timestamp < load_ended.timestamp()
                            for timestamp in samples.values()
                        )
                    ):
                        continue
                    measured_seconds = evaluated - measurement_started
                    window = f"{math.ceil(measured_seconds * 1000)}ms"
                    queries = metric_queries(
                        args.namespace,
                        revision,
                        window,
                        baseline["name"],
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
                        f"Prometheus did not collect fresh post-load router and "
                        f"resource samples with CPU and memory metrics for {target}"
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
