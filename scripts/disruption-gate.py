#!/usr/bin/env python3

import argparse
import base64
from contextlib import contextmanager
import json
import os
import socket
import ssl
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request

import fiberd_grants

# What the gate measures on a grant Pod running stock fiberd, which serves
# its own status only over mutual TLS to fiber-router:
#
# - Active Fibers are fiber-router's lampyr_router_fibers_busy for the
#   Revision. A WebSocket counts while the router proxies it.
# - fiberd's epoch is the newest epoch in the grant Pod's audit records,
#   which fiberd stamps with the Pod's UID: fiberd raises it on every start
#   and admits the grant again under it, and the spool lives in the Pod's
#   state emptyDir, so it outlasts a restart.
# - A policy violation is a CapacityGrant whose fiberd image GrantPolicy
#   doesn't list, which the cluster's admission policy must refuse. fiberd
#   now fetches its grant from fiberd-k8s-controller's issuer and verifies
#   its signature, so there is no grant Secret to tamper with and no
#   admissions endpoint to read a home's rejection from.
CONTROL_PLANE_DEPLOYMENTS = (
    "lampyr-serving-controller",
    "webhook",
    "net-kourier-controller",
)


def run(*args, input_text=None, check=True):
    result = subprocess.run(
        args,
        input=input_text,
        text=True,
        capture_output=True,
        check=False,
    )
    if check and result.returncode:
        raise RuntimeError(
            f"command {args!r} failed ({result.returncode}): "
            f"{result.stderr.strip() or result.stdout.strip()}"
        )
    return result


def output(*args, input_text=None):
    return run(*args, input_text=input_text).stdout.strip()


def request(url, timeout=10):
    context = ssl.create_default_context()
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE
    with urllib.request.urlopen(url, timeout=timeout, context=context) as response:
        return response.status, response.read().decode()


def wait_request(url, timeout=120):
    deadline = time.time() + timeout
    error = None
    while time.time() < deadline:
        try:
            return request(url)
        except Exception as current:
            error = current
            time.sleep(1)
    raise RuntimeError(f"service did not recover: {error}")


def active_fibers(context, revision):
    return fiberd_grants.router_fibers_busy(context, revision)


def fiber_epoch(context, home):
    return fiberd_grants.epoch(fiberd_grants.audit_records(context, home))


def wait_epoch(context, home, before, timeout=120):
    deadline = time.monotonic() + timeout
    current = before
    while time.monotonic() < deadline:
        try:
            current = fiber_epoch(context, home)
        except subprocess.SubprocessError:
            # The fiberd container is restarting.
            current = before
        if current > before:
            break
        time.sleep(1)
    return current


def policy_probe(resource, image):
    """A dry-run copy of the active grant whose fiberd image is outside
    GrantPolicy, and nothing else changed, so only that rule can refuse it."""
    probe = {
        "apiVersion": resource["apiVersion"],
        "kind": resource["kind"],
        "metadata": {
            "generateName": "policy-probe-",
            "namespace": resource["metadata"]["namespace"],
        },
        "spec": json.loads(json.dumps(resource["spec"])),
    }
    containers = probe["spec"]["podTemplate"]["spec"]["containers"]
    fiberd = next(container for container in containers if container["name"] == "fiberd")
    fiberd["image"] = image
    return probe


# Digest-pinned, so only the repository check can refuse it.
POLICY_PROBE_IMAGE = "registry.invalid/lampyr-policy-probe/fiberd@sha256:" + "0" * 64
# The end of the message fiberd-k8s-controller's admission policy gives for
# a fiberd image outside GrantPolicy's spec.images, from its messageExpression
# and its fallback message alike. Any other refusal, such as one for the
# helper image or a missing CRD, says nothing about that rule.
IMAGE_RULE_REFUSAL = "is not under a repository listed in GrantPolicy default spec.images"


def policy_violation_rejected(context, resource):
    result = run(
        "kubectl", "--context", context, "--namespace", "default",
        "create", "--dry-run=server", "--filename", "-", "--output", "name",
        input_text=json.dumps(policy_probe(resource, POLICY_PROBE_IMAGE)),
        check=False,
    )
    return result.returncode != 0 and IMAGE_RULE_REFUSAL in result.stderr


@contextmanager
def paused_agent(context, home):
    pod = json.loads(output(
        "kubectl", "--context", context, "--namespace", "default",
        "get", "pod", home, "-o", "json",
    ))
    node = ("docker", "exec", pod["spec"]["nodeName"])
    # Pod status can still name the exited container immediately after restart.
    identifiers = output(
        *node, "crictl", "ps", "--state", "running", "--name", "^fiberd$",
        "--label", f"io.kubernetes.pod.uid={pod['metadata']['uid']}", "--quiet",
    ).split()
    if len(identifiers) != 1:
        raise RuntimeError(
            f"expected one running fiberd container for {home}, got {identifiers!r}"
        )
    identifier = identifiers[0]
    task = (*node, "ctr", "--namespace", "k8s.io", "tasks")
    # SIGSTOP sent inside the container cannot stop its PID-namespace init.
    run(*task, "pause", identifier)
    try:
        yield
    finally:
        run(*task, "resume", identifier)


