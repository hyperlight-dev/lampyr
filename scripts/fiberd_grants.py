"""What the release gates can observe of a Revision's fiberd-k8s-controller grant.

A grant Pod runs stock fiberd, which serves only gRPC over mutual TLS to its
one caller, fiber-router, so the gates can't ask the home for its running
Fibers or epoch. They read instead the CapacityGrant's conditions, the grant
Pod the controller labels, fiberd's audit records for that Pod, and
fiber-router's Prometheus metrics.
"""

import collections
import contextlib
import json
import queue
import re
import subprocess
import tempfile
import threading
import urllib.request

GRANTS = "capacitygrants.fiberd.io"
# fiberd-k8s-controller names a grant's Deployment after the grant.
GRANT_DEPLOYMENT_PREFIX = "fiberd-"
# fiberd keeps its audit spool under the grant Pod's -state directory.
AUDIT_SPOOL = "/var/lib/fiberd/private/audit.jsonl"
ROUTER_NAMESPACE = "lampyr-system"
ROUTER_SELECTOR = "app=fiber-router"
ROUTER_METRICS_PORT = 9090
# fiberd's grant UID is the grant Pod's own UID, not the CapacityGrant's:
# fiberd-k8s-controller mints each Pod a grant of its own, so a Pod's audit
# records carry that Pod's UID.
Home = collections.namedtuple("Home", "name uid")
# The audit events of a Fiber's birth: a fresh clone or a parked session
# restored.
STARTS = {"create", "resume"}


def kubectl(context, *args, namespace="default", timeout=60):
    return subprocess.check_output(
        ("kubectl", "--context", context, "--namespace", namespace, *args),
        text=True,
        timeout=timeout,
    ).strip()


def grant(context, revision):
    return json.loads(kubectl(context, "get", GRANTS, revision, "--output", "json"))


def grant_deployment(context, resource):
    """The Deployment fiberd-k8s-controller runs the grant's Pod with, or
    None before the controller has made it."""
    metadata = resource["metadata"]
    raw = kubectl(
        context, "get", "deployment", GRANT_DEPLOYMENT_PREFIX + metadata["name"],
        "--ignore-not-found", "--output", "json",
        namespace=metadata.get("namespace", "default"),
    )
    return json.loads(raw) if raw else None


def grant_settled(context, revision):
    """Reports whether the Revision's grant is ready. The Deployment is read
    after the grant, so it is never older than the generation the grant
    reports."""
    resource = grant(context, revision)
    return grant_ready(resource, grant_deployment(context, resource))


def grant_ready(resource, deployment):
    """Reports whether the controller reports the grant's current spec Ready
    and its rollout finished, as Lampyr's serving controller reads it
    (capacityGrantReadiness)."""
    status = resource.get("status", {})
    if status.get("observedGeneration") != resource["metadata"].get("generation"):
        return False
    conditions = {
        condition.get("type"): condition for condition in status.get("conditions", [])
    }
    if not (
        conditions.get("Ready", {}).get("status") == "True"
        and conditions.get("Progressing", {}).get("reason") == "NewReplicaSetAvailable"
    ):
        return False
    # Just after a spec change the controller can still report the old
    # Pod's conditions under the new generation, so only the Deployment
    # shows whether the replacement Pod is the one serving.
    return deployment is not None and deployment_rolled_out(deployment)


def deployment_rolled_out(deployment):
    """Reports whether the Deployment runs its current template on every
    replica and keeps no Pod of an older one."""
    replicas = deployment.get("spec", {}).get("replicas")
    if replicas is None:
        replicas = 1
    status = deployment.get("status", {})
    return (
        status.get("observedGeneration") == deployment["metadata"].get("generation")
        and status.get("updatedReplicas", 0) == replicas
        and status.get("replicas", 0) == replicas
        and status.get("availableReplicas", 0) >= replicas
    )


def pod_selector(resource):
    metadata = resource["metadata"]
    return f"fiberd.io/grant={metadata['name']},fiberd.io/grant-uid={metadata['uid']}"


