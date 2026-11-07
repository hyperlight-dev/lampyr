import hashlib
import json
import os
from pathlib import Path
import re
import subprocess

import hyperlight
import yaml

from process import ProviderError, bounded_run, fail


OWNER_LABEL = "lampyr.dev/installation-id"
WORKSPACE = Path("/workspace/k3d-config.yaml")
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


def registry_name(request):
    return "lampyr-platform-" + request["installationId"]


def registry_port(request):
    saved = state("platform-registry")
    return request["config"].get("registryPort", saved["port"] if saved else 5002)


def image_volume(request, create=False):
    name = "k3d-" + request["config"]["name"] + "-images"
    names = tool("docker", "volume", "ls", "--filter", "name=^" + name + "$",
                 "--format", "{{.Name}}").splitlines()
    if name not in names:
        if not create:
            return None
        # A missing volume can't have been replaced. Forget its old identity first,
        # so a volume created here is adoptable even if this call fails later.
        save(None, "image-volume")
        # app=k3d lets `k3d cluster delete` find and remove the volume.
        tool("docker", "volume", "create", "--label", "app=k3d",
             "--label", OWNER_LABEL + "=" + request["installationId"],
             "--label", "k3d.cluster=" + request["config"]["name"], name)
    volume = json.loads(tool("docker", "volume", "inspect", name))[0]
    if (volume.get("Labels") or {}).get(OWNER_LABEL) != request["installationId"]:
        fail("not-owned", "k3d image volume does not belong to this installation")
    saved = state("image-volume")
    identity = {"name": name, "createdAt": volume["CreatedAt"]}
    if saved and saved != identity:
        fail("ownership-mismatch", "k3d image volume has been replaced")
    if create:
        save(identity, "image-volume")
    return name


def verify_cluster_volumes(request):
    # `k3d cluster delete` removes every volume labelled app=k3d and
    # k3d.cluster=NAME, so all of them must be this installation's image volume
    # (whose owner label callers check with image_volume()).
    name = "k3d-" + request["config"]["name"] + "-images"
    for volume in tool("docker", "volume", "ls", "--filter", "label=app=k3d",
                       "--filter", "label=k3d.cluster=" + request["config"]["name"],
                       "--format", "{{.Name}}").splitlines():
        if volume != name:
            fail("not-owned", f"k3d would delete volume {volume}, which this installation does not own")


def tool_result(program, *args):
    if program not in ("k3d", "docker"):
        fail("invalid-tool", "Infrastructure operations may execute only packaged k3d and Docker")
    return bounded_run(
        ["/usr/local/bin/" + program, *args], env=dict(os.environ),
        timeout=300, stdout_limit=2 * 1024 * 1024, stderr_limit=24 * 1024,
    )


def tool(program, *args):
    result = tool_result(program, *args)
    if result.returncode:
        fail("tool-failed", f"Packaged {program} exited {result.returncode}: {result.stderr.strip()}")
    return result.stdout


def validate(request):
    config = request.get("config")
    if not isinstance(config, dict):
        fail("invalid-config", "k3d configuration must be an object")
    allowed = {
        "name", "interactive", "context", "deleteCluster", "images", "fiberdImage",
        "routerImage", "domain", "httpsPort", "eventing", "teardown",
        "source", "httpPort", "registryPort",
    }
    unknown = config.keys() - allowed
    if unknown:
        fail("invalid-config", "Unknown k3d configuration keys: " + ", ".join(sorted(map(str, unknown))))
    name = config.get("name")
    if not isinstance(name, str) or not re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,30}[a-z0-9])?", name):
        fail("invalid-config", "k3d cluster name must be a DNS label of at most 32 characters")
    if not re.fullmatch(r"[0-9a-f]{32}", request.get("installationId", "")):
        fail("invalid-config", "Invalid installation identity")
    for key in ("interactive", "deleteCluster", "eventing", "teardown"):
        if key in config and type(config[key]) is not bool:
            fail("invalid-config", f"{key} must be a boolean")
    for key in ("httpPort", "httpsPort", "registryPort"):
        minimum = 1 if key == "registryPort" else 0
        if key in config and (type(config[key]) is not int or not minimum <= config[key] <= 65535):
            fail("invalid-config", f"{key} must be an integer from {minimum} to 65535")
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


