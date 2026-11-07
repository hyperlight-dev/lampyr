import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess

import hyperlight
import yaml

from process import ProviderError, bounded_run, fail, stop_process

OWNER_LABEL = "lampyr.dev/installation-id"
WORKSPACE = Path("/workspace/kind-config.yaml")
INGRESS_PORTS = [
    {"name": "http2", "port": 80, "protocol": "TCP", "targetPort": 8080, "nodePort": 30080},
    {"name": "https", "port": 443, "protocol": "TCP", "targetPort": 8443, "nodePort": 30443},
]


def state(key="cluster"):
    return hyperlight.call("state.read", key)


def save(value, key="cluster"):
    hyperlight.call("state.write", {"key": key, "value": value})


def network_name(request):
    return "lampyr-" + request["installationId"]


def tool_result(program, *args, request=None):
    if program not in ("kind", "docker"):
        fail("invalid-tool", "Infrastructure operations may execute only packaged Kind and Docker")
    environment = dict(os.environ)
    if request is not None:
        environment["KIND_EXPERIMENTAL_DOCKER_NETWORK"] = network_name(request)
    return bounded_run(
        ["/usr/local/bin/" + program, *args], env=environment,
        timeout=300,
        stdout_limit=2 * 1024 * 1024,
        stderr_limit=24 * 1024,
    )


def tool(program, *args, request=None):
    result = tool_result(program, *args, request=request)
    if result.returncode:
        fail("tool-failed", f"Packaged {program} exited {result.returncode}: {result.stderr.strip()}")
    return result.stdout


def validate(request):
    config = request.get("config")
    if not isinstance(config, dict):
        fail("invalid-config", "Kind configuration must be an object")
    # Persisted provider settings and shared platform fields accompany every lifecycle operation.
    allowed = {
        "name", "interactive", "context", "deleteCluster", "images", "fiberdImage",
        "routerImage", "domain", "httpsPort", "eventing", "teardown",
        "source", "httpPort", "registryPort",
    }
    unknown = config.keys() - allowed
    if unknown:
        fail("invalid-config", "Unknown Kind configuration keys: " + ", ".join(sorted(map(str, unknown))))
    name = config.get("name")
    if not isinstance(name, str) or not re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?", name):
        fail("invalid-config", "Kind cluster name must be a DNS label")
    if not re.fullmatch(r"[0-9a-f]{32}", request.get("installationId", "")):
        fail("invalid-config", "Invalid installation identity")
    for key in ("interactive", "deleteCluster", "eventing", "teardown"):
        if key in config and type(config[key]) is not bool:
            fail("invalid-config", f"{key} must be a boolean")
    for key in ("httpPort", "httpsPort"):
        if key in config and (type(config[key]) is not int or not 0 <= config[key] <= 65535):
            fail("invalid-config", f"{key} must be an integer from 0 to 65535")
    if "registryPort" in config and (
        type(config["registryPort"]) is not int or not 1 <= config["registryPort"] <= 65535
    ):
        fail("invalid-config", "registryPort must be an integer from 1 to 65535")
    if "context" in config and (
        not isinstance(config["context"], str) or not config["context"]
        or config["context"].startswith("-") or any(ord(char) < 32 for char in config["context"])
    ):
        fail("invalid-config", "Invalid Kubernetes context")
    images = config.get("images", [])
    if not isinstance(images, list) or any(
        not isinstance(image, str) or not image or image.startswith("-")
        or any(char.isspace() for char in image) for image in images
    ):
        fail("invalid-config", "images must contain valid prebuilt image references")


def configuration(source):
    try:
        document = yaml.safe_load(source)
    except yaml.YAMLError as error:
        fail("invalid-source", f"Cannot parse Kind configuration: {error}")
    if (
        not isinstance(document, dict) or document.get("kind") != "Cluster"
        or document.get("apiVersion") != "kind.x-k8s.io/v1alpha4"
    ):
        fail("invalid-source", "Kind configuration must describe a Kind v1alpha4 Cluster")
    nodes = document.get("nodes")
    if not isinstance(nodes, list) or not nodes or any(
        not isinstance(node, dict) or not isinstance(node.get("labels", {}), dict)
        for node in nodes
    ):
        fail("invalid-source", "Kind configuration must contain nodes with mapping labels")
    return document