def running_home(pods):
    """The name and UID of the one running grant Pod that isn't being
    deleted."""
    homes = [
        Home(pod["metadata"]["name"], pod["metadata"]["uid"])
        for pod in pods
        if pod.get("status", {}).get("phase") == "Running"
        and "deletionTimestamp" not in pod["metadata"]
    ]
    if len(homes) != 1:
        raise RuntimeError(
            f"expected one running grant Pod, got {[home.name for home in homes]!r}"
        )
    return homes[0]


def home_pod(context, resource):
    pods = json.loads(kubectl(
        context, "get", "pods", "--selector", pod_selector(resource), "--output", "json",
    ))["items"]
    return running_home(pods)


def audit_records(context, home):
    """fiberd's audit records for the grant Pod `home`."""
    raw = kubectl(context, "exec", f"pod/{home.name}", "--container", "fiberd", "--",
        "cat", AUDIT_SPOOL)
    return grant_records(raw, home.uid)


def grant_records(raw, pod_uid):
    """The records of `raw` that fiberd stamped with the grant Pod's UID.
    fiberd's Fence has no JSON tags, so its fields keep their Go names."""
    records = []
    for line in raw.splitlines():
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        if record.get("fence", {}).get("GrantUID") == pod_uid:
            records.append(record)
    return records


def fibers_started(records):
    return sum(1 for record in records if record.get("event") in STARTS)


def epoch(records):
    """fiberd raises its epoch on every start and admits the grant again
    under it, so the newest epoch in the spool is the running fiberd's."""
    return max((record["fence"].get("Epoch", 0) for record in records), default=0)


_SAMPLE = re.compile(r"^lampyr_router_fibers_busy\{(?P<labels>[^}]*)\}\s+(?P<value>\S+)$")
_LABEL = re.compile(r'(\w+)="((?:[^"\\]|\\.)*)"')


def parse_fibers_busy(metrics, namespace, revision):
    """The Revision's lampyr_router_fibers_busy in one scrape. The router
    creates the series on the first Fiber, so no series is zero."""
    total = 0.0
    for line in metrics.splitlines():
        match = _SAMPLE.match(line.strip())
        if not match:
            continue
        labels = dict(_LABEL.findall(match.group("labels")))
        if labels.get("namespace") == namespace and labels.get("revision") == revision:
            total += float(match.group("value"))
    return int(total)


@contextlib.contextmanager
def port_forward(context, namespace, target, port, timeout=30):
    # port-forward reaches the Pod through the kubelet, so the router's
    # NetworkPolicy, which admits no scraper, doesn't apply.
    # kubectl blocks once a pipe nobody reads fills, so stderr goes to a
    # file and stdout is drained; a port-forward that never reports its
    # port fails the gate after `timeout` instead of hanging it.
    with tempfile.TemporaryFile(mode="w+") as errors:
        process = subprocess.Popen(
            ("kubectl", "--context", context, "--namespace", namespace,
                "port-forward", "--address", "127.0.0.1", target, f":{port}"),
            stdout=subprocess.PIPE,
            stderr=errors,
            text=True,
        )
        lines = queue.Queue()

        def drain():
            for output in process.stdout:
                lines.put(output)
            lines.put("")

        threading.Thread(target=drain, daemon=True).start()
        try:
            try:
                line = lines.get(timeout=timeout)
            except queue.Empty:
                line = ""
            match = re.search(r"127\.0\.0\.1:(\d+)", line)
            if not match:
                stop(process)
                errors.seek(0)
                reason = line.strip() or errors.read().strip() or f"no port within {timeout}s"
                raise RuntimeError(f"port-forward to {target} failed: {reason}")
            yield int(match.group(1))
        finally:
            stop(process)


def stop(process):
    process.terminate()
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()


def router_fibers_busy(context, revision, namespace="default"):
    """Fibers serving a request for the Revision right now, across every
    fiber-router Pod. Idle pooled reuse Fibers aren't counted."""
    pods = kubectl(context, "get", "pods", "--selector", ROUTER_SELECTOR,
        "--output", "name", namespace=ROUTER_NAMESPACE).split()
    if not pods:
        raise RuntimeError("no fiber-router Pod to scrape")
    total = 0
    for pod in pods:
        with port_forward(context, ROUTER_NAMESPACE, pod, ROUTER_METRICS_PORT) as port:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/metrics", timeout=10) as response:
                total += parse_fibers_busy(response.read().decode(), namespace, revision)
    return total