def configuration(source, request):
    try:
        document = yaml.safe_load(source)
    except yaml.YAMLError as error:
        fail("invalid-source", f"Cannot parse k3d configuration: {error}")
    if (
        not isinstance(document, dict) or document.get("kind") != "Simple"
        or document.get("apiVersion") != "k3d.io/v1alpha5"
    ):
        fail("invalid-source", "k3d configuration must describe a v1alpha5 Simple cluster")
    if document.get("metadata", {}).get("name") != request["config"]["name"]:
        fail("invalid-source", "k3d source name must match the installation name")
    if document.get("network") != network_name(request):
        fail("invalid-source", "k3d source must retain the installation's Docker network")
    api = document.get("kubeAPI", {})
    if api.get("host") != "127.0.0.1" or api.get("hostIP") != "127.0.0.1":
        fail("invalid-source", "k3d source must publish the Kubernetes API on loopback")
    labels = document.get("options", {}).get("runtime", {}).get("labels", [])
    owner = {"label": OWNER_LABEL + "=" + request["installationId"],
             "nodeFilters": ["server:*", "agent:*"]}
    if owner not in labels:
        fail("invalid-source", "k3d source must retain ownership labels on every node")
    if document.get("registries"):
        fail("invalid-source", "The provider manages registries; use registryPort instead")
    ports = document.get("ports") or []
    if not any(
        isinstance(entry, dict) and re.fullmatch(r"127\.0\.0\.1:[0-9]+:30443(?:/tcp)?", str(entry.get("port", "")))
        for entry in ports
    ):
        fail("invalid-source", "k3d source must publish ingress port 30443 on loopback")
    options = document.get("options", {}).get("k3d", {})
    if not options.get("disableLoadbalancer") or options.get("disableImageVolume"):
        fail("invalid-source", "k3d source must disable the load balancer and retain the image volume")
    return document


def initialize(request):
    if WORKSPACE.exists():
        configuration(WORKSPACE.read_text(encoding="utf-8"), request)
        return {"workspace": "k3d-config.yaml"}
    config = request["config"]
    source = config.get("source")
    if source is None:
        source = Path("/templates/k3d-config.yaml").read_text(encoding="utf-8")
        for key, value in {
            "NAME": config["name"], "NETWORK": network_name(request),
            "INSTALLATION_ID": request["installationId"],
            "HTTP_PORT": config.get("httpPort", 0), "HTTPS_PORT": config.get("httpsPort", 0),
        }.items():
            source = source.replace("__" + key + "__", str(value))
    else:
        # Initial customer source receives the same ownership fields as the template.
        document = yaml.safe_load(source)
        if not isinstance(document, dict):
            fail("invalid-source", "k3d source must be a mapping")
        metadata = document.setdefault("metadata", {})
        if metadata.get("name", config["name"]) != config["name"]:
            fail("invalid-source", "k3d source name must match the installation name")
        metadata["name"] = config["name"]
        document["network"] = network_name(request)
        labels = document.setdefault("options", {}).setdefault("runtime", {}).setdefault("labels", [])
        labels.append({"label": OWNER_LABEL + "=" + request["installationId"],
                       "nodeFilters": ["server:*", "agent:*"]})
        source = yaml.safe_dump(document, sort_keys=False)
    configuration(source, request)
    hyperlight.call("workspace.scaffold", {"k3d-config.yaml": source})
    return {"workspace": "k3d-config.yaml"}


def containers(request):
    return tool(
        "docker", "ps", "--all", "--filter", "label=k3d.cluster=" + request["config"]["name"],
        "--format", "{{.Names}}",
    ).splitlines()


def verify_network(request, create=False):
    name = network_name(request)
    names = tool("docker", "network", "ls", "--filter", "name=^" + name + "$", "--format", "{{.Name}}").splitlines()
    if name not in names:
        if not create:
            fail("not-owned", "The installation's Docker network is missing")
        tool("docker", "network", "create", "--label", OWNER_LABEL + "=" + request["installationId"], name)
    owner = tool("docker", "network", "inspect", "--format", '{{index .Labels "lampyr.dev/installation-id"}}', name).strip()
    if owner != request["installationId"]:
        fail("not-owned", "Docker network does not belong to this installation")


def verify_owner(request, saved, nodes, partial=False):
    if (
        not isinstance(saved, dict) or saved.get("name") != request["config"]["name"]
        or saved.get("installationId") != request["installationId"]
        or saved.get("phase") not in ("creating", "ready")
    ):
        fail("not-owned", "k3d cluster is not owned by this installation")
    verify_network(request)
    observed = {}
    for node in nodes:
        container = json.loads(tool("docker", "inspect", node))[0]
        labels = container["Config"].get("Labels") or {}
        tools_node = node == "k3d-" + request["config"]["name"] + "-tools"
        if (
            (not tools_node and labels.get(OWNER_LABEL) != request["installationId"])
            or labels.get("k3d.cluster") != request["config"]["name"]
            or network_name(request) not in container["NetworkSettings"]["Networks"]
        ):
            fail("ownership-mismatch", "k3d node does not belong to this installation")
        observed[node] = container["Id"]
    expected = saved.get("nodes")
    if partial and expected:
        # An interrupted delete may leave some recorded nodes; none may be new or
        # replaced. The tools container has no owner label, so it must be recorded too.
        if any(expected.get(node) != identity for node, identity in observed.items()):
            fail("ownership-mismatch", "k3d node containers have been replaced")
    elif expected and expected != observed:
        fail("ownership-mismatch", "k3d node containers have been replaced or removed")
    return observed