def owned_source(request):
    document = configuration(WORKSPACE.read_text(encoding="utf-8"))
    if any(node.get("labels", {}).get(OWNER_LABEL) != request["installationId"] for node in document["nodes"]):
        fail("invalid-source", f"Every Kind node must retain its {OWNER_LABEL} label")
    if "name" in document and document["name"] != request["config"]["name"]:
        fail("invalid-source", "Kind source name must match the installation name")
    return document


def initialize(request):
    if WORKSPACE.exists():
        owned_source(request)
        return {"workspace": "kind-config.yaml"}
    config = request["config"]
    source = config.get("source")
    if source is None:
        source = Path("/templates/kind-config.yaml").read_text(encoding="utf-8")
        source = source.replace("__INSTALLATION_ID__", json.dumps(request["installationId"]))
        source = source.replace("__HTTP_PORT__", str(config.get("httpPort", 0)))
        source = source.replace("__HTTPS_PORT__", str(config.get("httpsPort", 0)))
    document = configuration(source)
    if "name" in document and document["name"] != config["name"]:
        fail("invalid-source", "Kind source name must match the installation name")
    for node in document["nodes"]:
        node.setdefault("labels", {})[OWNER_LABEL] = request["installationId"]
    hyperlight.call("workspace.scaffold", {"kind-config.yaml": yaml.safe_dump(document, sort_keys=False)})
    return {"workspace": "kind-config.yaml"}


def exists(name):
    return name in tool("kind", "get", "clusters").splitlines()


def verify_network(request, create=False):
    name = network_name(request)
    networks = tool("docker", "network", "ls", "--filter", "name=^" + name + "$", "--format", "{{.Name}}").splitlines()
    if name not in networks:
        if not create:
            fail("not-owned", "The installation's Docker network is missing")
        tool("docker", "network", "create", "--label", OWNER_LABEL + "=" + request["installationId"], name)
    owner = tool("docker", "network", "inspect", "--format", '{{index .Labels "lampyr.dev/installation-id"}}', name).strip()
    if owner != request["installationId"]:
        fail("not-owned", "Docker network does not belong to this installation")


def verify_owner(request, saved):
    name = request["config"]["name"]
    if (
        not isinstance(saved, dict) or saved.get("name") != name
        or saved.get("installationId") != request["installationId"]
        or saved.get("phase") not in ("creating", "ready")
    ):
        fail("not-owned", f"Kind cluster {name} is not owned by this installation")
    verify_network(request)
    nodes = tool("kind", "get", "nodes", "--name", name, request=request).splitlines()
    if not nodes:
        fail("not-owned", "The owned Kind cluster has no nodes")
    observed = {}
    for node in nodes:
        if not re.fullmatch(re.escape(name) + r"-[a-z0-9-]+", node):
            fail("ownership-mismatch", "Kind returned an unexpected node identity")
        container = json.loads(tool("docker", "inspect", node))[0]
        if (
            container["Config"]["Labels"].get("io.x-k8s.kind.cluster") != name
            or network_name(request) not in container["NetworkSettings"]["Networks"]
        ):
            fail("ownership-mismatch", "Kind node is outside the installation's Docker network")
        observed[node] = container["Id"]
    if saved.get("nodes") and observed != saved["nodes"]:
        fail("ownership-mismatch", "Kind node containers have been replaced")
    return observed