def open_websocket(url):
    parsed = urllib.parse.urlsplit(url)
    host = parsed.hostname
    port = parsed.port or (443 if parsed.scheme == "https" else 80)
    address = "127.0.0.1" if host.endswith(".localhost") else host
    connection = socket.create_connection((address, port), timeout=15)
    try:
        if parsed.scheme == "https":
            context = ssl.create_default_context()
            context.check_hostname = False
            context.verify_mode = ssl.CERT_NONE
            connection = context.wrap_socket(connection, server_hostname=host)
        key = base64.b64encode(os.urandom(16)).decode()
        connection.sendall(
            (
                f"GET /ws HTTP/1.1\r\nHost: {parsed.netloc}\r\n"
                "Upgrade: websocket\r\nConnection: Upgrade\r\n"
                f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
            ).encode()
        )
        response = b""
        while b"\r\n\r\n" not in response:
            chunk = connection.recv(4096)
            if not chunk:
                raise RuntimeError("WebSocket connection closed before the upgrade")
            response += chunk
        if not response.startswith(b"HTTP/1.1 101 "):
            raise RuntimeError(f"WebSocket upgrade failed: {response!r}")
        return connection
    except Exception:
        connection.close()
        raise


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
    parser.add_argument("--service", required=True)
    parser.add_argument("--url", required=True)
    parser.add_argument("--context", default="kind-lampyr")
    parser.add_argument("--output", default="")
    parser.add_argument(
        "--placement-timeout-seconds",
        type=int,
        default=5,
        help="Require a placement HTTP 503 within this many seconds (default: 5)",
    )
    args = parser.parse_args()
    if args.placement_timeout_seconds <= 0:
        parser.error("--placement-timeout-seconds must be positive")

    service = json.loads(output(
        "kubectl",
        "--context",
        args.context,
        "-n",
        "default",
        "get",
        "service.serving.knative.dev",
        args.service,
        "-o",
        "json",
    ))
    revision = active_revision(service)
    grant_resource = fiberd_grants.grant(args.context, revision)
    home = fiberd_grants.home_pod(args.context, grant_resource)

    # Keep a real request active through the supported consumer during restart.
    connection = open_websocket(args.url)
    try:
        deadline = time.monotonic() + 10
        fibers_before_restart = active_fibers(args.context, revision)
        while fibers_before_restart == 0 and time.monotonic() < deadline:
            time.sleep(0.1)
            fibers_before_restart = active_fibers(args.context, revision)
        if fibers_before_restart == 0:
            raise RuntimeError("WebSocket did not retain an active fiber before restart")
        before_epoch = fiber_epoch(args.context, home)
        # PID 1 is tini, which passes the signal on to fiberd.
        run("kubectl", "--context", args.context, "--namespace", "default",
            "exec", f"pod/{home.name}", "--container", "fiberd", "--",
            "sh", "-c", "kill -TERM 1")
    finally:
        connection.close()
    after_epoch = wait_epoch(args.context, home, before_epoch)
    status, body = wait_request(args.url)
    restart_passed = (
        after_epoch > before_epoch
        and active_fibers(args.context, revision) == 0
        and status == 200
    )

    placement_failed_fast = False
    placement_status = None
    placement_error = None
    with paused_agent(args.context, home.name):
        started = time.perf_counter()
        try:
            placement_status, _ = request(args.url, timeout=args.placement_timeout_seconds)
        except urllib.error.HTTPError as error:
            placement_status = error.code
        except (urllib.error.URLError, TimeoutError, OSError) as error:
            placement_error = str(error)
        placement_elapsed = time.perf_counter() - started
        placement_failed_fast = (
            placement_status == 503
            and placement_elapsed < args.placement_timeout_seconds
        )
    recovered_status, _ = wait_request(args.url)

    bad_policy_rejected = policy_violation_rejected(args.context, grant_resource)
    wait_request(args.url)

    for controller in CONTROL_PLANE_DEPLOYMENTS:
        run(
            "kubectl",
            "--context",
            args.context,
            "-n",
            "knative-serving",
            "scale",
            f"deployment/{controller}",
            "--replicas=0",
        )
    outage_status, _ = request(args.url)
    for controller in CONTROL_PLANE_DEPLOYMENTS:
        run(
            "kubectl",
            "--context",
            args.context,
            "-n",
            "knative-serving",
            "scale",
            f"deployment/{controller}",
            "--replicas=1",
        )
    run(
        "kubectl",
        "--context",
        args.context,
        "-n",
        "knative-serving",
        "wait",
        "--for=condition=Available",
        *[f"deployment/{name}" for name in CONTROL_PLANE_DEPLOYMENTS],
        "--timeout=180s",
    )

    result = {
        "service": args.service,
        "revision": revision,
        "activeFibersBeforeRestart": fibers_before_restart,
        "agentRestart": restart_passed,
        "placementFailureFast": placement_failed_fast,
        "placementStatus": placement_status,
        "placementElapsedSeconds": round(placement_elapsed, 3),
        "placementTimeoutSeconds": args.placement_timeout_seconds,
        "placementError": placement_error,
        "placementRecovery": recovered_status == 200,
        "policyViolationRejected": bad_policy_rejected,
        "controlPlaneOutageServing": outage_status == 200,
        "activeFibersAfter": active_fibers(args.context, revision),
    }
    rendered = json.dumps(result, indent=2, sort_keys=True)
    print(rendered)
    if args.output:
        with open(args.output, "w", encoding="utf-8") as target:
            target.write(rendered + "\n")
    if not all(
        [
            result["agentRestart"],
            result["placementFailureFast"],
            result["placementRecovery"],
            result["policyViolationRejected"],
            result["controlPlaneOutageServing"],
            result["activeFibersAfter"] == 0,
        ]
    ):
        raise SystemExit("disruption gate failed")


if __name__ == "__main__":
    main()