def registry(request, create=False):
    name = registry_name(request)
    saved = state("platform-registry")
    names = tool("docker", "container", "ls", "--all", "--filter", "name=^/" + name + "$", "--format", "{{.Names}}").splitlines()
    port = registry_port(request)
    if name not in names:
        if not create:
            return None
        # A missing registry can't have been replaced. Record the port with a
        # pending ID first, so a failed run leaves an adoptable owned container.
        saved = {"name": name, "id": None, "port": port}
        save(saved, "platform-registry")
        tool("docker", "run", "-d", "--name", name,
             "--label", OWNER_LABEL + "=" + request["installationId"],
             "--network", network_name(request), "--restart=always",
             "-p", f"127.0.0.1:{port}:5000", "registry:2")
    container = json.loads(tool("docker", "inspect", name))[0]
    if (container["Config"].get("Labels") or {}).get(OWNER_LABEL) != request["installationId"]:
        fail("not-owned", "Local registry does not belong to this installation")
    if saved and saved["id"] is not None and saved["id"] != container["Id"]:
        fail("ownership-mismatch", "Local registry container has been replaced")
    if create:
        if container["HostConfig"]["PortBindings"].get("5000/tcp") != [
            {"HostIp": "127.0.0.1", "HostPort": str(port)}
        ]:
            fail("invalid-config", "Registry port changed; explicitly delete and recreate the installation")
        if network_name(request) not in container["NetworkSettings"]["Networks"]:
            fail("ownership-mismatch", "Local registry is outside the installation network")
        save({"name": name, "id": container["Id"], "port": port}, "platform-registry")
        tool("docker", "start", name)
    return name


def published_port(nodes, port):
    for node in nodes:
        container = json.loads(tool("docker", "inspect", node))[0]
        for binding in container["NetworkSettings"]["Ports"].get(port) or []:
            if binding["HostIp"] == "127.0.0.1" and 1 <= int(binding["HostPort"]) <= 65535:
                return int(binding["HostPort"])
    fail("invalid-config", f"k3d source must publish {port} on loopback")


def ingress_port(nodes):
    return published_port(nodes, "30443/tcp")


def apply(request):
    name = request["config"]["name"]
    document = configuration(WORKSPACE.read_text(encoding="utf-8"), request)
    digest = hashlib.sha256(json.dumps(document, sort_keys=True).encode()).hexdigest()
    images = []
    for image in request["config"].get("images", []):
        if "@" in image and image.startswith("lampyr.local/"):
            fail("missing-image", f"Local image {image} must be referenced by tag so k3d can import it")
        result = tool_result("docker", "image", "inspect", image)
        if result.returncode == 0:
            images.append(image)
        elif not re.search(r"No such (image|object):", result.stderr):
            fail("image-inspect", result.stderr.strip())
        elif image.startswith("lampyr.local/"):
            fail("missing-image", f"Prebuilt image {image} is missing; build it before installation")
    saved = state()
    nodes = containers(request)
    if nodes:
        verify_owner(request, saved, nodes)
        # A partial cluster from a failed create has nothing to reconcile, so a
        # corrected source may replace it below.
        if saved["phase"] != "creating" and saved.get("sourceDigest") != digest:
            fail("changed-source", "k3d cannot reconcile changed source; explicitly delete and recreate the installation")
        if not image_volume(request):
            fail("ownership-mismatch", "The owned k3d image volume is missing")
        if saved["phase"] == "creating":
            verify_cluster_volumes(request)
            tool("k3d", "cluster", "delete", name)
            nodes = containers(request)
            if nodes:
                fail("delete-failed", "Incomplete owned k3d cluster still exists")
            if image_volume(request):
                fail("delete-failed", "Incomplete owned k3d image volume still exists")
            save(None, "image-volume")
    if not nodes:
        if saved and (saved.get("name") != name or saved.get("installationId") != request["installationId"]):
            fail("not-owned", "Saved k3d installation has a different identity")
        verify_network(request, create=True)
        # k3d reuses its predictable tools-container name; never adopt a preexisting one.
        tools_name = "k3d-" + name + "-tools"
        if tool("docker", "container", "ls", "--all", "--filter",
                "name=^/" + tools_name + "$", "--format", "{{.Names}}").strip():
            fail("not-owned", "A k3d tools container already exists outside this installation")
        image_volume(request, create=True)
    registry_host = registry(request, create=True)
    port = registry_port(request)
    mirrors = {f"localhost:{port}": {"registry": registry_host + ":5000", "plainHttp": True}}
    if not nodes:
        saved = {"name": name, "installationId": request["installationId"],
                 "phase": "creating", "sourceDigest": digest}
        save(saved)
        # Keep generated registry settings out of customer-owned source.
        document["registries"] = {"config": yaml.safe_dump({
            "mirrors": {f"localhost:{port}": {"endpoint": [f"http://{registry_host}:5000"]}}
        })}
        effective = Path("/tmp/k3d-effective.yaml")
        effective.write_text(yaml.safe_dump(document, sort_keys=False), encoding="utf-8")
        hyperlight.call("progress.message", f"Creating k3d cluster {name} inside the provider guest")
        tool("k3d", "cluster", "create", name, "--config", str(effective),
             "--kubeconfig-update-default=false", "--kubeconfig-switch-context=false")
        nodes = containers(request)
        if not nodes:
            fail("create-failed", "k3d returned without creating nodes")
    observed = verify_owner(request, saved, nodes)
    saved.update(phase="ready", nodes=observed)
    save(saved)
    # k3d imports only images it finds by tag; digest references are left for
    # the node to pull, so they keep their pinned identity.
    importable = [image for image in images if "@" not in image]
    if importable:
        tool("k3d", "image", "import", "--mode", "direct", "--cluster", name, *importable)
    save({
        "options": {"httpsPort": ingress_port(nodes), "ingressPorts": INGRESS_PORTS,
                    "registryMirrors": mirrors},
        "before": [], "after": [], "cleanup": [],
    }, "runtime")
    return {"name": name, "ready": True}