def ensure_registry(request):
    owned_name = "lampyr-platform-" + request["installationId"]
    saved = state("platform-registry")
    owned = saved.get("owned", True) if saved else True
    name = saved["name"] if saved else owned_name
    if owned and name != owned_name:
        fail("not-owned", "Saved local registry has another identity")
    port = request["config"].get("registryPort", saved["port"] if saved else 5002)
    binding = [{"HostIp": "127.0.0.1", "HostPort": str(port)}]
    if not saved:
        candidates = tool("docker", "ps", "--filter", f"publish={port}", "--format", "{{.Names}}").splitlines()
        for candidate in candidates:
            container = json.loads(tool("docker", "inspect", candidate))[0]
            if container["HostConfig"]["PortBindings"].get("5000/tcp") == binding:
                name = candidate
                owned = name == owned_name
                break
    if owned:
        existing = tool("docker", "container", "ls", "--all", "--filter", "name=^/" + name + "$", "--format", "{{.Names}}").splitlines()
        if name not in existing:
            tool("docker", "run", "-d", "--name", name, "--label", OWNER_LABEL + "=" + request["installationId"],
                 "--restart=always", "-p", f"127.0.0.1:{port}:5000", "registry:2")
            # The registry we just created cannot retain the removed container's ID.
            saved = None
    container = json.loads(tool("docker", "inspect", name))[0]
    owner = (container["Config"].get("Labels") or {}).get(OWNER_LABEL)
    if owned:
        if owner != request["installationId"]:
            fail("not-owned", "Local registry is not owned by this installation")
    else:
        if owner is not None:
            fail("not-owned", "Cannot reuse a registry owned by another installation")
        image = container["Config"].get("Image", "").split("@", 1)[0]
        if image not in ("registry:2", "registry:3", "docker.io/library/registry:2", "docker.io/library/registry:3"):
            fail("invalid-registry", "The requested port is not served by a supported registry container")
        if not container["State"]["Running"]:
            fail("invalid-registry", "The shared local registry must be running")
    if saved and saved.get("id") != container["Id"]:
        fail("ownership-mismatch", "Local registry container has been replaced")
    actual = container["HostConfig"]["PortBindings"].get("5000/tcp")
    if actual != binding:
        fail("invalid-config", "Local registry does not use the requested loopback port")
    save({"name": name, "port": port, "id": container["Id"], "owned": owned}, "platform-registry")
    if owned:
        tool("docker", "start", name)
    if network_name(request) not in container["NetworkSettings"]["Networks"]:
        tool("docker", "network", "connect", network_name(request), name)
    path = Path("/tmp/registry-hosts.toml")
    path.write_text(
        f'server = "http://{name}:5000"\n\n[host."http://{name}:5000"]\n'
        '  capabilities = ["pull", "resolve", "push"]\n', encoding="utf-8",
    )
    for node in verify_owner(request, state()):
        directory = f"/etc/containerd/certs.d/localhost:{port}"
        tool("docker", "exec", node, "mkdir", "-p", directory)
        tool("docker", "cp", str(path), f"{node}:{directory}/hosts.toml")
    return {f"localhost:{port}": {"registry": name + ":5000", "plainHttp": True}}


def ingress_port(request, nodes):
    for node in nodes:
        container = json.loads(tool("docker", "inspect", node))[0]
        for binding in container["NetworkSettings"]["Ports"].get("30443/tcp") or []:
            if binding["HostIp"] == "127.0.0.1" and 1 <= int(binding["HostPort"]) <= 65535:
                return int(binding["HostPort"])
    fail("invalid-config", "Kind source must publish HTTPS ingress on loopback")


def apply(request):
    name = request["config"]["name"]
    document = owned_source(request)
    images = []
    for image in request["config"].get("images", []):
        result = tool_result("docker", "image", "inspect", image)
        if result.returncode == 0:
            images.append(image)
        elif not re.search(r"No such (image|object):", result.stderr):
            fail("image-inspect", result.stderr.strip())
        elif image.startswith("lampyr.local/"):
            fail("missing-image", f"Prebuilt image {image} is missing; build it before installation")
    digest = hashlib.sha256(json.dumps(document, sort_keys=True).encode()).hexdigest()
    saved = state()
    present = exists(name)
    if present and saved and saved.get("phase") == "creating":
        verify_owner(request, saved)
        if saved.get("sourceDigest") and saved["sourceDigest"] != digest:
            fail("changed-source", "Kind cannot resume a partial cluster created from different configuration")
        hyperlight.call("progress.message", f"Recreating incomplete Kind cluster {name}")
        tool("kind", "delete", "cluster", "--name", name, request=request)
        if exists(name):
            fail("delete-failed", "Incomplete owned Kind cluster still exists")
        present = False
    if not present:
        if saved and (saved.get("name") != name or saved.get("installationId") != request["installationId"]):
            fail("not-owned", "Saved Kind installation has a different identity")
        verify_network(request, create=True)
        saved = {
            "name": name,
            "installationId": request["installationId"],
            "phase": "creating",
            "sourceDigest": digest,
        }
        save(saved)
        hyperlight.call("progress.message", f"Creating Kind cluster {name} inside the provider guest")
        tool("kind", "create", "cluster", "--name", name, "--config", str(WORKSPACE),
             "--kubeconfig", "/tmp/cluster-kubeconfig", "--wait", "180s", request=request)
    nodes = verify_owner(request, saved)
    if saved.get("sourceDigest") and saved["sourceDigest"] != digest:
        fail("changed-source", "Kind cannot reconcile changed cluster configuration; explicitly delete and recreate this installation")
    saved.update(phase="ready", nodes=nodes, sourceDigest=digest)
    save(saved)
    for image in images:
        # Parallel bulk uploads can starve TLS handshakes in the cooperative guest.
        for node in nodes:
            tool("kind", "load", "docker-image", "--name", name, "--nodes", node, image, request=request)
    runtime = {
        "options": {"httpsPort": ingress_port(request, nodes), "ingressPorts": INGRESS_PORTS,
                    "registryMirrors": ensure_registry(request)},
        "before": [], "after": [], "cleanup": [],
    }
    save(runtime, "runtime")
    return {"name": name, "ready": True}


def inspect(request):
    name = request["config"]["name"]
    if not exists(name):
        return {"name": name, "exists": False, "owned": False, "ready": False}
    saved = state()
    verify_owner(request, saved)
    return {"name": name, "exists": True, "owned": True, "ready": saved.get("phase") == "ready"}


def connect(request):
    name = request["config"]["name"]
    verify_owner(request, state())
    document = yaml.safe_load(tool("kind", "get", "kubeconfig", "--name", name, request=request))
    original = "kind-" + name
    if document.get("current-context") != original or len(document.get("contexts", [])) != 1:
        fail("invalid-connection", "Kind returned an unexpected kubeconfig")
    context = request["config"].get("context", original)
    document["contexts"][0]["name"] = context
    document["current-context"] = context
    return document


def delete(request):
    name = request["config"]["name"]
    saved = state()
    if request["config"].get("deleteCluster", True) is not True:
        if exists(name):
            verify_owner(request, saved)
        return {"name": name, "deleted": True, "clusterRetained": True}
    if exists(name):
        verify_owner(request, saved)
        tool("kind", "delete", "cluster", "--name", name, request=request)
        if exists(name):
            fail("delete-failed", "Owned Kind cluster still exists")
    registry = state("platform-registry")
    expected = "lampyr-platform-" + request["installationId"]
    shared = registry is not None and not registry.get("owned", True)
    if shared:
        registry_name = registry["name"]
        existing = tool("docker", "container", "ls", "--all", "--filter", "name=^/" + registry_name + "$", "--format", "{{.Names}}").splitlines()
        if registry_name in existing:
            container = json.loads(tool("docker", "inspect", registry_name))[0]
            if registry.get("id") != container["Id"]:
                fail("ownership-mismatch", "Shared local registry container has been replaced")
            if network_name(request) in container["NetworkSettings"]["Networks"]:
                tool("docker", "network", "disconnect", network_name(request), registry_name)
    elif registry and registry.get("name") != expected:
        fail("not-owned", "Saved local registry has another identity")
    existing = tool("docker", "container", "ls", "--all", "--filter", "name=^/" + expected + "$", "--format", "{{.Names}}").splitlines()
    if expected in existing:
        container = json.loads(tool("docker", "inspect", expected))[0]
        if container["Config"].get("Labels", {}).get(OWNER_LABEL) != request["installationId"]:
            fail("not-owned", "Local registry belongs to another installation")
        if registry and not shared and registry.get("id") != container["Id"]:
            fail("ownership-mismatch", "Local registry container has been replaced")
        tool("docker", "rm", "--force", expected)
    save(None, "platform-registry")
    networks = tool("docker", "network", "ls", "--format", "{{.Name}}").splitlines()
    if network_name(request) in networks:
        verify_network(request)
        tool("docker", "network", "rm", network_name(request))
    save({"name": name, "installationId": request["installationId"], "phase": "deleted"})
    return {"name": name, "deleted": True, "clusterRetained": False}


def invoke(request):
    try:
        if request.get("protocolVersion") != 1:
            fail("protocol-version", "Unsupported provider protocol")
        operation = request.get("operation")
        if operation == "describe":
            result = json.loads(Path("/provider/manifest.json").read_text(encoding="utf-8"))
        else:
            validate(request)
            operations = {"init": initialize, "apply": apply, "inspect": inspect, "connect": connect, "delete": delete}
            if operation not in operations:
                fail("unsupported-operation", f"Unsupported Kind operation: {operation}")
            result = operations[operation](request)
        return {"protocolVersion": 1, "status": "ok", "result": result}
    except ProviderError as error:
        return {"protocolVersion": 1, "status": "error", "code": error.code, "message": str(error)}
    except (OSError, ValueError, KeyError, TypeError, subprocess.TimeoutExpired) as error:
        return {"protocolVersion": 1, "status": "error", "code": "provider-failed", "message": str(error)}