def inspect(request):
    nodes = containers(request)
    if not nodes:
        return {"name": request["config"]["name"], "exists": False, "owned": False, "ready": False}
    saved = state()
    # Uninstall inspects before deleting, so it must accept the survivors of an
    # interrupted delete; everything else keeps the exact node set.
    verify_owner(request, saved, nodes, partial=request["config"].get("teardown") is True)
    return {"name": request["config"]["name"], "exists": True, "owned": True,
            "ready": saved["phase"] == "ready"}


def connect(request):
    nodes = containers(request)
    verify_owner(request, state(), nodes)
    if not nodes:
        fail("missing-cluster", "The owned k3d cluster has no nodes")
    name = request["config"]["name"]
    document = yaml.safe_load(tool("k3d", "kubeconfig", "get", name))
    original = "k3d-" + name
    if document.get("current-context") != original or len(document.get("contexts", [])) != 1:
        fail("invalid-connection", "k3d returned an unexpected kubeconfig")
    context = request["config"].get("context", original)
    document["contexts"][0]["name"] = context
    document["current-context"] = context
    cluster = document["contexts"][0]["context"]["cluster"]
    matches = [entry for entry in document.get("clusters", []) if entry["name"] == cluster]
    if len(matches) != 1:
        fail("invalid-connection", "k3d returned an unexpected kubeconfig cluster")
    # k3d records the requested API port (including 0), not Docker's allocated port.
    matches[0]["cluster"]["server"] = f"https://127.0.0.1:{published_port(nodes, '6443/tcp')}"
    return document


def delete(request):
    name = request["config"]["name"]
    nodes = containers(request)
    if nodes:
        verify_owner(request, state(), nodes, partial=True)
    if request["config"].get("deleteCluster", True) is not True:
        return {"name": name, "deleted": True, "clusterRetained": True}
    # Validate all resources before the first destructive operation.
    registry_host = registry(request)
    volume = image_volume(request)
    networks = tool("docker", "network", "ls", "--format", "{{.Name}}").splitlines()
    if network_name(request) in networks:
        verify_network(request)
    if nodes:
        verify_cluster_volumes(request)
        tool("k3d", "cluster", "delete", name)
        if containers(request):
            fail("delete-failed", "Owned k3d cluster still exists")
    if registry_host:
        tool("docker", "rm", "--force", registry_host)
    save(None, "platform-registry")
    # k3d normally removes the volume; a failed create may leave only the volume behind.
    if volume and image_volume(request):
        tool("docker", "volume", "rm", volume)
    save(None, "image-volume")
    if network_name(request) in networks:
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
            operations = {"init": initialize, "apply": apply, "inspect": inspect,
                          "connect": connect, "delete": delete}
            if operation not in operations:
                fail("unsupported-operation", f"Unsupported k3d operation: {operation}")
            result = operations[operation](request)
        return {"protocolVersion": 1, "status": "ok", "result": result}
    except ProviderError as error:
        return {"protocolVersion": 1, "status": "error", "code": error.code, "message": str(error)}
    except (OSError, ValueError, KeyError, TypeError, AttributeError, yaml.YAMLError, subprocess.TimeoutExpired) as error:
        return {"protocolVersion": 1, "status": "error", "code": "provider-failed", "message": str(error)}
